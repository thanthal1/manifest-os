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

MANIFEST="" ISO="" DO_BOOT=0 FAIL_ON="HIGH"
while [ $# -gt 0 ]; do case "$1" in
  -i) ISO="$2"; shift 2 ;;
  --boot) DO_BOOT=1; shift ;;
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
# Preferred: a pacoloco caching proxy on the host so every VM reuses one copy of
# every package. If PACOLOCO_URL is set we rewrite the VM's mirrorlist to it.
#   pacoloco: https://github.com/anatol/pacoloco   (run it, then:)
#   export PACOLOCO_URL=http://10.0.2.2:9129/repo/archlinux   # 10.0.2.2 = NAT host
PACOLOCO_URL="${PACOLOCO_URL:-}"

vm="review-$(date +%s)"
vdi="$(win "$repo/marketplace/$vm.vdi")"
echo "[$vm] fresh UEFI VM from $(basename "$ISO")"
"$VBOX" createvm --name "$vm" --ostype ArchLinux_64 --register >/dev/null
"$VBOX" modifyvm "$vm" --memory 6144 --cpus 4 --firmware efi --nic1 nat \
   --graphicscontroller vmsvga --vram 64 --boot1 dvd --boot2 disk >/dev/null
"$VBOX" createmedium disk --filename "$vdi" --size 25000 >/dev/null
"$VBOX" storagectl "$vm" --name SATA --add sata --controller IntelAhci >/dev/null
"$VBOX" storageattach "$vm" --storagectl SATA --port 0 --device 0 --type hdd --medium "$vdi" >/dev/null
"$VBOX" storageattach "$vm" --storagectl SATA --port 1 --device 0 --type dvddrive --medium "$(win "$ISO")" >/dev/null
"$VBOX" startvm "$vm" --type headless >/dev/null

gx() { "$VBOX" guestcontrol "$vm" run --username root --password "" --exe /usr/bin/bash -- -lc "$1" 2>&1; }
cleanup() { "$VBOX" controlvm "$vm" poweroff >/dev/null 2>&1; sleep 1; "$VBOX" unregistervm "$vm" --delete >/dev/null 2>&1; }
trap cleanup EXIT

echo "[$vm] waiting for the live environment…"
t0=$SECONDS; until gx "echo READY" | grep -q READY; do sleep 8; [ $((SECONDS-t0)) -gt 360 ] && { echo "live env never came up"; exit 1; }; done

# Point at the caching proxy if one is configured, and copy the submission in.
[ -n "$PACOLOCO_URL" ] && gx "echo 'Server = $PACOLOCO_URL/\$repo/os/\$arch' > /etc/pacman.d/mirrorlist" >/dev/null
"$VBOX" guestcontrol "$vm" copyto "$(win "$MANIFEST")" /root/submission.json --username root --password "" >/dev/null 2>&1

echo "[$vm] installing (manifest provision)…"
gx "rm -f /tmp/prov.exit; setsid bash -c 'manifest provision /root/submission.json --disk /dev/sda --user reviewer --password review1234 --no-reboot >/tmp/manifest-install.log 2>&1; echo \$? >/tmp/prov.exit' </dev/null >/dev/null 2>&1 & echo launched" >/dev/null
code=""; t1=$SECONDS
while [ $((SECONDS-t1)) -lt 2400 ]; do
  raw="$(gx 'cat /tmp/prov.exit 2>/dev/null' | tr -d '[:space:]')"
  case "$raw" in ''|*[!0-9]*) : ;; *) code="$raw"; break ;; esac
  sleep 15
done

echo "----- install log (tail) -----"; gx 'tail -n 25 /tmp/manifest-install.log'
if [ "$code" = "0" ]; then
  echo ">>> INSTALL OK."
  # TODO (DESIGN.md, stage 2 behavioural capture): boot the installed disk, log
  # in, run the compositor validator, diff enabled services, list outbound
  # connections + listeners, capture a screenshot. For now the install
  # completing cleanly is the pass signal.
  exit 0
else
  echo ">>> INSTALL FAILED (exit ${code:-timeout}). See log above."
  exit 1
fi
