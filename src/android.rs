//! Android apps via **Waydroid** — the "android stratum" (`docs/strata-design.md`
//! §13). A single Android container on the *host* kernel (never booted as init),
//! composited into the Wayland session and launchable from fuzzel/rofi.
//!
//! **Lazy lifecycle** (Android is *not* kept in the background):
//! - Nothing runs at boot — the container service is left disabled.
//! - `waydroid-launch <pkg>` (what every app launcher calls) brings Android up on
//!   demand: start the container (passwordless via a scoped sudoers rule), start
//!   the session, launch the app, and stamp an activity marker.
//! - A per-user `waydroid-idle.timer` runs `waydroid-idle` every few minutes; when
//!   no Waydroid window is open and the last activity is older than the configured
//!   idle timeout (default 45 min), it stops the session **and** the container so
//!   Android releases its RAM.
//!
//! Two setup phases, because Waydroid app management needs a live user Wayland
//! session that doesn't exist at `manifest install` time (root, no display):
//! install-time work here, session-dependent work in a guarded first-login hook.

use crate::exec::Ctx;
use crate::manifest::Android;
use anyhow::{bail, Result};

const INSTALLER: &str = "/usr/local/bin/android-install";
const LAUNCHER: &str = "/usr/local/bin/waydroid-launch";
const IDLE: &str = "/usr/local/bin/waydroid-idle";
const FIRSTRUN: &str = "/usr/local/bin/waydroid-firstrun";
const AUTOSTART: &str = "/etc/xdg/autostart/manifest-waydroid-firstrun.desktop";
const SUDOERS: &str = "/etc/sudoers.d/manifest-waydroid";
const IDLE_SERVICE: &str = "/etc/systemd/user/waydroid-idle.service";
const IDLE_TIMER: &str = "/etc/systemd/user/waydroid-idle.timer";
/// Default auto-stop timeout when `idle_minutes` is unset.
const DEFAULT_IDLE_MIN: u32 = 45;

pub fn apply(a: &Android, ctx: &Ctx) -> Result<()> {
    ensure_waydroid(ctx)?;
    ensure_binder(ctx)?;
    waydroid_init(a, ctx)?;
    // Lazy: never auto-run. Undo any prior `enable --now` (e.g. from -19) so the
    // container only comes up on demand.
    ctx.shell(
        "systemctl disable --now waydroid-container.service 2>/dev/null || true",
        true,
    )?;
    write_sudoers(ctx)?;
    write_installer(ctx)?;
    write_launcher(ctx)?;
    write_idle(a, ctx)?;
    write_firstrun(a, ctx)?;
    Ok(())
}

/// Install Waydroid (AUR → bootstrap paru first, then install as the user).
fn ensure_waydroid(ctx: &Ctx) -> Result<()> {
    if ctx.check("sh", &["-c", "command -v waydroid"]) {
        println!("  · waydroid already installed");
        return Ok(());
    }
    println!("  · installing waydroid (AUR)");
    crate::pacman::bootstrap_paru(ctx)?;
    ctx.shell("paru -S --needed --noconfirm waydroid", false)
}

/// `binderfs` is the kernel gate — try to load/mount it, best-effort, and warn
/// loudly (don't hard-fail) if the kernel lacks it.
fn ensure_binder(ctx: &Ctx) -> Result<()> {
    println!("  · ensuring binderfs (kernel gate for Waydroid)");
    ctx.shell(
        "modprobe binder_linux 2>/dev/null || modprobe binderfs 2>/dev/null || \
         echo 'android: no binder module — kernel needs CONFIG_ANDROID_BINDERFS; \
Waydroid will not run without it' >&2",
        true,
    )
}

/// `waydroid init`, idempotent (skip if already initialised). Pins the system
/// image type when declared.
fn waydroid_init(a: &Android, ctx: &Ctx) -> Result<()> {
    if ctx.check("sh", &["-c", "test -f /var/lib/waydroid/waydroid.cfg"]) {
        println!("  · waydroid already initialised — skipping");
        return Ok(());
    }
    println!("  · waydroid init");
    ctx.shell(&waydroid_init_cmd(a), true)
}

/// Build the `waydroid init` command line. Pure — unit-tested.
fn waydroid_init_cmd(a: &Android) -> String {
    let mut cmd = String::from("waydroid init");
    if let Some(sys) = &a.system {
        cmd.push_str(" -s ");
        cmd.push_str(&shq(sys));
    }
    cmd
}

