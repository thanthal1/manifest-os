//! Android apps via **Waydroid** — the "android stratum" (`docs/strata-design.md`
//! §13). A single Android container on the *host* kernel (never booted as init),
//! its apps composited into the Wayland session and launchable straight from
//! fuzzel/rofi. The engine stays a thin orchestrator of the standard `waydroid`
//! tool, mirroring [`crate::strata`]/[`crate::flatpak`].
//!
//! Two phases, because Waydroid app management needs a running **user Wayland
//! session** that doesn't exist at `manifest install` time (root, no display):
//!
//! 1. **Install time (here, root):** install Waydroid (AUR), ensure `binderfs`,
//!    `waydroid init` (pinned system image), enable the container service, and
//!    drop two host commands — `android-install` (the cmd installer) and a
//!    first-login hook.
//! 2. **First graphical session (the hook, as the user):** start the Waydroid
//!    session, set multi-window mode, install an in-Android app store (F-Droid),
//!    install the declared apps, and ensure a launcher exists for each exposed
//!    app so it shows in fuzzel. Guarded to run its heavy steps once per user.

use crate::exec::Ctx;
use crate::manifest::Android;
use anyhow::Result;

/// Host cmd installer: installs an APK path or an F-Droid package id into the
/// Waydroid container. This is the "cmd installer on the android".
const INSTALLER: &str = "/usr/local/bin/android-install";
/// First-login hook script (needs the user's Wayland session).
const FIRSTRUN: &str = "/usr/local/bin/waydroid-firstrun";
/// System XDG autostart entry that runs the hook once per user session.
const AUTOSTART: &str = "/etc/xdg/autostart/manifest-waydroid-firstrun.desktop";

pub fn apply(a: &Android, ctx: &Ctx) -> Result<()> {
    ensure_waydroid(ctx)?;
    ensure_binder(ctx)?;
    waydroid_init(a, ctx)?;
    ctx.sudo(
        "systemctl",
        &["enable", "--now", "waydroid-container.service"],
    )?;
    write_installer(ctx)?;
    write_firstrun(a, ctx)?;
    Ok(())
}

/// Install Waydroid. It lives in the AUR, so bootstrap `paru` first (reused from
/// the package pipeline), then install as the user — makepkg refuses root.
fn ensure_waydroid(ctx: &Ctx) -> Result<()> {
    if ctx.check("sh", &["-c", "command -v waydroid"]) {
        println!("  · waydroid already installed");
        return Ok(());
    }
    println!("  · installing waydroid (AUR)");
    crate::pacman::bootstrap_paru(ctx)?;
    ctx.shell("paru -S --needed --noconfirm waydroid", false)
}

/// `binderfs` is the kernel gate. Modern mainline kernels build it in; try to
/// load the module and mount the fs, best-effort. Waydroid's own service also
/// sets this up, so a failure here isn't fatal — but warn, because without
/// binder there is no Android.
fn ensure_binder(ctx: &Ctx) -> Result<()> {
    println!("  · ensuring binderfs (kernel gate for Waydroid)");
    ctx.shell(
        "modprobe binder_linux 2>/dev/null || modprobe binderfs 2>/dev/null || \
         echo 'strata/android: no binder module — kernel needs CONFIG_ANDROID_BINDERFS; \
Waydroid will fail without it' >&2",
        true,
    )
}

/// `waydroid init`, idempotent: skip if already initialised. Pins the system
/// image type when declared (VANILLA/GAPPS/FOSS).
fn waydroid_init(a: &Android, ctx: &Ctx) -> Result<()> {
    if ctx.check("sh", &["-c", "test -f /var/lib/waydroid/waydroid.cfg"]) {
        println!("  · waydroid already initialised — skipping");
        return Ok(());
    }
    println!("  · waydroid init{}", a.system.as_deref().map(|s| format!(" (-s {s})")).unwrap_or_default());
    ctx.shell(&waydroid_init_cmd(a), true)
}

/// Build the `waydroid init` command line. Pure — unit-tested.
fn waydroid_init_cmd(a: &Android) -> String {
    let mut cmd = String::from("waydroid init");
    if let Some(sys) = &a.system {
        // Guard against shell-meta in a declared value.
        cmd.push_str(" -s ");
        cmd.push_str(&shq(sys));
    }
    cmd
}

