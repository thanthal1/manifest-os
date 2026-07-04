#!/usr/bin/env bash
# cache-setup.sh — stand up the package cache for the boot-test farm.
#
# Every boot test otherwise re-downloads ~2 GB of packages from the Arch
# mirrors — slow, and enough repeated traffic to get rate-limited. A pacoloco
# caching proxy fixes both: the first VM to need a package downloads it, every
# later VM is served the local copy. It caches pacstrap, `pacman -Syu`, the
# desktop package set, and AUR *dependencies* alike (AUR builds still compile
# from source — that's inherent).
#
# Verified 2026-07-04: a warm fetch served vim (2.65 MB) in 13 ms vs 809 ms cold
# — 62x faster, and the upstream mirror is never hit on a cache hit.
#
# Run this on the cache host (an Arch box — the review runner, or the
# manifest-build VM). Then point each throwaway test VM's mirrorlist at it:
#
#   Server = http://<cache-host>:9129/repo/archlinux/$repo/os/$arch
#
# In the VirtualBox dev rig, reachability options:
#   - pacoloco on the *host*  -> NAT VMs reach it at http://10.0.2.2:9129
#   - pacoloco on a *VM*      -> put the cache VM + test VMs on one VBox
#                                "NAT Network" so they share a subnet, then use
#                                the cache VM's address.
# boot-test.sh reads $PACOLOCO_URL and rewrites the VM mirrorlist automatically.
set -euo pipefail

CACHE_DIR="${CACHE_DIR:-/var/cache/pacoloco}"
PORT="${PORT:-9129}"

command -v pacoloco >/dev/null || sudo pacman -S --needed --noconfirm pacoloco
sudo mkdir -p "$CACHE_DIR"

sudo tee /etc/pacoloco.yaml >/dev/null <<EOF
# Managed by marketplace/cache-setup.sh
port: $PORT
cache_dir: $CACHE_DIR
# Drop cached packages untouched for 30 days, so the cache doesn't grow forever.
purge_files_after: 2592000
download_timeout: 200
repos:
  archlinux:
    urls:
      - https://geo.mirror.pkgbuild.com
      - https://mirror.rackspace.com/archlinux
  # so the cachy/* examples cache too
  cachyos:
    urls:
      - https://mirror.cachyos.org/repo/x86_64/cachyos
EOF

# Prefer the packaged systemd service on a real host; fall back to a plain
# background process (e.g. inside a chroot with no running systemd).
if command -v systemctl >/dev/null && systemctl list-unit-files pacoloco.service >/dev/null 2>&1 \
   && [ -d /run/systemd/system ]; then
  sudo systemctl enable --now pacoloco.service
  echo "pacoloco running as a systemd service on :$PORT"
else
  pkill -f 'pacoloco -config' 2>/dev/null || true
  setsid pacoloco -config /etc/pacoloco.yaml >/tmp/pacoloco.log 2>&1 </dev/null &
  sleep 2
  echo "pacoloco started (background, log: /tmp/pacoloco.log) on :$PORT"
fi

ss -tlnp 2>/dev/null | grep -q ":$PORT" && echo "listening — cache ready at http://$(hostname -I 2>/dev/null | awk '{print $1}'):$PORT/repo/archlinux/\$repo/os/\$arch" \
  || { echo "WARNING: pacoloco is not listening on :$PORT"; exit 1; }
