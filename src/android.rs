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
use anyhow::Result;

const INSTALLER: &str = "/usr/local/bin/android-install";
const LAUNCHER: &str = "/usr/local/bin/waydroid-launch";
const IDLE: &str = "/usr/local/bin/waydroid-idle";
const FIRSTRUN: &str = "/usr/local/bin/waydroid-firstrun";
const AUTOSTART: &str = "/etc/xdg/autostart/manifest-waydroid-firstrun.desktop";
const SUDOERS: &str = "/etc/sudoers.d/manifest-waydroid";
const IDLE_SERVICE: &str = "/etc/systemd/user/waydroid-idle.service";
const IDLE_TIMER: &str = "/etc/systemd/user/waydroid-idle.timer";
const APPLICATIONS_DIR: &str = "/usr/share/applications";
const MIME_XML: &str = "/usr/share/mime/packages/manifest-android-bundles.xml";
const MIME_HANDLER: &str = "/usr/share/applications/manifest-apkm-install.desktop";
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
    write_mime(ctx)?;
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
        ctx.sudo("mv", &["-f", &staged, SUDOERS])
    } else {
        // Non-fatal: passwordless container start is only a convenience. If this
        // system's `visudo` rejects the drop-in, warn and carry on — Android
        // still works, the lazy launcher just prompts for a password the first
        // time it brings the container up. Don't leave the bad staged file behind.
        let _ = ctx.sudo("rm", &["-f", &staged]);
        eprintln!(
            "  · warning: the sudoers drop-in failed `visudo -c` — skipping it. \
             Android still works; you'll be asked for your password when it starts \
             the container. (Report this if it persists.)"
        );
        Ok(())
    }
}

/// ASCII-only, minimal (`.service` form only — the scripts call that), one Cmnd
/// line. Kept deliberately plain so any `visudo`/locale accepts it.
fn sudoers_content() -> &'static str {
    "# ManifestOS Waydroid - passwordless container start/stop (generated).\n\
     # Scoped to one service; Android app management runs unprivileged as you.\n\
     ALL ALL=(root) NOPASSWD: /usr/bin/systemctl start waydroid-container.service, /usr/bin/systemctl stop waydroid-container.service\n"
}

/// Shell snippet that brings Android up on demand: start the container
/// (passwordless), then the session, waiting for it to come up.
fn ensure_up() -> &'static str {
    "systemctl is-active --quiet waydroid-container.service 2>/dev/null || \
       sudo systemctl start waydroid-container.service\n\
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

