//! The **Windows VM tier** (`docs/strata-design.md` §14, Phase 6b) — individual
//! Windows apps painted onto the Linux desktop, for everything the wine tier
//! ([`crate::windows`]) can't run.
//!
//! ## What this is, and what it deliberately isn't
//!
//! The heavy lifting is done by **[WinApps]** (FreeRDP RemoteApp against a
//! Windows VM) and **dockur/windows** (a container that installs Windows
//! unattended). Both are **GPL-3.0**; this repo is MIT. So, exactly as
//! [`crate::strata`]'s design rules out vendoring crossfs (§1.3), we **never copy
//! their source into this tree**. We:
//!
//! 1. install them as *separate components* (AUR package / upstream clone),
//! 2. generate their configuration from the manifest, and
//! 3. drive their CLIs.
//!
//! That's the same "thin orchestrator of standard tools" contract as pacman,
//! debootstrap and waydroid — and it keeps the licence boundary clean.
//!
//! ## Why it's lazy
//!
//! Setting this up downloads and installs a **real Windows** (multi-GB, tens of
//! minutes, and the user's own licence question). Nothing here runs at install
//! time: `manifest windows-vm` is invoked on demand, the same "add if used" flow
//! as strata/Android/wine.
//!
//! [WinApps]: https://github.com/winapps-org/winapps

use crate::exec::Ctx;
use crate::manifest::WindowsVm;
use anyhow::{bail, Result};

/// Where the generated compose file lives (user-owned; it holds the VM's spec).
const COMPOSE_DIR: &str = "$HOME/.local/share/manifest-os/windows-vm";
/// WinApps reads this.
const WINAPPS_CONF_DIR: &str = "$HOME/.config/winapps";

/// Set up the Windows VM tier. Long-running and interactive by nature — the user
/// watches Windows install in a browser — so this is a command, never a step in
/// `install`.
pub fn setup(vm: &WindowsVm, ctx: &Ctx) -> Result<()> {
    let backend = vm.backend();
    if !matches!(backend, "docker" | "podman" | "libvirt") {
        bail!("windows.vm.backend must be docker, podman or libvirt (got '{backend}')");
    }
    println!("  · Windows VM tier — backend: {backend}");
    if backend == "libvirt" {
        println!(
            "  · libvirt backend: you provide the Windows VM; this only configures WinApps \
             to connect to it"
        );
    }

    ensure_deps(vm, ctx)?;
    ensure_winapps(ctx)?;

    // The ISO is fetched automatically — say so, because "where do I get Windows?"
    // is the first thing a newcomer wonders.
    println!(
        "  · Windows {} will be downloaded from Microsoft automatically (no ISO to find)",
        vm.version()
    );

    // Offer the product key here rather than demanding it in the manifest: a key
    // is a licence credential, and Windows runs fine without one.
    let mut vm = vm.clone();
    if vm.product_key.is_none() {
        if let Some(k) = prompt_product_key() {
            vm.product_key = Some(k);
        }
    }
    let vm = &vm;

    // The Windows account password is written into the guest while Windows
    // installs, and nothing afterwards can change it from out here. Minting a
    // fresh one on a re-run therefore doesn't rotate anything -- it just locks
    // us out of the guest we already have, and every RDP connect dies at
    // post_connect with no message that says why. So: an existing setup's
    // password wins, and a new one is only generated when there is nothing yet
    // to be locked out of.
    let pass = vm
        .password
        .clone()
        .or_else(|| password_in_compose(&read_existing_compose().unwrap_or_default()))
        .unwrap_or_else(generated_password);
    if vm.password.is_some() {
        println!(
            "  · note: a password in the manifest is a credential leak if you share it — \
             prefer omitting it and letting one be generated"
        );
    }

    // The lazy runtime: on-demand start + idle stop, mirroring Android's
    // waydroid-launch/waydroid-idle. Thin stubs so `pacman -Syu` updates them.
    write_root_if_changed(
        ctx,
        "/usr/local/bin/windows-vm-run",
        &crate::android::thin_stub("windows-vm-run"),
        true,
    )?;
    let idle = vm.idle_minutes.unwrap_or(30);
    println!(
        "  · lazy lifecycle: the VM starts when an app needs it{}",
        if idle == 0 { ", and stays up (idle_minutes: 0)".to_string() }
        else { format!(", and stops after {idle} min idle") }
    );
    write_root_if_changed(ctx, "/usr/local/bin/windows-vm-idle", &vm_idle_script(idle), true)?;
    write_root_if_changed(ctx, "/usr/local/bin/manifest-freerdp", freerdp_wrapper(), true)?;
    write_root_if_changed(
        ctx,
        "/etc/systemd/user/windows-vm-idle.service",
        idle_service_unit(),
        false,
    )?;
    write_root_if_changed(
        ctx,
        "/etc/systemd/user/windows-vm-idle.timer",
        idle_timer_unit(),
        false,
    )?;
    ctx.shell("systemctl --user enable --now windows-vm-idle.timer >/dev/null 2>&1 || true", false)?;

    // WinApps' own config: how it reaches the guest.
    ctx.shell(&format!("mkdir -p \"{WINAPPS_CONF_DIR}\""), false)?;
    ctx.write_user(
        &expand(&format!("{WINAPPS_CONF_DIR}/winapps.conf")),
        &winapps_conf(vm, &pass),
    )?;
    ctx.shell(&format!("chmod 600 \"{WINAPPS_CONF_DIR}/winapps.conf\""), false)?;

    if backend != "libvirt" {
        ctx.shell(&format!("mkdir -p \"{COMPOSE_DIR}\""), false)?;
        // Debloat runs *during Windows setup* via dockur's /oem hook, so the junk
        // never gets a first run. Off only if the manifest says so.
        // The guest MUST be told to allow arbitrary RemoteApp programs, or every
        // launch is refused and FreeRDP exits instantly -- which looks exactly
        // like "the window didn't open". WinApps ships the registry files that
        // do it (oem/RDPApps.reg sets TSAppAllowList\fDisabledAllowList=1), and
        // dockur runs C:\OEM\install.bat during Windows setup.
        //
        // Their files are GPL-3.0, so they are COPIED AT RUNTIME from the clone
        // on the user's machine -- never into this tree. Same boundary as the
        // rest of this module.
        ctx.shell(&format!("mkdir -p \"{COMPOSE_DIR}/oem\""), false)?;
        ctx.shell(&setup_oem_step(), false)?;
        if vm.debloat.unwrap_or(true) {
            println!("  · debloating: removing preinstalled Store apps, Cortana and telemetry");
            ctx.write_user(&expand(&format!("{COMPOSE_DIR}/oem/debloat.ps1")), debloat_ps1())?;
            // Chain onto WinApps' install.bat rather than replacing it: dockur
            // runs exactly one install.bat, and theirs is the one that enables
            // RemoteApp. Ours must not be what overwrites it.
            ctx.shell(&setup_debloat_bat_step(), false)?;
            // No WinApps oem at all: our own bat is better than nothing.
            ctx.shell(&setup_fallback_bat_step(), false)?;
        }
        let compose = compose_yaml(vm, &pass);
        ctx.write_user(&expand(&format!("{COMPOSE_DIR}/compose.yaml")), &compose)?;
        // WinApps reads ~/.config/winapps/compose.yaml — the path is hardcoded in
        // its script, not configurable — and it inspects a container hardcoded to
        // the name "WinApps". Without both, it exits "no such object: WinApps"
        // before it ever tries RDP. Same file, same absolute volumes, so the two
        // copies describe one container over one disk.
        ctx.write_user(&expand(&format!("{WINAPPS_CONF_DIR}/compose.yaml")), &compose)?;

        // An earlier setup named the container manifest-windows. Retire it, or it
        // holds ports 3389/8006 and WinApps still can't find what it's looking
        // for. The Windows disk lives in the storage volume, so nothing is lost.
        ctx.shell(
            &format!(
                "{docker}                 if dk inspect manifest-windows >/dev/null 2>&1; then                   echo '  · renaming the Windows container to what WinApps expects (your Windows install is kept)'
                   dk stop manifest-windows >/dev/null 2>&1 || true
                   dk rm manifest-windows >/dev/null 2>&1 || true
                 fi",
                docker = docker_fn()
            ),
            false,
        )?;
        println!("  · starting the Windows container (first run installs Windows)");
        let up = if backend == "podman" {
            format!("cd \"{COMPOSE_DIR}\" && podman compose up -d")
        } else {
            // `usermod -aG docker` does NOT affect the session already running, so
            // the socket is unreachable until re-login. Try the current session,
            // then `sg docker` (picks up the new membership without logging out),
            // then root. Whichever works, the container comes up now.
            format!(
                "cd \"{COMPOSE_DIR}\" && {{                    docker compose up -d 2>/dev/null                    || {{ echo '  · using the newly-added docker group (no re-login needed)';                          sg docker -c 'docker compose up -d'; }} 2>/dev/null                    || {{ echo '  · falling back to running docker as root';                          sudo docker compose up -d; }}; }}"
            )
        };
        ctx.shell(&up, false)?;
        // Start the idle clock now. Otherwise the watchdog inherits whenever an
        // app was last used -- possibly hours ago -- and a container we just
        // started is already past its deadline.
        ctx.shell(
            "a=\"${XDG_STATE_HOME:-$HOME/.local/state}/windows-vm-activity\"; \
             mkdir -p \"$(dirname \"$a\")\" 2>/dev/null || true; : > \"$a\" 2>/dev/null || true",
            false,
        )?;
        println!();
        println!("  Windows is installing inside the container. Watch it at:");
        println!("      http://localhost:8006");
        println!("  This takes a while (typically 20-40 minutes) and needs no input.");
        println!("  When the desktop appears, finish with:");
        println!("      manifest windows-vm --link");
    }
    Ok(())
}