/// Write the `android-install` command. Pure content in [`installer_script`].
fn write_installer(ctx: &Ctx) -> Result<()> {
    println!("  · installing the `android-install` command");
    ctx.write_root(INSTALLER, installer_script())?;
    ctx.sudo("chmod", &["0755", INSTALLER])
}

/// `android-install <apk-path | fdroid-id> …` — installs into Waydroid. An APK
/// path installs directly; a bare id is fetched from F-Droid (its API resolves
/// the suggested version) then installed. Runs as the user (needs their session).
fn installer_script() -> &'static str {
    "#!/bin/sh\n\
     # ManifestOS — install an Android app into Waydroid (generated; do not edit).\n\
     # Usage: android-install <file.apk | fdroid.package.id> [more…]\n\
     set -e\n\
     [ $# -ge 1 ] || { echo 'usage: android-install <file.apk | fdroid.package.id> …' >&2; exit 2; }\n\
     command -v waydroid >/dev/null || { echo 'android-install: waydroid is not installed' >&2; exit 1; }\n\
     for app in \"$@\"; do\n  \
       case \"$app\" in\n    \
         *.apk)\n      \
           waydroid app install \"$app\" ;;\n    \
         *)\n      \
           # Treat as an F-Droid package id: resolve the suggested version, fetch, install.\n      \
           vc=$(curl -fsSL \"https://f-droid.org/api/v1/packages/$app\" | \
                sed -n 's/.*\"suggestedVersionCode\"[: ]*\\([0-9]*\\).*/\\1/p' | head -n1)\n      \
           [ -n \"$vc\" ] || { echo \"android-install: '$app' not found on F-Droid\" >&2; exit 1; }\n      \
           tmp=$(mktemp --suffix=.apk)\n      \
           curl -fsSL -o \"$tmp\" \"https://f-droid.org/repo/${app}_${vc}.apk\"\n      \
           waydroid app install \"$tmp\"\n      \
           rm -f \"$tmp\" ;;\n  \
       esac\n\
     done\n"
}

/// Write the first-login hook script + its autostart entry.
fn write_firstrun(a: &Android, ctx: &Ctx) -> Result<()> {
    println!("  · installing the first-login Waydroid setup hook");
    ctx.write_root(FIRSTRUN, &firstrun_script(a))?;
    ctx.sudo("chmod", &["0755", FIRSTRUN])?;
    ctx.write_root(AUTOSTART, autostart_entry())
}

/// The first-graphical-session setup: start the session, multi-window mode, an
/// in-Android app store (F-Droid), the declared apps, and a launcher per exposed
/// app so it shows in fuzzel. Guarded to run once per user. Pure — unit-tested.
fn firstrun_script(a: &Android) -> String {
    let apps = a.apps.iter().map(|s| shq(s)).collect::<Vec<_>>().join(" ");
    let expose = a.expose.iter().map(|s| shq(s)).collect::<Vec<_>>().join(" ");
    let multi = if a.mode.as_deref() == Some("fullscreen") { "false" } else { "true" };
    format!(
        "#!/bin/sh\n\
         # ManifestOS — first-session Waydroid setup (generated; do not edit).\n\
         # Needs the user's Wayland session, so it can't run at install time.\n\
         command -v waydroid >/dev/null || exit 0\n\
         MARK=\"${{XDG_DATA_HOME:-$HOME/.local/share}}/manifest-waydroid-firstrun.done\"\n\
         [ -e \"$MARK\" ] && exit 0\n\
         # Start the session and wait (best-effort) for it to come up.\n\
         waydroid session start >/dev/null 2>&1 &\n\
         i=0; while ! waydroid status 2>/dev/null | grep -qi 'session.*running'; do\n  \
           i=$((i+1)); [ \"$i\" -gt 30 ] && break; sleep 2\n\
         done\n\
         # Multi-window so each app is its own toplevel (fuzzel-friendly).\n\
         waydroid prop set persist.waydroid.multi_windows {multi} 2>/dev/null || true\n\
         # An in-Android app store — the GUI installer on the Android side.\n\
         waydroid app list 2>/dev/null | grep -q org.fdroid.fdroid || {{\n  \
           curl -fsSL -o /tmp/fdroid.apk https://f-droid.org/F-Droid.apk && \
             waydroid app install /tmp/fdroid.apk || true\n\
         }}\n\
         # Declared apps (via the host cmd installer).\n\
         for a in {apps}; do android-install \"$a\" || true; done\n\
         # Ensure a launcher exists for each exposed app so fuzzel/rofi shows it.\n\
         APPDIR=\"${{XDG_DATA_HOME:-$HOME/.local/share}}/applications\"\n\
         mkdir -p \"$APPDIR\"\n\
         for p in {expose}; do\n  \
           d=\"$APPDIR/waydroid.$p.desktop\"\n  \
           [ -e \"$d\" ] || printf '[Desktop Entry]\\nType=Application\\nName=%s\\nExec=waydroid app launch %s\\nIcon=waydroid\\nCategories=Android;\\nX-ManifestOS-Strata=android\\n' \"$p\" \"$p\" > \"$d\"\n\
         done\n\
         mkdir -p \"$(dirname \"$MARK\")\"; : > \"$MARK\"\n"
    )
}

