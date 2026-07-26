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
const ARM_SETUP: &str = "/usr/local/bin/waydroid-arm-setup";
const IDLE: &str = "/usr/local/bin/waydroid-idle";
const FIRSTRUN: &str = "/usr/local/bin/waydroid-firstrun";
const AUTOSTART: &str = "/etc/xdg/autostart/manifest-waydroid-firstrun.desktop";
const SUDOERS: &str = "/etc/sudoers.d/manifest-waydroid";
const IDLE_SERVICE: &str = "/etc/systemd/user/waydroid-idle.service";
const IDLE_TIMER: &str = "/etc/systemd/user/waydroid-idle.timer";
const APPLICATIONS_DIR: &str = "/usr/share/applications";
const MIME_XML: &str = "/usr/share/mime/packages/manifest-android-bundles.xml";
const MIME_HANDLER: &str = "/usr/share/applications/manifest-apkm-install.desktop";
/// GUI-friendly installer wrapper (opens a terminal if there is one, else runs
/// headless with desktop notifications) — what the file-manager handlers Exec.
const GUI_INSTALL: &str = "/usr/local/bin/manifest-install-gui";
/// System-wide XDG default-application map (merged, never clobbered).
const MIMEAPPS: &str = "/etc/xdg/mimeapps.list";
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
    write_arm_setup(ctx)?;
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
     }\n\
     # `session running` only means the container is up — Android's framework\n\
     # services (package manager) register later, and `pm` fails with\n\
     # \"Can't find service\" until then. Wait for the boot to actually complete.\n\
     wait_android_ready() {\n  \
       k=0\n  \
       while [ $k -lt 120 ]; do\n    \
         bc=$(sudo waydroid shell -- getprop sys.boot_completed 2>/dev/null | tr -d '\\r\\n')\n    \
         if [ \"$bc\" = 1 ] && sudo waydroid shell -- cmd package list packages >/dev/null 2>&1; then return 0; fi\n    \
         [ $k = 0 ] && echo \"  waiting for Android to finish booting...\"\n    \
         k=$((k+1)); sleep 2\n  \
       done\n  \
       echo \"  ! Android did not finish booting in time (pm may fail)\" >&2; return 1\n\
     }\n\
     wait_android_ready || true\n"
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

/// A thin stub installed on the host that execs the *current* binary's script
/// logic (`manifest __script <name>`), so behaviour updates with `pacman -Syu` —
/// no need to re-run the generator. `$0` is set to `manifest` and `"$@"` is
/// forwarded to the fetched script.
pub fn thin_stub(name: &str) -> String {
    format!(
        "#!/bin/sh\n\
         # ManifestOS thin stub — the real logic lives in the `manifest` binary and\n\
         # updates with `pacman -Syu` (no regeneration needed). Do not edit.\n\
         exec sh -c \"$(manifest __script {name})\" manifest \"$@\"\n"
    )
}

fn write_installer(ctx: &Ctx) -> Result<()> {
    println!("  · installing the `android-install` command (thin stub → binary)");
    ctx.write_root(INSTALLER, &thin_stub("android-install"))?;
    ctx.sudo("chmod", &["0755", INSTALLER])
}

fn write_arm_setup(ctx: &Ctx) -> Result<()> {
    println!("  · installing the `waydroid-arm-setup` command (thin stub → binary)");
    ctx.write_root(ARM_SETUP, &thin_stub("waydroid-arm-setup"))?;
    ctx.sudo("chmod", &["0755", ARM_SETUP])
}

/// Install **libndk** ARM→x86 translation into Waydroid, so ARM-only apps run on
/// an x86 host (idempotent). Uses the standard `waydroid_script` tool. Called
/// automatically by `android-install` when it detects an ARM-only app on x86.
/// Pure — structure unit-tested.
pub fn arm_setup_script() -> &'static str {
    "#!/bin/sh\n\
     # ManifestOS — install ARM translation into Waydroid via waydroid_script,\n\
     # in an isolated venv (Arch python is externally-managed). Generated logic.\n\
     command -v waydroid >/dev/null || { echo 'waydroid-arm-setup: waydroid not installed' >&2; exit 1; }\n\
     if sudo waydroid shell getprop ro.dalvik.vm.native.bridge 2>/dev/null | grep -qiE 'libndk|libhoudini'; then\n  \
       echo 'waydroid-arm-setup: ARM translation already installed'; exit 0\n\
     fi\n\
     echo 'waydroid-arm-setup: installing ARM translation (one-time; downloads translation blobs)'\n\
     sudo pacman -S --needed --noconfirm git python lzip >/dev/null 2>&1 || true\n\
     d=$(mktemp -d)\n\
     if ! git clone --depth 1 https://github.com/casualsnek/waydroid_script \"$d/ws\" >/dev/null 2>&1; then\n  \
       echo '  ! could not clone waydroid_script' >&2; sudo rm -rf \"$d\"; exit 1\n\
     fi\n\
     echo '  · setting up its Python deps in a venv'\n\
     python -m venv \"$d/venv\" >/dev/null 2>&1\n\
     if [ -f \"$d/ws/requirements.txt\" ]; then \"$d/venv/bin/pip\" install -q -r \"$d/ws/requirements.txt\" >/dev/null 2>&1; fi\n\
     \"$d/venv/bin/pip\" install -q InquirerPy requests tqdm >/dev/null 2>&1 || true\n\
     waydroid session stop >/dev/null 2>&1 || true\n\
     PY=\"$d/venv/bin/python\"\n\
     if sudo \"$PY\" \"$d/ws/main.py\" install libndk; then\n  \
       echo 'waydroid-arm-setup: done (libndk) — ARM apps run after the session restarts'; sudo rm -rf \"$d\"; exit 0\n\
     elif sudo \"$PY\" \"$d/ws/main.py\" install libhoudini; then\n  \
       echo 'waydroid-arm-setup: done (libhoudini) — ARM apps run after the session restarts'; sudo rm -rf \"$d\"; exit 0\n\
     else\n  \
       echo '  ! ARM translation install failed (tried libndk and libhoudini)' >&2; sudo rm -rf \"$d\"; exit 1\n\
     fi\n"
}

/// `android-install <file | fdroid-id> …` — brings Android up if needed and
/// installs. Handles a plain **`.apk`**, a **split bundle** (`.apkm`/`.apks`/
/// `.xapk` — a ZIP of base + split APKs, e.g. from APKMirror), or a bare
/// **F-Droid id** (resolved via F-Droid's API). Bundles are unpacked, the right
/// splits selected (base + best ABI + best density + all languages + feature
/// modules), and installed as one split-install session via `pm` in the
/// container; on failure it falls back to installing the base APK alone. The
/// cmd installer on the Android side. Pure — structure unit-tested.
pub fn installer_script() -> String {
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
  # Bundles are ZIPs. Try each available extractor and only complain if they all
  # fail — showing the real errors (they used to be hidden by 2>/dev/null).
  extracted=0
  if command -v bsdtar >/dev/null 2>&1; then
    if bsdtar -xf "$bundle" -C "$dir"; then extracted=1; else echo "  · bsdtar could not read it, trying another extractor" >&2; fi
  fi
  if [ "$extracted" = 0 ] && command -v unzip >/dev/null 2>&1; then
    unzip -qo "$bundle" -d "$dir" && extracted=1
  fi
  if [ "$extracted" = 0 ] && command -v python3 >/dev/null 2>&1; then
    python3 -c 'import sys,zipfile; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])' "$bundle" "$dir" && extracted=1
  fi
  if [ "$extracted" = 0 ]; then
    echo "  · installing unzip and retrying" >&2
    sudo pacman -S --needed --noconfirm unzip >/dev/null 2>&1 && unzip -qo "$bundle" -d "$dir" && extracted=1
  fi
  [ "$extracted" = 1 ] || { echo "  ! cannot read $bundle (no working extractor)" >&2; rm -rf "$dir"; return 1; }
  # mktemp -d is 0700; make it traversable/readable so the base-APK fallback's
  # `waydroid app install <path>` can read the file.
  chmod 755 "$dir" 2>/dev/null; find "$dir" -name '*.apk' -exec chmod 644 {} + 2>/dev/null
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
  # Auto-detect an ARM-only app on an x86 host: the selected ABI split is arm* and
  # there's no x86 one. Set up ARM translation (libndk), then bring the session
  # back (arm-setup stops it to patch the image) so the install can proceed.
  case "$(uname -m)" in
    x86_64|amd64|i?86)
      case "$abi" in
        *arm*)
          echo "  this app is ARM-only and your Waydroid is x86 — setting up ARM translation..."
          # Run the stub if installed, else the logic straight from the binary
          # (so this works right after `pacman -Syu`, before `manifest android`).
          if command -v waydroid-arm-setup >/dev/null 2>&1; then armrun() { waydroid-arm-setup; }
          else armrun() { sh -c "$(manifest __script waydroid-arm-setup)" manifest; }; fi
          armrun; arm_rc=$?
          # arm-setup stops the session to patch the image — bring it back either way
          # so the install (or the base-APK fallback) can proceed. After an image
          # patch Android cold-boots, so wait for the framework, not just the
          # container (otherwise pm fails with "Can't find service").
          waydroid session start >/dev/null 2>&1 &
          j=0; while ! waydroid status 2>/dev/null | grep -qi 'session.*running'; do j=$((j+1)); [ "$j" -gt 60 ] && break; sleep 1; done
          wait_android_ready || true
          [ "$arm_rc" = 0 ] || echo "  ! ARM translation setup failed — the app may not run" >&2 ;;
      esac ;;
  esac
  total=0; for f in $sel; do total=$((total + $(stat -c%s "$f"))); done
  out=$(sudo waydroid shell -- pm install-create -S "$total" 2>&1)
  sid=$(printf '%s' "$out" | sed -n 's/.*\[\([0-9]*\)\].*/\1/p' | head -n1)
  ok=1
  [ -n "$sid" ] || { echo "  ! pm install-create failed: $out" >&2; ok=0; }
  i=0
  [ "$ok" = 1 ] && for f in $sel; do
    i=$((i+1)); bn=$(basename "$f"); sz=$(stat -c%s "$f")
    # Stream the split to pm over stdin (what `adb install-multiple` does): pm
    # writes it into its own session with the right SELinux context, so no host
    # file in /data/local/tmp (which Android's confined installer can't open).
    echo "  writing $bn ($sz bytes)"
    r=$(sudo waydroid shell -- pm install-write -S "$sz" "$sid" "split$i" - < "$f" 2>&1)
    streamed=$(printf '%s' "$r" | sed -n 's/.*streamed \([0-9][0-9]*\) bytes.*/\1/p')
    if [ "${streamed:-0}" != "$sz" ]; then
      echo "  ! install-write $bn: ${r:-no output} (streamed ${streamed:-0}/$sz)" >&2; ok=0
    fi
  done
  if [ "$ok" = 1 ]; then
    r=$(sudo waydroid shell -- pm install-commit "$sid" 2>&1)
    if printf '%s' "$r" | grep -qi success; then
      echo "  installed $(basename "$bundle") - it can take a few seconds to appear in the launcher"
    else echo "  ! install-commit: $r" >&2; ok=0; fi
  fi
  [ -n "$sid" ] && [ "$ok" != 1 ] && sudo waydroid shell -- pm install-abandon "$sid" >/dev/null 2>&1
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
/// file manager (or `xdg-open`) installs it into Waydroid via `android-install`,
/// and make that handler the **default** for those types (merged into the XDG
/// system defaults so double-click Just Works in any compliant file manager).
pub fn refresh_file_handlers(ctx: &Ctx) -> Result<()> { write_mime(ctx) }

