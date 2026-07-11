#!/usr/bin/env bash
# Generate the Manifest OS repo signing key — ONCE, on the maintainer machine.
#
# Security model (read packaging/README.md for the full picture):
#   * The PRIVATE key is created inside your local GnuPG home (~/.gnupg) and
#     NEVER leaves it. This script never exports, copies, or prints it, and
#     nothing under packaging/ ever contains it.
#   * Only the PUBLIC key is exported, into packaging/keyring/ — that's what
#     ships to users (inside manifest-os-keyring) so pacman can VERIFY
#     signatures. Public keys can't sign anything; committing them is safe.
#   * GnuPG also writes a revocation certificate to
#     ~/.gnupg/openpgp-revocs.d/<FPR>.rev — keep it: it's how you'd publicly
#     invalidate the key if the machine were ever compromised.
#
# Idempotent: refuses to run if a Manifest OS signing key already exists.

set -euo pipefail
cd "$(dirname "$0")"

UID_NAME="Manifest OS Signing Key <matthew.mccabe816@gmail.com>"

if gpg --list-secret-keys "$UID_NAME" >/dev/null 2>&1; then
    echo "A Manifest OS signing key already exists — not creating another." >&2
    echo "(Re-exporting the public files instead.)" >&2
else
    echo "Generating the signing key (ed25519, sign-only, no expiry)..."
    # Passphrase-less so signing can run non-interactively. The key still never
    # leaves ~/.gnupg; to add a passphrase later:  gpg --passwd "$UID_NAME"
    gpg --batch --pinentry-mode loopback --passphrase '' \
        --quick-generate-key "$UID_NAME" ed25519 sign 0
fi

FPR=$(gpg --list-keys --with-colons "$UID_NAME" | awk -F: '/^fpr:/ {print $10; exit}')
echo "Key fingerprint: $FPR"

# Export ONLY the public key + the trust/revoked lists the keyring package ships.
gpg --export --armor "$FPR" > keyring/manifest-os.asc
printf '%s:4:\n' "$FPR" > keyring/manifest-os-trusted
: > keyring/manifest-os-revoked

echo
echo "Wrote (public material only — safe to commit):"
echo "  keyring/manifest-os.asc       the public key"
echo "  keyring/manifest-os-trusted   trust list for pacman-key --populate"
echo "  keyring/manifest-os-revoked   (empty) revocation list"
echo
echo "The private key stays in ~/.gnupg. Back it up somewhere offline:"
echo "  gpg --export-secret-keys --armor '$UID_NAME' > <offline-backup-location>"
echo "  (do NOT put that file anywhere inside the repo)"
