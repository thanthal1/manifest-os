#!/usr/bin/env bash
# check-manifest.sh — one command to fully check out a manifest JSON.
#
#   scripts/check-manifest.sh <manifest.json> [--static] [-i ISO] [--fail-on SEV]
#
# Stages (each must pass before the next runs):
#   1. manifest verify   — structure + schema version (fast, local)
#   2. scan.py           — static security scan + package names checked
#                          against the real Arch repos
#   3. VM boot test      — real `manifest provision` install in a throwaway
#                          VirtualBox VM (skip with --static)
#
# Stage 3 routes every package download through the local cache proxy
# (marketplace/cache-proxy.py, auto-started by boot-test.sh): the first check
# of any manifest downloads its packages once into marketplace/pkg-cache/,
# and every later check — same manifest or another one sharing packages — is
# served from disk. That is what makes re-checking a JSON after an edit cheap.
#
# Exit codes: 0 all stages passed · 1 a stage failed · 2 usage error.
set -u
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"

# Default --fail-on CRITICAL: this checks *your own* manifest, where sudo users
# etc. are expected (marketplace review keeps its stricter HIGH gate).
MANIFEST="" STATIC=0 FAIL_ON="CRITICAL" PASS=()
while [ $# -gt 0 ]; do case "$1" in
  --static) STATIC=1; shift ;;
  --fail-on) FAIL_ON="$2"; shift 2 ;;
  -i) PASS+=("$1" "$2"); shift 2 ;;
  -*) echo "unknown flag $1"; exit 2 ;;
  *) MANIFEST="$1"; shift ;;
esac; done
PASS+=(--fail-on "$FAIL_ON")
[ -z "$MANIFEST" ] && { grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
[ -f "$MANIFEST" ] || { echo "no such file: $MANIFEST"; exit 2; }

# ---- Stage 0/1: manifest verify (the schema validator) ---------------------
# Use whichever host build of the manifest binary exists; skip with a warning
# if none does (scan.py still parses the JSON, so nothing malformed gets by).
BIN=""
for c in "$repo/target/release/manifest.exe" "$repo/target/debug/manifest.exe" \
         "$repo/target/release/manifest"     "$repo/target/debug/manifest"; do
  [ -x "$c" ] && { BIN="$c"; break; }
done
echo "### Stage 0 — manifest verify (structure + schema)"
if [ -n "$BIN" ]; then
  "$BIN" verify "$MANIFEST" || { echo ">>> REJECTED by manifest verify."; exit 1; }
else
  echo "(no built manifest binary in target/ — run 'cargo build --release'; skipping)"
fi
echo

# ---- Stages 1+2: static scan, then the cached VM boot test -----------------
BOOT=(--boot); [ "$STATIC" -eq 1 ] && BOOT=()
exec bash "$repo/marketplace/boot-test.sh" "$MANIFEST" ${PASS[@]+"${PASS[@]}"} ${BOOT[@]+"${BOOT[@]}"}