fn write_mime(ctx: &Ctx) -> Result<()> {
    println!("  · registering .apk/.apkm/.apks/.xapk → open with Waydroid");
    ctx.write_root(MIME_XML, mime_xml())?;
    ctx.shell("update-mime-database /usr/share/mime 2>/dev/null || true", true)?;
    ctx.write_root(GUI_INSTALL, &thin_stub("manifest-install-gui"))?;
    ctx.sudo("chmod", &["0755", GUI_INSTALL])?;
    ctx.write_root(MIME_HANDLER, mime_handler_desktop())?;
    ctx.shell(
        &format!("update-desktop-database {APPLICATIONS_DIR} 2>/dev/null || true"),
        true,
    )?;
    write_mime_defaults(ctx)
}

/// Make our handlers the default application for the package types we own, by
/// **merging** into `/etc/xdg/mimeapps.list` (other entries are preserved — this
/// file may already carry the user's/desktop's own defaults).
pub fn write_mime_defaults_pub(ctx: &Ctx) -> Result<()> { write_mime_defaults(ctx) }

fn write_mime_defaults(ctx: &Ctx) -> Result<()> {
    println!("  · making them the default for package files (merged into {MIMEAPPS})");
    let existing = std::fs::read_to_string(MIMEAPPS).unwrap_or_default();
    ctx.write_root(MIMEAPPS, &merge_mimeapps(&existing, default_associations()))
}

