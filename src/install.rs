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

use crate::boot;
use crate::desktop;
use crate::dotfiles;
use crate::exec::Ctx;
use crate::files;
use crate::kernel;
use crate::manifest::Manifest;
use crate::pacman;
use crate::system;
use crate::users;
use anyhow::Result;

pub fn run(manifest: &Manifest, ctx: &Ctx) -> Result<()> {
    let m = &manifest.meta;
    println!(
        "\n→ Installing \"{}\"{}\n",
        if m.name.is_empty() { "(unnamed manifest)" } else { &m.name },
        if ctx.dry_run { "  [dry-run: nothing will be executed]" } else { "" }
    );

    // Resolve kernel + desktop up front so bad names fail before we touch the
    // system. The kernel defaults to stock Arch `linux` when unset.
    let kernel = kernel::resolve(manifest.system.kernel.as_deref())?;
    let desktop = desktop::resolve(manifest)?;

    step("Enabling repositories");
    pacman::enable_repos(manifest, kernel, ctx)?;

    step("Updating system");
    pacman::sync_system(ctx)?;

    step("Bootstrapping paru");
    pacman::bootstrap_paru(ctx)?;

    run_hooks("pre_install", &manifest.pre_install, ctx)?;

    step("Installing packages");
    println!("  · kernel: {} ({} + {})", kernel.display, kernel.package, kernel.headers);
    if kernel.key != crate::kernel::DEFAULT_KEY {
        println!("  · note: non-default kernel — ensure the bootloader has an entry for it");
    }
    let desktop_pkgs = desktop.as_ref().map(|d| d.packages.clone()).unwrap_or_default();
    if let Some(d) = &desktop {
        println!("  · desktop: {} (+{} packages)", d.display_name, d.packages.len());
    }
    pacman::install_packages(manifest, kernel, &desktop_pkgs, ctx)?;

    if !manifest.system.is_empty() {
        step("Configuring system");
        system::apply(&manifest.system, ctx)?;
    }

    if !manifest.users.is_empty() {
        step("Creating users");
        users::apply(&manifest.users, ctx)?;
    }

    if let Some(boot_cfg) = &manifest.boot {
        step("Configuring bootloader");
        boot::apply(boot_cfg, kernel, ctx)?;
    }

    if let Some(d) = &desktop {
        step("Configuring desktop");
        desktop::apply(d, ctx)?;
        if !d.aur.is_empty() {
            println!("  · note: AUR packages pulled — {}", d.aur.join(", "));
        }
    }

    if let Some(df) = &manifest.dotfiles {
        step("Installing dotfiles");
        dotfiles::install(df, ctx)?;
    }

    // After dotfiles, so an explicit `files` entry can override a dotfile.
    if !manifest.files.is_empty() {
        step("Writing files");
        files::apply(&manifest.files, ctx)?;
    }

    enable_services(manifest, ctx)?;
    run_hooks("post_install", &manifest.post_install, ctx)?;

    println!("\n✓ Done.{}", if ctx.dry_run { " (dry-run — no changes made)" } else { "" });
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
        ctx.sudo("systemctl", &["enable", unit])?;
    }
    for unit in &svc.user {
        ctx.run("systemctl", &["--user", "enable", unit])?;
    }
    Ok(())
}

/// Steps 5 & 9 — run author-provided shell hooks in order, at user level.
fn run_hooks(phase: &str, hooks: &[String], ctx: &Ctx) -> Result<()> {
    if hooks.is_empty() {
        return Ok(());
    }
    step(&format!("Running {phase} hooks"));
    for line in hooks {
        ctx.shell(line, false)?;
    }
    Ok(())
}

fn step(title: &str) {
    println!("\n[{title}]");
}