/// A scoped sudoers rule so the user's launcher / idle watchdog can start and
/// stop the (root-owned) Waydroid container service without a password — the only
/// privileged bit of the lazy lifecycle. Narrow: just `systemctl start|stop
/// waydroid-container(.service)`, nothing else. Validated with `visudo` first.
fn write_sudoers(ctx: &Ctx) -> Result<()> {
    println!("  · scoped sudoers for on-demand container start/stop");
    let staged = format!("{SUDOERS}.staged");
    ctx.write_root(&staged, sudoers_content())?;
    ctx.sudo("chmod", &["0440", &staged])?;
    if ctx.dry_run || ctx.check("visudo", &["-cf", &staged]) {
        ctx.sudo("mv", &[&staged, SUDOERS])
    } else {
        ctx.sudo("rm", &["-f", &staged])?;
        bail!("generated waydroid sudoers failed visudo validation");
    }
}

fn sudoers_content() -> &'static str {
    "# ManifestOS Waydroid — passwordless container start/stop for the lazy\n\
     # lifecycle (generated). Scoped to just this one service; Android app\n\
     # management still runs unprivileged as the user.\n\
     ALL ALL=(root) NOPASSWD: /usr/bin/systemctl start waydroid-container, \
     /usr/bin/systemctl start waydroid-container.service, \
     /usr/bin/systemctl stop waydroid-container, \
     /usr/bin/systemctl stop waydroid-container.service\n"
}

/// Shell snippet that brings Android up on demand: start the container
/// (passwordless), then the session, waiting for it to come up.
fn ensure_up() -> &'static str {
    "systemctl is-active --quiet waydroid-container 2>/dev/null || \
       sudo systemctl start waydroid-container\n\
     waydroid status 2>/dev/null | grep -qi 'session.*running' || {\n  \
       waydroid session start >/dev/null 2>&1 &\n  \
       i=0; while ! waydroid status 2>/dev/null | grep -qi 'session.*running'; do\n    \
         i=$((i+1)); [ \"$i\" -gt 60 ] && break; sleep 1\n  \
       done\n\
     }\n"
}

/// Snippet that stamps the activity marker (touched on launch + by the watchdog
/// while an app is visible).
fn stamp_activity() -> &'static str {
    "ACT=\"${XDG_STATE_HOME:-$HOME/.local/state}/waydroid-activity\"\n\
     mkdir -p \"$(dirname \"$ACT\")\"; : > \"$ACT\"\n"
}

/// Route Waydroid's own auto-generated launchers through our lazy wrapper so a
/// click from fuzzel brings Android up on demand instead of failing while it's
/// stopped. Rewrites only .desktop files that actually call `waydroid app launch`.
fn relaunch_rewrite() -> &'static str {
    "APPDIR=\"${XDG_DATA_HOME:-$HOME/.local/share}/applications\"\n\
     for f in \"$APPDIR\"/*.desktop; do\n  \
       [ -e \"$f\" ] || continue\n  \
       grep -q 'waydroid app launch' \"$f\" && \
         sed -i 's#waydroid app launch#/usr/local/bin/waydroid-launch#g' \"$f\"\n\
     done\n"
}

fn write_installer(ctx: &Ctx) -> Result<()> {
    println!("  · installing the `android-install` command");
    ctx.write_root(INSTALLER, &installer_script())?;
    ctx.sudo("chmod", &["0755", INSTALLER])
}

