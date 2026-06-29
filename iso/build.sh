#!/usr/bin/env bash
# Build the Manifest OS ISO. Must run on an Arch host as root, with the
# `archiso` package installed (provides mkarchiso). Typically run in the VM.
#
#   sudo ./iso/build.sh
#
# Output: ./iso/out/manifestos-*.iso
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"
profile="$here/manifest-os"
work="$here/work"
out="$here/out"

if [[ $EUID -ne 0 ]]; then
    echo "must run as root (mkarchiso needs it): sudo $0" >&2
    exit 1
fi
if ! command -v mkarchiso &>/dev/null; then
    echo "mkarchiso not found — install it: pacman -S archiso" >&2
    exit 1
fi

# Bake the freshly-built manifest binary into the live filesystem.
bin="$repo/target/release/manifest"
[[ -x "$bin" ]] || bin="$repo/dist/manifest"
if [[ ! -x "$bin" ]]; then
    echo "no manifest binary found (build with: cargo build --release)" >&2
    exit 1
fi
install -Dm755 "$bin" "$profile/airootfs/usr/local/bin/manifest"
echo "baked in: $bin"

# Bake the graphical installer too (built with: cargo build --release --features gui).
# If it's missing, the live session falls back to the text installer.
gui="$repo/target/release/manifest-gui"
[[ -x "$gui" ]] || gui="$repo/dist/manifest-gui"
if [[ -x "$gui" ]]; then
    install -Dm755 "$gui" "$profile/airootfs/usr/local/bin/manifest-gui"
    echo "baked in: $gui"
else
    echo "WARNING: manifest-gui not found — build it with 'cargo build --release --features gui';" >&2
    echo "         the ISO will boot straight to the text installer." >&2
fi

# Ship the example manifests so the TUI can list and install them by name.
for m in "$repo"/examples/*.json; do
    install -Dm644 "$m" "$profile/airootfs/usr/share/manifest-os/examples/$(basename "$m")"
done
echo "bundled $(ls "$repo"/examples/*.json | wc -l) example manifest(s)"

# Normalize line endings — a Windows checkout may carry CRLF, which makes
# mkarchiso choke when it sources profiledef.sh ($'\r': command not found).
# grep -I skips binary files (e.g. the baked-in manifest binary).
find "$profile" -type f -exec grep -Ilq . {} \; -exec sed -i 's/\r$//' {} + 2>/dev/null || true

# Repair systemd enablement symlinks. A Windows (no-symlink) git checkout turns
# airootfs/etc/systemd/system/*.wants/*.service symlinks into text files holding
# the target path, so systemd ignores them and pacman-init / vboxservice /
# networkd never get enabled (empty keyring -> pacstrap fails on the live ISO).
# Any single-line, space-free file naming a unit is a mangled link; relink it.
while IFS= read -r -d '' f; do
    c="$(cat "$f")"
    case "$c" in
        *$'\n'* | *" "*) continue ;;                       # multi-line / has spaces = real file
        *.service | *.socket | *.target | *.mount | *.timer | *.automount) ln -sfn "$c" "$f" ;;
    esac
done < <(find "$profile/airootfs/etc/systemd/system" -type f -print0)
echo "repaired systemd enablement symlinks"

rm -rf "$work"
mkarchiso -v -w "$work" -o "$out" "$profile"
echo "ISO written to: $out"