/// Second phase, after Windows has finished installing: ask WinApps to detect
/// the installed applications and write a `.desktop` for each.
pub fn link_apps(ctx: &Ctx) -> Result<()> {
    // WinApps' wizard needs `dialog`; installing it here means `--link` works
    // even if the VM was set up by an older version that didn't pull it in.
    // Only when something is genuinely missing: `--link` is re-entered from
    // `windows-vm-run`, which runs from a .desktop launcher with no TTY, and an
    // interactive sudo there aborts it. Same reasoning as `ensure_deps`.
    println!("  · checking WinApps' dependencies");
    ensure_packages(ctx, &["dialog", "gawk", "curl", "openbsd-netcat", "freerdp"])?;

    // Make sure winapps + winapps-setup are actually on PATH (an older setup may
    // have linked only into ~/.local/bin).
    ensure_winapps(ctx)?;

    println!("  · checking the Windows container is running");
    ctx.shell(&link_container_check_step(), false)?;

    // Fully automatic: no wizard, no commands for the user to run. WinApps
    // refuses to install over a previous installation (exit 3), so clear any
    // prior one first — uninstall is a no-op when there's nothing there — then
    // install non-interactively. `--system` first, `--user` as the fallback for
    // machines where the system-wide path isn't available.
    println!("  · installing WinApps and detecting your Windows apps");
    ctx.shell(link_install_step(), false)?;
    println!("  · done — installed Windows apps should now appear in your menu");
    Ok(())
}

/// Host packages the tier needs. FreeRDP is the actual window transport.
fn ensure_deps(vm: &WindowsVm, ctx: &Ctx) -> Result<()> {
    // `dialog` drives WinApps' setup wizard; gawk/curl/netcat are used by its
    // scripts. Missing any of them fails only at `--link`, long after setup.
    let mut pkgs = vec![
        "freerdp", "iproute2", "libnotify", "git", "dialog", "gawk", "curl", "openbsd-netcat",
    ];
    match vm.backend() {
        "podman" => pkgs.extend(["podman", "podman-compose"]),
        "libvirt" => pkgs.extend(["libvirt", "qemu-full", "virt-manager"]),
        _ => pkgs.extend(["docker", "docker-compose"]),
    }
    // Every root step below is skipped when it has already been done. `--needed`
    // makes pacman a no-op, but it still asks for a password -- and this command
    // is re-entered from `windows-vm-run`, i.e. from a .desktop launcher with no
    // TTY, where an interactive sudo aborts the whole setup with
    // "sudo: a terminal is required". A second run on a configured machine must
    // need no root at all.
    ensure_packages(ctx, &pkgs)?;
    if vm.backend() == "docker" {
        // Docker must actually be running, and the user needs to reach it.
        ensure_service(ctx, "docker.service")?;
        ensure_group(ctx, "docker")?;
    }
    if vm.backend() == "libvirt" {
        ensure_service(ctx, "libvirtd.service")?;
        ensure_group(ctx, "libvirt")?;
    }
    Ok(())
}

/// Install only the packages that are actually missing.
///
/// `pacman -S --needed` is already a no-op when everything is present, but it
/// still asks for a password — and both `manifest windows-vm` and
/// `manifest windows-vm --link` are re-entered from `windows-vm-run`, i.e. from
/// a .desktop launcher with no TTY, where that aborts the whole run with
/// "sudo: a terminal is required".
fn ensure_packages(ctx: &Ctx, pkgs: &[&str]) -> Result<()> {
    let missing: Vec<&str> =
        pkgs.iter().copied().filter(|p| !ctx.check("pacman", &["-Q", p])).collect();
    if missing.is_empty() {
        println!("  · host dependencies already installed: {}", pkgs.join(", "));
        return Ok(());
    }
    println!("  · installing host dependencies: {}", missing.join(", "));
    let mut args = vec!["-S", "--needed", "--noconfirm"];
    args.extend(missing.iter().copied());
    ctx.sudo("pacman", &args)
}

/// Start and enable a service, but only if it isn't already both. Root is asked
/// for exactly when there is something to change — see [`ensure_deps`].
fn ensure_service(ctx: &Ctx, unit: &str) -> Result<()> {
    if ctx.check("systemctl", &["is-active", "--quiet", unit])
        && ctx.check("systemctl", &["is-enabled", "--quiet", unit])
    {
        return Ok(());
    }
    ctx.sudo("systemctl", &["enable", "--now", unit])
}

/// Put the user in a group, but only if they aren't in it yet.
fn ensure_group(ctx: &Ctx, group: &str) -> Result<()> {
    if ctx.check("sh", &["-c", &format!("id -nG | tr ' ' '\\n' | grep -qx {group}")]) {
        return Ok(());
    }
    ctx.shell(&format!("sudo usermod -aG {group} \"$USER\" || true"), false)?;
    println!("  · added you to the `{group}` group (log out and back in if the next step fails)");
    Ok(())
}

/// Install WinApps itself — **as a separate component**, never vendored. Prefers
/// the AUR package; falls back to an upstream clone into the user's data dir.
fn ensure_winapps(ctx: &Ctx) -> Result<()> {
    // Only fetch the source. WinApps' own `setup.sh` is a real installer that
    // manages /usr/local/bin/winapps-src — symlinking our own `winapps` next to
    // it makes its conflict check fail with "EXISTING 'SYSTEM' WINAPPS
    // INSTALLATION", so we deliberately install nothing ourselves.
    println!("  · fetching WinApps (GPL-3.0, installed separately — not part of ManifestOS)");
    ctx.shell(ensure_winapps_step(), false)
}

/// Fetch the WinApps checkout and clear symlinks an older ManifestOS made.
/// Pure — unit-tested.
fn ensure_winapps_step() -> &'static str {
    r#"d="$HOME/.local/share/manifest-os/winapps"
       mkdir -p "$(dirname "$d")"
       if [ -d "$d/.git" ]; then
         git -C "$d" pull --ff-only >/dev/null 2>&1 || true
       else
         git clone --depth 1 https://github.com/winapps-org/winapps "$d" || exit 1
       fi
       chmod +x "$d/setup.sh" "$d/bin/winapps" 2>/dev/null || true
       # Remove symlinks an earlier version of ManifestOS created — they are
       # what WinApps' installer trips over: its conflict check is
       # `[ -f /usr/local/bin/winapps ]`, which follows symlinks, so ours reads
       # as an existing system-wide installation and BOTH --system and --user
       # refuse with exit 3. Only ours (symlinks into our checkout) are touched;
       # a real WinApps install is left alone.
       for l in /usr/local/bin/winapps /usr/local/bin/winapps-setup; do
         if [ -L "$l" ] && readlink "$l" | grep -q 'manifest-os/winapps'; then
           echo "  · removing our old symlink $l (it conflicts with WinApps' installer)"
           # Non-interactive first: this runs from a .desktop launcher too, and
           # a bare `sudo` there waits forever on a prompt nobody can see. Only
           # escalate interactively when there is a terminal to answer on.
           sudo -n rm -f "$l" 2>/dev/null ||
             { [ -t 0 ] && sudo rm -f "$l"; } ||
             echo "  ! could not remove $l — needs root; app detection will fail until it is gone" >&2
         fi
       done
       for l in "$HOME/.local/bin/winapps" "$HOME/.local/bin/winapps-setup"; do
         [ -L "$l" ] && readlink "$l" | grep -q 'manifest-os/winapps' && rm -f "$l"
       done
       true"#
}

/// Shell helper used by every generated script: run a docker command through
/// whichever privilege path works. `usermod -aG docker` doesn't affect a session
/// that's already running, so `sg docker` bridges the gap without a re-login.
fn docker_fn() -> &'static str {
    // NEVER interactive sudo here: these scripts are what .desktop launchers
    // Exec, and a launcher has no TTY — a password prompt there hangs forever
    // with nothing on screen (the same trap Waydroid's launchers hit). `-n`
    // fails immediately instead, and we say why somewhere the user can see it.
    "dk() {
         if docker \"$@\" 2>/dev/null; then return 0; fi
         if sg docker -c \"docker $*\" 2>/dev/null; then return 0; fi
         if sudo -n docker \"$@\" 2>/dev/null; then return 0; fi
         msg='Log out and back in once — your user was added to the `docker` group and a session only picks that up at login.'
         echo \"windows-vm: cannot reach docker. $msg\" >&2
         command -v notify-send >/dev/null 2>&1 && notify-send 'Windows VM' \"$msg\"
         return 1
     }
"
}

