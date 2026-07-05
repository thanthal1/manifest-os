#!/usr/bin/env bash
# audit-examples.sh — fast static audit of every bundled example manifest.
#
# Catches the classes of bug that `manifest verify` and `--dry-run` CANNOT,
# because they only surface against a real system — the ones that have actually
# shipped broken:
#   1. Dead embedded URLs      (a 404 wallpaper/dotfiles source aborts install)
#   2. Non-existent packages   (a typo'd/removed package name aborts install)
#   3. Invalid desktop config  (a compositor config that errors only on login)
#
# This does NOT do full disk installs — that's scripts/audit-vms.sh (slow, deep).
# This is the fast pass to run on EVERY example before building an ISO.
#
# Run it on an Arch box — the manifest-build VM, the live ISO, or any Arch host:
#     sudo pacman -Sy                 # so package-existence checks have a DB
#     ./scripts/audit-examples.sh                 # audit examples/*.json
#     ./scripts/audit-examples.sh path/to/one.json ...   # audit specific files
#
# Exit code is non-zero if any example has an ERROR (dead URL / missing package /
# invalid config). WARNINGS (e.g. a package only in the AUR, a config we can't
# statically validate) don't fail the run.
#
# Options:
#   -c   also validate embedded compositor configs (installs niri/sway/i3 +
#        hyprland validators on demand — needs network). Off by default so the
#        cheap checks stay instant.
set -u

DEEP_CONFIG=0
while getopts "c" o; do case "$o" in c) DEEP_CONFIG=1 ;; *) ;; esac; done
shift $((OPTIND-1))

repo="$(cd "$(dirname "$0")/.." && pwd)"
manifest_bin="$(command -v manifest || echo "$repo/target/release/manifest")"
[ -x "$manifest_bin" ] || manifest_bin="$repo/target/debug/manifest"

