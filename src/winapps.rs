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
/// The idle timeout, kept as data so the watchdog script can stay a thin stub.
const IDLE_CONF: &str = "/etc/manifest-os/windows-vm.conf";

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
    // Only before the first install: the size becomes the partition layout, and
    // changing it afterwards means evicting the Recovery partition Windows puts
    // behind C:. Asking again later would imply a choice that isn't free.
    if vm.disk.is_none() && !guest_disk_exists() {
        if let Some(d) = prompt_disk_size(vm.disk()) {
            vm.disk = Some(d);
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
    // The timeout is data; the script is a stub. Anything written as a real
    // file at setup time is stale forever -- `pacman -Syu` regenerates stubs,
    // never files -- and a watchdog that stops a running Windows install is
    // exactly the kind of fix that has to reach people who already ran setup.
    write_root_if_changed(
        ctx,
        IDLE_CONF,
        &format!("# Generated by ManifestOS. Minutes of disuse before the Windows VM stops;\n\
                  # 0 never stops it. Edit the manifest's windows.vm.idle_minutes.\n\
                  IDLE_MINUTES={idle}\n"),
        false,
    )?;
    write_root_if_changed(
        ctx,
        "/usr/local/bin/windows-vm-idle",
        &crate::android::thin_stub("windows-vm-idle"),
        true,
    )?;
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
        // Last, so it appends to whichever install.bat the steps above settled
        // on. NOT gated on `debloat` -- a single-window app depends on this.
        println!(
            "  · guest setup: turning off Windows' automatic sign-in, so apps can open \
             in their own windows"
        );
        println!(
            "    (this is why http://localhost:8006 shows a lock screen — sign in there \
             if you want the full desktop)"
        );
        ctx.shell(&setup_autologon_step(), false)?;
        println!(
            "  · guest setup: allowing apps to launch from your home folder (Windows blocks \
             programs on a network drive, and the prompt is fatal to a single-window app)"
        );
        ctx.shell(&setup_guest_policy_step(), false)?;
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
        // Deliberately does NOT touch the activity file. The watchdog already
        // counts from the container's start, and an activity file newer than
        // that start is exactly how it knows something has *used* the VM --
        // which is what tells an install apart from an idle desktop.
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
        // zenity is what makes opening a file show a progress window instead of
        // a terminal (see android::gui_install_script). GTK4, so it matches the
        // rest of the desktop.
        "freerdp", "iproute2", "libnotify", "git", "dialog", "gawk", "curl", "openbsd-netcat",
        "zenity",
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
         # A reinstalled Windows generates a NEW RDP certificate, and FreeRDP
         # pins the old one. Our own launches pass /cert:ignore, but WinApps'
         # connection test uses /cert:tofu, which refuses a *changed* key --
         # \"NEW HOST IDENTIFICATION\" then ERRCONNECT_TLS_CONNECT_FAILED, with
         # nothing pointing at the stale pin as the cause.
         rm -f \"${XDG_CONFIG_HOME:-$HOME/.config}\"/freerdp/server/127.0.0.1_3389.pem 2>/dev/null || true
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

# 3. Wait until Windows is actually serving RDP.
#
#    A plain TCP connect to 3389 proves nothing: docker publishes that port when
#    the CONTAINER starts, so it answers throughout the ~40 minutes Windows is
#    installing. Ask the RDP server to identify itself instead -- an X.224
#    Connection Request gets a Connection Confirm (starts 03 00) only from a real
#    RDP server, and nothing at all from docker's proxy. The image may not define
#    a healthcheck at all, so this is the only dependable readiness signal here.
rdp_ready() {{
  printf '\003\000\000\023\016\340\000\000\000\000\000\001\000\010\000\003\000\000\000' \
    | timeout 5 nc 127.0.0.1 3389 2>/dev/null | head -c 2 | od -An -tx1 | tr -d ' \n' \
    | grep -q '^0300'
}}
if ! rdp_ready; then
  echo "Waiting for Windows to be ready..."
  i=0
  while [ $i -lt 60 ]; do            # ~2 minutes, then try anyway
    rdp_ready && break
    i=$((i+1)); sleep 2
  done
fi

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
# Two ways your home reaches the guest. Z: FIRST, and the order matters:
#   Z:               dockur mounts our /shared volume ($HOME) as a drive inside
#                    the guest. Always there, needs nothing from the client.
#                    This is the one that works -- verified painting a real
#                    RAIL window on real hardware.
#   \\tsclient\home  FreeRDP's own redirection of $HOME into the session. Only
#                    exists if the client enables drive redirection, and we do
#                    not pass `+home-drive`, so the path does not resolve, the
#                    RemoteApp never starts, and no window is ever created.
#                    Kept as a fallback for a guest without the /shared mount.
# Getting this order wrong is invisible: the failing attempt still holds the
# connection open for ~20s, which the duration check below reads as success, so
# the working path is never reached.
echo
echo "Opening $base in Windows..."
: > "$WALOG" 2>/dev/null || true
APPSBEFORE=$(mktemp) && list_apps > "$APPSBEFORE"
# Where the file lands inside Windows. Set up front, not in the loop below:
# the loop is skipped on a guest that can't do RemoteApp, and the desktop
# fallback still has to tell the user where to find their installer.
WINPATH='Z:\Windows Transfer\'"$base"
opened=0

# Z: is a network location to Windows, so launching an .exe from it raises
# "Open File - Security Warning". As a RemoteApp that is fatal twice: the app
# never starts, and the modal stays parked in the guest's session -- and since
# RAIL surfaces every top-level window, the NEXT launch of any app shows that
# stale dialog instead. Setup writes this into C:\OEM\install.bat for new
# guests; a guest installed before that never ran it, so apply it once here.
# Costs one short RDP round-trip, exactly once per guest.
POLICY_STAMP="$VMDIR/.guest-policy-set"
WACONF="$HOME/.config/winapps/winapps.conf"
if [ ! -f "$POLICY_STAMP" ] && [ -r "$WACONF" ]; then
  RDP_USER=""; RDP_PASS=""; FREERDP_COMMAND=""
  . "$WACONF" 2>/dev/null || true
  if [ -n "$RDP_USER" ]; then
    timeout 60 "${{FREERDP_COMMAND:-xfreerdp3}}" /cert:ignore /u:"$RDP_USER" /p:"$RDP_PASS" \
      /v:127.0.0.1:3389 {GUEST_POLICY_CMD} >/dev/null 2>&1
  fi
  # Best-effort: if it fails the prompts may still appear, which is visible and
  # recoverable. Never block the launch on it.
  touch "$POLICY_STAMP" 2>/dev/null || true
fi

# Growing the disk is two steps, and only the first is dockur's. It enlarges the
# image when DISK_SIZE goes up, but Windows then just has unallocated space
# behind C: -- the volume itself is untouched, so the guest still reports the
# old size and "not enough storage" persists after an apparently successful
# resize. Extend it here, once per size change.
WANTED=$(sed -n 's/.*DISK_SIZE: *"\([^"]*\)".*/\1/p' "$VMDIR/compose.yaml" 2>/dev/null)
DISK_STAMP="$VMDIR/.disk-extended"
# Written into the shared home so the guest can run it as a file -- quoting a
# script this size through /app:program:...,cmd: is unreadable and fragile.
GROW_PS1=".manifest-grow-disk.ps1"
cat > "$HOME/$GROW_PS1" <<'GROWEOF'
$ErrorActionPreference = 'SilentlyContinue'
$c = Get-Partition -DriveLetter C
# Windows lays the disk out [EFI][MSR][C:][Recovery], so the space added at the
# END sits behind the Recovery partition and C: cannot reach it -- extending
# reports success while changing nothing. Recovery only holds WinRE, which a
# disposable container guest never boots into, so retire it and reclaim it.
$rec = Get-Partition -DiskNumber $c.DiskNumber |
       Where-Object {{ $_.Offset -gt $c.Offset -and $_.Type -eq 'Recovery' }}
if ($rec) {{
    reagentc /disable | Out-Null      # moves WinRE.wim onto C: first
    foreach ($r in $rec) {{
        Remove-Partition -DiskNumber $c.DiskNumber -PartitionNumber $r.PartitionNumber -Confirm:$false
    }}
}}
$max = (Get-PartitionSupportedSize -DriveLetter C).SizeMax
if ($max -gt (Get-Partition -DriveLetter C).Size) {{
    Resize-Partition -DriveLetter C -Size $max
}}
GROWEOF
if [ -n "$WANTED" ] && [ "$WANTED" != "$(cat "$DISK_STAMP" 2>/dev/null)" ] && [ -r "$WACONF" ]; then
  RDP_USER=""; RDP_PASS=""; FREERDP_COMMAND=""
  . "$WACONF" 2>/dev/null || true
  if [ -n "$RDP_USER" ]; then
    echo "Making the extra disk space usable inside Windows..."
    timeout 120 "${{FREERDP_COMMAND:-xfreerdp3}}" /cert:ignore /u:"$RDP_USER" /p:"$RDP_PASS" \
      /v:127.0.0.1:3389 "$(printf '/app:program:C:\\Windows\\System32\\cmd.exe,cmd:/C powershell -NoProfile -ExecutionPolicy Bypass -File Z:\\%s & tsdiscon' "$GROW_PS1")" \
      >/dev/null 2>&1
  fi
  # Record what we tried, so a guest that cannot grow doesn't retry every launch.
  printf '%s' "$WANTED" > "$DISK_STAMP" 2>/dev/null || true
fi
# Only a guest INSTALLED with TSAppAllowList\fDisabledAllowList=1 can run an
# arbitrary program as a RemoteApp. Without it the connection still succeeds --
# Windows just serves the console session instead, so you get a full desktop,
# plus an "Another user is signed in" prompt because dockur is already signed
# in there. That sits on screen well past any duration check, so attempting it
# anyway does not produce evidence: it produces a false success, a stray
# Windows desktop, and a launcher for an app nobody installed. Don't attempt
# what the guest cannot do -- the reinstall offer below is the actual fix.
if [ -f "$VMDIR/.remoteapp-enabled" ]; then
  for p in 'Z:\Windows Transfer\' '\\tsclient\home\Windows Transfer\'; do
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
pub fn vm_idle_script() -> String {
    format!(
        r####"#!/bin/sh
# ManifestOS — stop the Windows VM when idle (generated; do not edit).
# The timeout is DATA, read at runtime, not a number baked in when this file was
# written. This script is delivered as a thin stub regenerated by `pacman -Syu`
# (see android::thin_stub); baking the value would mean a fix to the logic here
# ships to nobody, because the file on disk never changes again.
IDLE_MINUTES=$([ -r {IDLE_CONF} ] && . {IDLE_CONF} 2>/dev/null; echo "${{IDLE_MINUTES:-30}}")
IDLE=$((IDLE_MINUTES * 60))
[ "$IDLE" -eq 0 ] && exit 0   # 0 = never auto-stop
{docker}
state=$(dk inspect -f '{{{{.State.Running}}}}' WinApps 2>/dev/null || echo missing)
[ "$state" = "true" ] || exit 0    # not running, nothing to do
# A live FreeRDP client means an app is on screen — keep it up.
if pgrep -x xfreerdp >/dev/null 2>&1 || pgrep -x xfreerdp3 >/dev/null 2>&1; then
  : > "${{XDG_STATE_HOME:-$HOME/.local/state}}/windows-vm-activity" 2>/dev/null || true
  exit 0
fi
ACT="${{XDG_STATE_HOME:-$HOME/.local/state}}/windows-vm-activity"
now=$(date +%s)
act=$([ -e "$ACT" ] && stat -c %Y "$ACT" 2>/dev/null || echo 0)
[ "$act" -eq 0 ] && {{ mkdir -p "$(dirname "$ACT")"; : > "$ACT"; exit 0; }}
# Idle means "up and unused for IDLE", not "the last app closed a while ago".
# A container started just now after a long gap is brand new, not stale: count
# from whichever is later, or a fresh start is instantly eligible to be killed.
started=$(dk inspect -f '{{{{.State.StartedAt}}}}' WinApps 2>/dev/null)
started=$(date -d "$started" +%s 2>/dev/null || echo 0)
{decide}
if should_stop "$now" "$act" "$started" "$IDLE"; then
  echo "windows-vm-idle: stopping the idle Windows VM"
  dk stop WinApps >/dev/null 2>&1 || true
fi
"####,
        IDLE_CONF = IDLE_CONF,
        docker = docker_fn(),
        decide = idle_decision_fn(),
    )
}

/// The idle watchdog's whole decision, as a shell function — kept separate so a
/// test can drive it with made-up clocks instead of a real container.
///
/// This has been got wrong twice, in both directions, and each mistake is
/// invisible until it costs someone 40 minutes:
///   * counting only from the activity file killed a Windows install 16 minutes
///     in, because an install touches nothing and looks exactly like idle;
///   * then guarding on a "still installing" signal that was always true
///     disabled the watchdog entirely.
///
/// So the rule is about *use*, not about installing: a VM that nothing has
/// touched since it started is either installing or freshly up, and either way
/// stopping it is wrong. `windows-vm-run` touches the activity file when it
/// launches something, so the moment the VM is genuinely used the normal idle
/// timeout takes over again. The ceiling stops a guest that never comes up from
/// pinning the VM on for good, while staying far beyond any real install.
fn idle_decision_fn() -> &'static str {
    "# should_stop <now> <activity> <container-started> <idle-secs>
should_stop() {
    _now=$1; _act=$2; _started=$3; _idle=$4
    # Idle means \"up and unused for IDLE\", not \"the last app closed a while
    # ago\": a container started just now after a long gap is brand new, not
    # stale, so count from whichever is later.
    _last=$_act
    [ \"$_started\" -gt \"$_last\" ] && _last=$_started
    # Nothing has used it since it started -> installing, or only just up.
    if [ \"$_act\" -lt \"$_started\" ] && [ $((_now - _started)) -lt 7200 ]; then
        return 1
    fi
    [ $((_now - _last)) -ge \"$_idle\" ]
}
"
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
/// Whether a Windows disk already exists. Empty counts as absent — docker
/// recreates the bind-mount directory, and a wipe can only empty it.
fn guest_disk_exists() -> bool {
    std::fs::read_dir(expand(&format!("{COMPOSE_DIR}/storage")))
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
}

/// Free bytes on the filesystem that will hold the guest's disk.
fn free_bytes_for_storage() -> Option<u64> {
    let dir = expand(COMPOSE_DIR);
    // The directory may not exist yet on a first run; ask about its parent.
    let probe = if std::path::Path::new(&dir).exists() { dir } else { expand("$HOME") };
    let out = std::process::Command::new("df")
        .args(["-B1", "--output=avail", &probe])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).lines().nth(1)?.trim().parse().ok()
}

/// Ask how big the guest's disk should be, before Windows installs.
///
/// Only asked once, when there is no guest yet: the size is written into the
/// partition layout at install time, and growing it afterwards means removing
/// the Recovery partition Windows puts *after* C: (see `windows-vm-run`). Much
/// better to be right the first time.
///
/// The image is sparse — it occupies what Windows has written, not what it was
/// declared as — so a generous number is nearly free. What it is *not* free of
/// is the host filling up later: this is one file that can grow to the declared
/// size, and if the filesystem runs out underneath it the guest gets I/O errors
/// mid-write. That is worth care on a machine dual-booting Windows, where this
/// Linux partition is only a slice of the disk and the free space here is not
/// the free space on the drive.
fn prompt_disk_size(default: &str) -> Option<String> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return None;
    }
    const GB: u64 = 1024 * 1024 * 1024;
    let free = free_bytes_for_storage();
    println!();
    println!("  How much disk should Windows get?");
    match free {
        Some(b) => println!("    · {} GB free on this filesystem right now.", b / GB),
        None => println!("    · (couldn't read the free space on this filesystem)"),
    }
    println!("    · The disk file is sparse: it only takes up what Windows actually");
    println!("      writes, so a large number costs nothing today.");
    println!("    · But it can grow to the full size later. Leave room — especially if");
    println!("      this machine dual-boots and Linux only has part of the drive.");
    println!("    · Windows itself uses ~25 GB. Growing it later is possible but");
    println!("      awkward, so pick generously now.");
    print!("  Size (e.g. 128G, or press Enter for {default}): ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return None;
    }
    let want = line.trim();
    if want.is_empty() {
        return None;
    }
    let parsed = parse_size_gb(want)?;
    if let Some(b) = free {
        let free_gb = b / GB;
        // Warn, don't refuse: over-committing is legitimate on a big filesystem
        // that will never actually be filled, and it is the user's disk.
        if parsed > free_gb {
            println!(
                "  ! {parsed} GB is more than the {free_gb} GB free here. That is allowed \
                 (the file is sparse), but if Windows ever fills it the host runs out first."
            );
        }
    }
    Some(want.to_string())
}

/// Parse a `128G` / `128` / `1T` size into whole GB. Pure — unit-tested.
fn parse_size_gb(s: &str) -> Option<u64> {
    let t = s.trim();
    let (num, mult) = match t.chars().last()? {
        'g' | 'G' => (&t[..t.len() - 1], 1),
        't' | 'T' => (&t[..t.len() - 1], 1024),
        c if c.is_ascii_digit() => (t, 1),
        _ => return None,
    };
    num.trim().parse::<u64>().ok().filter(|n| *n > 0).map(|n| n * mult)
}

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

/// Turn off Windows' automatic sign-in, by appending to `C:\OEM\install.bat`.
/// Pure — unit-tested.
///
/// **This is what makes a RemoteApp possible at all.** Windows client editions
/// allow one interactive session, and dockur's answer file autologons the same
/// user at the console (`<AutoLogon>`, `LogonCount` 65432). So a RemoteApp
/// request arrives at a session that is already taken: Windows serves the
/// existing desktop and an *"Another user is signed in"* prompt, nobody answers
/// it, and ~30 s later the connection ends `ERRINFO_LOGOFF_BY_USER`. Verified on
/// real hardware — `RDPApps.reg` alone does **not** fix this, which is what five
/// release cycles assumed.
///
/// dockur runs `install.bat` as a **FirstLogonCommand**, i.e. inside that very
/// session, so this takes effect from the guest's *next* boot — the first
/// restart after installation is what frees the console.
///
/// `DefaultUserName` is deliberately left alone: it prefills the name on the
/// lock screen, which costs nothing and is friendlier.
fn setup_autologon_step() -> String {
    // CRLF: this is a batch file read by Windows. `>nul 2>&1` because `reg
    // delete` of a value that isn't there is a non-zero exit, not a problem.
    let winlogon = r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon";
    format!(
        r#"b="{COMPOSE_DIR}/oem/install.bat"
           if ! grep -qi 'AutoAdminLogon' "$b" 2>/dev/null; then
             {{
               printf '%s\r\n' 'reg add "{winlogon}" /v AutoAdminLogon /t REG_SZ /d 0 /f >nul 2>&1'
               printf '%s\r\n' 'reg delete "{winlogon}" /v DefaultPassword /f >nul 2>&1'
               printf '%s\r\n' 'reg delete "{winlogon}" /v AutoLogonCount /f >nul 2>&1'
             }} >> "$b"
           fi"#
    )
}

/// Stop Windows blocking every app we launch, by appending to
/// `C:\OEM\install.bat`. Pure — unit-tested.
///
/// We hand the guest a path on `Z:` (dockur's `/shared`, i.e. the user's own
/// home). Windows treats that as a network location, so launching an `.exe`
/// from it raises **"Open File - Security Warning"** — and as a RemoteApp that
/// is fatal twice over: the app never starts, *and* the modal stays parked in
/// the guest's session. They accumulate, and because RAIL surfaces every
/// top-level window, the next launch of *any* app shows the old dialog instead
/// of what you asked for. Observed: launching Notepad produced a stale
/// "Open File - Security Warning" from an earlier attempt.
///
/// Zone 3 policy `1806` is "launching applications and unsafe files". Turning
/// the prompt off is a deliberate narrowing of a Windows security control, and
/// it is the right call *here*: the only network location involved is the
/// user's own `$HOME`, they picked the file themselves on the Linux side, and
/// the guest is a disposable container. It is not a general-purpose Windows.
fn setup_guest_policy_step() -> String {
    format!(
        r#"b="{COMPOSE_DIR}/oem/install.bat"
           if ! grep -qi 'Internet Settings' "$b" 2>/dev/null; then
             {{
{GUEST_POLICY_BAT}
             }} >> "$b"
           fi"#
    )
}

/// Registry policy for the guest, as `printf` lines for `install.bat`.
///
/// `1806` — see [`setup_guest_policy_step`].
///
/// `MaxDisconnectionTime` + `fResetBroken` are what stop *"Another user is
/// signed in"*. A RemoteApp session does **not** end when its program exits, it
/// merely disconnects — and with no disconnection limit set (Windows' default)
/// it lingers forever. They pile up, and a later connection collides with one,
/// which is the prompt. Windows client editions allow a single interactive
/// session, so there is no headroom to absorb the leak. 30 s after the last
/// client for a session goes away, log it off; the next launch then gets a
/// clean session instead of a fight over a stale one.
const GUEST_POLICY_BAT: &str = concat!(
    "               printf '%s\\r\\n' 'reg add \"HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings\\Zones\\3\" /v 1806 /t REG_DWORD /d 0 /f >nul 2>&1'\n",
    "               printf '%s\\r\\n' 'reg add \"HKLM\\SOFTWARE\\Policies\\Microsoft\\Windows NT\\Terminal Services\" /v MaxDisconnectionTime /t REG_DWORD /d 30000 /f >nul 2>&1'\n",
    "               printf '%s\\r\\n' 'reg add \"HKLM\\SOFTWARE\\Policies\\Microsoft\\Windows NT\\Terminal Services\" /v fResetBroken /t REG_DWORD /d 1 /f >nul 2>&1'",
);

/// The same policy as one `cmd.exe` line, for applying to a guest that was
/// installed before it existed. Ends in `tsdiscon` — a RemoteApp session does
/// not close when its program exits, so without it FreeRDP sits there until the
/// timeout, stalling the launch the user actually asked for.
const GUEST_POLICY_CMD: &str = concat!(
    r#"'/app:program:C:\Windows\System32\cmd.exe,cmd:/C "#,
    r#"reg add "HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings\Zones\3" /v 1806 /t REG_DWORD /d 0 /f & "#,
    r#"reg add "HKLM\SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services" /v MaxDisconnectionTime /t REG_DWORD /d 30000 /f & "#,
    r#"reg add "HKLM\SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services" /v fResetBroken /t REG_DWORD /d 1 /f & tsdiscon'"#,
);

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
        // Readiness asks the RDP server to identify itself. A plain TCP connect
        // proves nothing — docker publishes 3389 when the CONTAINER starts, so
        // it answers for the whole ~40 minutes Windows is installing (measured:
        // "connection succeeded" while the log still read "Downloading Windows
        // 11"). An X.224 Connection Request gets a Confirm, starting 03 00, only
        // from a real RDP server. The image's healthcheck isn't usable either —
        // it may report nothing at all.
        assert!(s.contains("Waiting for Windows to be ready"), "{s}");
        assert!(s.contains("rdp_ready"), "readiness must probe the RDP server: {s}");
        assert!(s.contains("'^0300'"), "an X.224 Connection Confirm, not a bare connect: {s}");
        assert!(!s.contains("/dev/tcp/127.0.0.1/3389"), "TCP probe is a false positive: {s}");
        assert!(!s.contains("nc -z"), "a bare connect answers while Windows installs: {s}");
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
        // Both routes $HOME takes into the guest are tried, and Z: goes FIRST.
        // It is dockur's /shared mount: always present, needs nothing from the
        // client, and verified painting a real RAIL window on real hardware.
        // \\tsclient\home needs drive redirection we don't ask for, so the path
        // doesn't resolve and no window is ever created -- but that attempt
        // still holds the connection ~20s, which the duration check reads as
        // success, so with the order reversed the working path is never tried.
        let z = s.find(r"'Z:\Windows Transfer\'").expect("Z: path");
        let tsclient = s.find(r"'\\tsclient\home\Windows Transfer\'").expect("tsclient path");
        assert!(z < tsclient, "the share that works must be tried first:\n{s}");
        // A reinstall regenerates the guest's RDP certificate; WinApps' own
        // connection test uses /cert:tofu, which refuses a CHANGED key.
        assert!(s.contains("freerdp/server/127.0.0.1_3389.pem"), "clear the stale pin: {s}");
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
        for (what, s) in [("vm_run", vm_run_script()), ("vm_idle", vm_idle_script())] {
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
        let s = vm_idle_script();
        // A live FreeRDP client means an app is on screen — must not stop.
        assert!(s.contains("pgrep -x xfreerdp"), "{s}");
        assert!(s.contains("dk stop WinApps"), "{s}");
        // The timeout is READ at runtime, never baked in. This script ships as a
        // thin stub that `pacman -Syu` regenerates; a baked number would mean
        // the file on disk is written once at setup and never again, so a fix to
        // the logic here reaches nobody who already ran setup. That is exactly
        // what happened to the fix for the install-killing watchdog.
        assert!(s.contains(IDLE_CONF), "the timeout is data: {s}");
        assert!(s.contains("IDLE=$((IDLE_MINUTES * 60))"), "{s}");
        assert!(!s.contains("IDLE=1800"), "a baked timeout can never be updated: {s}");
        // 0 still disables auto-stop entirely, and 30 is still the default.
        assert!(s.contains(r#"${IDLE_MINUTES:-30}"#), "default when unset: {s}");
        assert!(s.contains("[ \"$IDLE\" -eq 0 ] && exit 0"), "{s}");
        // Installing Windows takes 20-40 min with no client attached and nothing
        // touching the activity file -- this watchdog's exact definition of
        // idle. It killed a running install once, 16 minutes in. `manifest
        // windows-vm` promises the install "needs no input"; it must not need
        // babysitting either.
        let guard = s.find("should_stop() {").expect("install guard");
        let stop = s.find("if should_stop ").expect("stop");
        assert!(guard < stop, "must not stop mid-install:\n{s}");
        // Two signals that look right and are not. Both were tried here.
        //   · windows.boot: dockur writes it from finish(), on container
        //     SHUTDOWN, so it is absent for the whole life of a guest that has
        //     never been stopped -- guarding on it disables this permanently.
        //   · a TCP probe of 3389: docker publishes that port when the CONTAINER
        //     starts, so it answers while Windows is still downloading and says
        //     nothing whatsoever about the guest.
        assert!(!s.contains("storage/windows.boot"), "windows.boot only exists after a stop: {s}");
        assert!(!s.contains("3389"), "docker publishes 3389 before Windows listens: {s}");
        // The container's own start time has to reach the decision — the rest
        // of the rule is exercised against a clock in
        // `the_idle_decision_holds_up_against_a_clock`.
        assert!(s.contains("{{.State.StartedAt}}"), "count from the container start too: {s}");
        assert!(s.contains("should_stop \"$now\" \"$act\" \"$started\" \"$IDLE\""), "{s}");
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
            ("autologon step", setup_autologon_step()),
            ("guest policy step", setup_guest_policy_step()),
            ("link container check", link_container_check_step()),
            ("link install step", link_install_step().to_string()),
            ("ensure winapps", ensure_winapps_step().to_string()),
            // These are whole scripts. `windows-vm-run` is reachable via
            // `__script`, so the release loop can pipe it through `sh -n` by
            // hand -- the other two are written as real files and never were.
            ("windows-vm-run", vm_run_script()),
            ("windows-vm-idle", vm_idle_script()),
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

    /// The watchdog's decision, driven with made-up clocks. Both directions of
    /// this have already shipped wrong: counting only from the activity file
    /// killed a Windows install 16 minutes in, and the "still installing" guard
    /// that replaced it was always true, which disabled the watchdog outright.
    /// Neither shows up anywhere until someone loses 40 minutes.
    #[test]
    fn the_idle_decision_holds_up_against_a_clock() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        // ago_used, ago_started, expect_stop
        let cases = [
            // Mid-install: nothing touches the activity file for 20-90 minutes,
            // and the file itself is stale from whenever an app last ran.
            (10800, 2400, false, "installing 40 min, activity 3 h stale"),
            (10800, 5400, false, "installing 90 min, activity 3 h stale"),
            // Freshly started for a launch that hasn't happened yet.
            (10800, 30, false, "just started, activity 3 h stale"),
            // Genuinely in use.
            (300, 7200, false, "used 5 min ago, up 2 h"),
            // Genuinely idle -- the case this watchdog exists for.
            (2400, 7200, true, "used 40 min ago, up 2 h"),
            // Never came up at all: the ceiling has to release it eventually.
            (14400, 10800, true, "never used, up 3 h (past the ceiling)"),
        ];
        for (ago_used, ago_started, expect_stop, what) in cases {
            let script = format!(
                "{}\nnow=1000000\nshould_stop \"$now\" $((now-{ago_used})) $((now-{ago_started})) 1800",
                idle_decision_fn()
            );
            let mut sh = Command::new("sh")
                .arg("-s")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .expect("spawn sh");
            sh.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
            let stopped = sh.wait().expect("run sh").success();
            assert_eq!(stopped, expect_stop, "{what}: expected stop={expect_stop}");
        }
    }

    /// The size becomes the guest's partition layout at install time, so it is
    /// asked once, before Windows exists. Growing it afterwards means evicting
    /// the Recovery partition Windows parks behind C:.
    #[test]
    fn disk_sizes_parse_the_way_people_write_them() {
        assert_eq!(parse_size_gb("128G"), Some(128));
        assert_eq!(parse_size_gb("128g"), Some(128));
        assert_eq!(parse_size_gb("128"), Some(128));
        assert_eq!(parse_size_gb(" 256G "), Some(256));
        assert_eq!(parse_size_gb("1T"), Some(1024));
        // Nonsense must not silently become a disk size.
        assert_eq!(parse_size_gb(""), None);
        assert_eq!(parse_size_gb("big"), None);
        assert_eq!(parse_size_gb("0G"), None);
        assert_eq!(parse_size_gb("128M"), None, "MB-sized Windows is a typo, not a choice");
        assert_eq!(parse_size_gb("-5G"), None);
    }

    /// Windows client editions allow ONE interactive session, and dockur
    /// autologons the same user at the console -- so a RemoteApp request lands
    /// on a session that is already taken and gets the desktop plus "Another
    /// user is signed in" instead. `RDPApps.reg` does not fix that; freeing the
    /// session does. Confirmed on real hardware.
    #[test]
    fn automatic_sign_in_is_turned_off_so_a_remoteapp_has_a_session() {
        let s = setup_autologon_step();
        assert!(s.contains("AutoAdminLogon /t REG_SZ /d 0"), "{s}");
        assert!(s.contains("/v DefaultPassword /f"), "a stored password re-enables it: {s}");
        assert!(s.contains("/v AutoLogonCount /f"), "dockur sets LogonCount 65432: {s}");
        // Appended to the OEM hook we already own -- no custom.xml, nothing
        // vendored, and it must not replace WinApps' install.bat.
        assert!(s.contains("oem/install.bat"), "{s}");
        assert!(s.contains(">> \"$b\""), "append, never overwrite: {s}");
        // Idempotent: setup is re-entered freely, and this must not stack up.
        assert!(s.contains("grep -qi 'AutoAdminLogon'"), "{s}");
        // A batch file Windows reads: CRLF, not LF.
        assert!(s.contains(r"'%s\r\n'"), "batch files need CRLF: {s}");
    }

    /// Z: is a network location to Windows, so launching an .exe from it raises
    /// "Open File - Security Warning" — and as a RemoteApp that is fatal twice:
    /// the app never starts, and the modal stays parked in the guest session.
    /// They accumulate, and since RAIL surfaces every top-level window, the next
    /// launch of *any* app shows the stale dialog. Measured: asking for Notepad
    /// produced a security warning left over from an earlier attempt.
    #[test]
    fn apps_are_allowed_to_launch_from_the_shared_home() {
        let s = setup_guest_policy_step();
        // Zone 3, policy 1806 = "launching applications and unsafe files".
        assert!(s.contains(r"Internet Settings\Zones\3"), "{s}");
        assert!(s.contains("/v 1806 /t REG_DWORD /d 0 /f"), "{s}");
        assert!(s.contains(">> \"$b\""), "append to the OEM hook, never replace it: {s}");
        assert!(s.contains("grep -qi 'Internet Settings'"), "idempotent: {s}");
        // "Another user is signed in": a RemoteApp session does NOT end when its
        // program exits, it only disconnects, and Windows sets no disconnection
        // limit by default — so sessions linger forever, pile up, and a later
        // connection collides with a stale one. Client editions allow a single
        // interactive session, so there is no headroom to absorb the leak.
        // Observed on real hardware: sessions 2 Active and 3 Down at once, with
        // MaxDisconnectionTime unset.
        assert!(s.contains("MaxDisconnectionTime /t REG_DWORD /d 30000"), "{s}");
        assert!(s.contains("fResetBroken /t REG_DWORD /d 1"), "logging off needs both: {s}");
        // A guest installed before this existed never ran that install.bat, so
        // the launcher applies it once itself rather than telling the user to.
        let run = vm_run_script();
        assert!(run.contains(".guest-policy-set"), "existing guests get it too: {run}");
        assert!(run.contains("/v 1806 /t REG_DWORD /d 0 /f"), "{run}");
        assert!(run.contains("MaxDisconnectionTime"), "{run}");
        assert!(run.contains("tsdiscon"), "or FreeRDP sits there stalling the launch: {run}");
        let stamp = run.find("POLICY_STAMP=").expect("stamp");
        let launch = run.find("run_wa manual \"$WINPATH\"").expect("launch");
        assert!(stamp < launch, "must be set before the first launch, not after:\n{run}");
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
