//! Cross-desktop wallpaper.
//!
//! Setting "the wallpaper" is wildly inconsistent across environments — GNOME
//! uses gsettings, KDE has `plasma-apply-wallpaper`, Xfce uses xfconf, MATE a
//! different gsettings schema, and bare window managers need a daemon (swaybg on
//! Wayland, feh on X11). None of that can run during install, because there is
//! no graphical session yet.
//!
//! So this module does install-time setup that takes effect on first login:
//!   1. Save the image to a stable path under `/usr/share/backgrounds/manifest`.
//!   2. Install a small `manifest-wallpaper` script that detects the *running*
//!      desktop (`$XDG_CURRENT_DESKTOP`) and applies the image the right way.
//!   3. Run it from an XDG autostart entry — honored by every full DE — so the
//!      wallpaper appears the first time the user logs in (once, so they can
//!      still change it afterwards).
//!   4. For window managers (which don't process XDG autostart), make sure the
//!      right daemon is installed; the WM's own config calls `manifest-wallpaper`.
//!
//! This is where the OS's cross-system promise lives: one manifest field,
//! `wallpaper`, that works whatever desktop the same manifest sets up.

use crate::exec::Ctx;
use crate::manifest::Wallpaper;
use anyhow::Result;

/// Where the saved image and its stable `current` symlink live.
const DIR: &str = "/usr/share/backgrounds/manifest";
const SCRIPT_PATH: &str = "/usr/local/bin/manifest-wallpaper";
const AUTOSTART_PATH: &str = "/etc/xdg/autostart/manifest-wallpaper.desktop";

/// Window-manager desktop keys (no XDG-autostart processing of their own; they
/// rely on a wallpaper daemon launched from their config).
const WM_KEYS: &[&str] = &[
    "hyprland", "sway", "niri", "river", "labwc", "wayfire", "i3", "bspwm",
    "awesome", "qtile", "openbox", "xmonad", "herbstluftwm", "fluxbox", "icewm",
];

pub fn apply(w: &Wallpaper, desktop: Option<&str>, ctx: &Ctx) -> Result<()> {
    let src = w.source();
    let mode = w.mode();
    let ext = extension_of(src);
    let dest = format!("{DIR}/wallpaper.{ext}");

    // 1) Fetch (URL) or copy (local path) the image to the stable location, and
    //    point `current` at it so configs/scripts have one fixed path.
    println!("  · source: {src}  (mode: {mode})");
    let fetch = if src.starts_with("http://") || src.starts_with("https://") {
        format!("curl -fsSL --retry 2 -o '{dest}' '{src}'")
    } else {
        format!("cp -f '{src}' '{dest}'")
    };
    ctx.shell(
        &format!(
            "mkdir -p {DIR} && {fetch} && chmod 644 '{dest}' && ln -sf '{dest}' '{DIR}/current'"
        ),
        true,
    )?;

    // 2) The detector script + 3) the autostart entry that runs it.
    ctx.write_root(SCRIPT_PATH, &SCRIPT.replace("__MODE__", mode))?;
    ctx.sudo("chmod", &["755", SCRIPT_PATH])?;
    ctx.write_root(AUTOSTART_PATH, AUTOSTART)?;

    // 4) Window managers don't run XDG autostart; make sure the daemon their
    //    config will call (`manifest-wallpaper` → swaybg/feh) is present.
    //    Best-effort: a missing daemon shouldn't fail the whole install.
    if let Some(key) = desktop {
        if WM_KEYS.contains(&key) {
            let wayland = matches!(
                crate::desktop::recipe(key).map(|r| r.session),
                Some(crate::desktop::Session::Wayland)
            );
            let tool = if wayland { "swaybg" } else { "feh" };
            println!("  · {key} is a window manager — ensuring {tool} for its config to call manifest-wallpaper");
            let _ = ctx.sudo("pacman", &["-S", "--needed", "--noconfirm", tool]);
        }
    }
    println!("  · wallpaper set for first login (run `manifest-wallpaper` from a WM config if needed)");
    Ok(())
}

/// Image extension from a source path/URL (query string stripped); defaults to
/// `jpg` for anything unrecognized.
fn extension_of(src: &str) -> String {
    let path = src.split(['?', '#']).next().unwrap_or(src);
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    if matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "webp" | "bmp" | "gif") {
        ext
    } else {
        "jpg".to_string()
    }
}