/// `android-install <file | fdroid-id> …` — brings Android up if needed and
/// installs. Handles a plain **`.apk`**, a **split bundle** (`.apkm`/`.apks`/
/// `.xapk` — a ZIP of base + split APKs, e.g. from APKMirror), or a bare
/// **F-Droid id** (resolved via F-Droid's API). Bundles are unpacked, the right
/// splits selected (base + best ABI + best density + all languages + feature
/// modules), and installed as one split-install session via `pm` in the
/// container; on failure it falls back to installing the base APK alone. The
/// cmd installer on the Android side. Pure — structure unit-tested.
fn installer_script() -> String {
    let body = r####"#!/bin/sh
# ManifestOS — install an Android app/bundle into Waydroid (generated; do not edit).
# Usage: android-install <file.apk | file.apkm | file.apks | file.xapk | fdroid.package.id> …
set -e
[ $# -ge 1 ] || { echo 'usage: android-install <file.apk|.apkm|.apks|.xapk | fdroid.package.id> …' >&2; exit 2; }
command -v waydroid >/dev/null || { echo 'android-install: waydroid is not installed' >&2; exit 1; }
@ENSURE@
if waydroid status 2>/dev/null | grep -qi 'session.*running'; then
  echo "android-install: Waydroid session is up."
else
  echo "android-install: WARNING - Waydroid session is not running; the install will likely fail." >&2
  echo "  Start it from your desktop session first:  waydroid session start" >&2
fi
install_fdroid() {
  app="$1"
  vc=$(curl -fsSL "https://f-droid.org/api/v1/packages/$app" | sed -n 's/.*"suggestedVersionCode"[: ]*\([0-9]*\).*/\1/p' | head -n1)
  [ -n "$vc" ] || { echo "android-install: '$app' not found on F-Droid" >&2; return 1; }
  tmp=$(mktemp --suffix=.apk)
  curl -fsSL -o "$tmp" "https://f-droid.org/repo/${app}_${vc}.apk"
  waydroid app install "$tmp"; rm -f "$tmp"
}
# Install a split bundle (.apkm/.apks/.xapk = ZIP of base + split APKs). Loud on
# purpose — every step prints, so a failure is visible, not silent.
install_bundle() {
  bundle="$1"; dir=$(mktemp -d)
  echo "android-install: unpacking $(basename "$bundle")"
  bsdtar -xf "$bundle" -C "$dir" 2>/dev/null || unzip -qo "$bundle" -d "$dir" || {
    echo "  ! cannot read $bundle" >&2; rm -rf "$dir"; return 1; }
  apks=$(find "$dir" -name '*.apk')
  [ -n "$apks" ] || { echo "  ! no APKs inside $bundle" >&2; rm -rf "$dir"; return 1; }
  base=$(printf '%s\n' $apks | grep -iE '(^|/)base\.apk$' | head -n1)
  [ -n "$base" ] || base=$(printf '%s\n' $apks | grep -v 'split_' | head -n1)
  abi=
  for a in x86_64 x86 arm64_v8a arm64 armeabi_v7a armeabi; do
    m=$(printf '%s\n' $apks | grep -i "split_config\.$a\.apk" | head -n1); [ -n "$m" ] && { abi="$m"; break; }
  done
  dpi=
  for d in xxxhdpi xxhdpi xhdpi hdpi tvdpi mdpi nodpi; do
    m=$(printf '%s\n' $apks | grep -i "split_config\.$d\.apk" | head -n1); [ -n "$m" ] && { dpi="$m"; break; }
  done
  langs=$(printf '%s\n' $apks | grep -iE 'split_config\.[a-z][a-z]\.apk' || true)
  feats=$(printf '%s\n' $apks | grep -i 'split_' | grep -iv 'split_config\.' || true)
  sel=
  for f in "$base" "$abi" "$dpi" $langs $feats; do [ -n "$f" ] && sel="$sel $f"; done
  echo "  selected splits:$(for f in $sel; do printf ' %s' "$(basename "$f")"; done)"
  # Stage where the container can read them (Android /data = /var/lib/waydroid/data),
  # so pm reads real files instead of a stdin stream that waydroid shell may not forward.
  ctmp=/var/lib/waydroid/data/local/tmp
  sudo mkdir -p "$ctmp" 2>/dev/null || echo "  ! could not create $ctmp (is Waydroid initialised?)" >&2
  for f in $sel; do sudo cp "$f" "$ctmp/$(basename "$f")" 2>/dev/null && sudo chmod 644 "$ctmp/$(basename "$f")" 2>/dev/null; done
  total=0; for f in $sel; do total=$((total + $(stat -c%s "$f"))); done
  out=$(sudo waydroid shell -- pm install-create -S "$total" 2>&1)
  sid=$(printf '%s' "$out" | sed -n 's/.*\[\([0-9]*\)\].*/\1/p' | head -n1)
  ok=1
  [ -n "$sid" ] || { echo "  ! pm install-create failed: $out" >&2; ok=0; }
  i=0
  [ "$ok" = 1 ] && for f in $sel; do
    i=$((i+1)); bn=$(basename "$f")
    if ! r=$(sudo waydroid shell -- pm install-write -S "$(stat -c%s "$f")" "$sid" "split$i" "/data/local/tmp/$bn" 2>&1); then
      echo "  ! install-write $bn: $r" >&2; ok=0; fi
  done
  if [ "$ok" = 1 ]; then
    r=$(sudo waydroid shell -- pm install-commit "$sid" 2>&1)
    if printf '%s' "$r" | grep -qi success; then
      echo "  installed $(basename "$bundle") - it can take a few seconds to appear in the launcher"
    else echo "  ! install-commit: $r" >&2; ok=0; fi
  fi
  [ -n "$sid" ] && [ "$ok" != 1 ] && sudo waydroid shell -- pm install-abandon "$sid" >/dev/null 2>&1
  for f in $sel; do sudo rm -f "$ctmp/$(basename "$f")" 2>/dev/null; done
  if [ "$ok" != 1 ]; then
    echo "  split install failed; trying the base APK alone via waydroid..." >&2
    if waydroid app install "$base"; then echo "  installed base APK only (some resources may be missing)"
    else echo "  ! base install also failed - check that the session is up: waydroid status" >&2; fi
  fi
  rm -rf "$dir"
}
for app in "$@"; do
  case "$app" in
    *.apk)                 waydroid app install "$app" ;;
    *.apkm|*.apks|*.xapk)  install_bundle "$app" ;;
    *.deb|*.rpm)           # picked the wrong handler? hand off to strata-install.
      if command -v strata-install >/dev/null 2>&1; then strata-install "$app";
      else echo "android-install: '$app' is a Linux package — add a stratum: manifest strata add debian" >&2; fi ;;
    *)                     install_fdroid "$app" ;;
  esac
