//! Cross-desktop visual theming: widget theme, icons, cursor, fonts, dark.
//!
//! Like wallpaper and keybindings, "set the theme" means something different
//! everywhere. This module applies the manifest's single `theme` block in
//! three layers:
//!
//!   1. **Universal per-user GTK config files**, written at install time:
//!      `~/.config/gtk-3.0/settings.ini`, `~/.config/gtk-4.0/settings.ini`,
//!      `~/.gtkrc-2.0`, and `~/.icons/default/index.theme` (cursor). These are
//!      what every GTK app reads when no desktop settings daemon is running —
//!      i.e. they *are* the theme under bare window managers (niri, Hyprland,
//!      Sway, i3, …), and a sensible fallback everywhere else.
//!   2. **A cursor env drop-in** (`/etc/profile.d/manifest-theme.sh`,
//!      `XCURSOR_THEME`/`XCURSOR_SIZE`) so the cursor also applies to Qt apps
//!      and compositors that don't read the GTK files.
//!   3. **A first-login script** for full desktop environments, whose settings
//!      daemons ignore the raw files: it detects the running desktop (same
//!      pattern as [`crate::wallpaper`]) and applies the theme via its native
//!      mechanism — gsettings (GNOME/Budgie/Cinnamon/MATE), xfconf (Xfce), or
//!      kwriteconfig/plasma-apply-* (KDE). Runs once per user (a marker file)
//!      so the user can still change things afterwards.
//!
//! LXQt additionally gets its `~/.config/lxqt/lxqt.conf` icon-theme entry
//! written at install time (it's file-based, no session needed).
//!
//! The theme *packages* (papirus-icon-theme, materia-gtk-theme, a cursor
//! theme, fonts) belong in the manifest's `packages` list — this block only
//! selects them by name.

use crate::exec::Ctx;
use crate::files;
use crate::manifest::Theme;
use anyhow::Result;

const SCRIPT_PATH: &str = "/usr/local/bin/manifest-theme";
const AUTOSTART_PATH: &str = "/etc/xdg/autostart/manifest-theme.desktop";
const ENV_PATH: &str = "/etc/profile.d/manifest-theme.sh";

pub fn apply(theme: &Theme, desktop: Option<&str>, primary_user: Option<&str>, ctx: &Ctx) -> Result<()> {
    if theme.is_empty() {
        return Ok(());
    }

    // 1) Universal GTK files — the theme under bare WMs, fallback elsewhere.
    let ini = settings_ini(theme);
    let mut specs = vec![
        files::home_spec(primary_user, ".config/gtk-3.0/settings.ini", ini.clone()),
        files::home_spec(primary_user, ".config/gtk-4.0/settings.ini", ini),
        files::home_spec(primary_user, ".gtkrc-2.0", gtkrc2(theme)),
    ];
    if let Some(cursor) = &theme.cursor {
        specs.push(files::home_spec(primary_user, ".icons/default/index.theme", cursor_index(cursor)));
    }
    if desktop == Some("lxqt") {
        if let Some(conf) = lxqt_conf(theme) {
            specs.push(files::home_spec(primary_user, ".config/lxqt/lxqt.conf", conf));
        }
    }
    files::apply(&specs, ctx)?;

    // 1b) A global theme that isn't packaged: clone its repo and run its
    // installer now, so the first-login setter can select it. Declarative
    // stand-in for a post_install hook.
    if let Some(url) = &theme.global_source {
        install_global_source(url, theme.global_install.as_deref(), ctx)?;
    }

    // 2) Cursor env for Qt apps and compositors that skip the GTK files.
    if theme.cursor.is_some() || theme.cursor_size.is_some() {
        ctx.write_root(ENV_PATH, &env_dropin(theme))?;
    }

    // 3) First-login native setters for full DEs.
    ctx.write_root(SCRIPT_PATH, &runtime_script(theme))?;
    ctx.sudo("chmod", &["755", SCRIPT_PATH])?;
    ctx.write_root(AUTOSTART_PATH, AUTOSTART)?;
    println!("  · theme set: GTK files now, desktop-native settings at first login");
    Ok(())
}