/// The MIME type → handler `.desktop` mappings we own.
fn default_associations() -> &'static [(&'static str, &'static str)] {
    &[
        ("application/vnd.android.package-archive", "manifest-apkm-install.desktop"),
        ("application/vnd.apkm", "manifest-apkm-install.desktop"),
        ("application/vnd.apks", "manifest-apkm-install.desktop"),
        ("application/x-xapk", "manifest-apkm-install.desktop"),
        ("application/vnd.debian.binary-package", "manifest-strata-install.desktop"),
        ("application/x-deb", "manifest-strata-install.desktop"),
        ("application/x-rpm", "manifest-strata-install.desktop"),
        ("application/x-redhat-package-manager", "manifest-strata-install.desktop"),
    ]
}

/// Merge `entries` into a `mimeapps.list`, into both `[Default Applications]`
/// (what opens on double-click) and `[Added Associations]` (what shows in "Open
/// With"). Existing keys we own are replaced; everything else is preserved
/// verbatim, including unrelated sections. Pure — unit-tested.
fn merge_mimeapps(existing: &str, entries: &[(&str, &str)]) -> String {
    let sections = ["[Default Applications]", "[Added Associations]"];
    // Split the file into (section header, lines) keeping order and unknown parts.
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    let mut current = String::new();
    for line in existing.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            current = t.to_string();
            if !out.iter().any(|(h, _)| h == &current) {
                out.push((current.clone(), Vec::new()));
            }
        } else if !current.is_empty() {
            if let Some(sec) = out.iter_mut().find(|(h, _)| h == &current) {
                sec.1.push(line.to_string());
            }
        } else if !t.is_empty() {
            // Content before any section header (rare) — keep it at the top.
            out.insert(0, (String::new(), vec![line.to_string()]));
        }
    }
    for header in sections {
        if !out.iter().any(|(h, _)| h == header) {
            out.push((header.to_string(), Vec::new()));
        }
        let sec = out.iter_mut().find(|(h, _)| h == header).expect("just ensured");
        for (mime, desktop) in entries {
            // Drop any existing mapping for this type, then add ours.
            sec.1.retain(|l| {
                let key = l.split('=').next().unwrap_or("").trim();
                key != *mime
            });
            sec.1.push(format!("{mime}={desktop}"));
        }
        // Tidy: no blank lines inside the sections we manage.
        sec.1.retain(|l| !l.trim().is_empty());
    }
    let mut s = String::from(
        "# Managed in part by ManifestOS: the package-file entries below are\n\
         # rewritten on install. Other entries are preserved.\n",
    );
    for (header, lines) in out {
        if !header.is_empty() {
            s.push_str(&header);
            s.push('\n');
        }
        for l in lines {
            s.push_str(&l);
            s.push('\n');
        }
        s.push('\n');
    }
    s
}

