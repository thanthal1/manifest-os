#!/usr/bin/env bash
# Build every Manifest OS package and assemble the pacman repo database.
# Runs ON ARCH (the manifest-build VM chroot) as root — makepkg itself refuses
# root, so packages build as a throwaway `builder` user.
#
# Expects this layout (the host copies it in — see packaging/README.md):
#   $WORK/pkg/PKGBUILD + manifest-os-$MANIFEST_PKGVER.tar.gz  (git archive of HEAD)
#   $WORK/keyring/PKGBUILD + manifest-os.asc + -trusted + -revoked
#
# Produces $WORK/out/: *.pkg.tar.zst + manifest-os.db/.files (unsigned — the
# host signs them afterwards; the private key never enters this machine).

set -euo pipefail
# Not under /root — the build user can't traverse root's 700 home.
WORK="${WORK:-/home/pkgwork}"
export MANIFEST_PKGVER="${MANIFEST_PKGVER:?set MANIFEST_PKGVER (e.g. 0.1.0)}"

# makepkg must not run as root: use a build user that owns the work tree.
# runuser (util-linux) instead of sudo — always present, no config needed.
id builder &>/dev/null || useradd -m builder
chown -R builder "$WORK"

build_one() {
    local dir="$1"
    echo "== building in $dir =="
    # --nodeps: the chroot has the build toolchain (rust, gtk4, libadwaita)
    # but not necessarily every runtime dependency; dep resolution happens on
    # the user's machine at install time.
    (cd "$dir" && runuser -u builder -- env MANIFEST_PKGVER="$MANIFEST_PKGVER" \
        makepkg -f --nodeps --skipinteg --nocheck)
}

build_one "$WORK/pkg"
build_one "$WORK/keyring"

mkdir -p "$WORK/out"
rm -f "$WORK/out"/*.pkg.tar.zst "$WORK/out"/manifest-os.db* "$WORK/out"/manifest-os.files*
cp "$WORK"/pkg/*.pkg.tar.zst "$WORK"/keyring/*.pkg.tar.zst "$WORK/out/"

# The repo database. repo-add leaves manifest-os.db as a symlink to the
# .tar.gz — GitHub Releases can't host symlinks, so materialise real files.
(cd "$WORK/out" && repo-add manifest-os.db.tar.gz ./*.pkg.tar.zst)
(cd "$WORK/out" \
    && rm -f manifest-os.db manifest-os.files \
    && cp manifest-os.db.tar.gz manifest-os.db \
    && cp manifest-os.files.tar.gz manifest-os.files)

echo
echo "== repo contents =="
ls -la "$WORK/out"
echo "DONE"