/// Clone a `theme.global_source` repo and run its installer, so a global theme
/// that isn't in the repos/AUR lands on the system without a `post_install`
/// hook. Runs at user level; the installer command escalates with `sudo` itself
/// (the default, and how these theme `install.sh` scripts do a system-wide
/// install). The clone is a temp dir, removed afterwards.
fn install_global_source(url: &str, install: Option<&str>, ctx: &Ctx) -> Result<()> {
    // Default matches the near-universal convention of these theme repos: an
    // `install.sh` at the root that copies system-wide when run as root. `sh`
    // (not `./`) so a lost exec bit doesn't matter.
    let run = install.unwrap_or("sudo sh ./install.sh");
    println!("  · installing global theme from {url}");
    let cmd = format!(
        "d=$(mktemp -d) && git clone --depth 1 {url} \"$d/theme\" && \
         cd \"$d/theme\" && {run}; rc=$?; cd /; rm -rf \"$d\"; exit $rc",
        url = sh_quote(url),
    );
    ctx.shell(&cmd, false)
}

// ---------------------------------------------------------------------------
// Layer 1 — universal GTK config files
// ---------------------------------------------------------------------------

fn settings_ini(t: &Theme) -> String {
    let mut s = String::from("# Managed by Manifest OS\n[Settings]\n");
    if let Some(v) = &t.gtk {
        s.push_str(&format!("gtk-theme-name={v}\n"));
    }
    if let Some(v) = &t.icons {
        s.push_str(&format!("gtk-icon-theme-name={v}\n"));
    }
    if let Some(v) = &t.cursor {
        s.push_str(&format!("gtk-cursor-theme-name={v}\n"));
    }
    if let Some(n) = t.cursor_size {
        s.push_str(&format!("gtk-cursor-theme-size={n}\n"));
    }
    if let Some(v) = &t.font {
        s.push_str(&format!("gtk-font-name={v}\n"));
    }
    if let Some(dark) = t.dark {
        s.push_str(&format!("gtk-application-prefer-dark-theme={}\n", if dark { 1 } else { 0 }));
    }
    s
}

fn gtkrc2(t: &Theme) -> String {
    let mut s = String::from("# Managed by Manifest OS\n");
    if let Some(v) = &t.gtk {
        s.push_str(&format!("gtk-theme-name=\"{v}\"\n"));
    }
    if let Some(v) = &t.icons {
        s.push_str(&format!("gtk-icon-theme-name=\"{v}\"\n"));
    }
    if let Some(v) = &t.cursor {
        s.push_str(&format!("gtk-cursor-theme-name=\"{v}\"\n"));
    }
    if let Some(n) = t.cursor_size {
        s.push_str(&format!("gtk-cursor-theme-size={n}\n"));
    }
    if let Some(v) = &t.font {
        s.push_str(&format!("gtk-font-name=\"{v}\"\n"));
    }
    s
}

/// `~/.icons/default/index.theme` — the freedesktop way to pick the default
/// cursor theme, honored by X11 and Wayland toolkits alike.
fn cursor_index(cursor: &str) -> String {
    format!(
        "[Icon Theme]\nName=Default\nComment=Managed by Manifest OS\nInherits={cursor}\n"
    )
}

/// LXQt's own config is plain INI, writable at install time. Icon theme is
/// the piece LXQt reads from here; GTK apps use the settings.ini layer and
/// the cursor comes from `~/.icons`/env.
fn lxqt_conf(t: &Theme) -> Option<String> {
    let icons = t.icons.as_ref()?;
    Some(format!("[General]\nicon_theme={icons}\n"))
}

// ---------------------------------------------------------------------------
// Layer 2 — cursor environment
// ---------------------------------------------------------------------------