/// `android-install <apk-path | fdroid-id> …` — brings Android up if needed,
/// installs (APK directly; a bare id is resolved via F-Droid's API and fetched),
/// then relazies the fresh launchers. The cmd installer on the Android side.
fn installer_script() -> String {
    format!(
        "#!/bin/sh\n\
         # ManifestOS — install an Android app into Waydroid (generated; do not edit).\n\
         # Usage: android-install <file.apk | fdroid.package.id> [more…]\n\
         set -e\n\
         [ $# -ge 1 ] || {{ echo 'usage: android-install <file.apk | fdroid.package.id> …' >&2; exit 2; }}\n\
         command -v waydroid >/dev/null || {{ echo 'android-install: waydroid is not installed' >&2; exit 1; }}\n\
         {ensure}\
         for app in \"$@\"; do\n  \
           case \"$app\" in\n    \
             *.apk)\n      \
               waydroid app install \"$app\" ;;\n    \
             *)\n      \
               vc=$(curl -fsSL \"https://f-droid.org/api/v1/packages/$app\" | \
                    sed -n 's/.*\"suggestedVersionCode\"[: ]*\\([0-9]*\\).*/\\1/p' | head -n1)\n      \
               [ -n \"$vc\" ] || {{ echo \"android-install: '$app' not found on F-Droid\" >&2; exit 1; }}\n      \
               tmp=$(mktemp --suffix=.apk)\n      \
               curl -fsSL -o \"$tmp\" \"https://f-droid.org/repo/${{app}}_${{vc}}.apk\"\n      \
               waydroid app install \"$tmp\"; rm -f \"$tmp\" ;;\n  \
           esac\n\
         done\n\
         {relazy}\
         {stamp}",
        ensure = ensure_up(),
        relazy = relaunch_rewrite(),
        stamp = stamp_activity(),
    )
}

fn write_launcher(ctx: &Ctx) -> Result<()> {
    println!("  · installing the `waydroid-launch` lazy launcher");
    ctx.write_root(LAUNCHER, &launcher_script())?;
    ctx.sudo("chmod", &["0755", LAUNCHER])
}

/// `waydroid-launch <pkg>` — the Exec every Android app launcher points at.
/// Brings Android up on demand, launches, and stamps activity. Pure — tested.
fn launcher_script() -> String {
    format!(
        "#!/bin/sh\n\
         # ManifestOS — lazy-launch a Waydroid app (generated; do not edit).\n\
         set -e\n\
         [ $# -ge 1 ] || {{ echo 'usage: waydroid-launch <package>' >&2; exit 2; }}\n\
         {ensure}\
         waydroid app launch \"$1\"\n\
         {stamp}",
        ensure = ensure_up(),
        stamp = stamp_activity(),
    )
}

fn write_idle(a: &Android, ctx: &Ctx) -> Result<()> {
    let mins = a.idle_minutes.unwrap_or(DEFAULT_IDLE_MIN);
    println!("  · installing the idle watchdog ({})", if mins == 0 { "disabled — stays resident".into() } else { format!("auto-stop after {mins} min") });
    ctx.write_root(IDLE, &idle_script(mins))?;
    ctx.sudo("chmod", &["0755", IDLE])?;
    ctx.write_root(IDLE_SERVICE, idle_service_unit())?;
    ctx.write_root(IDLE_TIMER, idle_timer_unit())
}

/// `waydroid-idle` — run by the per-user timer. Stops the session + container
/// once Android has been unused (no Waydroid window open) for the timeout. A
/// zero timeout disables auto-stop. Pure — unit-tested.
fn idle_script(minutes: u32) -> String {
    let secs = minutes.saturating_mul(60);
    format!(
        "#!/bin/sh\n\
         # ManifestOS — stop idle Waydroid (generated; do not edit).\n\
         IDLE={secs}\n\
         [ \"$IDLE\" -eq 0 ] && exit 0   # 0 = never auto-stop\n\
         ACT=\"${{XDG_STATE_HOME:-$HOME/.local/state}}/waydroid-activity\"\n\
         # Nothing running → nothing to do.\n\
         waydroid status 2>/dev/null | grep -qi 'session.*running' || exit 0\n\
         # Any Waydroid window open right now? (best-effort per compositor.)\n\
         open=unknown\n\
         if command -v hyprctl >/dev/null 2>&1; then\n  \
           hyprctl clients -j 2>/dev/null | grep -qi waydroid && open=yes || open=no\n\
         elif command -v swaymsg >/dev/null 2>&1; then\n  \
           swaymsg -t get_tree 2>/dev/null | grep -qi waydroid && open=yes || open=no\n\
         elif command -v niri >/dev/null 2>&1; then\n  \
           niri msg windows 2>/dev/null | grep -qi waydroid && open=yes || open=no\n\
         fi\n\
         if [ \"$open\" = yes ]; then\n  \
           mkdir -p \"$(dirname \"$ACT\")\"; : > \"$ACT\"; exit 0   # in use → keep alive\n\
         fi\n\
         now=$(date +%s)\n\
         last=$([ -e \"$ACT\" ] && stat -c %Y \"$ACT\" 2>/dev/null || echo 0)\n\
         [ \"$last\" -eq 0 ] && {{ mkdir -p \"$(dirname \"$ACT\")\"; : > \"$ACT\"; exit 0; }}\n\
         if [ $((now - last)) -ge \"$IDLE\" ]; then\n  \
           waydroid session stop >/dev/null 2>&1 || true\n  \
           sudo systemctl stop waydroid-container >/dev/null 2>&1 || true\n\
         fi\n"
    )
}

