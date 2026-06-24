//! Manifest OS — declare a complete Arch Linux system in one manifest.json
//! and reproduce it with one command.
//!
//! This is the lean core CLI. Per the design, the core stays tiny: it reads a
//! manifest, and (eventually) defers schema-specific logic to a versioned
//! parser fetched per `schema_version`. Phase 1 implements the install flow
//! locally.

mod boot;
mod desktop;
mod dotfiles;
mod exec;
mod install;
mod kernel;
mod manifest;
mod pacman;
mod system;

use anyhow::Result;
use clap::{Parser, Subcommand};
use exec::Ctx;
use manifest::Manifest;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "manifest",
    version,
    about = "Declare it. Share it. Deploy it.",
    long_about = "Reproduce a complete Arch Linux system from a single manifest.json."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install from a local manifest file (catalog-by-name comes later).
    Install {
        /// Path to a manifest.json.
        file: PathBuf,
        /// Print every step without executing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Validate a manifest's structure and schema version.
    Verify {
        /// Path to a manifest.json.
        file: PathBuf,
    },
    /// Export the current system as a manifest (Phase 5).
    Export,
    /// Re-apply a manifest to update packages/config (Phase 5).
    Sync {
        file: PathBuf,
    },
    /// Show what an install would change (Phase 5).
    Diff {
        file: PathBuf,
    },
    /// List the desktop environments / window managers the installer can set up.
    Desktops,
    /// List the kernels the installer can install.
    Kernels,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("\nerror: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Install { file, dry_run } => {
            let manifest = Manifest::from_path(&file)?;
            install::run(&manifest, &Ctx::new(dry_run))
        }
        Command::Verify { file } => {
            let manifest = Manifest::from_path(&file)?;
            let kernel = kernel::resolve(manifest.system.kernel.as_deref())?;
            let default_note = if manifest.system.kernel.is_none() { " (default)" } else { "" };
            println!(
                "✓ valid — schema v{}, {} package(s), kernel: {}{}",
                manifest.schema_version,
                manifest.packages.len(),
                kernel.package,
                default_note,
            );
            Ok(())
        }
        Command::Desktops => {
            println!("Supported desktops (use as \"desktop\" in a manifest):\n");
            for r in desktop::catalog() {
                let kind = match r.session {
                    desktop::Session::Wayland => "wayland",
                    desktop::Session::X11 => "x11",
                    desktop::Session::Both => "wayland/x11",
                };
                let dm = r.default_dm.unwrap_or("(tty)");
                println!("  {:<14} {:<12} dm:{:<14} {}", r.key, kind, dm, r.display_name);
                println!("                 {}", r.notes);
            }
            println!("\nOverride the login manager with the manifest's \"display_manager\" field.");
            Ok(())
        }
        Command::Kernels => {
            println!("Supported kernels (use as system.kernel in a manifest):\n");
            for k in kernel::catalog() {
                let def = if k.key == kernel::DEFAULT_KEY { "  [default]" } else { "" };
                println!("  {:<16} {}{}", k.key, k.display, def);
                println!("                   {}", k.notes);
            }
            println!("\nUnset system.kernel installs `{}`. Headers are installed alongside.", kernel::DEFAULT_KEY);
            Ok(())
        }
        Command::Export | Command::Sync { .. } | Command::Diff { .. } => {
            anyhow::bail!("not implemented yet — planned for Phase 5 (export/sync/diff)")
        }
    }
}