/// The system XDG autostart entry that runs the hook in every user session (the
/// script self-guards to do its heavy work only once). Pure — unit-tested.
fn autostart_entry() -> &'static str {
    "[Desktop Entry]\n\
     Type=Application\n\
     Name=ManifestOS Android setup\n\
     Comment=First-run Waydroid session setup (self-guards; runs once)\n\
     Exec=/usr/local/bin/waydroid-firstrun\n\
     OnlyShowIn=GNOME;KDE;XFCE;LXQt;MATE;Cinnamon;Hyprland;sway;niri;Wayland;\n\
     NoDisplay=true\n\
     X-GNOME-Autostart-enabled=true\n"
}

/// Minimal single-quote shell escaping for a declared value going into a
/// generated script (package ids/paths; closes and re-opens the quote around any
/// embedded `'`). Pure — unit-tested.
fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn android(system: Option<&str>, mode: Option<&str>, apps: &[&str], expose: &[&str]) -> Android {
        Android {
            system: system.map(String::from),
            mode: mode.map(String::from),
            apps: apps.iter().map(|s| s.to_string()).collect(),
            expose: expose.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn init_cmd_pins_system_and_quotes() {
        assert_eq!(waydroid_init_cmd(&android(None, None, &[], &[])), "waydroid init");
        assert_eq!(
            waydroid_init_cmd(&android(Some("GAPPS"), None, &[], &[])),
            "waydroid init -s 'GAPPS'"
        );
    }

    #[test]
    fn installer_handles_apk_and_fdroid_id() {
        let s = installer_script();
        assert!(s.starts_with("#!/bin/sh"), "{s}");
        assert!(s.contains("*.apk)"), "apk branch: {s}");
        assert!(s.contains("waydroid app install"), "{s}");
        assert!(s.contains("f-droid.org/api/v1/packages/"), "fdroid resolve: {s}");
        assert!(s.contains("suggestedVersionCode"), "{s}");
    }

    #[test]
    fn firstrun_bakes_apps_expose_and_store() {
        let s = firstrun_script(&android(None, None, &["org.telegram.messenger"], &["org.telegram.messenger"]));
        // Guarded once-per-user.
        assert!(s.contains("manifest-waydroid-firstrun.done"), "{s}");
        // Installs the in-Android store.
        assert!(s.contains("org.fdroid.fdroid"), "{s}");
        assert!(s.contains("F-Droid.apk"), "{s}");
        // Declared app + a launcher for the exposed one.
        assert!(s.contains("android-install \"$a\""), "{s}");
        assert!(s.contains("'org.telegram.messenger'"), "quoted app: {s}");
        assert!(s.contains("waydroid app launch"), "launcher exec: {s}");
        // Multi-window default.
        assert!(s.contains("persist.waydroid.multi_windows true"), "{s}");
    }

    #[test]
    fn firstrun_fullscreen_disables_multiwindow() {
        let s = firstrun_script(&android(None, Some("fullscreen"), &[], &[]));
        assert!(s.contains("persist.waydroid.multi_windows false"), "{s}");
    }

    #[test]
    fn autostart_runs_the_hook() {
        let s = autostart_entry();
        assert!(s.contains("Exec=/usr/local/bin/waydroid-firstrun"), "{s}");
        assert!(s.contains("NoDisplay=true"), "{s}");
    }

    #[test]
    fn shq_escapes_single_quotes() {
        assert_eq!(shq("abc"), "'abc'");
        assert_eq!(shq("a'b"), "'a'\\''b'");
    }
}
