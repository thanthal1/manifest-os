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

    let pass = vm.password.clone().unwrap_or_else(generated_password);
    if vm.password.is_some() {
        println!(
            "  · note: a password in the manifest is a credential leak if you share it — \
             prefer omitting it and letting one be generated"
        );
    }

    // The lazy runtime: on-demand start + idle stop, mirroring Android's
    // waydroid-launch/waydroid-idle. Thin stubs so `pacman -Syu` updates them.
    ctx.write_root(
        "/usr/local/bin/windows-vm-run",
        &crate::android::thin_stub("windows-vm-run"),
    )?;
    ctx.sudo("chmod", &["0755", "/usr/local/bin/windows-vm-run"])?;
    let idle = vm.idle_minutes.unwrap_or(30);
    println!(
        "  · lazy lifecycle: the VM starts when an app needs it{}",
        if idle == 0 { ", and stays up (idle_minutes: 0)".to_string() }
        else { format!(", and stops after {idle} min idle") }
    );
    ctx.write_root("/usr/local/bin/windows-vm-idle", &vm_idle_script(idle))?;
    ctx.sudo("chmod", &["0755", "/usr/local/bin/windows-vm-idle"])?;
    ctx.write_root("/etc/systemd/user/windows-vm-idle.service", idle_service_unit())?;
    ctx.write_root("/etc/systemd/user/windows-vm-idle.timer", idle_timer_unit())?;
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
        if vm.debloat.unwrap_or(true) {
            println!("  · debloating: removing preinstalled Store apps, Cortana and telemetry");
            ctx.shell(&format!("mkdir -p \"{COMPOSE_DIR}/oem\""), false)?;
            ctx.write_user(&expand(&format!("{COMPOSE_DIR}/oem/install.bat")), debloat_bat())?;
            ctx.write_user(&expand(&format!("{COMPOSE_DIR}/oem/debloat.ps1")), debloat_ps1())?;
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
    println!("  · checking WinApps' dependencies");
    ctx.sudo(
        "pacman",
        &["-S", "--needed", "--noconfirm", "dialog", "gawk", "curl", "openbsd-netcat", "freerdp"],
    )?;

    // Make sure winapps + winapps-setup are actually on PATH (an older setup may
    // have linked only into ~/.local/bin).
    ensure_winapps(ctx)?;

    println!("  · checking the Windows container is running");
    ctx.shell(
        &format!(
            "{docker}             st=$(dk ps --filter name=WinApps --format '{{{{.Names}}}} {{{{.Status}}}}' 2>/dev/null)
             if [ -n \"$st\" ]; then echo \"  · container: $st\"; 
             else echo '  ! the Windows container is not running — start it with: manifest windows-vm' >&2; fi
",
            docker = docker_fn()
        ),
        false,
    )?;

    // Fully automatic: no wizard, no commands for the user to run. WinApps
    // refuses to install over a previous installation (exit 3), so clear any
    // prior one first — uninstall is a no-op when there's nothing there — then
    // install non-interactively. `--system` first, `--user` as the fallback for
    // machines where the system-wide path isn't available.
    println!("  · installing WinApps and detecting your Windows apps");
    ctx.shell(
        "SETUP=\"$HOME/.local/share/manifest-os/winapps/setup.sh\";          [ -x \"$SETUP\" ] || { echo '  ! WinApps source missing — re-run: manifest windows-vm' >&2; exit 1; };          run() { \"$SETUP\" \"$@\" 2>&1 || sg docker -c \"'$SETUP' $*\" 2>&1; };          # Clear any earlier installation so the conflict check can't stop us.          run --system --uninstall >/dev/null 2>&1 || true;          run --user --uninstall  >/dev/null 2>&1 || true;          out=$(run --system); rc=$?;          if [ $rc -ne 0 ]; then out=$(run --user); rc=$?; fi;          printf '%s\n' \"$out\" | sed 's/^/    /';          if [ $rc -ne 0 ]; then            echo >&2;            echo '  ! WinApps could not finish installing.' >&2;            echo '    If the message above mentions docker permissions, log out and back' >&2;            echo '    in once — that is the only thing this cannot do for you.' >&2;            exit 1;          fi",
        false,
    )?;
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
    println!("  · installing host dependencies: {}", pkgs.join(", "));
    let mut args = vec!["-S", "--needed", "--noconfirm"];
    args.extend(pkgs.iter().copied());
    ctx.sudo("pacman", &args)?;
    if vm.backend() == "docker" {
        // Docker must actually be running, and the user needs to reach it.
        ctx.sudo("systemctl", &["enable", "--now", "docker.service"])?;
        ctx.shell("sudo usermod -aG docker \"$USER\" || true", false)?;
        println!("  · added you to the `docker` group (log out and back in if the next step fails)");
    }
    if vm.backend() == "libvirt" {
        ctx.sudo("systemctl", &["enable", "--now", "libvirtd.service"])?;
        ctx.shell("sudo usermod -aG libvirt \"$USER\" || true", false)?;
    }
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
    ctx.shell(
        "d=\"$HOME/.local/share/manifest-os/winapps\";          mkdir -p \"$(dirname \"$d\")\";          if [ -d \"$d/.git\" ]; then git -C \"$d\" pull --ff-only >/dev/null 2>&1 || true;          else git clone --depth 1 https://github.com/winapps-org/winapps \"$d\" || exit 1; fi;          chmod +x \"$d/setup.sh\" \"$d/bin/winapps\" 2>/dev/null || true;          # Remove symlinks an earlier version of ManifestOS created — they are          # what WinApps' installer trips over. Only ours (symlinks into our          # checkout) are touched; a real WinApps install is left alone.          for l in /usr/local/bin/winapps /usr/local/bin/winapps-setup; do            if [ -L \"$l\" ] && readlink \"$l\" | grep -q 'manifest-os/winapps'; then              echo \"  · removing our old symlink $l (it conflicts with WinApps' installer)\";              sudo rm -f \"$l\";            fi;          done;          for l in \"$HOME/.local/bin/winapps\" \"$HOME/.local/bin/winapps-setup\"; do            [ -L \"$l\" ] && readlink \"$l\" | grep -q 'manifest-os/winapps' && rm -f \"$l\";          done; true",
        false,
    )
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
{docker}
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
WINPATH=
opened=0
for p in '\\tsclient\home\Windows Transfer\' 'Z:\Windows Transfer\'; do
  WINPATH="$p$base"
  t0=$(date +%s)
  run_wa manual "$WINPATH"
  t1=$(date +%s)
  # winapps exits 0 whether or not the window ever painted, so exit status
  # proves nothing. A session you actually saw and closed lasts longer than a
  # few seconds; an instant return means the path was wrong or RDP refused.
  if [ $((t1 - t0)) -ge 5 ]; then opened=1; break; fi
done

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
echo "Couldn't open it as its own window." >&2
[ -s "$WALOG" ] && {{ echo "What winapps reported:" >&2; sed 's/^/    /' "$WALOG" >&2; }}

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
    )
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
ACT="${{XDG_STATE_HOME:-$HOME/.local/state}}/windows-vm-activity"
now=$(date +%s)
last=$([ -e "$ACT" ] && stat -c %Y "$ACT" 2>/dev/null || echo 0)
[ "$last" -eq 0 ] && {{ mkdir -p "$(dirname "$ACT")"; : > "$ACT"; exit 0; }}
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
         FREERDP_COMMAND=\"xfreerdp3\"\n",
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
        assert!(c.contains("FREERDP_COMMAND=\"xfreerdp3\""), "{c}");
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
}