fn env_dropin(t: &Theme) -> String {
    let mut s = String::from("# Managed by Manifest OS — cursor theme (Qt apps, compositors)\n");
    if let Some(v) = &t.cursor {
        s.push_str(&format!("export XCURSOR_THEME={v}\n"));
    }
    if let Some(n) = t.cursor_size {
        s.push_str(&format!("export XCURSOR_SIZE={n}\n"));
    }
    s
}

// ---------------------------------------------------------------------------
// Layer 3 — first-login native setters for full DEs
// ---------------------------------------------------------------------------

const AUTOSTART: &str = "[Desktop Entry]\n\
Type=Application\n\
Name=Manifest OS theme\n\
Exec=/usr/local/bin/manifest-theme\n\
NoDisplay=true\n\
X-GNOME-Autostart-enabled=true\n\
OnlyShowIn=GNOME;KDE;XFCE;Cinnamon;X-Cinnamon;MATE;Budgie;LXQt;Unity;Deepin;COSMIC;\n";

/// Single-quote `s` for safe embedding in a POSIX shell command line.
/// (Same helper as keybindings.rs — small enough to keep local.)
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// "Family Size" → (family, size). "JetBrains Mono 11" → ("JetBrains Mono", 11).
/// A missing/unparsable size falls back to 10 (Qt/KDE need an explicit one).
fn split_font(font: &str) -> (String, u32) {
    if let Some((family, size)) = font.trim().rsplit_once(' ') {
        if let Ok(n) = size.parse::<u32>() {
            return (family.to_string(), n);
        }
    }
    (font.trim().to_string(), 10)
}