FILES=( "$@" )
[ "${#FILES[@]}" -eq 0 ] && FILES=( "$repo"/examples/*.json )

errors=0
warns=0
red()  { printf '\033[31m%s\033[0m' "$1"; }
grn()  { printf '\033[32m%s\033[0m' "$1"; }
yel()  { printf '\033[33m%s\033[0m' "$1"; }
err()  { echo "    $(red "ERROR")  $1"; errors=$((errors+1)); }
warn() { echo "    $(yel "WARN")   $1"; warns=$((warns+1)); }
ok()   { echo "    $(grn "ok")     $1"; }

# --- JSON extractors (python is always present on Arch) ---------------------
# Only the URLs the *installer* actually fetches can abort an install, so pull
# them from those fields specifically — wallpaper source, dotfiles source, and
# any URL in a pre/post_install hook. URLs buried in files[].content are just
# the user's own config text (doc-link comments etc.) and are NOT checked.
# Known "edit-me" placeholders are skipped (the installer skips them too), and
# trailing punctuation from prose is trimmed.
urls_of() {
  python - "$1" <<'PY'
import json,sys,re
d=json.load(open(sys.argv[1], encoding="utf-8"))
urls=[]
w=d.get("wallpaper")
if isinstance(w,str): urls.append(w)
elif isinstance(w,dict) and w.get("source"): urls.append(w["source"])
df=d.get("dotfiles")
if isinstance(df,dict) and df.get("source"): urls.append(df["source"])
for hook in d.get("pre_install",[])+d.get("post_install",[]):
    urls+=re.findall(r"https?://[^\s\"'|)]+",hook)
PLACEHOLDER=("github.com/you/","/yourusername/","example.com","changeme","<")
out=[]
for u in urls:
    u=u.rstrip(").,'\"")
    if not u.startswith("http"): continue
    if any(p in u for p in PLACEHOLDER): continue
    out.append(u)
print("\n".join(sorted(set(out))))
PY
}
packages_of() { python -c 'import json,sys; d=json.load(open(sys.argv[1], encoding="utf-8")); print("\n".join(d.get("packages",[])))' "$1"; }
desktop_of()  { python -c 'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8")).get("desktop") or "")' "$1"; }
# Emit "compositor<TAB>tmpfile" for each embedded file whose path is a known
# compositor config; the fragment's content is written to the tmpfile.
configs_of() {
  python - "$1" <<'PY'
import json,sys,os,tempfile
KNOWN={"hypr/hyprland.conf":"hyprland",".config/niri/config.kdl":"niri",
       "sway/config":"sway","i3/config":"i3"}
d=json.load(open(sys.argv[1], encoding="utf-8"))
for f in d.get("files",[]):
    p=f.get("path","")
    for suffix,comp in KNOWN.items():
        if p.endswith(suffix):
            fd,tmp=tempfile.mkstemp(suffix="."+comp); os.write(fd,f.get("content","").encode()); os.close(fd)
            print(f"{comp}\t{tmp}")
PY
}

# --- Checks -----------------------------------------------------------------
check_url() {
  local code
  code="$(curl -s -o /dev/null -L --max-time 20 -w '%{http_code}' -I "$1" 2>/dev/null)"
  case "$code" in
    2*|3*) ok "url $1 ($code)" ;;
    000)   err "url $1 — unreachable / timed out" ;;
    405)   # some hosts reject HEAD; retry with a ranged GET
           code="$(curl -s -o /dev/null -L --max-time 20 -r 0-0 -w '%{http_code}' "$1" 2>/dev/null)"
           case "$code" in 2*|3*) ok "url $1 ($code via GET)";; *) err "url $1 — HTTP $code";; esac ;;
    *)     err "url $1 — HTTP $code" ;;
  esac
}

have_db=0
pacman -Sl >/dev/null 2>&1 && have_db=1
paru_bin="$(command -v paru || true)"
check_pkg() {
  [ "$have_db" -eq 0 ] && { warn "pkg $1 — no synced pacman DB (run: sudo pacman -Sy)"; return; }
  if pacman -Si "$1" >/dev/null 2>&1; then ok "pkg $1"; return; fi
  if [ -n "$paru_bin" ] && paru -Si "$1" >/dev/null 2>&1; then ok "pkg $1 (AUR)"; return; fi
  # Not in official repos and (no paru, or paru miss): could be AUR — warn, don't fail.
  if [ -n "$paru_bin" ]; then err "pkg $1 — not found in repos or AUR (typo?)";
  else warn "pkg $1 — not in official repos (AUR? install paru to confirm)"; fi
}

ensure_validator() {
  case "$1" in
    niri)     command -v niri     >/dev/null || sudo pacman -S --needed --noconfirm niri     >/dev/null 2>&1 ;;
    sway)     command -v sway     >/dev/null || sudo pacman -S --needed --noconfirm sway     >/dev/null 2>&1 ;;
    i3)       command -v i3        >/dev/null || sudo pacman -S --needed --noconfirm i3-wm    >/dev/null 2>&1 ;;
    hyprland) command -v Hyprland >/dev/null || sudo pacman -S --needed --noconfirm hyprland >/dev/null 2>&1 ;;
  esac
}
check_config() {
  local comp="$1" f="$2" out
  [ "$DEEP_CONFIG" -eq 0 ] && { warn "config $comp — skipped (pass -c to validate the compositor config)"; rm -f "$f"; return; }
  ensure_validator "$comp"
  # wlroots compositors (sway) spin up a real backend even for a config check,
  # which fails in a headless VM — give them a software headless env so only
  # genuine *config* errors remain, and strip the GPU/EGL/Vulkan/pci noise.
  local rt="/tmp/audit-rt-$$"; mkdir -p "$rt"
  export XDG_RUNTIME_DIR="$rt" WLR_BACKENDS=headless WLR_RENDERER=pixman \
         WLR_RENDERER_ALLOW_SOFTWARE=1 WLR_LIBINPUT_NO_DEVICES=1
  local NOISE='wlr|egl|vulkan|pci id|Software rendering|render/|backend/'
  case "$comp" in
    niri) if command -v niri >/dev/null; then
            out="$(niri validate -c "$f" 2>&1)" && ok "config niri" || err "config niri — $(echo "$out" | grep -viE "$NOISE" | grep -i error | head -1)"
          else warn "config niri — validator unavailable"; fi ;;
    sway) if command -v sway >/dev/null; then
            if sway -C -c "$f" >/dev/null 2>&1; then ok "config sway"
            else out="$(sway -C -c "$f" 2>&1 | grep -viE "$NOISE" | grep -iE 'error|invalid|expected|unknown' | head -1)"
                 [ -n "$out" ] && err "config sway — $out" || warn "config sway — validator couldn't run headlessly (no config error found)"; fi
          else warn "config sway — validator unavailable"; fi ;;
    i3)   if command -v i3 >/dev/null; then
            out="$(i3 -C -c "$f" 2>&1)" && ok "config i3" || err "config i3 — $(echo "$out" | grep -iE 'error|expected' | head -1)"
          else warn "config i3 — validator unavailable"; fi ;;
    hyprland)
          # Hyprland has no offline --validate; run it headless just long enough
          # to parse the config, then read its own error list and quit.
          if command -v Hyprland >/dev/null; then
            local rt="/tmp/audit-hypr-$$"; mkdir -p "$rt"
            HYPRLAND_CONFIG="$f" XDG_RUNTIME_DIR="$rt" WLR_BACKENDS=headless \
              WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman \
              Hyprland --config "$f" >/"$rt"/log 2>&1 &
            local pid=$!; sleep 4
            out="$(HYPRLAND_INSTANCE_SIGNATURE= XDG_RUNTIME_DIR="$rt" hyprctl configerrors 2>/dev/null)"
            # Fall back to grepping the compositor's own startup log.
            [ -z "$out" ] && out="$(grep -i 'Config error' "$rt"/log 2>/dev/null)"
            kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; rm -rf "$rt"
            if echo "$out" | grep -qi 'error'; then err "config hyprland — $(echo "$out" | grep -i error | head -1)"
            else ok "config hyprland"; fi
          else warn "config hyprland — validator unavailable"; fi ;;
  esac
  rm -f "$f"; rm -rf "/tmp/audit-rt-$$" "/tmp/audit-hypr-$$"
}

# --- Run --------------------------------------------------------------------
echo "Auditing ${#FILES[@]} manifest(s)  [deep config: $([ "$DEEP_CONFIG" -eq 1 ] && echo on || echo off)]"
echo
for j in "${FILES[@]}"; do
  echo "== $(basename "$j") =="
  if ! "$manifest_bin" verify "$j" >/dev/null 2>&1; then
    err "manifest verify failed:"; "$manifest_bin" verify "$j" 2>&1 | sed 's/^/        /'
    echo; continue
  fi
  ok "manifest verify"
  # Strip any trailing CR: Windows Python prints CRLF, and a stray \r on a URL
  # or package name makes curl/pacman look up "<value>\r" (curl → exit 000).
  while IFS= read -r u; do u="${u%$'\r'}"; [ -n "$u" ] && check_url "$u"; done < <(urls_of "$j")
  while IFS= read -r p; do p="${p%$'\r'}"; [ -n "$p" ] && check_pkg "$p"; done < <(packages_of "$j")
  while IFS=$'\t' read -r comp f; do comp="${comp%$'\r'}"; f="${f%$'\r'}"; [ -n "$comp" ] && check_config "$comp" "$f"; done < <(configs_of "$j")
  echo
done

echo "=================================================="
if [ "$errors" -eq 0 ]; then
  echo "$(grn "PASS") — no errors ($warns warning(s))"
else
  echo "$(red "FAIL") — $errors error(s), $warns warning(s)"
fi
exit $(( errors > 0 ? 1 : 0 ))
