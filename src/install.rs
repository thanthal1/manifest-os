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
use crate::defaults;
use crate::desktop;
use crate::dotfiles;
use crate::exec::Ctx;
use crate::export;
use crate::files;
use crate::flatpak;
use crate::keybindings;
use crate::kernel;
use crate::manifest::Manifest;
use crate::pacman;
use crate::snippets;
use crate::system;
use crate::theming;
use crate::users;
use crate::wallpaper;
use anyhow::Result;

/// Whether we're doing a first-time install or re-applying an edited manifest
/// to an already-running system.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Install,
    Sync,
}

/// Apply a manifest to the current system for the first time.
pub fn run(manifest: &Manifest, ctx: &Ctx) -> Result<()> {
    apply(manifest, ctx, Mode::Install)
}

/// Re-apply an edited manifest to the already-running system: install whatever
/// the edit added (packages, a desktop, a theme, keybindings, …) and switch the
/// default desktop if it changed.
///
/// The whole pipeline is idempotent — packages install with `--needed`, repos
/// and paru check before acting, and every generated config file is overwritten
/// with the manifest's current content — so syncing is just running it again.
/// The one sync-specific step is retargeting the login manager (see
/// [`desktop::switch_default`]): re-running install alone would enable the new
/// DE's display manager but leave the old one also enabled.
pub fn sync(manifest: &Manifest, ctx: &Ctx) -> Result<()> {
    apply(manifest, ctx, Mode::Sync)
}

fn apply(manifest: &Manifest, ctx: &Ctx, mode: Mode) -> Result<()> {
    let m = &manifest.meta;
    let verb = if mode == Mode::Sync { "Syncing" } else { "Installing" };
    println!(
        "\n→ {verb} \"{}\"{}\n",
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

    if let Some(fp) = &manifest.flatpak {
        step("Installing Flatpak apps");
        flatpak::apply(fp, ctx)?;
    }

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

    let mut switched_desktop = false;
    if let Some(d) = &desktop {
        step("Configuring desktop");
        // On a sync, retarget the login manager first if the desktop changed,
        // so the freshly-enabled DM below becomes the boot default.
        if mode == Mode::Sync {
            switched_desktop = desktop::switch_default(d, ctx);
        }
        desktop::apply(d, ctx)?;
        if !d.aur.is_empty() {
            println!("  · note: AUR packages pulled — {}", d.aur.join(", "));
        }
    }

    // The manifest's primary account — user-level config files (theme,
    // keybindings) are written into *its* home, since the install itself runs
    // as a throwaway bootstrap user.
    let primary_user = manifest.users.first().map(|u| u.name.as_str());

    if let Some(w) = &manifest.wallpaper {
        step("Setting the wallpaper");
        // Best-effort: a wallpaper is cosmetic — a dead URL or offline mirror
        // must not fail an otherwise-complete install at this late stage.
        if let Err(e) = wallpaper::apply(w, manifest.desktop.as_deref(), ctx) {
            println!("  · warning: couldn't set the wallpaper ({e:#}) — continuing without it");
        }
    }

    if let Some(th) = &manifest.theme {
        step("Applying the theme");
        theming::apply(th, manifest.desktop.as_deref(), primary_user, ctx)?;
    }

    if !manifest.keybindings.is_empty() {
        step("Setting up keybindings");
        keybindings::apply(&manifest.keybindings, manifest.desktop.as_deref(), primary_user, ctx)?;
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

    // Last of the file layers: snippets edit *inside* whatever dotfiles/files
    // (or a generated keybindings config) put on disk.
    if !manifest.snippets.is_empty() {
        step("Inserting config snippets");
        snippets::apply(&manifest.snippets, primary_user, ctx)?;
    }

    if let Some(defaults_cfg) = &manifest.defaults {
        step("Setting default applications");
        defaults::apply(defaults_cfg, primary_user, ctx)?;
    }

    enable_services(manifest, ctx)?;
    run_hooks("post_install", &manifest.post_install, ctx)?;

    // Keep the system's declared state in sync with future package changes.
    // Last, so nothing in this run self-triggers the hook. Best-effort — a
    // failure here shouldn't fail an otherwise-complete install.
    step("Enabling package tracking");
    if let Err(e) = export::enable_tracking(ctx) {
        println!("  · note: couldn't enable package tracking ({e:#})");
    }

    let done = if mode == Mode::Sync { "Synced" } else { "Done" };
    println!("\n✓ {done}.{}", if ctx.dry_run { " (dry-run — no changes made)" } else { "" });
    if switched_desktop && !ctx.dry_run {
        println!("  · log out (or reboot) to enter your new desktop.");
    }
    Ok(())
}

/// Step 8 — enable systemd units, system and user scope.
///
/// Best-effort: a service whose package wasn't installed (or a user unit that
/// can't be enabled without a session, common at install time) only warns — it
/// must not abort an otherwise-complete install at the very last step.
fn enable_services(manifest: &Manifest, ctx: &Ctx) -> Result<()> {
    let svc = &manifest.services;
    if svc.system.is_empty() && svc.user.is_empty() {
        return Ok(());
    }
    step("Enabling services");

    for unit in &svc.system {
        if ctx.sudo("systemctl", &["enable", unit]).is_err() {
            println!("  · warning: couldn't enable {unit} — is its package in `packages`? Skipping.");
        }
    }
    for unit in &svc.user {
        if ctx.run("systemctl", &["--user", "enable", unit]).is_err() {
            println!("  · warning: couldn't enable user unit {unit} (no session at install time?). Skipping.");
        }
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