/// The GUI-launched installer: file managers can't show a long-running install,
/// and `Terminal=true` silently fails where no terminal is configured. So this
/// opens the user's terminal when one exists (keeping it open so the log is
/// readable) and otherwise runs headless, reporting via desktop notifications.
/// Dispatches by extension to `android-install` or `strata-install`.
pub fn gui_install_script() -> &'static str {
    // Env-passing (MOS_*) keeps the terminal payload single-quoted, so no nested
    // quoting games with user file names.
    r####"#!/bin/sh
# ManifestOS — install a package file opened from a file manager (generated logic).
[ $# -ge 1 ] || { echo 'usage: manifest-install-gui <file>' >&2; exit 2; }
MOS_FILE=$1
case "$MOS_FILE" in
  *.apk|*.apkm|*.apks|*.xapk)
    MOS_TOOL=android-install; MOS_WHAT="Android app support (Waydroid)"; MOS_SETUP="manifest android" ;;
  *.deb|*.rpm)
    # strata-install offers to add the matching stratum itself.
    MOS_TOOL=strata-install;  MOS_WHAT="Linux package support"; MOS_SETUP="" ;;
  *) echo "manifest-install-gui: don't know how to install '$MOS_FILE'" >&2; exit 1 ;;
esac
export MOS_FILE MOS_TOOL MOS_WHAT MOS_SETUP
# Runs inside the terminal: offer to set support up if it isn't there, then install.
inner='
if ! command -v "$MOS_TOOL" >/dev/null 2>&1; then
  echo "$MOS_WHAT is not set up on this system yet."
  if [ -n "$MOS_SETUP" ]; then
    printf "Set it up now? (downloads and configures it - a few minutes) [y/N] "
    read r
    case "$r" in
      [yY]|[yY][eE][sS])
        if ! $MOS_SETUP; then
          echo; echo "Setup failed - see the messages above."
          printf "Press Enter to close "; read _; exit 1
        fi ;;
      *) echo "Cancelled."; printf "Press Enter to close "; read _; exit 1 ;;
    esac
  else
    echo "Install it with: manifest strata add debian"
    printf "Press Enter to close "; read _; exit 1
  fi