done
@RELAZY@
@STAMP@
"####;
    body.replace("@ENSURE@\n", ensure_up())
        .replace("@RELAZY@\n", relaunch_rewrite())
        .replace("@STAMP@\n", stamp_activity())
}

/// Register `.apkm`/`.apks`/`.xapk` as file types + a handler so opening one in a
/// file manager (or `xdg-open`) installs it into Waydroid via `android-install`.
fn write_mime(ctx: &Ctx) -> Result<()> {
    println!("  · registering .apkm/.apks/.xapk → open with Waydroid");
    ctx.write_root(MIME_XML, mime_xml())?;
    ctx.shell("update-mime-database /usr/share/mime 2>/dev/null || true", true)?;
    ctx.write_root(MIME_HANDLER, mime_handler_desktop())?;
    ctx.shell(
        &format!("update-desktop-database {APPLICATIONS_DIR} 2>/dev/null || true"),
        true,
    )
}

fn mime_xml() -> &'static str {
    // `.apkm`/`.apks`/`.xapk` are ZIP archives, so magic-sniffing would call them
    // application/zip. Declaring each as a sub-class-of application/zip (like .jar
    // /.apk/.docx) makes the more-specific glob match win, so a file manager sees
    // e.g. application/vnd.apkm and offers the Android installer. High glob weight
    // for good measure.
    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
     <mime-info xmlns=\"http://www.freedesktop.org/standards/shared-mime-info\">\n  \
       <mime-type type=\"application/vnd.apkm\"><comment>Android APKM bundle</comment><sub-class-of type=\"application/zip\"/><glob pattern=\"*.apkm\" weight=\"80\"/></mime-type>\n  \
       <mime-type type=\"application/vnd.apks\"><comment>Android split APKs</comment><sub-class-of type=\"application/zip\"/><glob pattern=\"*.apks\" weight=\"80\"/></mime-type>\n  \
       <mime-type type=\"application/x-xapk\"><comment>Android XAPK bundle</comment><sub-class-of type=\"application/zip\"/><glob pattern=\"*.xapk\" weight=\"80\"/></mime-type>\n\
     </mime-info>\n"
}

