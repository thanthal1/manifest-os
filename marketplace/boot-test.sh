#!/usr/bin/env bash
# boot-test.sh — review ONE marketplace submission: static scan, then (opt) a
# real install + boot in a throwaway VM. See DESIGN.md for the full pipeline.
#
#   marketplace/boot-test.sh <manifest.json> [-i ISO] [--boot] [--fail-on SEV]
#
#   (default)   stage 1 only: run scan.py and print the verdict.
#   --boot      also stage 2: full `manifest provision` install + boot in a
#               throwaway VBox VM, with a warm package cache. Needs the runner
#               host (VirtualBox + a built ISO); see DESIGN.md.
#
# Exit non-zero if the static scan hits --fail-on (default HIGH) or the boot
# test fails — so a CI gate can reject the submission.
set -u
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"

MANIFEST="" ISO="" DO_BOOT=0 FAIL_ON="HIGH" KEEP=0
while [ $# -gt 0 ]; do case "$1" in
  -i) ISO="$2"; shift 2 ;;
  --boot) DO_BOOT=1; shift ;;
  --keep) KEEP=1; DO_BOOT=1; shift ;;   # keep the VM afterwards to open + explore
  --fail-on) FAIL_ON="$2"; shift 2 ;;
  -*) echo "unknown flag $1"; exit 2 ;;
  *) MANIFEST="$1"; shift ;;
esac; done
[ -z "$MANIFEST" ] && { grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 2; }

# ---- Stage 1: static scan (the cheap gate) --------------------------------
echo "### Stage 1 — static scan"
python "$here/scan.py" "$MANIFEST" --check-packages --fail-on "$FAIL_ON"
scan_rc=$?
if [ "$scan_rc" -ne 0 ]; then
  echo
  echo ">>> REJECTED at static scan (findings at/above $FAIL_ON). Not booting."
  exit 1
fi
echo ">>> Static scan clean (below $FAIL_ON)."
[ "$DO_BOOT" -eq 0 ] && { echo "(pass --boot to also run the VM boot test)"; exit 0; }

# ---- Stage 2: real install + boot in a throwaway VM -----------------------
# This is the deep check. It reuses the rig in scripts/audit-vms.sh; the pieces
# specific to *review* (behavioural capture, teardown) are called out in
# DESIGN.md. Below is the minimal install-and-observe with a warm cache.
echo
echo "### Stage 2 — boot test (throwaway VM)"
export MSYS_NO_PATHCONV=1
VBOX="${VBOX:-/c/Program Files/Oracle/VirtualBox/VBoxManage.exe}"
[ -z "$ISO" ] && ISO="$(ls -t "$repo"/dist/manifestos-*.iso 2>/dev/null | head -1)"
[ -z "$ISO" ] && { echo "no ISO — build one or pass -i ISO"; exit 2; }
win() { cygpath -m "$1" 2>/dev/null || echo "$1"; }

# --- Package cache (the "don't install every time" ask; see DESIGN.md) ------
# Every VM's downloads go through one caching proxy on the host, so the first
# boot test downloads each package once and every later test gets it from disk.
# If PACOLOCO_URL is already set (e.g. a real pacoloco on an Arch cache host —
# see cache-setup.sh) we use that; otherwise we auto-start the local
# cache-proxy.py (stdlib Python, caches into marketplace/pkg-cache/).
# 10.0.2.2 is the VirtualBox NAT alias for the host's loopback.
PACOLOCO_URL="${PACOLOCO_URL:-}"
CACHE_PORT="${CACHE_PORT:-9129}"
if [ -z "$PACOLOCO_URL" ]; then
  if ! curl -sf -m 2 "http://127.0.0.1:$CACHE_PORT/ping" >/dev/null 2>&1; then
    echo "[cache] starting cache-proxy.py on :$CACHE_PORT (log: marketplace/cache-proxy.log)"
    nohup python "$here/cache-proxy.py" --port "$CACHE_PORT" >>"$here/cache-proxy.log" 2>&1 &
    sleep 2
  fi
  if curl -sf -m 2 "http://127.0.0.1:$CACHE_PORT/ping" >/dev/null 2>&1; then
    PACOLOCO_URL="http://10.0.2.2:$CACHE_PORT/repo/archlinux"
    echo "[cache] using local cache proxy at $PACOLOCO_URL"
  else
    echo "[cache] WARNING: no cache proxy — every package downloads from the real mirrors"
  fi