fi
"$MOS_TOOL" "$MOS_FILE"
echo; echo "--- done - press Enter to close ---"; read _
'
for t in "$TERMINAL" kitty foot alacritty wezterm konsole gnome-terminal xfce4-terminal \
         mate-terminal tilix ghostty x-terminal-emulator xterm; do
  [ -n "$t" ] || continue
  command -v "$t" >/dev/null 2>&1 || continue
  case "$t" in
    gnome-terminal) exec "$t" -- sh -c "$inner" ;;
    *)              exec "$t" -e sh -c "$inner" ;;
  esac
done
# No terminal at all: can't ask, so report what to run and bail.
if ! command -v "$MOS_TOOL" >/dev/null 2>&1; then
  msg="$MOS_WHAT is not set up. Run: ${MOS_SETUP:-manifest strata add debian}"
  command -v notify-send >/dev/null 2>&1 && notify-send -u critical -a ManifestOS 'Not set up' "$msg"
  echo "$msg" >&2; exit 1
fi
command -v notify-send >/dev/null 2>&1 && notify-send -a ManifestOS 'Installing' "Installing $(basename "$MOS_FILE")..."
if out=$("$MOS_TOOL" "$MOS_FILE" 2>&1); then
  command -v notify-send >/dev/null 2>&1 && notify-send -a ManifestOS 'Installed' "$(basename "$MOS_FILE")"
  printf '%s\n' "$out"
else
  command -v notify-send >/dev/null 2>&1 && notify-send -u critical -a ManifestOS 'Install failed' "$(printf '%s' "$out" | tail -n2)"
  printf '%s\n' "$out" >&2; exit 1
fi
"####
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
    // Terminal=false + the GUI wrapper: `Terminal=true` silently fails in file
    // managers with no configured terminal emulator. The wrapper opens one when
    // it can and otherwise notifies, so a double-click always does something.
    "[Desktop Entry]\n\
     Type=Application\n\
     Name=Install to Android (Waydroid)\n\
     Comment=Install an APK / APKM / APKS / XAPK into Waydroid\n\
     Exec=/usr/local/bin/manifest-install-gui %f\n\
     TryExec=/usr/local/bin/manifest-install-gui\n\
     Icon=waydroid\n\
     Terminal=false\n\
     Categories=System;\n\
     MimeType=application/vnd.apkm;application/vnd.apks;application/x-xapk;application/vnd.android.package-archive;\n\
     NoDisplay=false\n"
}

fn write_launcher(ctx: &Ctx) -> Result<()> {
    println!("  · installing the `waydroid-launch` lazy launcher (thin stub → binary)");
    ctx.write_root(LAUNCHER, &thin_stub("waydroid-launch"))?;
    ctx.sudo("chmod", &["0755", LAUNCHER])
}