fn idle_service_unit() -> &'static str {
    "[Unit]\n\
     Description=Stop idle Waydroid (ManifestOS)\n\
     [Service]\n\
     Type=oneshot\n\
     ExecStart=/usr/local/bin/waydroid-idle\n"
}

fn idle_timer_unit() -> &'static str {
    "[Unit]\n\
     Description=Check Waydroid idle (ManifestOS)\n\
     [Timer]\n\
     OnBootSec=5min\n\
     OnUnitActiveSec=5min\n\
     [Install]\n\
     WantedBy=timers.target\n"
}

/// Write the first-login hook script + its autostart entry.
fn write_firstrun(a: &Android, ctx: &Ctx) -> Result<()> {
    println!("  · installing the first-login setup hook");
    ctx.write_root(FIRSTRUN, &firstrun_script(a))?;
    ctx.sudo("chmod", &["0755", FIRSTRUN])?;
    ctx.write_root(AUTOSTART, autostart_entry())
}

/// First-graphical-session setup (guarded once per user): bring Android up, set
/// multi-window, install an in-Android F-Droid store + the declared apps, route
/// every launcher through the lazy wrapper (so fuzzel entries stay lazy), write a
/// launcher for each exposed app, enable the per-user idle timer, then **stop**
/// Android again so it returns to the lazy state. Pure — unit-tested.
fn firstrun_script(a: &Android) -> String {
    let apps = a.apps.iter().map(|s| shq(s)).collect::<Vec<_>>().join(" ");
    let expose = a.expose.iter().map(|s| shq(s)).collect::<Vec<_>>().join(" ");
    let multi = if a.mode.as_deref() == Some("fullscreen") { "false" } else { "true" };
    format!(
        "#!/bin/sh\n\
         # ManifestOS — first-session Waydroid setup (generated; do not edit).\n\
         command -v waydroid >/dev/null || exit 0\n\
         MARK=\"${{XDG_DATA_HOME:-$HOME/.local/share}}/manifest-waydroid-firstrun.done\"\n\
         [ -e \"$MARK\" ] && exit 0\n\
         {ensure}\
         waydroid prop set persist.waydroid.multi_windows {multi} 2>/dev/null || true\n\
         # An in-Android app store — the GUI installer on the Android side.\n\
         waydroid app list 2>/dev/null | grep -q org.fdroid.fdroid || {{\n  \
           curl -fsSL -o /tmp/fdroid.apk https://f-droid.org/F-Droid.apk && \
             waydroid app install /tmp/fdroid.apk || true\n\
         }}\n\
         # Declared apps (via the host cmd installer).\n\
         for app in {apps}; do android-install \"$app\" || true; done\n\
         # Route Waydroid's own launchers through the lazy wrapper.\n\
         {relazy}\
         # Ensure a launcher exists for each exposed app (fuzzel/rofi).\n\
         APPDIR=\"${{XDG_DATA_HOME:-$HOME/.local/share}}/applications\"; mkdir -p \"$APPDIR\"\n\
         for p in {expose}; do\n  \
           d=\"$APPDIR/waydroid.$p.desktop\"\n  \
           [ -e \"$d\" ] || printf '[Desktop Entry]\\nType=Application\\nName=%s\\nExec=/usr/local/bin/waydroid-launch %s\\nIcon=waydroid\\nCategories=Android;\\nX-ManifestOS-Strata=android\\n' \"$p\" \"$p\" > \"$d\"\n\
         done\n\
         # Turn on the per-user idle watchdog.\n\
         systemctl --user enable --now waydroid-idle.timer >/dev/null 2>&1 || true\n\
         # Return to the lazy state — don't leave Android running after setup.\n\
         waydroid session stop >/dev/null 2>&1 || true\n\
         sudo systemctl stop waydroid-container >/dev/null 2>&1 || true\n\
         mkdir -p \"$(dirname \"$MARK\")\"; : > \"$MARK\"\n",
        ensure = ensure_up(),
        relazy = relaunch_rewrite(),
    )
}