fi

vm="review-$(date +%s)"
vdi="$(win "$repo/marketplace/$vm.vdi")"
echo "[$vm] fresh UEFI VM from $(basename "$ISO")"
"$VBOX" createvm --name "$vm" --ostype ArchLinux_64 --register >/dev/null
# --nat-localhostreachable1: since VBox 6.1.28 NAT *refuses* guest traffic to
# 10.0.2.2 (host loopback) by default — without this flag the package cache is
# unreachable from the VM (instant "connection refused", not a firewall issue).
"$VBOX" modifyvm "$vm" --memory 6144 --cpus 4 --firmware efi --nic1 nat \
   --nat-localhostreachable1 on \
   --graphicscontroller vmsvga --vram 64 --boot1 dvd --boot2 disk >/dev/null
"$VBOX" createmedium disk --filename "$vdi" --size 25000 >/dev/null
"$VBOX" storagectl "$vm" --name SATA --add sata --controller IntelAhci >/dev/null
"$VBOX" storageattach "$vm" --storagectl SATA --port 0 --device 0 --type hdd --medium "$vdi" >/dev/null
"$VBOX" storageattach "$vm" --storagectl SATA --port 1 --device 0 --type dvddrive --medium "$(win "$ISO")" >/dev/null
"$VBOX" startvm "$vm" --type headless >/dev/null

gx() { "$VBOX" guestcontrol "$vm" run --username root --password "" --exe /usr/bin/bash -- -lc "$1" 2>&1; }
KEPT=0   # set to 1 once we've handed the VM off to be kept, so cleanup skips it
cleanup() { [ "$KEPT" = 1 ] && return; "$VBOX" controlvm "$vm" poweroff >/dev/null 2>&1; sleep 1; "$VBOX" unregistervm "$vm" --delete >/dev/null 2>&1; }
trap cleanup EXIT

echo "[$vm] waiting for the live environment…"
t0=$SECONDS; until gx "echo READY" | grep -q READY; do sleep 8; [ $((SECONDS-t0)) -gt 360 ] && { echo "live env never came up"; exit 1; }; done

# Point at the caching proxy if one is configured, and copy the submission in.
# The mirrorlist must be PINNED, not just rewritten: `manifest provision`
# overwrites /etc/pacman.d/mirrorlist early on (rank_mirrors() in installer.rs),
# which would silently bypass the cache for the whole install. A read-only bind
# mount makes that overwrite fail harmlessly (rank_mirrors is best-effort by
# design) while pacstrap still reads — and copies — the cache mirrorlist into
# the target, so the chrooted package installs are cached too.
if [ -n "$PACOLOCO_URL" ]; then
  pin="$(gx "echo 'Server = $PACOLOCO_URL/\$repo/os/\$arch' > /root/mirrorlist.cache \
    && cp /root/mirrorlist.cache /etc/pacman.d/mirrorlist \
    && mount --bind /root/mirrorlist.cache /etc/pacman.d/mirrorlist \
    && mount -o remount,ro,bind /etc/pacman.d/mirrorlist \
    && echo PINNED")"
  case "$pin" in *PINNED*) echo "[$vm] mirrorlist pinned to the package cache" ;;
    *) echo "[$vm] WARNING: could not pin mirrorlist ($pin) — install may bypass the cache" ;;
  esac
  # Preflight from *inside the guest*: if the VM can't reach the cache, unpin
  # and fall back to the real mirrors now — otherwise pacstrap fails 10 minutes
  # in with "could not connect to 10.0.2.2". (Bit us once: a loopback-bound
  # proxy passes the host-side ping but is unreachable from the guest.)
  if ! gx "curl -sf -m 5 http://10.0.2.2:$CACHE_PORT/ping" | grep -q ok; then
    echo "[$vm] WARNING: cache unreachable from the guest — unpinning, install will use real mirrors"
    gx "umount /etc/pacman.d/mirrorlist 2>/dev/null; printf 'Server = https://geo.mirror.pkgbuild.com/\$repo/os/\$arch\n' > /etc/pacman.d/mirrorlist" >/dev/null
  fi
