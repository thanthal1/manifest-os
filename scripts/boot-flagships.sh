#!/usr/bin/env bash
# boot-flagships.sh — end-to-end boot-test of the 4 flagship desktop examples.
#
# For each of the four picker defaults (Hyprland/KDE Plasma/Niri/Sway), this:
#   1. attaches the given ISO to a dedicated VM and boots the live installer
#   2. runs an unattended `manifest provision <flagship>.json --disk /dev/sda`
#   3. reboots into the installed system
#   4. blind-injects the manifest's own username/password to log into the DM
#      (installed systems have no guest additions — no guestcontrol there)
#   5. saves a desktop screenshot
#
# Usage:
#   ./scripts/boot-flagships.sh [path/to.iso]     # defaults to newest dist/*.iso
#
# Requires the 4 VMs to already exist (see the vmdisks/ + VBoxManage createvm
# invocation used to set them up: kdetest, niritest, swaytest, plus ricetest
# for Hyprland), each BIOS firmware, vmsvga + accelerate3d on, an IDE
# controller with an empty dvddrive slot, boot order dvd,disk.
set -uo pipefail
export MSYS_NO_PATHCONV=1
VBOX="/c/Program Files/Oracle/VirtualBox/VBoxManage.exe"
repo="$(cd "$(dirname "$0")/.." && pwd)"

ISO="${1:-$(ls -t "$repo"/dist/*.iso 2>/dev/null | head -1)}"
[ -f "$ISO" ] || { echo "no ISO found — pass a path or build one first (iso/build.sh)" >&2; exit 1; }
ISO_WIN="$(cygpath -m "$ISO")"

stamp="$(date +%Y%m%d-%H%M%S)"
out="$repo/audit-results/flagships-$stamp"
mkdir -p "$out"
echo "ISO: $ISO"
echo "Results: $out"

vb() { "$VBOX" "$@"; }
gx() { "$VBOX" guestcontrol "$1" run --username root --password "" --timeout "${GX_TIMEOUT:-60000}" --exe /usr/bin/bash -- -lc "$2"; }

# vm:manifest:user:password:dm(sddm|greetd)
SCENARIOS=(
  "ricetest:tokyonight-aurora:dev:dev:sddm"
  "kdetest:catppuccin-plasma:kai:kai:sddm"
  "niritest:niri-rice:niri:niri:greetd"
  "swaytest:sway-pro:dev:dev:greetd"
)

wait_for_guestcontrol() {
  local vm="$1" tries="${2:-30}"
  for i in $(seq 1 "$tries"); do
    gx "$vm" "true" >/dev/null 2>&1 && return 0
    sleep 5
  done
  return 1
}

for spec in "${SCENARIOS[@]}"; do
  IFS=: read -r vm man user pass dm <<<"$spec"
  log="$out/$vm.log"
  : > "$log"
  echo "[$vm] manifest=$man user=$user dm=$dm" | tee -a "$log"

  vb controlvm "$vm" poweroff >/dev/null 2>&1
  sleep 2
  vb storageattach "$vm" --storagectl IDE --port 0 --device 0 --type dvddrive --medium "$ISO_WIN" >>"$log" 2>&1
  vb modifyvm "$vm" --boot1 dvd --boot2 disk --boot3 none >>"$log" 2>&1
  vb startvm "$vm" --type headless >>"$log" 2>&1

  echo "[$vm] waiting for live env guestcontrol..." | tee -a "$log"
  if ! wait_for_guestcontrol "$vm" 40; then
    echo "[$vm] FAIL — guestcontrol never came up" | tee -a "$log"
    continue
  fi

  echo "[$vm] provisioning (unattended, this is the long step)..." | tee -a "$log"
  GX_TIMEOUT=1800000 gx "$vm" "manifest provision /usr/share/manifest-os/examples/$man.json --disk /dev/sda --no-reboot > /root/install.log 2>&1; echo INSTALL_EXIT=\$? >> /root/install.log" >>"$log" 2>&1
  gx "$vm" "tail -30 /root/install.log" >>"$log" 2>&1
  tail -5 "$log"

  vb controlvm "$vm" poweroff >>"$log" 2>&1
  sleep 3
  vb storageattach "$vm" --storagectl IDE --port 0 --device 0 --type dvddrive --medium none >>"$log" 2>&1
  vb modifyvm "$vm" --boot1 disk --boot2 none >>"$log" 2>&1

  echo "[$vm] booting installed system..." | tee -a "$log"
  vb startvm "$vm" --type headless >>"$log" 2>&1
  sleep 100   # systemd + the display manager reaching a login screen

  # Installed system has no guest additions — blind keystrokes only.
  vb controlvm "$vm" keyboardputstring "$user" >>"$log" 2>&1
  sleep 1
  vb controlvm "$vm" keyboardputscancode 1c 9c >>"$log" 2>&1   # Enter
  sleep 2
  vb controlvm "$vm" keyboardputstring "$pass" >>"$log" 2>&1
  sleep 1
  vb controlvm "$vm" keyboardputscancode 1c 9c >>"$log" 2>&1   # Enter
  sleep_after_login=25
  [ "$dm" = "sddm" ] && sleep_after_login=30
  sleep "$sleep_after_login"

  shot="$out/$vm-desktop.png"
  vb controlvm "$vm" screenshotpng "$shot" >>"$log" 2>&1
  echo "[$vm] screenshot -> $shot" | tee -a "$log"
done

echo "ALL DONE — results in $out" | tee -a "$out/SUMMARY.txt"
