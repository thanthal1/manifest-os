#!/usr/bin/env bash
#
# audit-vms.sh — spin up a fleet of throwaway VirtualBox VMs and run a real,
# unattended Manifest OS install in each, to find where features break.
#
# Each scenario boots the freshly-built ISO in its own UEFI VM and runs
# `manifest provision` (headless installer) against a blank disk — or, for an
# "alongside" scenario, a synthetic existing-OS disk we lay down first. We
# capture the exit code, the failing [step], and the tail of the install log,
# then tear the VM down. Several run in parallel (tune with -j).
#
# Usage:
#   scripts/audit-vms.sh [-i ISO] [-j N] [-m MB] [-c CPUS] [-d DISK_MB]
#                        [-t SECS] [-k] [SCENARIO ...]
#
#   -i  ISO path           (default: newest dist/manifestos-*-gui-*.iso)
#   -j  parallel VMs       (default: 3)
#   -m  RAM MB per VM      (default: 8192)
#   -c  vCPUs per VM       (default: 4)
#   -d  disk MB per VM     (default: 30000)
#   -t  per-VM timeout sec (default: 2400 = 40 min)
#   -k  keep failed VMs    (don't delete, for inspection)
#
#   SCENARIO = name:manifest:mode   (mode = erase | alongside)
#   Default scenarios: every bundled manifest (erase) + minimal alongside.
#
# Results land in ./audit-results/<timestamp>/: one <scenario>.log each, plus
# summary.txt.  Requires VirtualBox; the live ISO must support guestcontrol
# (it does — root, empty password).
set -u

VBOX="${VBOX:-/c/Program Files/Oracle/VirtualBox/VBoxManage.exe}"
export MSYS_NO_PATHCONV=1

JOBS=2 ; MEM=8192 ; CPUS=4 ; DISK=30000 ; TIMEOUT=2400 ; KEEP=0 ; ISO=""
while getopts "i:j:m:c:d:t:k" o; do case "$o" in
  i) ISO="$OPTARG" ;; j) JOBS="$OPTARG" ;; m) MEM="$OPTARG" ;; c) CPUS="$OPTARG" ;;
  d) DISK="$OPTARG" ;; t) TIMEOUT="$OPTARG" ;; k) KEEP=1 ;;
  *) grep '^#' "$0" | sed 's/^# \{0,1\}//' ; exit 1 ;;
esac; done
shift $((OPTIND-1))

repo="$(cd "$(dirname "$0")/.." && pwd)"
[ -z "$ISO" ] && ISO="$(ls -t "$repo"/dist/manifestos-*-gui-*.iso 2>/dev/null | head -1)"
[ -z "$ISO" ] && { echo "no ISO found in dist/ — pass -i ISO"; exit 1; }

# VBoxManage is a Windows .exe: it needs Windows-style paths, not MSYS /c/...
# ones (which it mangles to C:\c\...). Convert every host path we hand it.
win() { cygpath -m "$1" 2>/dev/null || echo "$1"; }
ISO_WIN="$(win "$ISO")"

# name:manifest:mode  (manifest = bundled example basename)
SCENARIOS=( "$@" )
if [ "${#SCENARIOS[@]}" -eq 0 ]; then
  SCENARIOS=(
    minimal:minimal:erase
    bootstrap:bootstrap:erase
    survey:survey-demo:erase
    gnome:gnome:erase
    niri:niri-rice:erase
    hyprland:hyprland-rice:erase
    dualboot:minimal:alongside
    "luks:minimal:erase:--encrypt --passphrase test1234 --filesystem xfs"
    "poweruser:minimal:erase:--root-password rootpw123 --autologin --install-nvidia"
  )
fi

stamp="$(date +%Y%m%d-%H%M%S)"
out="$repo/audit-results/$stamp"; mkdir -p "$out"
echo "ISO: $ISO"
echo "Results: $out"
echo "Scenarios: ${SCENARIOS[*]}"
echo "Parallel: $JOBS   RAM: ${MEM}M   CPU: $CPUS   disk: ${DISK}M   timeout: ${TIMEOUT}s"
echo

vb()  { "$VBOX" "$@" 2>&1; }
# Run a shell line in the guest as root (empty password).
gx()  { "$VBOX" guestcontrol "$1" run --username root --password "" --exe /usr/bin/bash -- -lc "$2" 2>&1; }

destroy() {
  vb controlvm "$1" poweroff >/dev/null 2>&1
  sleep 1
  vb unregistervm "$1" --delete >/dev/null 2>&1
}

