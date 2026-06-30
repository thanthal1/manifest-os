//! Manifest OS — declare a complete Arch Linux system in one manifest.json
//! and reproduce it with one command.
//!
//! This is the lean core CLI. Per the design, the core stays tiny: it reads a
//! manifest, and (eventually) defers schema-specific logic to a versioned
//! parser fetched per `schema_version`. Phase 1 implements the install flow
//! locally.

use anyhow::Result;
use clap::{Parser, Subcommand};
use manifest::exec::Ctx;
use manifest::manifest::Manifest;
use manifest::probe::{Account, InstallPlan};
use manifest::{desktop, install, installer, kernel, survey, tui};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "manifest",
    version,
    about = "Declare it. Share it. Deploy it.",
    long_about = "Reproduce a complete Linux system from a single manifest.json."
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
        /// JSON object of survey answers ({"id": value}) for unattended installs.
        #[arg(long)]
        answers: Option<PathBuf>,
    },
    /// Validate a manifest's structure and schema version.
    Verify {
        /// Path to a manifest.json.
        file: PathBuf,
    },
    /// Export the current system as a manifest (Phase 5).
    Export,
    /// Re-apply a manifest to update packages/config (Phase 5).
    Sync { file: PathBuf },
    /// Show what an install would change (Phase 5).
    Diff { file: PathBuf },
    /// List the desktop environments / window managers the installer can set up.
    Desktops,
    /// List the kernels the installer can install.
    Kernels,
    /// Launch the guided installer TUI (the friendly first-boot experience).
    Tui {
        /// Preview the install steps without touching the disk.
        #[arg(long)]
        dry_run: bool,
    },
    /// Unattended full install to a disk (the TUI/GUI flow, headless). Drives
    /// the same `installer::execute`, so it's scriptable for automation + CI.
    Provision {
        /// Path to a manifest.json.
        file: PathBuf,
        /// Target disk, e.g. /dev/sda.
        #[arg(long)]
        disk: String,
        /// "erase" (wipe the disk) or "alongside" (dual-boot an existing OS).
        #[arg(long, default_value = "erase")]
        mode: String,
        /// Root filesystem: "ext4" or "btrfs".
        #[arg(long, default_value = "ext4")]
        filesystem: String,
        /// Swap: "zram", "none", "swapfile", or "partition".
        #[arg(long, default_value = "zram")]
        swap: String,
        /// Swap size in GiB (swapfile/partition).
        #[arg(long)]
        swap_size_gib: Option<u32>,
        /// GiB to give Manifest OS when mode=alongside.
        #[arg(long)]
        alongside_gib: Option<u32>,
        /// Create this admin account (with --password).
        #[arg(long)]
        user: Option<String>,
        /// Password for --user.
        #[arg(long)]
        password: Option<String>,
        /// Override the hostname.
        #[arg(long)]
        hostname: Option<String>,
        /// Encrypt the root with LUKS2 (erase installs only); needs --passphrase.
        #[arg(long)]
        encrypt: bool,
        /// LUKS passphrase for --encrypt.
        #[arg(long)]
        passphrase: Option<String>,
        /// Timezone (e.g. America/New_York), locale (e.g. en_US.UTF-8), keymap.
        #[arg(long)]
        timezone: Option<String>,
        #[arg(long)]
        locale: Option<String>,
        #[arg(long)]
        keymap: Option<String>,
        /// JSON object of survey answers for the manifest's questions.
        #[arg(long)]
        answers: Option<PathBuf>,
        /// Preview every step without touching the disk.
        #[arg(long)]
        dry_run: bool,
        /// Don't reboot when done (so a harness can inspect the result).
        #[arg(long)]
        no_reboot: bool,
    },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("\nerror: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Install {
            file,
            dry_run,
            answers,
        } => {
            let raw = std::fs::read_to_string(&file)?;
            // Run the survey, inject {{answers}}, then parse the final manifest.
            let answered = survey::collect(&raw, answers.as_deref())?;
            let substituted = survey::substitute(&raw, &answered);
            let mut manifest = Manifest::from_str(&substituted)?;
            let extra = survey::conditional_packages(&manifest.conditional_packages, &answered);
            if !extra.is_empty() {
                println!("survey: +{} conditional package(s)", extra.len());
                manifest.packages.extend(extra);
            }
            install::run(&manifest, &Ctx::new(dry_run))
        }
        Command::Verify { file } => {
            let manifest = Manifest::from_path(&file)?;
            let kernel = kernel::resolve(manifest.system.kernel.as_deref())?;
            let default_note = if manifest.system.kernel.is_none() {
                " (default)"
            } else {
                ""
            };
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
                println!(
                    "  {:<14} {:<12} dm:{:<14} {}",
                    r.key, kind, dm, r.display_name
                );
                println!("                 {}", r.notes);
            }
            println!("\nOverride the login manager with the manifest's \"display_manager\" field.");
            Ok(())
        }
        Command::Kernels => {
            println!("Supported kernels (use as system.kernel in a manifest):\n");
            for k in kernel::catalog() {
                let def = if k.key == kernel::DEFAULT_KEY {
                    "  [default]"
                } else {
                    ""
                };
                println!("  {:<16} {}{}", k.key, k.display, def);
                println!("                   {}", k.notes);
            }
            println!(
                "\nUnset system.kernel installs `{}`. Headers are installed alongside.",
                kernel::DEFAULT_KEY
            );
            Ok(())
        }
        Command::Tui { dry_run } => match tui::run()? {
            Some(plan) => {
                installer::execute(&plan, &Ctx::new(dry_run))?;
                if !dry_run {
                    installer::finish_and_reboot();
                }
                Ok(())
            }
            None => {
                println!("Installer cancelled.");
                Ok(())
            }
        },
        Command::Provision {
            file,
            disk,
            mode,
            filesystem,
            swap,
            swap_size_gib,
            alongside_gib,
            user,
            password,
            hostname,
            encrypt,
            passphrase,
            timezone,
            locale,
            keymap,
            answers,
            dry_run,
            no_reboot,
        } => {
            // Survey answers (optional JSON object {"id": value}).
            let answers_vec: Vec<(String, String)> = match &answers {
                Some(p) => {
                    let raw = std::fs::read_to_string(p)?;
                    let v: serde_json::Value = serde_json::from_str(&raw)?;
                    v.as_object()
                        .map(|o| {
                            o.iter()
                                .map(|(k, val)| {
                                    let s = val
                                        .as_str()
                                        .map(str::to_string)
                                        .unwrap_or_else(|| val.to_string());
                                    (k.clone(), s)
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                }
                None => Vec::new(),
            };
            let account = match (user, password) {
                (Some(u), Some(pw)) => Some(Account {
                    full_name: u.clone(),
                    username: u,
                    password: pw,
                }),
                _ => None,
            };
            let plan = InstallPlan {
                disk,
                install_mode: mode,
                alongside_gib,
                filesystem,
                swap,
                swap_size_gib,
                manifest: file.to_string_lossy().to_string(),
                answers: answers_vec,
                account,
                hostname,
                encrypt,
                encrypt_passphrase: passphrase.unwrap_or_default(),
                timezone,
                locale,
                keymap,
            };
            installer::execute(&plan, &Ctx::new(dry_run))?;
            if !dry_run && !no_reboot {
                installer::finish_and_reboot();
            }
            Ok(())
        }
        Command::Export | Command::Sync { .. } | Command::Diff { .. } => {
            anyhow::bail!("not implemented yet — planned for Phase 5 (export/sync/diff)")
        }
    }
}