/// Erase the Windows disk, used by `windows-vm-run`'s reinstall offer.
///
/// dockur creates `storage/` as `root:root`, so the obvious `rm -rf` from the
/// user's shell removes **nothing** — and with its error swallowed the
/// reinstall looks like it worked while booting the very same Windows, costing
/// another 20-40 minutes to find out. Delete it with the privilege that made
/// it: docker itself.
fn wipe_storage_fn() -> &'static str {
    "wipe_storage() {
         rm -rf \"$1\" 2>/dev/null
         # The image is already local, so this pulls nothing.
         if [ -n \"$(ls -A \"$1\" 2>/dev/null)\" ]; then
             dk run --rm -v \"$1:/wipe\" --entrypoint /bin/sh dockurr/windows \\
                 -c 'rm -rf /wipe/..?* /wipe/.[!.]* /wipe/*' >/dev/null 2>&1 || true
         fi
         # Never interactive: this is reachable from a launcher (see dk).
         if [ -n \"$(ls -A \"$1\" 2>/dev/null)\" ]; then
             sudo -n rm -rf \"$1\" >/dev/null 2>&1 || true
         fi
         rmdir \"$1\" 2>/dev/null || true
         # Empty is as good as gone: docker recreates the bind-mount directory.
         [ -z \"$(ls -A \"$1\" 2>/dev/null)\" ]
     }
"
}

/// `windows-vm-run <file.exe>` — the lazy VM entry point, mirroring Android's
/// `waydroid-launch`: set the VM up if it has never been set up, **start it only
/// when something needs it**, then hand the installer over. Pure — unit-tested.
pub fn vm_run_script() -> String {
    format!(
        r####"#!/bin/sh
# ManifestOS — run a Windows program in the Windows VM (generated; do not edit).
# usage: windows-vm-run <file.exe|file.msi>
[ $# -ge 1 ] || {{ echo 'usage: windows-vm-run <file.exe|file.msi>' >&2; exit 2; }}
f=$1
VMDIR="$HOME/.local/share/manifest-os/windows-vm"
{docker}{wipe}
# 1. First use? Set the VM up (downloads and installs Windows — one time).
if [ ! -f "$VMDIR/compose.yaml" ]; then
  echo "The Windows VM isn't set up yet."
  echo "  It installs a real Windows once (a few GB, 20-40 min); after that,"
  echo "  Windows apps open like normal windows."
  printf 'Set it up now? [y/N] '
  read r
  case "$r" in
    [yY]|[yY][eE][sS]) manifest windows-vm || exit 1 ;;
    *) echo "Cancelled."; exit 1 ;;
  esac
fi

# 1b. Migrate a setup made before we knew what WinApps requires. Two things are
#     hardcoded in its script and not configurable: the container must be named
#     "WinApps", and it reads its OWN ~/.config/winapps/compose.yaml. An older
#     compose satisfies neither, so winapps quits with "no such object: WinApps"
#     without ever reaching RDP. compose.yaml is a file written at setup time,
#     so a package upgrade cannot fix it -- do it here, where it is noticed.
if [ -f "$VMDIR/compose.yaml" ] && ! grep -q 'container_name: WinApps' "$VMDIR/compose.yaml" 2>/dev/null; then
  echo "Updating the Windows VM to work with WinApps (your Windows install is kept)..."
  # Volumes become absolute at the same time: the file now lives in two
  # directories, and a relative path would mean a different disk in each.
  sed -i "s|container_name: manifest-windows|container_name: WinApps|; s|- \./storage:/storage|- $VMDIR/storage:/storage|; s|- \./oem:/oem|- $VMDIR/oem:/oem|" "$VMDIR/compose.yaml" 2>/dev/null || true
  # The old container still holds 3389/8006, and the disk is in the volume.
  dk stop manifest-windows >/dev/null 2>&1 || true
  dk rm manifest-windows >/dev/null 2>&1 || true
fi
mkdir -p "$HOME/.config/winapps" 2>/dev/null || true
cp -f "$VMDIR/compose.yaml" "$HOME/.config/winapps/compose.yaml" 2>/dev/null || true

# 2. Lazy start: the VM is not kept running, so bring it up on demand.
state=$(dk inspect -f '{{{{.State.Running}}}}' WinApps 2>/dev/null || echo missing)
if [ "$state" != "true" ]; then
  echo "Starting Windows (this takes a moment)..."
  dk start WinApps >/dev/null 2>&1 || (cd "$VMDIR" && dk compose up -d) || {{
    echo "windows-vm-run: could not start the Windows container" >&2; exit 1; }}
fi

# 3. Give Windows a moment to be ready — but never insist. The image may not
#    define a healthcheck at all (the field comes back empty), and a TCP probe of
#    3389 is useless because docker publishes that port before Windows listens on
#    it. So: wait *if* health is reported, otherwise just get on with it and let
#    the actual launch be the test.
health() {{ dk inspect -f '{{{{.State.Health.Status}}}}' WinApps 2>/dev/null | tr -d '
'; }}
h=$(health)
case "$h" in
  healthy) ;;                       # ready
  ""|"<no value>") ;;               # image reports no health — don't block on it
  *)
    echo "Waiting for Windows to be ready..."
    i=0
    while [ $i -lt 60 ]; do          # ~2 minutes, then try anyway
      [ "$(health)" = "healthy" ] && break
      i=$((i+1)); sleep 2
    done ;;
esac

# 3b. First time Windows is ready: finish WinApps setup ourselves. You should
#     never have to run a command to make this work.
if ! command -v winapps >/dev/null 2>&1    && [ ! -x "$HOME/.local/share/manifest-os/winapps/bin/winapps" ]; then
  echo "Finishing Windows setup (first time only)..."
  manifest windows-vm --link || true
fi

# 4. The installer has to be reachable from inside Windows. dockur shares your
#    home as a network drive, so copy it there and say where it landed.
base=$(basename "$f")
mkdir -p "$HOME/Windows Transfer"
cp -f "$f" "$HOME/Windows Transfer/$base" 2>/dev/null || true
: > "${{XDG_STATE_HOME:-$HOME/.local/state}}/windows-vm-activity" 2>/dev/null || true

# Resolve winapps: its own installer puts it on PATH, but fall back to the
# checkout if `--link` hasn't been run yet.
WA=$(command -v winapps 2>/dev/null)
[ -n "$WA" ] || WA="$HOME/.local/share/manifest-os/winapps/bin/winapps"
[ -x "$WA" ] || {{ echo "windows-vm-run: winapps isn't set up — run: manifest windows-vm --link" >&2; exit 1; }}
# winapps runs docker itself, so it needs the same group bridge dk() gives us.
# Keep its output — we need it to tell "the window opened" from "it bailed".
WALOG="${{XDG_STATE_HOME:-$HOME/.local/state}}/windows-vm-launch.log"
mkdir -p "$(dirname "$WALOG")" 2>/dev/null || true
run_wa() {{ "$WA" "$@" >>"$WALOG" 2>&1 || sg docker -c "'$WA' $*" >>"$WALOG" 2>&1; }}

# Which Windows-app launchers exist right now, so we can name whatever appears
# later. Match on CONTENT, not filename: WinApps writes "<exe>.desktop" with no
# prefix of its own, so there is nothing in the name to key off — but every
# entry it generates Execs winapps.
list_apps() {{
  for d in "$HOME/.local/share/applications" /usr/share/applications; do
    [ -d "$d" ] || continue
    grep -rlsi 'winapps' "$d" --include='*.desktop' 2>/dev/null || true
  done | sed 's#.*/##' | sort -u
}}

# Run the installer as a RemoteApp: its own window on your desktop. No Windows
# desktop, no Windows taskbar, no browser tab — which is the entire point of
# this tier.
#
# Two ways your home reaches the guest, and which one is live depends on how
# the session came up, so try both:
#   \\tsclient\home  FreeRDP redirects $HOME into the session itself, so this
#                    exists for the RemoteApp we are about to start.
#   Z:               dockur mounts our /shared volume ($HOME) as a drive.
echo
echo "Opening $base in Windows..."
: > "$WALOG" 2>/dev/null || true
APPSBEFORE=$(mktemp) && list_apps > "$APPSBEFORE"
# Where the file lands inside Windows. Set up front, not in the loop below:
# the loop is skipped on a guest that can't do RemoteApp, and the desktop
# fallback still has to tell the user where to find their installer.
WINPATH='Z:\Windows Transfer\'"$base"
opened=0
# Only a guest INSTALLED with TSAppAllowList\fDisabledAllowList=1 can run an
# arbitrary program as a RemoteApp. Without it the connection still succeeds --
# Windows just serves the console session instead, so you get a full desktop,
# plus an "Another user is signed in" prompt because dockur is already signed
# in there. That sits on screen well past any duration check, so attempting it
# anyway does not produce evidence: it produces a false success, a stray
# Windows desktop, and a launcher for an app nobody installed. Don't attempt
# what the guest cannot do -- the reinstall offer below is the actual fix.
if [ -f "$VMDIR/.remoteapp-enabled" ]; then
  for p in '\\tsclient\home\Windows Transfer\' 'Z:\Windows Transfer\'; do
    WINPATH="$p$base"
    t0=$(date +%s)
    run_wa manual "$WINPATH"
    t1=$(date +%s)
    # winapps blocks on `wait $FREERDP_PID`, so a session you actually saw and
    # closed lasts longer than a few seconds. An instant return means FreeRDP
    # died on the spot -- and winapps launches it with `&>/dev/null`, so its
    # reason never reaches us.
    if [ $((t1 - t0)) -ge 5 ]; then opened=1; break; fi
  done
fi

# NOTE: RDP's "alternate shell" (/shell:) was tried here and does NOT work on
# this stack. Windows client editions allow one interactive session, and dockur
# signs the user in at the console automatically -- so an RDP connection as that
# user TAKES OVER the existing session instead of creating one, and the
# alternate shell is ignored. The result is an ordinary Windows desktop with
# explorer running, which also looks "successful" to any duration check. The
# only real single-window mechanism here is RemoteApp, which needs the guest
# registry key below.

if [ "$opened" = 1 ]; then
  echo
  echo "$base has closed."
  # Whatever it installed should now be in your launcher — re-detect so you
  # never have to run anything yourself.
  echo "Checking for newly installed Windows apps..."
  manifest windows-vm --link >/dev/null 2>&1 || true
  APPSAFTER=$(mktemp) && list_apps > "$APPSAFTER"
  new=$(comm -13 "$APPSBEFORE" "$APPSAFTER" 2>/dev/null)
  rm -f "$APPSBEFORE" "$APPSAFTER" 2>/dev/null || true

  if [ -n "$new" ]; then
    echo "Added to your app launcher:"
    printf '%s\n' "$new" | sed 's/\.desktop$//; s/^/  · /'
  else
    # Nothing new was installed — which is normal: plenty of Windows tools are
    # a single portable .exe that IS the app (Rufus, for one). There is nothing
    # for a scan to find, so make a launcher for the file itself.
    echo "Nothing was installed — this looks like a portable app (the .exe is"
    echo "the program itself). Adding it to your launcher directly."
    name=$(printf '%s' "$base" | sed 's/\.[Ee][Xx][Ee]$//; s/\.[Mm][Ss][Ii]$//')
    slug=$(printf '%s' "$name" | tr 'A-Z' 'a-z' | tr -c 'a-z0-9' '-' | sed 's/-\{{1,\}}/-/g; s/^-//; s/-$//')
    apps="$HOME/.local/share/applications"
    mkdir -p "$apps"
    {{
      echo '[Desktop Entry]'
      echo 'Type=Application'
      echo "Name=$name"
      echo 'Comment=Windows app, running in the Windows VM'
      echo "Exec=windows-vm-run \"$HOME/Windows Transfer/$base\""
      echo 'Icon=applications-other'
      echo 'Terminal=false'
      echo 'Categories=Utility;'
    }} > "$apps/manifest-winvm-$slug.desktop"
    command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$apps" 2>/dev/null || true
    echo "  · $name"
  fi
  echo
  echo "Done — look for it in your app launcher."
  exit 0
fi

# Both share paths came back instantly: the RemoteApp never painted. Show what
# winapps actually said rather than guessing.
rm -f "$APPSBEFORE" 2>/dev/null || true
# Only report on an attempt we actually made.
if [ -f "$VMDIR/.remoteapp-enabled" ]; then
  echo "Couldn't open it as its own window." >&2
  [ -s "$WALOG" ] && {{ echo "What winapps reported:" >&2; tail -n 20 "$WALOG" | sed 's/^/    /' >&2; }}
  # winapps throws FreeRDP's own output away, so the wrapper's log is usually
  # the only place the real reason appears.
  FRLOG="${{XDG_STATE_HOME:-$HOME/.local/state}}/windows-vm-freerdp.log"
  [ -s "$FRLOG" ] && {{ echo "What FreeRDP reported:" >&2; tail -n 20 "$FRLOG" | sed 's/^/    /' >&2; }}
fi
# Windows only runs an arbitrary program as a RemoteApp when the guest has
# TSAppAllowList\fDisabledAllowList=1, which is applied during Windows SETUP
# (WinApps' oem/RDPApps.reg, via dockur's C:\OEM\install.bat). A guest built
# before we mounted those files can never do it, and there is no way to re-run
# setup on an installed Windows -- so offer the one thing that does fix it.
if [ ! -f "$VMDIR/.remoteapp-enabled" ]; then
  echo >&2
  echo "This Windows was set up before ManifestOS knew to enable single-window" >&2
  echo "apps, and that switch is only thrown while Windows installs." >&2
  echo >&2
  if [ -t 0 ]; then
    echo "Windows can reinstall itself with it enabled. It runs unattended and" >&2
    echo "takes 20-40 minutes. ANYTHING INSIDE THIS WINDOWS IS ERASED -- your" >&2
    echo "Linux files are untouched." >&2
    printf 'Reinstall Windows now? [y/N] ' >&2
    read ans
    case "$ans" in
      [yY]|[yY][eE][sS])
        echo "Removing the old Windows VM..." >&2
        dk stop WinApps >/dev/null 2>&1 || true
        dk rm WinApps >/dev/null 2>&1 || true
        # If the old disk survives, `manifest windows-vm` just boots the SAME
        # Windows -- the reinstall silently does nothing and the 40 minutes are
        # wasted. Never claim it happened without checking.
        wipe_storage "$VMDIR/storage" || {{
          echo "Couldn't erase the old Windows disk." >&2
          echo "It belongs to root (docker created it), and neither docker nor" >&2
          echo "passwordless root could remove it. Delete this folder as root," >&2
          echo "then open this file again:" >&2
          echo "    $VMDIR/storage" >&2
          exit 1; }}
        echo "Reinstalling — watch it at http://localhost:8006" >&2
        manifest windows-vm || exit 1
        echo >&2
        echo "When Windows finishes, open this file again and it will get its own window." >&2
        exit 0 ;;
      *) echo "Left as is — opening the Windows desktop instead." >&2 ;;
    esac
  else
    echo "Run this from a terminal to be offered the fix." >&2
  fi