run_scenario() {
  local spec="$1" name man mode vm log result
  name="${spec%%:*}"; man="$(echo "$spec" | cut -d: -f2)"; mode="$(echo "$spec" | cut -d: -f3)"
  local extra; extra="$(echo "$spec" | cut -d: -f4-)"   # optional extra provision flags
  vm="audit-$name-$stamp"
  log="$out/$name.log"
  : > "$log"
  echo "[$name] manifest=$man mode=$mode  vm=$vm" | tee -a "$log"

  destroy "$vm"
  # Dual-boot needs a disk big enough to shrink the existing OS by the carve's
  # default (40 GiB) and still leave it 20+ GiB — give those scenarios more room.
  local disk_mb="$DISK"; local along_gib=""
  if [ "$mode" = "alongside" ]; then disk_mb=90000; along_gib="--alongside-gib 25"; fi
  local vdi_win; vdi_win="$(win "$out/$vm.vdi")"
  # Fresh UEFI VM with NAT internet (pacstrap needs it) + a blank disk + the ISO.
  vb createvm --name "$vm" --ostype ArchLinux_64 --register >>"$log" 2>&1
  vb modifyvm "$vm" --memory "$MEM" --cpus "$CPUS" --firmware efi \
     --nic1 nat --graphicscontroller vmsvga --vram 64 --boot1 dvd --boot2 disk >>"$log" 2>&1
  vb createmedium disk --filename "$vdi_win" --size "$disk_mb" >>"$log" 2>&1
  vb storagectl "$vm" --name SATA --add sata --controller IntelAhci >>"$log" 2>&1
  vb storageattach "$vm" --storagectl SATA --port 0 --device 0 --type hdd --medium "$vdi_win" >>"$log" 2>&1
  vb storageattach "$vm" --storagectl SATA --port 1 --device 0 --type dvddrive --medium "$ISO_WIN" >>"$log" 2>&1
  vb startvm "$vm" --type headless >>"$log" 2>&1

  # Wait for the live system's guest agent. Boots are slow under parallel load,
  # so be patient and only reset once, late, if it truly stalls.
  local up=0 reset=0 t0=$SECONDS
  while [ $((SECONDS-t0)) -lt 360 ]; do
    if gx "$vm" "echo READY" 2>/dev/null | grep -q READY; then up=1; break; fi
    sleep 8
    if [ $((SECONDS-t0)) -gt 240 ] && [ "$reset" -eq 0 ]; then vb controlvm "$vm" reset >/dev/null 2>&1; reset=1; fi
  done
  if [ "$up" -ne 1 ]; then
    echo "[$name] FAIL: live system never came up" | tee -a "$log"
    echo "$name FAIL boot" >> "$out/summary.txt"; [ "$KEEP" -eq 1 ] || destroy "$vm"; return
  fi

  # For dual boot, lay down a synthetic existing OS (ESP + bootmgfw + NTFS).
  if [ "$mode" = "alongside" ]; then
    gx "$vm" 'set -e
      sgdisk --zap-all /dev/sda >/dev/null
      sgdisk -n 1:0:+550M -t 1:ef00 /dev/sda >/dev/null
      sgdisk -n 2:0:0     -t 2:0700 /dev/sda >/dev/null
      partprobe /dev/sda; sleep 1
      mkfs.fat -F32 /dev/sda1 >/dev/null
      mkfs.ntfs -Q -L Windows /dev/sda2 >/dev/null
      mkdir -p /run/e && mount /dev/sda1 /run/e && mkdir -p /run/e/EFI/Microsoft/Boot
      printf MZ > /run/e/EFI/Microsoft/Boot/bootmgfw.efi && umount /run/e' >>"$log" 2>&1
  fi

  # Run the unattended install in the background on the guest; poll for its exit.
  local mflag=""; [ "$mode" = "alongside" ] && mflag="--mode alongside $along_gib"
  gx "$vm" "rm -f /tmp/prov.exit; setsid bash -c 'manifest provision /usr/share/manifest-os/examples/$man.json --disk /dev/sda $mflag $extra --user tester --password test1234 --no-reboot >/tmp/prov.log 2>&1; echo \$? >/tmp/prov.exit' </dev/null >/dev/null 2>&1 & echo launched" >>"$log" 2>&1

  # Poll for the install's exit code. Only accept a pure number — guestcontrol
  # itself times out under load, and that error text must not be mistaken for a
  # result (we just retry on the next tick).
  local code="" raw t1=$SECONDS
  while [ $((SECONDS-t1)) -lt "$TIMEOUT" ]; do
    raw="$(gx "$vm" 'cat /tmp/prov.exit 2>/dev/null' | tr -d '[:space:]')"
    case "$raw" in
      ''|*[!0-9]*) : ;;            # empty or non-numeric (transient) — keep waiting
      *) code="$raw"; break ;;
    esac
    sleep 15
  done

  echo "----- /tmp/prov.log (tail) -----" >> "$log"
  gx "$vm" 'tail -n 40 /tmp/prov.log 2>/dev/null' >> "$log" 2>&1
  vb controlvm "$vm" screenshotpng "$(win "$out/$name.png")" >/dev/null 2>&1

  if [ -z "$code" ]; then
    echo "[$name] FAIL: timed out after ${TIMEOUT}s" | tee -a "$log"
    echo "$name FAIL timeout" >> "$out/summary.txt"
  elif [ "$code" = "0" ]; then
    echo "[$name] PASS" | tee -a "$log"
    echo "$name PASS" >> "$out/summary.txt"
  else
    local laststep
    laststep="$(gx "$vm" "grep -oE '^\[[^]]+\]' /tmp/prov.log | tail -1")"
    echo "[$name] FAIL (exit $code) at ${laststep:-?}" | tee -a "$log"
    echo "$name FAIL exit=$code step=${laststep:-?}" >> "$out/summary.txt"
  fi

  if [ "$KEEP" -eq 1 ] && [ "$code" != "0" ]; then
    echo "[$name] keeping VM $vm for inspection" | tee -a "$log"
  else
    destroy "$vm"
  fi
}

# Launch scenarios with a concurrency cap.
for s in "${SCENARIOS[@]}"; do
  while [ "$(jobs -rp | wc -l)" -ge "$JOBS" ]; do sleep 5; done
  run_scenario "$s" &
  sleep 3   # stagger VM creation
done
wait

echo
echo "================ AUDIT SUMMARY ($stamp) ================"
sort "$out/summary.txt" 2>/dev/null
echo "========================================================"
echo "Logs + screenshots: $out"
