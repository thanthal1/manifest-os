#!/usr/bin/env bash
# Detach-sign every package + the repo database in packaging/out/.
# Runs on the MAINTAINER machine — the only place the private key exists.
#
# A detached signature (.sig) is a separate file proving "the holder of the
# Manifest OS private key vouches for these exact bytes". pacman downloads
# <pkg> and <pkg>.sig together and verifies them against the public key that
# manifest-os-keyring installed. The key itself is never uploaded — only
# signatures, which cannot be used to sign anything else.

set -euo pipefail
cd "$(dirname "$0")/out"

UID_NAME="Manifest OS Signing Key <matthew.mccabe816@gmail.com>"

for f in *.pkg.tar.zst manifest-os.db manifest-os.files; do
    [ -f "$f" ] || { echo "missing $f — run the repo build first" >&2; exit 1; }
    rm -f "$f.sig"
    gpg --batch --pinentry-mode loopback --passphrase '' \
        --detach-sign --local-user "$UID_NAME" --output "$f.sig" "$f"
    gpg --verify "$f.sig" "$f" 2>/dev/null && echo "signed + verified: $f"
done

echo
echo "All artifacts signed. Publish with packaging/publish-repo.sh"