fi

# Older WinApps (no `manual`), or the launch didn't take: fall back to the full
# desktop so the file is still reachable by hand.
echo "Opening the Windows desktop instead." >&2
echo "Your installer is inside Windows at:" >&2
echo "    $WINPATH" >&2
echo "    (also on the Shared folder on the desktop)" >&2
run_wa windows || {{
    echo >&2
    echo "Couldn't open Windows yet. The usual reasons:" >&2
    echo >&2
    echo "  · Windows is still installing (the first run takes 20-40 minutes)." >&2
    echo "    Look at http://localhost:8006 — if setup is running, just wait and" >&2
    echo "    open this again once the desktop appears." >&2
    echo >&2
    echo "  · Remote Desktop hasn't started yet, even though Windows looks up." >&2
    echo "    Give it a minute after the desktop appears, then retry." >&2
    echo >&2
    echo "  · Docker permissions — log out and back in once (your user was added" >&2
    echo "    to the 'docker' group and a session only picks that up at login)." >&2
    exit 1
  }}
"####,
        docker = docker_fn(),
        wipe = wipe_storage_fn(),
    )
}

/// `manifest-freerdp` — what WinApps runs instead of FreeRDP directly.
///
/// WinApps launches FreeRDP as `$FREERDP_COMMAND … &>/dev/null &`, so when a
/// launch fails there is nothing at all to read — the failure that cost us
/// three rounds of guessing produced an empty log. This wrapper keeps stderr in
/// a file, then execs the real binary. Pure — unit-tested.
pub fn freerdp_wrapper() -> &'static str {
    r####"#!/bin/sh
# ManifestOS — FreeRDP wrapper (generated; do not edit).
# Exists purely so FreeRDP's errors survive winapps' `&>/dev/null`.
LOG="${XDG_STATE_HOME:-$HOME/.local/state}/windows-vm-freerdp.log"
mkdir -p "$(dirname "$LOG")" 2>/dev/null || true
# Keep the log from growing without bound.
[ -f "$LOG" ] && [ "$(wc -c <"$LOG" 2>/dev/null || echo 0)" -gt 262144 ] && : > "$LOG"
{
  echo "--- $(date '+%Y-%m-%d %H:%M:%S')"
  echo "    args: $*"
} >> "$LOG" 2>/dev/null || true

for c in xfreerdp3 xfreerdp sdl-freerdp3 wlfreerdp; do
    if command -v "$c" >/dev/null 2>&1; then
        exec 2>>"$LOG"
        exec "$c" "$@"
    fi
done
echo "    no FreeRDP binary found (looked for xfreerdp3, xfreerdp, sdl-freerdp3, wlfreerdp)" >> "$LOG"
exit 127
"####
}

/// `windows-vm-idle` — the watchdog that stops the VM when nothing is using it,
/// the same shape as Android's `waydroid-idle`. Pure — unit-tested.
pub fn vm_idle_script(minutes: u32) -> String {
    let secs = minutes.saturating_mul(60);
    format!(
        r####"#!/bin/sh
# ManifestOS — stop the Windows VM when idle (generated; do not edit).
IDLE={secs}
[ "$IDLE" -eq 0 ] && exit 0   # 0 = never auto-stop
{docker}
state=$(dk inspect -f '{{{{.State.Running}}}}' WinApps 2>/dev/null || echo missing)
[ "$state" = "true" ] || exit 0    # not running, nothing to do
# A live FreeRDP client means an app is on screen — keep it up.
if pgrep -x xfreerdp >/dev/null 2>&1 || pgrep -x xfreerdp3 >/dev/null 2>&1; then
  : > "${{XDG_STATE_HOME:-$HOME/.local/state}}/windows-vm-activity" 2>/dev/null || true
  exit 0
fi
# Installing Windows takes 20-40 minutes with no client attached and nothing
# touching the activity file -- which is this watchdog's exact definition of
# idle. It stopped a running install once; `manifest windows-vm` promises the
# user it "needs no input", so it must not need babysitting either.
# windows.boot is dockur's own marker for "installation finished" (its
# skipInstall reads it), so its absence means setup is still running.
VMDIR="$HOME/.local/share/manifest-os/windows-vm"
if [ -d "$VMDIR/storage" ] && [ ! -f "$VMDIR/storage/windows.boot" ]; then
  exit 0
fi
ACT="${{XDG_STATE_HOME:-$HOME/.local/state}}/windows-vm-activity"
now=$(date +%s)
last=$([ -e "$ACT" ] && stat -c %Y "$ACT" 2>/dev/null || echo 0)
[ "$last" -eq 0 ] && {{ mkdir -p "$(dirname "$ACT")"; : > "$ACT"; exit 0; }}
# Idle means "up and unused for IDLE", not "the last app closed a while ago".
# A container started just now after a long gap is brand new, not stale: count
# from whichever is later, or a fresh start is instantly eligible to be killed.
started=$(dk inspect -f '{{{{.State.StartedAt}}}}' WinApps 2>/dev/null)
started=$(date -d "$started" +%s 2>/dev/null || echo 0)
[ "$started" -gt "$last" ] && last=$started
if [ $((now - last)) -ge "$IDLE" ]; then
  echo "windows-vm-idle: stopping the idle Windows VM"
  dk stop WinApps >/dev/null 2>&1 || true
fi
"####,
        secs = secs,
        docker = docker_fn(),
    )
}