/// Runs at first login (DEs) or from a WM config (WMs). Detects the desktop and
/// sets the wallpaper via its native mechanism; the DE setters run once (a
/// marker file) so the user can change it later, while the WM daemon path runs
/// every session. `__MODE__` is substituted with the manifest's fit mode.
const SCRIPT: &str = r#"#!/bin/sh
# Managed by Manifest OS — set the desktop wallpaper for whatever is running.
IMG="/usr/share/backgrounds/manifest/current"
MODE="__MODE__"
[ -e "$IMG" ] || exit 0
uri="file://$IMG"
de=$(printf '%s' "${XDG_CURRENT_DESKTOP:-}${DESKTOP_SESSION:-}" | tr 'A-Z' 'a-z')

# Per-tool option for the chosen fit mode.
case "$MODE" in
  fit)     gopt=scaled;    xfs=4; feh=--bg-max;    sway=fit;     lxqt=fit;     lxde=fit ;;
  stretch) gopt=stretched; xfs=3; feh=--bg-scale;  sway=stretch; lxqt=stretch; lxde=stretch ;;
  center)  gopt=centered;  xfs=1; feh=--bg-center; sway=center;  lxqt=center;  lxde=center ;;
  tile)    gopt=wallpaper; xfs=2; feh=--bg-tile;   sway=tile;    lxqt=tile;    lxde=tile ;;
  *)       gopt=zoom;      xfs=5; feh=--bg-fill;   sway=fill;    lxqt=zoom;    lxde=crop ;;
esac

marker="${XDG_CONFIG_HOME:-$HOME/.config}/manifest-wallpaper.set"
once() { [ -e "$marker" ] && exit 0; mkdir -p "$(dirname "$marker")"; : > "$marker"; }

case "$de" in
  *gnome*|*budgie*|*unity*|*ubuntu*)
    once
    gsettings set org.gnome.desktop.background picture-uri "$uri" 2>/dev/null
    gsettings set org.gnome.desktop.background picture-uri-dark "$uri" 2>/dev/null
    gsettings set org.gnome.desktop.background picture-options "$gopt" 2>/dev/null ;;
  *cinnamon*)
    once
    gsettings set org.cinnamon.desktop.background picture-uri "$uri" 2>/dev/null
    gsettings set org.cinnamon.desktop.background picture-options "$gopt" 2>/dev/null ;;
  *mate*)
    once
    gsettings set org.mate.background picture-filename "$IMG" 2>/dev/null
    gsettings set org.mate.background picture-options "$gopt" 2>/dev/null ;;
  *kde*|*plasma*)
    once
    plasma-apply-wallpaper "$IMG" 2>/dev/null ;;
  *xfce*)
    once
    for p in $(xfconf-query -c xfce4-desktop -l 2>/dev/null | grep 'last-image$'); do
      xfconf-query -c xfce4-desktop -p "$p" -s "$IMG" 2>/dev/null
    done
    for p in $(xfconf-query -c xfce4-desktop -l 2>/dev/null | grep 'image-style$'); do
      xfconf-query -c xfce4-desktop -p "$p" -s "$xfs" 2>/dev/null
    done ;;
  *lxqt*)
    once
    pcmanfm-qt --set-wallpaper "$IMG" --wallpaper-mode="$lxqt" 2>/dev/null ;;
  *lxde*)
    once
    pcmanfm --set-wallpaper="$IMG" --wallpaper-mode="$lxde" 2>/dev/null ;;
  *)
    # Bare window manager / unknown — run a wallpaper daemon for this session.
    # (Call this from the WM config; it has no persistent setter to mark.)
    if [ -n "$WAYLAND_DISPLAY" ] && command -v swaybg >/dev/null 2>&1; then
      pkill -x swaybg 2>/dev/null
      swaybg -i "$IMG" -m "$sway" >/dev/null 2>&1 &
    elif command -v feh >/dev/null 2>&1; then
      feh "$feh" "$IMG" >/dev/null 2>&1
    fi ;;
esac
"#;

/// XDG autostart entry — full desktops run this on login (window managers
/// generally ignore /etc/xdg/autostart and call the script from their config).
const AUTOSTART: &str = "[Desktop Entry]\n\
Type=Application\n\
Name=Manifest OS wallpaper\n\
Exec=/usr/local/bin/manifest-wallpaper\n\
NoDisplay=true\n\
X-GNOME-Autostart-enabled=true\n\
OnlyShowIn=GNOME;KDE;XFCE;Cinnamon;X-Cinnamon;MATE;Budgie;LXQt;LXDE;Unity;Deepin;COSMIC;\n";