fn runtime_script(t: &Theme) -> String {
    let mut s = String::from(
        "#!/bin/sh\n\
         # Managed by Manifest OS — apply the declared theme for the running desktop.\n\
         marker=\"${XDG_CONFIG_HOME:-$HOME/.config}/manifest-theme.set\"\n\
         [ -e \"$marker\" ] && exit 0\n\
         de=$(printf '%s' \"${XDG_CURRENT_DESKTOP:-}${DESKTOP_SESSION:-}\" | tr 'A-Z' 'a-z')\n\n\
         case \"$de\" in\n",
    );

    // GNOME family — gsettings on org.gnome.desktop.interface.
    s.push_str("  *gnome*|*budgie*|*unity*|*ubuntu*)\n");
    push_gsettings(&mut s, t, "org.gnome.desktop.interface");
    if let Some(dark) = t.dark {
        let scheme = if dark { "prefer-dark" } else { "default" };
        s.push_str(&format!(
            "    gsettings set org.gnome.desktop.interface color-scheme '{scheme}' 2>/dev/null\n"
        ));
    }
    s.push_str("    ;;\n");

    // Cinnamon — its own schema, plus the desktop-shell theme name.
    s.push_str("  *cinnamon*)\n");
    push_gsettings(&mut s, t, "org.cinnamon.desktop.interface");
    if let Some(v) = &t.gtk {
        s.push_str(&format!("    gsettings set org.cinnamon.theme name {} 2>/dev/null\n", sh_quote(v)));
    }
    s.push_str("    ;;\n");

    // MATE — split across three schemas.
    s.push_str("  *mate*)\n");
    if let Some(v) = &t.gtk {
        s.push_str(&format!("    gsettings set org.mate.interface gtk-theme {} 2>/dev/null\n", sh_quote(v)));
        s.push_str(&format!("    gsettings set org.mate.Marco.general theme {} 2>/dev/null\n", sh_quote(v)));
    }
    if let Some(v) = &t.icons {
        s.push_str(&format!("    gsettings set org.mate.interface icon-theme {} 2>/dev/null\n", sh_quote(v)));
    }
    if let Some(v) = &t.font {
        s.push_str(&format!("    gsettings set org.mate.interface font-name {} 2>/dev/null\n", sh_quote(v)));
    }
    if let Some(v) = &t.monospace_font {
        s.push_str(&format!("    gsettings set org.mate.interface monospace-font-name {} 2>/dev/null\n", sh_quote(v)));
    }
    if let Some(v) = &t.cursor {
        s.push_str(&format!("    gsettings set org.mate.peripherals-mouse cursor-theme {} 2>/dev/null\n", sh_quote(v)));
    }
    if let Some(n) = t.cursor_size {
        s.push_str(&format!("    gsettings set org.mate.peripherals-mouse cursor-size {n} 2>/dev/null\n"));
    }
    s.push_str("    ;;\n");

    // KDE — kwriteconfig for kdeglobals, plasma-apply-* for cursor/colors.
    // The GTK theme itself reaches KDE's GTK apps via our settings.ini layer.
    s.push_str("  *kde*|*plasma*)\n");
    s.push_str("    kw=kwriteconfig6; command -v $kw >/dev/null 2>&1 || kw=kwriteconfig5\n");
    if let Some(v) = &t.global {
        // A global theme (look-and-feel) sets the WHOLE look at once — colors,
        // widget style, window decorations, icons, plasma theme, cursor — from
        // the theme's own `defaults`. Apply it, and also record it as the
        // default so System Settings shows it selected even if the live apply is
        // partial. Runs from the session's autostart, so DBus/plasmashell are up.
        s.push_str(&format!("    plasma-apply-lookandfeel -a {} 2>/dev/null\n", sh_quote(v)));
        s.push_str(&format!(
            "    $kw --file kdeglobals --group KDE --key LookAndFeelPackage {} 2>/dev/null\n",
            sh_quote(v)
        ));
    }
    // Per-piece colour/icon settings would CLOBBER the look a global theme just
    // applied (e.g. forcing Papirus/BreezeDark over the theme's own icons and
    // colours), so only apply them when no global theme owns the look.
    if t.global.is_none() {
        if let Some(v) = &t.icons {
            s.push_str(&format!("    $kw --file kdeglobals --group Icons --key Theme {} 2>/dev/null\n", sh_quote(v)));
        }
        if let Some(true) = t.dark {
            s.push_str("    plasma-apply-colorscheme BreezeDark 2>/dev/null\n");
        }
    }
    // Cursor and fonts are additive — safe to set alongside a global theme
    // (only applied when the manifest explicitly asks for them).
    if let Some(v) = &t.cursor {
        s.push_str(&format!("    plasma-apply-cursortheme {} 2>/dev/null\n", sh_quote(v)));
    }
    if let Some(v) = &t.font {
        let (family, size) = split_font(v);
        s.push_str(&format!(
            "    $kw --file kdeglobals --group General --key font {} 2>/dev/null\n",
            sh_quote(&format!("{family},{size},-1,5,50,0,0,0,0,0"))
        ));
    }
    if let Some(v) = &t.monospace_font {
        let (family, size) = split_font(v);
        s.push_str(&format!(
            "    $kw --file kdeglobals --group General --key fixed {} 2>/dev/null\n",
            sh_quote(&format!("{family},{size},-1,5,50,0,0,0,0,0"))
        ));
    }
    s.push_str("    ;;\n");

    // Xfce — xfconf; -n -t creates the property if the channel lacks it.
    s.push_str("  *xfce*)\n");
    let xf = |s: &mut String, channel: &str, prop: &str, typ: &str, val: &str| {
        s.push_str(&format!(
            "    xfconf-query -c {channel} -p {prop} -n -t {typ} -s {val} 2>/dev/null\n"
        ));
    };
    if let Some(v) = &t.gtk {
        xf(&mut s, "xsettings", "/Net/ThemeName", "string", &sh_quote(v));
        xf(&mut s, "xfwm4", "/general/theme", "string", &sh_quote(v));
    }
    if let Some(v) = &t.icons {
        xf(&mut s, "xsettings", "/Net/IconThemeName", "string", &sh_quote(v));
    }
    if let Some(v) = &t.cursor {
        xf(&mut s, "xsettings", "/Gtk/CursorThemeName", "string", &sh_quote(v));
    }
    if let Some(n) = t.cursor_size {
        xf(&mut s, "xsettings", "/Gtk/CursorThemeSize", "int", &n.to_string());
    }
    if let Some(v) = &t.font {
        xf(&mut s, "xsettings", "/Gtk/FontName", "string", &sh_quote(v));
    }
    if let Some(v) = &t.monospace_font {
        xf(&mut s, "xsettings", "/Gtk/MonospaceFontName", "string", &sh_quote(v));
    }
    s.push_str("    ;;\n");

    // Bare WMs / anything else: the GTK files + env vars already cover it.
    // No marker either — if the user later logs into a full DE, still apply.
    s.push_str("  *) exit 0 ;;\nesac\n\nmkdir -p \"$(dirname \"$marker\")\"\n: > \"$marker\"\n");
    s
}