/// The compose file for the container-hosted Windows (dockur/windows). Pure —
/// unit-tested.
pub fn compose_yaml(vm: &WindowsVm, password: &str) -> String {
    // dockur/windows fetches the ISO straight from Microsoft for the requested
    // VERSION — nothing to source by hand. KEY/LANGUAGE are emitted only when
    // set; without a key Windows still installs and runs, just unactivated
    // (watermark + personalisation locked).
    let key_line = vm
        .product_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(|k| format!("      KEY: \"{k}\"\n"))
        .unwrap_or_default();
    // dockur copies /oem into the guest and runs install.bat during setup — the
    // supported hook for post-install tweaks like debloating.
    // Absolute, not `./oem`: WinApps insists on reading a compose file from its
    // own directory (~/.config/winapps/compose.yaml, hardcoded), so the same
    // file is written in two places. Relative volumes would resolve against
    // whichever directory the file happens to sit in and silently split the
    // Windows disk across two locations.
    let oem_line = if vm.debloat.unwrap_or(true) {
        format!("      - {COMPOSE_DIR}/oem:/oem\n")
    } else {
        String::new()
    };
    let lang_line = vm
        .language
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| format!("      LANGUAGE: \"{l}\"\n"))
        .unwrap_or_default();
    format!(
        "# Generated by ManifestOS (`manifest windows-vm`). Edit the manifest, not this.\n\
         # Runs Windows in a container (dockur/windows) so WinApps can show its\n\
         # applications as normal windows. Watch the install at http://localhost:8006\n\
         services:\n  \
           windows:\n    \
             image: dockurr/windows\n    \
             container_name: WinApps\n    \
             environment:\n      \
               VERSION: \"{version}\"\n      \
               RAM_SIZE: \"{ram}\"\n      \
               CPU_CORES: \"{cpus}\"\n      \
               DISK_SIZE: \"{disk}\"\n      \
               USERNAME: \"{user}\"\n      \
               PASSWORD: \"{password}\"\n\
         {key_line}{lang_line}      \
               HOME: \"${{HOME}}\"\n    \
             devices:\n      \
               - /dev/kvm\n      \
               - /dev/net/tun\n    \
             cap_add:\n      \
               - NET_ADMIN\n    \
             ports:\n      \
               - 8006:8006   # web viewer (watch the install)\n      \
               - 3389:3389/tcp   # RDP — how apps are painted onto your desktop\n      \
               - 3389:3389/udp\n    \
             volumes:\n      \
               - {COMPOSE_DIR}/storage:/storage\n      \
               - ${{HOME}}:/shared\n\
         {oem_line}    \
             restart: on-failure\n    \
             stop_grace_period: 2m\n",
        version = vm.version(),
        ram = vm.ram(),
        cpus = vm.cpus(),
        disk = vm.disk(),
        user = vm.username(),
        password = password,
        key_line = key_line,
        lang_line = lang_line,
        oem_line = oem_line,
    )
}

/// WinApps' own config — how it reaches the guest. Pure — unit-tested.
pub fn winapps_conf(vm: &WindowsVm, password: &str) -> String {
    let flavor = match vm.backend() {
        "podman" => "podman",
        "libvirt" => "libvirt",
        _ => "docker",
    };
    format!(
        "# Generated by ManifestOS (`manifest windows-vm`). Contains a password —\n\
         # keep it 0600. Edit the manifest's windows.vm block, not this file.\n\
         RDP_USER=\"{user}\"\n\
         RDP_PASS=\"{password}\"\n\
         RDP_DOMAIN=\"\"\n\
         RDP_IP=\"127.0.0.1\"\n\
         WAFLAVOR=\"{flavor}\"\n\
         RDP_SCALE=100\n\
         # Make it feel native: resize with the window, no cert prompt for our\n\
         # own loopback VM, reconnect quietly, and full-screen if a desktop is\n\
         # ever opened. RemoteApp windows are borderless regardless.\n\
         RDP_FLAGS=\"/dynamic-resolution /cert:ignore +auto-reconnect\"\n\
         # A RemoteApp window is already borderless and should size itself, so\n\
         # only the full-desktop fallback goes full-screen.\n\
         RDP_FLAGS_WINDOWS=\"/cert:ignore +auto-reconnect /f\"\n\
         RDP_FLAGS_NON_WINDOWS=\"/dynamic-resolution /cert:ignore +auto-reconnect\"\n\
         MULTIMON=\"false\"\n\
         DEBUG=\"true\"\n\
         # A wrapper, not xfreerdp3 directly: winapps launches FreeRDP with\n\
         # `&>/dev/null`, so when a launch fails there is nothing to read. The\n\
         # wrapper keeps stderr in a log we can show the user.\n\
         FREERDP_COMMAND=\"manifest-freerdp\"\n",
        user = vm.username(),
        password = password,
        flavor = flavor,
    )
}

/// Ask for a Windows product key, explaining plainly what skipping costs. Only
/// prompts on a terminal (never blocks a scripted run), and an empty answer is a
/// perfectly good answer.
fn prompt_product_key() -> Option<String> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return None;
    }
    println!();
    println!("  Windows product key (optional).");
    println!("    · Leave blank to install without one — Windows works, but shows an");
    println!("      \"Activate Windows\" watermark and personalisation stays locked.");
    println!("    · Enter a key you own to activate it and clear the watermark.");
    print!("  Key (or press Enter to skip): ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return None;
    }
    let k = line.trim();
    if k.is_empty() {
        println!("  · no key — Windows will run unactivated (that's fine, and legal)");
        None
    } else {
        Some(k.to_string())
    }
}

fn idle_service_unit() -> &'static str {
    "[Unit]
     Description=Stop the idle Windows VM (ManifestOS)
     [Service]
     Type=oneshot
     ExecStart=/usr/local/bin/windows-vm-idle
"
}

fn idle_timer_unit() -> &'static str {
    "[Unit]
     Description=Check whether the Windows VM is idle (ManifestOS)
     [Timer]
     OnBootSec=10min
     OnUnitActiveSec=5min
     [Install]
     WantedBy=timers.target
"
}

/// dockur runs `oem/install.bat` inside Windows during setup. Keep it a one-liner
/// that hands off to PowerShell, where the real work is readable.
fn debloat_bat() -> &'static str {
    r#"@echo off
rem ManifestOS - runs during Windows setup (generated).
powershell -NoProfile -ExecutionPolicy Bypass -File %~dp0debloat.ps1 >%~dp0debloat.log 2>&1
exit /b 0
"#
}

/// What actually gets removed. Deliberately conservative: it uninstalls *apps*
/// and flips *policy/preference* registry values -- no system-file surgery, no
/// service deletion, nothing that stops Windows updating or being repaired. This
/// VM exists to run one application, so the consumer extras are pure overhead.
fn debloat_ps1() -> &'static str {
    r#"# ManifestOS - Windows debloat (generated; runs once during setup).
# Conservative by design: removes preinstalled Store apps and turns off
# consumer suggestions/telemetry. No OS surgery - Windows still updates.
$ErrorActionPreference = 'SilentlyContinue'

$bloat = @(
  'Microsoft.3DBuilder','Microsoft.549981C3F5F10','Microsoft.BingNews',
  'Microsoft.BingWeather','Microsoft.BingSearch','Microsoft.GetHelp',
  'Microsoft.Getstarted','Microsoft.Messaging','Microsoft.MicrosoftOfficeHub',
  'Microsoft.MicrosoftSolitaireCollection','Microsoft.MicrosoftStickyNotes',
  'Microsoft.MixedReality.Portal','Microsoft.OneConnect','Microsoft.People',
  'Microsoft.SkypeApp','Microsoft.Wallet','Microsoft.WindowsFeedbackHub',
  'Microsoft.WindowsMaps','Microsoft.WindowsSoundRecorder','Microsoft.Xbox.TCUI',
  'Microsoft.XboxApp','Microsoft.XboxGameOverlay','Microsoft.XboxGamingOverlay',
  'Microsoft.XboxIdentityProvider','Microsoft.XboxSpeechToTextOverlay',
  'Microsoft.YourPhone','Microsoft.ZuneMusic','Microsoft.ZuneVideo',
  'Clipchamp.Clipchamp','MicrosoftTeams','MSTeams','Microsoft.Todos',
  'Microsoft.PowerAutomateDesktop','Microsoft.MicrosoftJournal'
)
foreach ($b in $bloat) {
  Get-AppxPackage -AllUsers -Name $b | Remove-AppxPackage -AllUsers
  Get-AppxProvisionedPackage -Online | Where-Object DisplayName -eq $b |
    Remove-AppxProvisionedPackage -Online -AllUsers
}

# Consumer suggestions / 'recommended' junk and silent app installs.
$cd = 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\CloudContent'
New-Item -Path $cd -Force | Out-Null
Set-ItemProperty -Path $cd -Name 'DisableWindowsConsumerFeatures' -Value 1 -Type DWord
Set-ItemProperty -Path $cd -Name 'DisableSoftLanding' -Value 1 -Type DWord

