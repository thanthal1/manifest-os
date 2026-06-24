//! The install pipeline — the heart of Manifest OS.
//!
//! Implements the Phase 1 flow against an already-running Arch system:
//!   3. add required repos       6. install packages (paru)
//!   4. bootstrap paru           7. install + link dotfiles
//!   5. run pre_install hooks     8. enable services
//!                               9. run post_install hooks
//!
//! (Steps 1–2, reading schema_version and fetching the parser, are the CLI
//! bootstrap; 10 is the final report.) Network, disk and partitioning are NOT
//! here — those belong to the ISO's TUI layer, never the manifest.

use crate::exec::Ctx;
use crate::manifest::Manifest;
use anyhow::Result;

pub fn run(manifest: &Manifest, ctx: &Ctx) -> Result<()> {
    let m = &manifest.meta;
    println!(
        "\n→ Installing \"{}\"{}\n",
        if m.name.is_empty() { "(unnamed manifest)" } else { &m.name },
        if ctx.dry_run { "  [dry-run: nothing will be executed]" } else { "" }
    );

    enable_repos(manifest, ctx)?;
    bootstrap_paru(ctx)?;
    run_hooks("pre_install", &manifest.pre_install, ctx)?;
    install_packages(manifest, ctx)?;
    install_dotfiles(manifest, ctx)?;
    enable_services(manifest, ctx)?;
    run_hooks("post_install", &manifest.post_install, ctx)?;

    println!("\n✓ Done.{}", if ctx.dry_run { " (dry-run — no changes made)" } else { "" });
    Ok(())
}

/// Step 3 — enable multilib / CachyOS repos as declared. CachyOS is also
/// implied by `kernel: "cachy"`, since linux-cachyos lives in that repo.
fn enable_repos(manifest: &Manifest, ctx: &Ctx) -> Result<()> {
    let repos = &manifest.repos;
    let needs_cachy = repos.cachyos || manifest.system.kernel.as_deref() == Some("cachy");

    if !repos.multilib && !needs_cachy {
        return Ok(());
    }
    step("Enabling repositories");

    if repos.multilib {
        // [multilib] lives in /etc/pacman.conf; uncommenting it is the real op.
        // TODO(phase1): edit pacman.conf in place instead of just signaling intent.
        eprintln!("  · multilib (uncomment [multilib] in /etc/pacman.conf)");
    }
    if needs_cachy {
        // CachyOS ships an installer script that adds the repo + signing key.
        // TODO(phase1): fetch + run the official key/repo bootstrap.
        eprintln!("  · cachyos repo + signing key");
        if repos.cachy_optimized_packages {
            eprintln!("  · cachyos-v3/v4 optimized package repos");
        }
    }
    Ok(())
}

/// Step 4 — ensure paru exists. paru is the one hardcoded AUR helper.
/// Bootstrapped from the AUR via base-devel + git + makepkg.
fn bootstrap_paru(ctx: &Ctx) -> Result<()> {
    step("Bootstrapping paru");
    // If paru is already present we're done. (Detection is skipped in dry-run.)
    // TODO(phase1): `command -v paru` check, then clone paru-bin + makepkg -si.
    ctx.run("sh", &["-c", "command -v paru >/dev/null 2>&1 && echo 'paru present' || echo 'would bootstrap paru-bin'"])?;
    Ok(())
}

/// Step 6 — install every package (plus the kernel package) via paru. paru
/// transparently resolves both official-repo and AUR packages in one call.
fn install_packages(manifest: &Manifest, ctx: &Ctx) -> Result<()> {
    let mut pkgs: Vec<&str> = Vec::new();
    if let Some(kernel) = manifest.kernel_package() {
        pkgs.push(kernel);
    }
    pkgs.extend(manifest.packages.iter().map(String::as_str));

    if pkgs.is_empty() {
        return Ok(());
    }
    step(&format!("Installing {} package(s) via paru", pkgs.len()));

    let mut args = vec!["-S", "--needed", "--noconfirm"];
    args.extend(pkgs);
    ctx.run("paru", &args)?;
    Ok(())
}

/// Step 7 — clone dotfiles. Phase 1 is git-clone only; symlink/copy placement
/// is a later refinement.
fn install_dotfiles(manifest: &Manifest, ctx: &Ctx) -> Result<()> {
    let Some(df) = &manifest.dotfiles else {
        return Ok(());
    };
    step("Installing dotfiles");
    ctx.run(
        "git",
        &["clone", "--branch", &df.branch, "--depth", "1", &df.source, "/tmp/manifest-dotfiles"],
    )?;
    // TODO(phase1): place files per `method` (symlink|copy) into $HOME.
    Ok(())
}

/// Step 8 — enable systemd units, system and user scope.
fn enable_services(manifest: &Manifest, ctx: &Ctx) -> Result<()> {
    let svc = &manifest.services;
    if svc.system.is_empty() && svc.user.is_empty() {
        return Ok(());
    }
    step("Enabling services");

    for unit in &svc.system {
        ctx.run("systemctl", &["enable", unit])?;
    }
    for unit in &svc.user {
        ctx.run("systemctl", &["--user", "enable", unit])?;
    }
    Ok(())
}

/// Steps 5 & 9 — run author-provided shell hooks in order.
fn run_hooks(phase: &str, hooks: &[String], ctx: &Ctx) -> Result<()> {
    if hooks.is_empty() {
        return Ok(());
    }
    step(&format!("Running {phase} hooks"));
    for line in hooks {
        ctx.shell(line)?;
    }
    Ok(())
}

fn step(title: &str) {
    println!("\n[{title}]");
}