fi
# Get the submission into the VM, and prove it arrived: a silently-failed copy
# makes `manifest provision` mis-resolve /root/submission.json as a catalog
# name and die confusingly. copyto first; if it fails, push the file base64'd
# through guestcontrol run (the same proven channel as every gx call).
mabs="$(cd "$(dirname "$MANIFEST")" && pwd)/$(basename "$MANIFEST")"
if ! "$VBOX" guestcontrol "$vm" copyto "$(win "$mabs")" /root/submission.json \
      --username root --password "" >/dev/null 2>&1; then
  echo "[$vm] copyto failed — falling back to base64 transfer"
  gx ": > /root/submission.b64" >/dev/null
  # small chunks: guestcontrol rejects args of a few KB+ (VERR_NOT_SUPPORTED)
  base64 -w0 "$mabs" | fold -w 2000 | while read -r chunk; do
    gx "printf %s '$chunk' >> /root/submission.b64" >/dev/null
  done
  gx "base64 -d /root/submission.b64 > /root/submission.json && rm -f /root/submission.b64" >/dev/null
fi
want="$(sha256sum "$mabs" | cut -d' ' -f1)"
got="$(gx "sha256sum /root/submission.json 2>/dev/null | cut -d' ' -f1" | tr -d '[:space:]')"
if [ "$got" != "$want" ]; then
  echo "[$vm] FAIL: submission never arrived intact in the VM (sha $got != $want)"; exit 1
fi
echo "[$vm] submission copied in (sha256 verified)"

echo "[$vm] installing (manifest provision)…"
# NB: log to /root/install.log, NOT /tmp/manifest-install.log — the live GUI
# session (.zlogin: `cage -- manifest-gui >/tmp/manifest-install.log`) owns
# that file, and two writers clobber each other's output.
gx "rm -f /tmp/prov.exit; setsid bash -c 'manifest provision /root/submission.json --disk /dev/sda --user reviewer --password review1234 --no-reboot >/root/install.log 2>&1; echo \$? >/tmp/prov.exit' </dev/null >/dev/null 2>&1 & echo launched" >/dev/null
code=""; t1=$SECONDS
while [ $((SECONDS-t1)) -lt 2400 ]; do
  raw="$(gx 'cat /tmp/prov.exit 2>/dev/null' | tr -d '[:space:]')"
  case "$raw" in ''|*[!0-9]*) : ;; *) code="$raw"; break ;; esac
  sleep 15
done

echo "----- install log (tail) -----"; gx 'tail -n 25 /root/install.log'

# --keep: hand the VM off instead of destroying it, so you can open it in the
# VirtualBox GUI and look around. On success we drop the ISO (boots straight
# into the installed system) and rename it out of the review-* namespace.
keep_vm() {
  local slug newname
  gx 'sync' >/dev/null 2>&1                       # flush ext4 before power-off
  [ "$1" = ok ] && "$VBOX" storageattach "$vm" --storagectl SATA --port 1 --device 0 --type dvddrive --medium none >/dev/null 2>&1
  "$VBOX" controlvm "$vm" poweroff >/dev/null 2>&1; sleep 2
  slug="$(basename "$MANIFEST" .json | tr -c 'a-z0-9' '-' | sed 's/-\{2,\}/-/g;s/^-//;s/-$//')"
  newname="kept-${slug:-manifest}-$(date +%s | tail -c 5)"
  "$VBOX" modifyvm "$vm" --name "$newname" >/dev/null 2>&1 && vm="$newname"
  KEPT=1
  echo ">>> KEPT VM '$vm' (powered off). Open VirtualBox, start it$([ "$1" = ok ] && echo ', log in as niri / niri')."
}

if [ "$code" = "0" ]; then
  echo ">>> INSTALL OK."
  # TODO (DESIGN.md, stage 2 behavioural capture): boot the installed disk, log
  # in, run the compositor validator, diff enabled services, list outbound
  # connections + listeners, capture a screenshot. For now the install
  # completing cleanly is the pass signal.
  [ "$KEEP" = 1 ] && keep_vm ok
  exit 0
else
  echo ">>> INSTALL FAILED (exit ${code:-timeout}). See log above."
  [ "$KEEP" = 1 ] && keep_vm fail
  exit 1
fi
