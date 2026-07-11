#!/usr/bin/env bash
# Publish packaging/out/ to the GitHub release that serves as the pacman repo.
#
# The repo lives on one fixed release tag (`repo`); every publish overwrites
# the artifacts in place (--clobber), so the Server URL in pacman.conf never
# changes. Only public artifacts are uploaded: packages, signatures, and the
# repo database — never any key material.

set -euo pipefail
cd "$(dirname "$0")/out"

TAG="${TAG:-repo}"
REPO="${REPO:-thanthal1/manifest-os}"

# Refuse to publish unsigned artifacts.
for f in *.pkg.tar.zst manifest-os.db; do
    [ -f "$f.sig" ] || { echo "$f has no signature — run sign-repo.sh first" >&2; exit 1; }
done

if ! gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1; then
    gh release create "$TAG" --repo "$REPO" \
        --title "Package repository" \
        --notes "The [manifest-os] pacman repository. Add to /etc/pacman.conf:

\`\`\`
[manifest-os]
SigLevel = Required DatabaseOptional
Server = https://github.com/$REPO/releases/download/$TAG
\`\`\`

Then: \`sudo pacman -Sy manifest-os-keyring manifest-os\` (first install needs the key trusted — see packaging/README.md)."
fi

gh release upload "$TAG" --repo "$REPO" --clobber \
    ./*.pkg.tar.zst ./*.sig manifest-os.db manifest-os.files \
    manifest-os.db.tar.gz manifest-os.files.tar.gz

echo
echo "Published. Users install with:"
echo "  sudo pacman -Syu manifest-os manifest-os-gui manifest-os-plugins"