# Telemetry to the minimum the OS supports.
$dc = 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\DataCollection'
New-Item -Path $dc -Force | Out-Null
Set-ItemProperty -Path $dc -Name 'AllowTelemetry' -Value 0 -Type DWord

# Cortana and web results in search - useless in a single-app VM.
$ws = 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\Windows Search'
New-Item -Path $ws -Force | Out-Null
Set-ItemProperty -Path $ws -Name 'AllowCortana' -Value 0 -Type DWord
Set-ItemProperty -Path $ws -Name 'DisableWebSearch' -Value 1 -Type DWord
Set-ItemProperty -Path $ws -Name 'ConnectedSearchUseWeb' -Value 0 -Type DWord

# Widgets / news feed.
$dsh = 'HKLM:\SOFTWARE\Policies\Microsoft\Dsh'
New-Item -Path $dsh -Force | Out-Null
Set-ItemProperty -Path $dsh -Name 'AllowNewsAndInterests' -Value 0 -Type DWord

# Faster, quieter desktop for RemoteApp use.
$adv = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced'
New-Item -Path $adv -Force | Out-Null
Set-ItemProperty -Path $adv -Name 'ShowTaskViewButton' -Value 0 -Type DWord
Set-ItemProperty -Path $adv -Name 'TaskbarDa' -Value 0 -Type DWord

Write-Output 'ManifestOS debloat: done'
"#
}

/// A random password for the Windows account, so a manifest never has to carry
/// one. Not cryptographic-grade secrecy — it guards a local loopback RDP service
/// — but unguessable and unique per machine.
fn generated_password() -> String {
    // Cheap entropy without adding a dependency: time + pid, hashed by mixing.
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        ^ (std::process::id() as u128).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut x = n | 1;
    let mut out = String::with_capacity(20);
    for _ in 0..20 {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        out.push(ALPHABET[((x >> 33) as usize) % ALPHABET.len()] as char);
    }
    out
}