/// `waydroid-launch <pkg>` — the Exec every Android app launcher points at.
/// Brings Android up on demand, launches, and stamps activity. Pure — tested.
pub fn launcher_script() -> String {
    // Launched from a menu (fuzzel/rofi) there is NO terminal, so nothing here may
    // block on a sudo password prompt — that's why menu launches failed while the
    // same command worked from a shell. Every privileged call uses `sudo -n`
    // (non-interactive, fails instead of prompting), readiness is probed with
    // unprivileged commands, and problems are surfaced via notify-send.
    format!(
        "#!/bin/sh\n\
         # ManifestOS — lazy-launch a Waydroid app (generated; do not edit).\n\
         [ $# -ge 1 ] || {{ echo 'usage: waydroid-launch <package>' >&2; exit 2; }}\n\
         pkg=$1\n\
         say() {{ echo \"waydroid-launch: $1\" >&2; command -v notify-send >/dev/null 2>&1 && notify-send -a Android 'Android' \"$1\"; }}\n\
         # 1. Container up (passwordless rule; never prompt from a menu launch).\n\
         if ! systemctl is-active --quiet waydroid-container.service 2>/dev/null; then\n  \
           if ! sudo -n systemctl start waydroid-container.service 2>/dev/null; then\n    \
             say 'cannot start Android (no passwordless rule). Run: sudo systemctl start waydroid-container.service'\n    \
             exit 1\n  \
           fi\n\
         fi\n\
         # 2. Session up (runs as the user — no sudo needed).\n\
         if ! waydroid status 2>/dev/null | grep -qi 'session.*running'; then\n  \
           waydroid session start >/dev/null 2>&1 &\n  \
           i=0; while ! waydroid status 2>/dev/null | grep -qi 'session.*running'; do\n    \
             i=$((i+1)); [ \"$i\" -gt 60 ] && break; sleep 1\n  \
           done\n\
         fi\n\
         # 3. Launch, retrying while Android's framework finishes registering.\n\
         # (Probing readiness needs root, so instead just retry the launch itself —\n\
         # it fails harmlessly until the package service is up.)\n\
         n=0\n\
         while [ $n -lt 45 ]; do\n  \
           if waydroid app launch \"$pkg\" >/dev/null 2>&1; then\n    \
             {stamp_indented}    exit 0\n  \
           fi\n  \
           n=$((n+1)); sleep 2\n\
         done\n\
         say \"could not launch $pkg (Android may still be starting - try again in a moment)\"\n\
         exit 1\n",
        stamp_indented = stamp_activity(),
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
           sudo -n systemctl stop waydroid-container.service >/dev/null 2>&1 || true\n\
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
         sudo -n systemctl stop waydroid-container.service >/dev/null 2>&1 || true\n\
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
        assert!(s.contains("sudo -n systemctl start waydroid-container.service"), "lazy start: {s}");
        assert!(s.contains("waydroid session start"), "{s}");
        assert!(s.contains("waydroid app launch \"$pkg\""), "{s}");
        assert!(s.contains("waydroid-activity"), "activity stamp: {s}");
    }

    #[test]
    fn launcher_never_blocks_on_a_password_from_a_menu() {
        let s = launcher_script();
        // Menu launches have no TTY: every privileged *call* must be `sudo -n`
        // (fails instead of prompting). Skip the advice string we print to tell
        // the user what to run themselves — that's text, not an invocation.
        for line in s
            .lines()
            .map(str::trim_start)
            .filter(|l| l.contains("sudo "))
            .filter(|l| !l.starts_with('#') && !l.starts_with("say "))
        {
            assert!(line.contains("sudo -n "), "interactive sudo in launcher: {line}");
        }
        // Readiness is probed by retrying the launch, not by a root-only command.
        assert!(!s.contains("sudo -n waydroid shell"), "no root shell probe: {s}");
        assert!(s.contains("notify-send"), "surfaces failures to the desktop: {s}");
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
        assert!(d.contains("Exec=/usr/local/bin/manifest-install-gui %f"), "{d}");
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
        assert!(s.contains("sudo -n systemctl stop waydroid-container.service"), "{s}");
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
        assert!(s.contains("sudo -n systemctl stop waydroid-container.service"), "{s}");
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
    fn arm_setup_installs_libndk_idempotently() {
        let s = arm_setup_script();
        assert!(s.contains("waydroid_script"), "uses waydroid_script: {s}");
        assert!(s.contains("install libndk"), "installs libndk: {s}");
        // Idempotent — skips if the native bridge is already set.
        assert!(s.contains("ro.dalvik.vm.native.bridge"), "idempotency check: {s}");
    }

    #[test]
    fn installer_auto_sets_up_arm_translation_on_x86() {
        let s = installer_script();
        assert!(s.contains("uname -m"), "checks host arch: {s}");
        assert!(s.contains("*arm*)"), "detects arm-only abi split: {s}");
        assert!(s.contains("waydroid-arm-setup"), "invokes arm setup: {s}");
    }

    #[test]
    fn gui_wrapper_offers_setup_when_support_is_missing() {
        let s = gui_install_script();
        // Opening an .apk on a box with no Waydroid must OFFER to set it up
        // (the "add if used" flow), not just fail.
        assert!(s.contains("MOS_SETUP=\"manifest android\""), "android setup cmd: {s}");
        assert!(s.contains("is not set up on this system yet."), "{s}");
        assert!(s.contains("Set it up now?"), "interactive offer: {s}");
        // Headless (no terminal to ask in) still says exactly what to run.
        assert!(s.contains("${MOS_SETUP:-manifest strata add debian}"), "headless advice: {s}");
    }

    #[test]
    fn merge_mimeapps_sets_defaults_without_clobbering_others() {
        let existing = "[Default Applications]\n\
                        text/html=firefox.desktop\n\
                        application/pdf=org.pwmt.zathura.desktop\n\
                        \n\
                        [Added Associations]\n\
                        text/html=firefox.desktop;\n";
        let out = merge_mimeapps(existing, default_associations());
        // Ours become the default.
        assert!(out.contains("application/vnd.apkm=manifest-apkm-install.desktop"), "{out}");
        assert!(out.contains("application/vnd.android.package-archive=manifest-apkm-install.desktop"), "{out}");
        assert!(out.contains("application/x-rpm=manifest-strata-install.desktop"), "{out}");
        // Unrelated entries survive untouched.
        assert!(out.contains("text/html=firefox.desktop"), "{out}");
        assert!(out.contains("application/pdf=org.pwmt.zathura.desktop"), "{out}");
        // Both sections present.
        assert!(out.contains("[Default Applications]") && out.contains("[Added Associations]"), "{out}");
    }

    #[test]
    fn merge_mimeapps_replaces_a_previous_owner_and_is_idempotent() {
        let existing = "[Default Applications]\napplication/x-rpm=some-other.desktop\n";
        let once = merge_mimeapps(existing, default_associations());
        assert!(!once.contains("some-other.desktop"), "old owner replaced: {once}");
        assert_eq!(once.matches("application/x-rpm=").count(), 2, "one per section: {once}");
        // Re-running must not duplicate anything.
        let twice = merge_mimeapps(&once, default_associations());
        assert_eq!(twice.matches("application/x-rpm=").count(), 2, "idempotent: {twice}");
    }

    #[test]
    fn merge_mimeapps_from_empty_creates_both_sections() {
        let out = merge_mimeapps("", default_associations());
        assert!(out.contains("[Default Applications]"), "{out}");
        assert!(out.contains("[Added Associations]"), "{out}");
        assert!(out.contains("application/x-xapk=manifest-apkm-install.desktop"), "{out}");
    }

    #[test]
    fn gui_wrapper_dispatches_and_never_needs_a_terminal() {
        let s = gui_install_script();
        assert!(s.contains("MOS_TOOL=android-install"), "apk→android: {s}");
        assert!(s.contains("MOS_TOOL=strata-install"), "deb→strata: {s}");
        // Tries terminals, but works without one via notifications.
        assert!(s.contains("notify-send"), "{s}");
        assert!(s.contains("x-terminal-emulator") && s.contains("gnome-terminal"), "terminal list: {s}");
        // The handler must not rely on Terminal=true.
        assert!(mime_handler_desktop().contains("Terminal=false"), "{}", mime_handler_desktop());
        assert!(mime_handler_desktop().contains("Exec=/usr/local/bin/manifest-install-gui %f"), "{}", mime_handler_desktop());
    }

    #[test]
    fn thin_stub_execs_the_current_binary() {
        let s = thin_stub("android-install");
        assert!(s.starts_with("#!/bin/sh"), "{s}");
        // Fetches the live logic from the binary and execs it with the args.
        assert!(s.contains("exec sh -c \"$(manifest __script android-install)\" manifest \"$@\""), "{s}");
    }

    #[test]
    fn shq_escapes_single_quotes() {
        assert_eq!(shq("abc"), "'abc'");
        assert_eq!(shq("a'b"), "'a'\\''b'");
    }
}