fn autostart_entry() -> &'static str {
    "[Desktop Entry]\n\
     Type=Application\n\
     Name=ManifestOS Android setup\n\
     Comment=First-run Waydroid setup (self-guards; runs once)\n\
     Exec=/usr/local/bin/waydroid-firstrun\n\
     OnlyShowIn=GNOME;KDE;XFCE;LXQt;MATE;Cinnamon;Hyprland;sway;niri;Wayland;\n\
     NoDisplay=true\n\
     X-GNOME-Autostart-enabled=true\n"
}

/// Minimal single-quote shell escaping. Pure — unit-tested.
fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn android(system: Option<&str>, mode: Option<&str>, apps: &[&str], expose: &[&str], idle: Option<u32>) -> Android {
        Android {
            system: system.map(String::from),
            mode: mode.map(String::from),
            apps: apps.iter().map(|s| s.to_string()).collect(),
            expose: expose.iter().map(|s| s.to_string()).collect(),
            idle_minutes: idle,
        }
    }

    #[test]
    fn init_cmd_pins_system_and_quotes() {
        assert_eq!(waydroid_init_cmd(&android(None, None, &[], &[], None)), "waydroid init");
        assert_eq!(
            waydroid_init_cmd(&android(Some("GAPPS"), None, &[], &[], None)),
            "waydroid init -s 'GAPPS'"
        );
    }

    #[test]
    fn launcher_is_lazy_and_stamps() {
        let s = launcher_script();
        assert!(s.contains("sudo systemctl start waydroid-container"), "lazy start: {s}");
        assert!(s.contains("waydroid session start"), "{s}");
        assert!(s.contains("waydroid app launch \"$1\""), "{s}");
        assert!(s.contains("waydroid-activity"), "activity stamp: {s}");
    }

    #[test]
    fn installer_handles_apk_and_fdroid_and_relazies() {
        let s = installer_script();
        assert!(s.starts_with("#!/bin/sh"), "{s}");
        assert!(s.contains("*.apk)"), "{s}");
        assert!(s.contains("f-droid.org/api/v1/packages/"), "{s}");
        assert!(s.contains("/usr/local/bin/waydroid-launch"), "relazy rewrite: {s}");
    }

    #[test]
    fn idle_script_stops_after_timeout_and_checks_windows() {
        let s = idle_script(45);
        assert!(s.contains("IDLE=2700"), "45 min = 2700s: {s}");
        assert!(s.contains("hyprctl clients") && s.contains("swaymsg") && s.contains("niri msg windows"), "per-compositor window check: {s}");
        assert!(s.contains("waydroid session stop"), "{s}");
        assert!(s.contains("sudo systemctl stop waydroid-container"), "{s}");
    }

    #[test]
    fn idle_zero_disables_autostop() {
        let s = idle_script(0);
        assert!(s.contains("IDLE=0"), "{s}");
        assert!(s.contains("[ \"$IDLE\" -eq 0 ] && exit 0"), "disable guard: {s}");
    }

    #[test]
    fn firstrun_installs_store_apps_and_returns_to_lazy() {
        let s = firstrun_script(&android(None, None, &["org.telegram.messenger"], &["org.telegram.messenger"], None));
        assert!(s.contains("manifest-waydroid-firstrun.done"), "guarded: {s}");
        assert!(s.contains("F-Droid.apk"), "in-android store: {s}");
        assert!(s.contains("android-install \"$app\""), "declared apps: {s}");
        assert!(s.contains("Exec=/usr/local/bin/waydroid-launch"), "lazy launchers: {s}");
        assert!(s.contains("systemctl --user enable --now waydroid-idle.timer"), "idle timer on: {s}");
        // Returns to lazy state after setup.
        assert!(s.contains("waydroid session stop"), "stop after setup: {s}");
        assert!(s.contains("sudo systemctl stop waydroid-container"), "{s}");
    }

    #[test]
    fn sudoers_is_scoped_to_container_startstop() {
        let s = sudoers_content();
        assert!(s.contains("systemctl start waydroid-container"), "{s}");
        assert!(s.contains("systemctl stop waydroid-container"), "{s}");
        // Not a blanket rule.
        assert!(!s.contains("NOPASSWD: ALL"), "{s}");
    }

    #[test]
    fn shq_escapes_single_quotes() {
        assert_eq!(shq("abc"), "'abc'");
        assert_eq!(shq("a'b"), "'a'\\''b'");
    }
}