fn mime_handler_desktop() -> &'static str {
    "[Desktop Entry]\n\
     Type=Application\n\
     Name=Install to Android (Waydroid)\n\
     Comment=Install an APK / APKM / APKS / XAPK into Waydroid\n\
     Exec=android-install %f\n\
     Icon=waydroid\n\
     Terminal=true\n\
     Categories=System;\n\
     MimeType=application/vnd.apkm;application/vnd.apks;application/x-xapk;application/vnd.android.package-archive;\n\
     NoDisplay=false\n"
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
           sudo systemctl stop waydroid-container.service >/dev/null 2>&1 || true\n\
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
         sudo systemctl stop waydroid-container.service >/dev/null 2>&1 || true\n\
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
    fn installer_handles_apk_fdroid_and_split_bundles() {
        let s = installer_script();
        assert!(s.starts_with("#!/bin/sh"), "{s}");
        assert!(s.contains("*.apk)"), "plain apk: {s}");
        assert!(s.contains("*.apkm|*.apks|*.xapk)"), "bundle dispatch: {s}");
        assert!(s.contains("f-droid.org/api/v1/packages/"), "fdroid id: {s}");
        // Split-bundle handling: unpack, ABI/density/language selection, pm session.
        assert!(s.contains("install_bundle"), "{s}");
        assert!(s.contains("split_config") && s.contains("x86_64"), "abi selection: {s}");
        assert!(s.contains("pm install-create") && s.contains("pm install-commit"), "split session: {s}");
        assert!(s.contains("waydroid app install \"$base\""), "base-only fallback: {s}");
        // Under `set -e`, grep-that-finds-nothing (apps with no lang/feature
        // splits) must not kill the script — guarded with `|| true`.
        assert!(s.contains("split_config\\.[a-z][a-z]\\.apk' || true"), "langs grep guarded: {s}");
        assert!(s.contains("grep -iv 'split_config\\.' || true"), "feats grep guarded: {s}");
        // Placeholders got substituted (no @…@ left) and relazy/stamp are present.
        assert!(!s.contains("@ENSURE@") && !s.contains("@RELAZY@") && !s.contains("@STAMP@"), "placeholders unsubstituted: {s}");
        assert!(s.contains("/usr/local/bin/waydroid-launch"), "relazy rewrite: {s}");
        assert!(s.contains("waydroid-activity"), "activity stamp: {s}");
    }

    #[test]
    fn mime_registers_bundle_types_and_handler() {
        let xml = mime_xml();
        assert!(xml.contains("*.apkm") && xml.contains("*.apks") && xml.contains("*.xapk"), "globs: {xml}");
        // ZIP-based, so declared sub-class-of application/zip or magic wins.
        assert_eq!(xml.matches("<sub-class-of type=\"application/zip\"/>").count(), 3, "zip subclass on all three: {xml}");
        let d = mime_handler_desktop();
        assert!(d.contains("Exec=android-install %f"), "{d}");
        assert!(d.contains("application/vnd.apkm"), "mimetype assoc: {d}");
    }

    #[test]
    fn installer_delegates_deb_rpm_to_strata_install() {
        let s = installer_script();
        assert!(s.contains("*.deb|*.rpm)"), "deb/rpm handled: {s}");
        assert!(s.contains("strata-install \"$app\""), "delegates to strata-install: {s}");
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
    fn sudoers_is_scoped_ascii_and_service_form() {
        let s = sudoers_content();
        assert!(s.contains("systemctl start waydroid-container.service"), "{s}");
        assert!(s.contains("systemctl stop waydroid-container.service"), "{s}");
        // Not a blanket rule.
        assert!(!s.contains("NOPASSWD: ALL"), "{s}");
        // ASCII-only — some visudo/locale setups reject non-ASCII even in comments.
        assert!(s.is_ascii(), "sudoers must be ASCII: {s}");
    }

    #[test]
    fn shq_escapes_single_quotes() {
        assert_eq!(shq("abc"), "'abc'");
        assert_eq!(shq("a'b"), "'a'\\''b'");
    }
}
