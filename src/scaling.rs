//! Cross-desktop display scaling (HiDPI).
//!
//! "Set the UI scale" is, like wallpaper and theming, different everywhere. This
//! applies the manifest's `display.scale` in two layers:
//!
//!   1. A **universal env drop-in** (`/etc/profile.d/manifest-scale.sh`):
//!      `GDK_SCALE`/`GDK_DPI_SCALE` (GTK), `QT_SCALE_FACTOR` (Qt) and a scaled
//!      `XCURSOR_SIZE`. This scales the *applications* — including the GTK bits
//!      of a bare-WM rice (waybar, wofi, nm-applet) — whatever compositor runs.
//!   2. A **first-login script** for full desktop environments, whose settings
//!      daemons ignore the raw env: it detects the running desktop (same pattern
//!      as [`crate::wallpaper`]) and sets the native scale — gsettings
//!      text-scaling (GNOME/Cinnamon/MATE), forceFontDPI (KDE), Xft/DPI (Xfce).
//!      Runs once per user (a marker file) so they can still change it.
//!
//! A Wayland compositor's own per-output scale lives in *its* config, not an
//! env var — so a WM rice sets `monitor …, <scale>` (Hyprland) / `scale <n>`
//! (niri/sway) itself, ideally via the `{{scale}}` token (the auto-detected
//! `scale` fact) so it tracks the panel. Layer 1 still enlarges every GTK/Qt
//! app on top of that.

use crate::exec::Ctx;
use anyhow::Result;

// `zz-` so it sources AFTER manifest-theme.sh / manifest-desktop.sh — otherwise
// the theme's fixed cursor_size (XCURSOR_SIZE) would clobber the scaled cursor.
const ENV_PATH: &str = "/etc/profile.d/zz-manifest-scale.sh";
const SCRIPT_PATH: &str = "/usr/local/bin/manifest-scale";
const AUTOSTART_PATH: &str = "/etc/xdg/autostart/manifest-scale.desktop";