/// Stage WinApps' `oem/` files for dockur, and decide whether the guest can
/// actually run a RemoteApp. Pure — unit-tested.
///
/// Windows refuses to run an arbitrary program as a RemoteApp unless
/// `TSAppAllowList\fDisabledAllowList=1`, which WinApps' `oem/RDPApps.reg`
/// sets — applied by dockur running `C:\OEM\install.bat` **during Windows
/// setup**. There is no re-running setup on an installed Windows, so this is
/// decided once, at install, and never again.
///
/// Hence `.remoteapp-enabled` answers *"was the Windows in `storage/` installed
/// with those files"*, which is emphatically not *"do we have those files
/// now"*. Stamping the latter is silent and permanent: `windows-vm-run` reads
/// the stamp to decide whether to offer the reinstall, so a wrong stamp
/// withdraws the only available fix.
///
/// This runs **before** the new compose is written, so `compose.yaml` here is
/// still the one the existing guest was built from.
fn setup_oem_step() -> String {
    // WinApps' oem files are GPL-3.0: copied at runtime from the user's clone,
    // never into this MIT tree. Same boundary as the rest of this module.
    format!(
        r#"src="$HOME/.local/share/manifest-os/winapps/oem"
           if [ -f "$src/RDPApps.reg" ]; then
             cp -f "$src"/*.reg "$src"/*.ps1 "$src"/install.bat "{COMPOSE_DIR}/oem/" 2>/dev/null || true
             # Empty counts as absent: docker recreates the bind-mount
             # directory, and a wipe can only empty it.
             if [ -z "$(ls -A "{COMPOSE_DIR}/storage" 2>/dev/null)" ] || grep -qs '/oem:/oem' "{COMPOSE_DIR}/compose.yaml"; then
               echo '  · guest setup: enabling single-window apps (WinApps oem/RDPApps.reg)'
               touch "{COMPOSE_DIR}/.remoteapp-enabled"
             else
               rm -f "{COMPOSE_DIR}/.remoteapp-enabled"
               echo '  ! this Windows was installed before single-window apps could be'
               echo '    enabled; opening an app will offer to reinstall it with them on'
             fi
           else
             rm -f "{COMPOSE_DIR}/.remoteapp-enabled"
             echo '  ! WinApps oem files not found — Windows will refuse single-window apps' >&2
           fi"#
    )
}

/// Report whether the Windows container is up, for `--link`. Pure — unit-tested.
fn link_container_check_step() -> String {
    format!(
        r#"{docker}
           st=$(dk ps --filter name=WinApps --format '{{{{.Names}}}} {{{{.Status}}}}' 2>/dev/null)
           if [ -n "$st" ]; then
             echo "  · container: $st"
           else
             echo '  ! the Windows container is not running — start it with: manifest windows-vm' >&2
           fi"#,
        docker = docker_fn()
    )
}

/// Run WinApps' own installer to detect the guest's apps. Pure — unit-tested.
///
/// Fully automatic: no wizard, no commands for the user to run. WinApps refuses
/// to install over a previous installation (exit 3), so any prior one is cleared
/// first — uninstall is a no-op when there is nothing there. `--system` first,
/// `--user` as the fallback for machines where the system-wide path isn't
/// available.
fn link_install_step() -> &'static str {
    r#"SETUP="$HOME/.local/share/manifest-os/winapps/setup.sh"
       [ -x "$SETUP" ] || { echo '  ! WinApps source missing — re-run: manifest windows-vm' >&2; exit 1; }
       run() { "$SETUP" "$@" 2>&1 || sg docker -c "'$SETUP' $*" 2>&1; }
       run --system --uninstall >/dev/null 2>&1 || true
       run --user --uninstall  >/dev/null 2>&1 || true
       out=$(run --system); rc=$?
       if [ $rc -ne 0 ]; then out=$(run --user); rc=$?; fi
       printf '%s\n' "$out" | sed 's/^/    /'
       if [ $rc -ne 0 ]; then
         echo >&2
         echo '  ! WinApps could not finish installing.' >&2
         echo '    If the message above mentions docker permissions, log out and back' >&2
         echo '    in once — that is the only thing this cannot do for you.' >&2
         exit 1
       fi"#
}

/// Append our debloat call to WinApps' `install.bat`. Pure — unit-tested.
///
/// Chained onto theirs rather than replacing it: dockur runs exactly one
/// `install.bat`, and theirs is the one that enables RemoteApp.
fn setup_debloat_bat_step() -> String {
    // CRLF, because this is a batch file read by Windows.
    format!(
        r#"b="{COMPOSE_DIR}/oem/install.bat"
           if [ -f "$b" ] && ! grep -qi 'debloat.ps1' "$b"; then
             printf '%s\r\n' 'powershell -NoProfile -ExecutionPolicy Bypass -File %~dp0debloat.ps1 >%~dp0debloat.log 2>&1' >> "$b"
           fi"#
    )
}

/// Write our own `install.bat` when WinApps' clone had none to chain onto.
/// Pure — unit-tested.
fn setup_fallback_bat_step() -> String {
    format!(
        "[ -f \"{COMPOSE_DIR}/oem/install.bat\" ] || printf '%s' \"$(cat <<'EOF'\n{bat}EOF\n)\" > \"{COMPOSE_DIR}/oem/install.bat\"",
        bat = debloat_bat()
    )
}

/// The compose file from a previous setup, if there is one. It is the only
/// record of what the installed guest was actually built with.
fn read_existing_compose() -> Option<String> {
    std::fs::read_to_string(expand(&format!("{COMPOSE_DIR}/compose.yaml"))).ok()
}

/// The Windows account password recorded in a compose file. Pure — unit-tested.
pub fn password_in_compose(compose: &str) -> Option<String> {
    compose
        .lines()
        .find_map(|l| l.trim().strip_prefix("PASSWORD:"))
        .map(|v| v.trim().trim_matches('"').to_string())
        .filter(|p| !p.is_empty())
}

/// Write a root-owned file only when its contents (or executable bit) would
/// actually change.
///
/// Re-running setup must not need a password. `windows-vm-run` re-enters
/// `manifest windows-vm` to heal a half-finished setup, and it does so from a
/// .desktop launcher with no TTY — where a single unnecessary `sudo` aborts
/// everything with "sudo: a terminal is required" and leaves the user with the
/// same broken state they started with.
fn write_root_if_changed(ctx: &Ctx, path: &str, content: &str, executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if !ctx.dry_run {
        let same = std::fs::read_to_string(path).map(|c| c == content).unwrap_or(false);
        let mode_ok = !executable
            || std::fs::metadata(path)
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false);
        if same && mode_ok {
            return Ok(());
        }
    }
    ctx.write_root(path, content)?;
    if executable {
        ctx.sudo("chmod", &["0755", path])?;
    }
    Ok(())
}

/// Expand a leading `$HOME` for the write_user path (which is a real path, not a
/// shell string).
fn expand(p: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) => p.replace("$HOME", &h),
        Err(_) => p.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm() -> WindowsVm {
        WindowsVm::default()
    }

    #[test]
    fn compose_uses_declared_spec_and_kvm() {
        let mut v = vm();
        v.version = Some("10".into());
        v.ram = Some("16G".into());
        v.cpus = Some(8);
        v.disk = Some("128G".into());
        v.username = Some("matt".into());
        let c = compose_yaml(&v, "secret123");
        assert!(c.contains("VERSION: \"10\""), "{c}");
        assert!(c.contains("RAM_SIZE: \"16G\""), "{c}");
        assert!(c.contains("CPU_CORES: \"8\""), "{c}");
        assert!(c.contains("DISK_SIZE: \"128G\""), "{c}");
        assert!(c.contains("USERNAME: \"matt\""), "{c}");
        assert!(c.contains("PASSWORD: \"secret123\""), "{c}");
        // Needs KVM and the RDP port, or apps can't be painted onto the desktop.
        assert!(c.contains("/dev/kvm"), "{c}");
        assert!(c.contains("3389:3389/tcp"), "{c}");
        assert!(c.contains("8006:8006"), "web viewer: {c}");
    }

    #[test]
    fn compose_defaults_are_sane() {
        let c = compose_yaml(&vm(), "p");
        assert!(c.contains("VERSION: \"11\""), "{c}");
        assert!(c.contains("RAM_SIZE: \"4G\""), "{c}");
        assert!(c.contains("CPU_CORES: \"4\""), "{c}");
        assert!(c.contains("USERNAME: \"manifest\""), "{c}");
    }

    #[test]
    fn product_key_and_language_are_optional() {
        // No key: Windows still installs, just unactivated — so no KEY line.
        let c = compose_yaml(&vm(), "p");
        assert!(!c.contains("KEY:"), "no key line when none given: {c}");
        assert!(!c.contains("LANGUAGE:"), "{c}");
        // A blank/whitespace key counts as absent, not as an empty key.
        let mut v = vm();
        v.product_key = Some("   ".into());
        assert!(!compose_yaml(&v, "p").contains("KEY:"), "blank key ignored");
        // A real key is passed through to the unattended install.
        v.product_key = Some("XXXXX-YYYYY-ZZZZZ-11111-22222".into());
        v.language = Some("English".into());
        let c = compose_yaml(&v, "p");
        assert!(c.contains("KEY: \"XXXXX-YYYYY-ZZZZZ-11111-22222\""), "{c}");
        assert!(c.contains("LANGUAGE: \"English\""), "{c}");
    }

    #[test]
    fn compose_still_parses_as_yaml_shape_with_a_key() {
        let mut v = vm();
        v.product_key = Some("AAAAA-BBBBB-CCCCC-DDDDD-EEEEE".into());
        let c = compose_yaml(&v, "p");
        // The KEY line must sit inside `environment:` at the same indent as its
        // siblings, or compose rejects the file.
        let env_indent = c.lines().find(|l| l.contains("VERSION:")).map(|l| l.len() - l.trim_start().len());
        let key_indent = c.lines().find(|l| l.contains("KEY:")).map(|l| l.len() - l.trim_start().len());
        assert_eq!(env_indent, key_indent, "KEY must align with VERSION:
{c}");
    }

    /// WinApps hardcodes BOTH of these -- `readonly CONTAINER_NAME="WinApps"`
    /// and `readonly COMPOSE_PATH="${HOME}/.config/winapps/compose.yaml"`.
    /// Neither is configurable, and getting either wrong makes it exit
    /// "no such object: WinApps" before it ever attempts RDP.
    #[test]
    fn container_and_compose_match_what_winapps_hardcodes() {
        let c = compose_yaml(&vm(), "p");
        assert!(c.contains("container_name: WinApps"), "winapps only inspects this name: {c}");
        let s = vm_run_script();
        // A setup made before we knew this must repair itself -- compose.yaml is
        // written once at setup time, so a package upgrade never touches it.
        assert!(s.contains("container_name: WinApps"), "detects the stale compose: {s}");
        assert!(s.contains("dk rm manifest-windows"), "retires the old container: {s}");
        assert!(s.contains(".config/winapps/compose.yaml"), "puts compose where winapps looks: {s}");
    }

    #[test]
    fn debloat_is_on_by_default_and_optional() {
        // Default on: this VM runs one app, so the consumer extras are overhead.
        assert!(compose_yaml(&vm(), "p").contains("/oem:/oem"), "oem hook mounted by default");
        let mut v = vm();
        v.debloat = Some(false);
        assert!(!compose_yaml(&v, "p").contains("/oem:/oem"), "opt-out leaves Windows stock");
        // Volumes must be ABSOLUTE: the same compose file is written to two
        // directories (ours and the one WinApps hardcodes), and a relative path
        // would resolve differently in each -- two storage dirs, two Windows
        // installs, neither of them the one the user set up.
        let c = compose_yaml(&vm(), "p");
        assert!(!c.contains("- ./"), "relative volumes split the disk in two: {c}");
        assert!(c.contains("$HOME/.local/share/manifest-os/windows-vm/storage:/storage"), "{c}");
    }

    #[test]
    fn debloat_is_conservative_not_os_surgery() {
        let ps = debloat_ps1();
        // Removes apps and flips policy — nothing that breaks servicing.
        assert!(ps.contains("Remove-AppxPackage"), "{ps}");
        assert!(ps.contains("DisableWindowsConsumerFeatures"), "{ps}");
        assert!(ps.contains("AllowTelemetry"), "{ps}");
        // Must NOT disable Windows Update or strip components: a VM that can't
        // patch itself is a worse outcome than a bloated one.
        assert!(!ps.to_lowercase().contains("wuauserv"), "must not touch Windows Update: {ps}");
        assert!(!ps.to_lowercase().contains("remove-windowspackage"), "no component removal: {ps}");
        // The batch shim hands off to it.
        assert!(debloat_bat().contains("debloat.ps1"), "{}", debloat_bat());
    }

    #[test]
    fn winapps_conf_matches_the_backend() {
        let mut v = vm();
        assert!(winapps_conf(&v, "p").contains("WAFLAVOR=\"docker\""));
        v.backend = Some("podman".into());
        assert!(winapps_conf(&v, "p").contains("WAFLAVOR=\"podman\""));
        v.backend = Some("libvirt".into());
        assert!(winapps_conf(&v, "p").contains("WAFLAVOR=\"libvirt\""));
    }

    #[test]
    fn winapps_conf_carries_credentials_and_freerdp() {
        let c = winapps_conf(&vm(), "hunter2");
        assert!(c.contains("RDP_USER=\"manifest\""), "{c}");
        assert!(c.contains("RDP_PASS=\"hunter2\""), "{c}");
        assert!(c.contains("RDP_IP=\"127.0.0.1\""), "{c}");
        // A wrapper, because winapps discards FreeRDP's output entirely.
        assert!(c.contains("FREERDP_COMMAND=\"manifest-freerdp\""), "{c}");
        let w = freerdp_wrapper();
        assert!(w.contains("xfreerdp3"), "wrapper resolves a real binary: {w}");
        assert!(w.contains("exec 2>>\"$LOG\""), "wrapper must keep stderr: {w}");
    }

    #[test]
    fn vm_run_is_lazy_like_android() {
        let s = vm_run_script();
        // First use sets the VM up; after that it only STARTS it on demand.
        assert!(s.contains("isn't set up yet"), "{s}");
        assert!(s.contains("manifest windows-vm"), "first-run setup: {s}");
        assert!(s.contains("dk start WinApps"), "lazy start: {s}");
        // Readiness uses the image's healthcheck, NOT a TCP probe of 3389 —
        // docker publishes that port before Windows is listening on it.
        assert!(s.contains("Waiting for Windows to be ready"), "{s}");
        assert!(s.contains(".State.Health.Status"), "health-based readiness: {s}");
        assert!(!s.contains("/dev/tcp/127.0.0.1/3389"), "TCP probe is a false positive: {s}");
        // An image that reports NO health must not block us — that produced a
        // "still installing" message while Windows was visibly running.
        assert!(s.contains(r#""<no value>") ;;"#), "empty health must not block: {s}");
        // If the launch does fail, explain the real causes (install still
        // running, RDP not up yet, docker group) rather than one guess.
        assert!(s.contains("still installing"), "{s}");
        assert!(s.contains("http://localhost:8006"), "points at the viewer: {s}");
        // Records activity so the idle watchdog can tell it's in use.
        assert!(s.contains("windows-vm-activity"), "{s}");
        // The share path must interpolate the real filename: `\$base` inside
        // double quotes prints a literal "$base", so the value is concatenated
        // outside the quoted literal instead.
        assert!(s.contains(r#"WINPATH="$p$base""#), "filename must expand: {s}");
        assert!(!s.contains(r#"Transfer\$base""#), "an escaped $ would print literally: {s}");
        // Both routes $HOME takes into the guest are tried: FreeRDP's own
        // redirect, and the drive dockur maps our /shared volume to.
        assert!(s.contains(r"'\\tsclient\home\Windows Transfer\'"), "{s}");
        assert!(s.contains(r"'Z:\Windows Transfer\'"), "{s}");
        // winapps talks to docker itself, so it needs the group bridge too.
        assert!(s.contains("sg docker -c \"'$WA' $*\""), "{s}");
        // The installer must open as a RemoteApp — its own window — not as a
        // Windows desktop in a browser tab.
        assert!(s.contains("run_wa manual \"$WINPATH\""), "runs the exe as a native window: {s}");
        // Opening a desktop is only the fallback.
        let manual_at = s.find("run_wa manual").expect("manual launch");
        let desktop_at = s.find("run_wa windows").expect("desktop fallback");
        assert!(manual_at < desktop_at, "RemoteApp must be tried before the desktop:\n{s}");
        // After installing, newly-installed apps get added to the launcher.
        assert!(s.contains("manifest windows-vm --link"), "re-detects apps: {s}");
        // winapps exits 0 even when nothing painted, so success must be judged
        // by how long the session lasted -- never by exit status alone.
        assert!(s.contains("t1 - t0"), "launch success is timed, not assumed: {s}");
        assert!(!s.contains("Installer finished."), "must not claim success blindly: {s}");
        // A portable .exe installs nothing, so it gets a launcher of its own
        // rather than a cheerful message about an app that isn't there.
        assert!(s.contains("manifest-winvm-$slug.desktop"), "portable apps get a launcher: {s}");
        assert!(s.contains(r#"Exec=windows-vm-run \"$HOME/Windows Transfer/$base\""#), "{s}");
        // And when apps DO get installed, they are named, not merely implied.
        assert!(s.contains("comm -13"), "reports the actual delta: {s}");
        // WinApps writes "<exe>.desktop" with NO prefix of its own, so the scan
        // has to match file CONTENT. Keying off the filename finds nothing and
        // every real install looks like "nothing was installed".
        assert!(s.contains("--include='*.desktop'"), "delta matches content: {s}");
        assert!(!s.contains("grep -i 'winapps'"), "filename matching is wrong here: {s}");
        // POSIX sh: no process substitution (this runs under #!/bin/sh).
        assert!(!s.contains("<("), "process substitution is not POSIX sh: {s}");
        // The RDP alternate shell is a dead end here: Windows client editions
        // are single-session and dockur signs the user in at the console, so an
        // RDP connect TAKES OVER that session and /shell: is ignored -- you get
        // a plain desktop that also looks "successful" to a duration check.
        assert!(!s.contains("/shell:\""), "alternate shell cannot work on this stack: {s}");
        // Erasing a Windows install must never happen without being asked.
        let rm_at = s.find("wipe_storage \"$VMDIR/storage\"").expect("reinstall path");
        let prompt_at = s.find("Reinstall Windows now?").expect("prompt");
        assert!(prompt_at < rm_at, "must confirm BEFORE deleting the install:
{s}");
        assert!(s.contains("[ -t 0 ]"), "never prompt where there is no terminal: {s}");
        // A guest without the registry key cannot run a RemoteApp, but it CAN
        // serve the console session -- a full Windows desktop that outlives the
        // 5-second check and reports success. Verified on real hardware: the
        // desktop appears with an "Another user is signed in" prompt, because
        // dockur is already signed in at the console. So the attempt has to be
        // gated on the stamp, not merely explained after the fact.
        let gate = s.find("if [ -f \"$VMDIR/.remoteapp-enabled\" ]; then").expect("gate");
        let attempt = s.find("run_wa manual \"$WINPATH\"").expect("attempt");
        assert!(gate < attempt, "must not attempt what the guest cannot do:\n{s}");
        // dockur creates storage/ as root:root, so a plain `rm -rf` deletes
        // NOTHING -- and with the error swallowed the reinstall looks like it
        // worked while booting the very same Windows. It must be wiped with
        // docker's privilege, and the result must be checked.
        assert!(s.contains("dk run --rm -v \"$1:/wipe\""), "wipe with docker's privilege: {s}");
        assert!(
            s.contains("wipe_storage \"$VMDIR/storage\" || {"),
            "a failed wipe must abort, not pretend it reinstalled: {s}"
        );
    }

    /// Every generated script here is something a .desktop launcher Execs, and
    /// a launcher has no TTY: an interactive `sudo` prompt there waits forever
    /// with nothing on screen. This is the exact bug that made the Waydroid
    /// launchers fail silently, so pin it down for the Windows tier too.
    #[test]
    fn generated_scripts_never_prompt_for_a_password() {
        for (what, s) in [("vm_run", vm_run_script()), ("vm_idle", vm_idle_script(45))] {
            for line in s.lines() {
                let t = line.trim_start();
                if t.starts_with('#') || t.starts_with("//") {
                    continue;
                }
                if t.contains("sudo ") {
                    assert!(
                        t.contains("sudo -n "),
                        "{what}: sudo must be non-interactive (-n), got: {line}"
                    );
                }
            }
        }
    }

    #[test]
    fn vm_idle_stops_only_when_unused() {
        let s = vm_idle_script(30);
        assert!(s.contains("IDLE=1800"), "30 min: {s}");
        // A live FreeRDP client means an app is on screen — must not stop.
        assert!(s.contains("pgrep -x xfreerdp"), "{s}");
        assert!(s.contains("dk stop WinApps"), "{s}");
        // 0 disables auto-stop entirely.
        let z = vm_idle_script(0);
        assert!(z.contains("IDLE=0") && z.contains("[ \"$IDLE\" -eq 0 ] && exit 0"), "{z}");
        // Installing Windows takes 20-40 min with no client attached and nothing
        // touching the activity file -- this watchdog's exact definition of
        // idle. It killed a running install once, 16 minutes in. `manifest
        // windows-vm` promises the install "needs no input"; it must not need
        // babysitting either.
        let guard = s.find("windows.boot").expect("install guard");
        let stop = s.find("dk stop WinApps").expect("stop");
        assert!(guard < stop, "must not stop mid-install:\n{s}");
        // And idle means "up and unused for IDLE", not "the last app closed a
        // while ago" -- a container started just now is new, however stale the
        // activity file is, or a fresh start is instantly eligible to be killed.
        assert!(s.contains("{{.State.StartedAt}}"), "count from the container start too: {s}");
        assert!(s.contains("[ \"$started\" -gt \"$last\" ] && last=$started"), "{s}");
    }

    #[test]
    fn generated_passwords_are_long_and_vary() {
        let a = generated_password();
        assert_eq!(a.len(), 20, "{a}");
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()), "{a}");
        // Two calls in the same process must not collide (pid is constant, so the
        // time component has to be doing work).
        let b = generated_password();
        assert_ne!(a, b, "passwords must not repeat");
    }

    /// The guest's password is written into Windows while it installs, and
    /// nothing out here can change it afterwards. A re-run that mints a fresh
    /// one doesn't rotate anything — it locks us out of the guest we already
    /// have, and FreeRDP's only symptom is a connection reset with no reason
    /// attached. So an existing setup's password has to be recoverable.
    #[test]
    fn an_existing_setups_password_is_read_back_from_its_compose() {
        let c = compose_yaml(&vm(), "hunter2");
        assert_eq!(password_in_compose(&c).as_deref(), Some("hunter2"), "{c}");
        // A round trip has to survive the exact file we write, quotes and all.
        let generated = generated_password();
        let c = compose_yaml(&vm(), &generated);
        assert_eq!(password_in_compose(&c).as_deref(), Some(generated.as_str()));
        // Nothing to reuse is a perfectly good answer — that's a first run.
        assert_eq!(password_in_compose(""), None);
        assert_eq!(password_in_compose("services:\n  windows:\n"), None);
        assert_eq!(password_in_compose("      PASSWORD: \"\"\n"), None);
        // USERNAME must not be mistaken for it.
        assert_eq!(password_in_compose("      USERNAME: \"manifest\"\n"), None);
    }

    /// Every shell fragment this module hands to `sh -c` must at least *parse*.
    ///
    /// The release loop pipes `manifest __script <name>` through `sh -n`, but
    /// that only ever covered the scripts `__script` exposes — the fragments
    /// below go straight to `ctx.shell()` and were checked by nothing. One of
    /// them ended `then : fi`, with `fi` separated from `:` by spaces alone, so
    /// `fi` parsed as an argument and the `if` was never closed. Being a *parse*
    /// error it fired whichever branch would have been taken, aborting
    /// `manifest windows-vm` for every user, one step before it wrote
    /// compose.yaml — leaving a guest built from whatever compose came before.
    #[test]
    fn every_generated_shell_fragment_parses() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        for (what, fragment) in [
            ("oem step", setup_oem_step()),
            ("debloat bat step", setup_debloat_bat_step()),
            ("fallback bat step", setup_fallback_bat_step()),
            ("link container check", link_container_check_step()),
            ("link install step", link_install_step().to_string()),
            ("ensure winapps", ensure_winapps_step().to_string()),
            // These are whole scripts. `windows-vm-run` is reachable via
            // `__script`, so the release loop can pipe it through `sh -n` by
            // hand -- the other two are written as real files and never were.
            ("windows-vm-run", vm_run_script()),
            ("windows-vm-idle", vm_idle_script(30)),
            ("manifest-freerdp", freerdp_wrapper().to_string()),
        ] {
            let mut sh = Command::new("sh")
                .arg("-n")
                .stdin(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn sh");
            sh.stdin.take().unwrap().write_all(fragment.as_bytes()).unwrap();
            let out = sh.wait_with_output().expect("run sh -n");
            assert!(
                out.status.success(),
                "{what} is not valid sh:\n{}\n--- fragment ---\n{fragment}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// The stamp answers "was the Windows in storage/ INSTALLED with
    /// RDPApps.reg", which is not the same question as "do we have that file
    /// now". Getting it wrong is silent: windows-vm-run stops offering the
    /// reinstall that is the only way to fix an older guest.
    #[test]
    fn the_remoteapp_stamp_describes_the_guest_not_the_clone() {
        let s = setup_oem_step();
        // Decided from the guest's own evidence: an empty/absent disk (nothing
        // installed yet) or a compose that already mounted /oem.
        assert!(s.contains("ls -A"), "an empty storage dir counts as no guest: {s}");
        assert!(s.contains("grep -qs '/oem:/oem'"), "check what built the guest: {s}");
        // And it must actively clear a stamp it can't justify, not just skip it.
        assert!(s.contains("rm -f"), "an unjustified stamp must be removed: {s}");
        assert!(s.contains("touch"), "{s}");
    }
}