/// The gsettings lines shared by the GNOME and Cinnamon interface schemas
/// (same key names, different schema prefix).
fn push_gsettings(s: &mut String, t: &Theme, schema: &str) {
    if let Some(v) = &t.gtk {
        s.push_str(&format!("    gsettings set {schema} gtk-theme {} 2>/dev/null\n", sh_quote(v)));
    }
    if let Some(v) = &t.icons {
        s.push_str(&format!("    gsettings set {schema} icon-theme {} 2>/dev/null\n", sh_quote(v)));
    }
    if let Some(v) = &t.cursor {
        s.push_str(&format!("    gsettings set {schema} cursor-theme {} 2>/dev/null\n", sh_quote(v)));
    }
    if let Some(n) = t.cursor_size {
        s.push_str(&format!("    gsettings set {schema} cursor-size {n} 2>/dev/null\n"));
    }
    if let Some(v) = &t.font {
        s.push_str(&format!("    gsettings set {schema} font-name {} 2>/dev/null\n", sh_quote(v)));
    }
    if let Some(v) = &t.monospace_font {
        s.push_str(&format!("    gsettings set {schema} monospace-font-name {} 2>/dev/null\n", sh_quote(v)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_theme() -> Theme {
        Theme {
            global: Some("org.kde.breezedark.desktop".into()),
            global_source: None,
            global_install: None,
            gtk: Some("Materia-dark".into()),
            icons: Some("Papirus-Dark".into()),
            cursor: Some("Adwaita".into()),
            cursor_size: Some(24),
            font: Some("Noto Sans 11".into()),
            monospace_font: Some("JetBrains Mono 11".into()),
            dark: Some(true),
        }
    }

    #[test]
    fn settings_ini_has_every_declared_key() {
        let ini = settings_ini(&full_theme());
        assert!(ini.contains("[Settings]"));
        assert!(ini.contains("gtk-theme-name=Materia-dark"));
        assert!(ini.contains("gtk-icon-theme-name=Papirus-Dark"));
        assert!(ini.contains("gtk-cursor-theme-name=Adwaita"));
        assert!(ini.contains("gtk-cursor-theme-size=24"));
        assert!(ini.contains("gtk-font-name=Noto Sans 11"));
        assert!(ini.contains("gtk-application-prefer-dark-theme=1"));
    }

    #[test]
    fn settings_ini_omits_unset_fields() {
        let t = Theme {
            global: None,
            global_source: None,
            global_install: None,
            gtk: Some("Adwaita".into()),
            icons: None,
            cursor: None,
            cursor_size: None,
            font: None,
            monospace_font: None,
            dark: None,
        };
        let ini = settings_ini(&t);
        assert!(ini.contains("gtk-theme-name=Adwaita"));
        assert!(!ini.contains("icon-theme"));
        assert!(!ini.contains("prefer-dark"));
    }

    #[test]
    fn gtkrc2_quotes_strings_but_not_sizes() {
        let rc = gtkrc2(&full_theme());
        assert!(rc.contains("gtk-theme-name=\"Materia-dark\""));
        assert!(rc.contains("gtk-cursor-theme-size=24"));
    }

    #[test]
    fn cursor_index_inherits_the_theme() {
        assert!(cursor_index("Bibata").contains("Inherits=Bibata"));
    }

    #[test]
    fn env_dropin_exports_cursor_vars() {
        let env = env_dropin(&full_theme());
        assert!(env.contains("export XCURSOR_THEME=Adwaita"));
        assert!(env.contains("export XCURSOR_SIZE=24"));
    }

    #[test]
    fn split_font_handles_multiword_families() {
        assert_eq!(split_font("JetBrains Mono 11"), ("JetBrains Mono".into(), 11));
        assert_eq!(split_font("Noto Sans 11"), ("Noto Sans".into(), 11));
        assert_eq!(split_font("NoSize"), ("NoSize".into(), 10));
    }

    #[test]
    fn runtime_script_covers_every_de_family() {
        // full_theme() has a `global`, so the KDE branch applies the look-and-feel
        // (not per-piece Icons/colorscheme overrides — see the global-vs-per-piece
        // test below). These needles cover the other DE families + additive bits.
        let script = runtime_script(&full_theme());
        for needle in [
            "gsettings set org.gnome.desktop.interface gtk-theme 'Materia-dark'",
            "color-scheme 'prefer-dark'",
            "gsettings set org.cinnamon.desktop.interface gtk-theme",
            "gsettings set org.cinnamon.theme name 'Materia-dark'",
            "gsettings set org.mate.interface gtk-theme",
            "gsettings set org.mate.peripherals-mouse cursor-theme",
            "plasma-apply-cursortheme 'Adwaita'",
            "'Noto Sans,11,-1,5,50,0,0,0,0,0'",
            "xfconf-query -c xsettings -p /Net/ThemeName",
            "xfconf-query -c xfwm4 -p /general/theme",
            "/Gtk/CursorThemeSize -n -t int -s 24",
        ] {
            assert!(script.contains(needle), "script missing: {needle}\n---\n{script}");
        }
    }

    #[test]
    fn kde_global_theme_applies_and_records_default_without_clobbering() {
        // With a global theme, apply the look-and-feel + record it as the KDE
        // default, and DON'T let per-piece icons/colorscheme override its look.
        let script = runtime_script(&full_theme()); // global = org.kde.breezedark.desktop
        assert!(script.contains("plasma-apply-lookandfeel -a 'org.kde.breezedark.desktop'"));
        assert!(script.contains("--group KDE --key LookAndFeelPackage 'org.kde.breezedark.desktop'"));
        assert!(!script.contains("--key Theme 'Papirus-Dark'"), "global theme must not be clobbered by icons override");
        assert!(!script.contains("plasma-apply-colorscheme BreezeDark"), "global theme must not be clobbered by colorscheme override");
    }

    #[test]
    fn kde_without_global_still_sets_pieces() {
        // No global → the per-piece KDE setters (icons, dark colorscheme) apply.
        let t = Theme { global: None, ..full_theme() };
        let script = runtime_script(&t);
        assert!(script.contains("--file kdeglobals --group Icons --key Theme 'Papirus-Dark'"));
        assert!(script.contains("plasma-apply-colorscheme BreezeDark"));
        assert!(!script.contains("plasma-apply-lookandfeel"));
    }

    #[test]
    fn runtime_script_marks_only_after_a_de_matched() {
        let script = runtime_script(&full_theme());
        // WMs exit before the marker is written, so a later DE login still applies.
        assert!(script.contains("*) exit 0 ;;"));
        assert!(script.trim_end().ends_with(": > \"$marker\""));
    }

    #[test]
    fn lxqt_conf_sets_icon_theme() {
        assert_eq!(lxqt_conf(&full_theme()).unwrap(), "[General]\nicon_theme=Papirus-Dark\n");
        let no_icons = Theme { icons: None, ..full_theme() };
        assert!(lxqt_conf(&no_icons).is_none());
    }

    #[test]
    fn empty_theme_is_detected() {
        let t = Theme {
            global: None,
            global_source: None,
            global_install: None,
            gtk: None,
            icons: None,
            cursor: None,
            cursor_size: None,
            font: None,
            monospace_font: None,
            dark: None,
        };
        assert!(t.is_empty());
        assert!(!full_theme().is_empty());
    }
}
