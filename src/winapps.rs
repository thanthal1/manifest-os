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

    // WinApps' own config: how it reaches the guest.
    ctx.shell(&format!("mkdir -p \"{WINAPPS_CONF_DIR}\""), false)?;
    ctx.write_user(
        &expand(&format!("{WINAPPS_CONF_DIR}/winapps.conf")),
        &winapps_conf(vm, &pass),
    )?;
    ctx.shell(&format!("chmod 600 \"{WINAPPS_CONF_DIR}/winapps.conf\""), false)?;

    if backend != "libvirt" {
        ctx.shell(&format!("mkdir -p \"{COMPOSE_DIR}\""), false)?;
        ctx.write_user(&expand(&format!("{COMPOSE_DIR}/compose.yaml")), &compose_yaml(vm, &pass))?;
        println!("  · starting the Windows container (first run installs Windows)");
        let up = format!(
            "cd \"{COMPOSE_DIR}\" && {cmd} compose up -d",
            cmd = if backend == "podman" { "podman" } else { "docker" }
        );
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
    println!("  · asking WinApps to detect installed Windows apps");
    // WinApps' installer generates the per-app launchers; `--user` keeps them in
    // the user's own applications dir.
    ctx.shell("winapps-setup --user || winapps-setup || true", false)?;
    println!("  · done — installed Windows apps should now appear in your menu");
    Ok(())
}

/// Host packages the tier needs. FreeRDP is the actual window transport.
fn ensure_deps(vm: &WindowsVm, ctx: &Ctx) -> Result<()> {
    let mut pkgs = vec!["freerdp", "iproute2", "libnotify", "git"];
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
    if ctx.check("sh", &["-c", "command -v winapps"]) {
        println!("  · winapps already installed");
        return Ok(());
    }
    println!("  · installing WinApps (GPL-3.0, installed separately — not part of ManifestOS)");
    ctx.shell(
        "paru -S --needed --noconfirm winapps-git || { \
           echo '  · AUR install failed — cloning upstream instead' >&2; \
           d=\"$HOME/.local/share/manifest-os/winapps\"; \
           mkdir -p \"$(dirname \"$d\")\"; \
           { [ -d \"$d/.git\" ] && git -C \"$d\" pull --ff-only; } || \
             git clone --depth 1 https://github.com/winapps-org/winapps \"$d\"; \
           mkdir -p \"$HOME/.local/bin\"; \
           ln -sf \"$d/bin/winapps\" \"$HOME/.local/bin/winapps\"; \
           ln -sf \"$d/setup.sh\" \"$HOME/.local/bin/winapps-setup\"; \
           echo '  · installed to ~/.local/bin (ensure it is on your PATH)'; \
         }",
        false,
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
             container_name: manifest-windows\n    \
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
               - ./storage:/storage\n      \
               - ${{HOME}}:/shared\n    \
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
         RDP_FLAGS=\"\"\n\
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