pub fn apply(scale: f64, ctx: &Ctx) -> Result<()> {
    // 100% (or anything invalid) is a no-op — nothing to scale.
    if !(scale.is_finite() && scale > 1.0) {
        return Ok(());
    }
    println!("  · display scale: {}×", fmt(scale));
    ctx.write_root(ENV_PATH, &env_dropin(scale))?;
    ctx.write_root(SCRIPT_PATH, &runtime_script(scale))?;
    ctx.sudo("chmod", &["755", SCRIPT_PATH])?;
    ctx.write_root(AUTOSTART_PATH, AUTOSTART)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Layer 1 — universal GTK/Qt env
// ---------------------------------------------------------------------------

fn env_dropin(scale: f64) -> String {
    // GDK_SCALE is integer; fold the fractional remainder into GDK_DPI_SCALE so
    // e.g. 1.5 = GDK_SCALE 2 × GDK_DPI_SCALE 0.75. Qt takes the raw factor.
    let gdk_scale = scale.round().max(1.0);
    let gdk_dpi = scale / gdk_scale;
    let cursor = (24.0 * scale).round() as u32;
    format!(
        "# Managed by Manifest OS — display scaling ({sc}×)\n\
         export GDK_SCALE={gs}\n\
         export GDK_DPI_SCALE={dpi}\n\
         export QT_AUTO_SCREEN_SCALE_FACTOR=0\n\
         export QT_SCALE_FACTOR={sc}\n\
         export XCURSOR_SIZE={cursor}\n\
         export ELECTRON_OZONE_PLATFORM_HINT=auto\n",
        sc = fmt(scale),
        gs = gdk_scale as u32,
        dpi = fmt(gdk_dpi),
        cursor = cursor,
    )
}

// ---------------------------------------------------------------------------
// Layer 2 — first-login native scale for full DEs
// ---------------------------------------------------------------------------

const AUTOSTART: &str = "[Desktop Entry]\n\
Type=Application\n\
Name=Manifest OS scale\n\
Exec=/usr/local/bin/manifest-scale\n\
NoDisplay=true\n\
X-GNOME-Autostart-enabled=true\n\
OnlyShowIn=GNOME;KDE;XFCE;Cinnamon;X-Cinnamon;MATE;Budgie;Unity;Deepin;COSMIC;\n";

fn runtime_script(scale: f64) -> String {
    let sc = fmt(scale);
    let int_scale = scale.round().max(1.0) as u32; // for integer-only knobs
    let dpi = (96.0 * scale).round() as u32; // Xft / KDE font DPI
    format!(
        "#!/bin/sh\n\
         # Managed by Manifest OS — apply the display scale for the running desktop.\n\
         marker=\"${{XDG_CONFIG_HOME:-$HOME/.config}}/manifest-scale.set\"\n\
         [ -e \"$marker\" ] && exit 0\n\
         de=$(printf '%s' \"${{XDG_CURRENT_DESKTOP:-}}${{DESKTOP_SESSION:-}}\" | tr 'A-Z' 'a-z')\n\n\
         case \"$de\" in\n\
         \x20 *gnome*|*budgie*|*unity*)\n\
         \x20   gsettings set org.gnome.desktop.interface text-scaling-factor {sc} 2>/dev/null\n\
         \x20   gsettings set org.gnome.desktop.interface scaling-factor {int_scale} 2>/dev/null ;;\n\
         \x20 *cinnamon*)\n\
         \x20   gsettings set org.cinnamon.desktop.interface text-scaling-factor {sc} 2>/dev/null\n\
         \x20   gsettings set org.cinnamon.desktop.interface scaling-factor {int_scale} 2>/dev/null ;;\n\
         \x20 *mate*)\n\
         \x20   gsettings set org.mate.interface window-scaling-factor {int_scale} 2>/dev/null ;;\n\
         \x20 *kde*|*plasma*)\n\
         \x20   # KDE scales the whole UI itself. Use its scale factor — NOT\n\
         \x20   # forceFontDPI, which scales fonts only and (atop Plasma's own\n\
         \x20   # HiDPI auto-scale) overflows the panel off-screen. kscreen-doctor\n\
         \x20   # applies it live on Wayland; ScaleFactor persists it for X11.\n\
         \x20   kw=kwriteconfig6; command -v $kw >/dev/null 2>&1 || kw=kwriteconfig5\n\
         \x20   $kw --file kdeglobals --group KScreen --key ScaleFactor {sc} 2>/dev/null\n\
         \x20   if command -v kscreen-doctor >/dev/null 2>&1; then\n\
         \x20     for o in $(kscreen-doctor -o 2>/dev/null | sed -n 's/^Output: [0-9]* \\([^ ]*\\).*/\\1/p'); do\n\
         \x20       kscreen-doctor output.$o.scale.{sc} 2>/dev/null\n\
         \x20     done\n\
         \x20   fi ;;\n\
         \x20 *xfce*)\n\
         \x20   xfconf-query -c xsettings -p /Xft/DPI -n -t int -s {dpi} 2>/dev/null ;;\n\
         \x20 *) exit 0 ;;\n\
         esac\n\n\
         mkdir -p \"$(dirname \"$marker\")\"\n\
         : > \"$marker\"\n",
        sc = sc,
        int_scale = int_scale,
        dpi = dpi,
    )
}

/// Format a scale factor compactly: `2`, `1.5`, `0.75` — no trailing zeros.
fn fmt(n: f64) -> String {
    let s = format!("{n:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_trims_trailing_zeros() {
        assert_eq!(fmt(2.0), "2");
        assert_eq!(fmt(1.5), "1.5");
        assert_eq!(fmt(0.75), "0.75");
    }

    #[test]
    fn env_dropin_splits_integer_and_fractional_scale() {
        let two = env_dropin(2.0);
        assert!(two.contains("export GDK_SCALE=2"));
        assert!(two.contains("export GDK_DPI_SCALE=1"));
        assert!(two.contains("export QT_SCALE_FACTOR=2"));
        assert!(two.contains("export XCURSOR_SIZE=48"));

        let onefive = env_dropin(1.5);
        assert!(onefive.contains("export GDK_SCALE=2"));
        assert!(onefive.contains("export GDK_DPI_SCALE=0.75"));
        assert!(onefive.contains("export QT_SCALE_FACTOR=1.5"));
        assert!(onefive.contains("export XCURSOR_SIZE=36"));
    }

    #[test]
    fn runtime_script_covers_the_de_families() {
        let s = runtime_script(2.0);
        for needle in [
            "gsettings set org.gnome.desktop.interface text-scaling-factor 2",
            "gsettings set org.cinnamon.desktop.interface scaling-factor 2",
            "gsettings set org.mate.interface window-scaling-factor 2",
            "--file kdeglobals --group KScreen --key ScaleFactor 2",
            "kscreen-doctor output.$o.scale.2",
            "xfconf-query -c xsettings -p /Xft/DPI -n -t int -s 192",
            "*) exit 0 ;;",
        ] {
            assert!(s.contains(needle), "missing: {needle}\n---\n{s}");
        }
    }

    #[test]
    fn scale_one_is_a_noop() {
        let ctx = Ctx::new(true);
        // 1.0 (and NaN / <1) write nothing and return Ok.
        assert!(apply(1.0, &ctx).is_ok());
        assert!(apply(f64::NAN, &ctx).is_ok());
    }
}
