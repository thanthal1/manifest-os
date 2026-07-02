//! Manifest OS — declare a complete Arch Linux system in one manifest.json
//! and reproduce it with one command.
//!
//! This is the lean core CLI. Per the design, the core stays tiny: it reads a
//! manifest, and (eventually) defers schema-specific logic to a versioned
//! parser fetched per `schema_version`. Phase 1 implements the install flow
//! locally.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use manifest::exec::Ctx;
use manifest::manifest::Manifest;
use manifest::probe::{Account, ExtraUser, InstallPlan, StaticIp};
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
    /// Re-apply an edited manifest to the running system. Installs whatever the
    /// edit added — packages, a desktop, a theme, keybindings — and switches the
    /// default desktop if `desktop` changed. Idempotent; safe to re-run.
    Sync {
        /// Path to a manifest.json.
        file: PathBuf,
        /// Print every step without executing anything.
        #[arg(long)]
        dry_run: bool,
        /// JSON object of survey answers ({"id": value}) for unattended syncs.
        #[arg(long)]
        answers: Option<PathBuf>,
    },
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
    /// Either give `file` + flags, or skip the wizard entirely with a single
    /// `--config preseed.json` (an InstallPlan as JSON — see the GUI's Review
    /// page "equivalent command" or any of the flags below for its shape).
    Provision {
        /// Path to a manifest.json. Omit if using --config.
        file: Option<PathBuf>,
        /// A preseed file: an InstallPlan as JSON, bypassing every flag below.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Target disk, e.g. /dev/sda.
        #[arg(long)]
        disk: Option<String>,
        /// "erase" (wipe the disk) or "alongside" (dual-boot an existing OS).
        #[arg(long, default_value = "erase")]
        mode: String,
        /// Root filesystem: "ext4", "btrfs", or "xfs".
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
        /// Additional account, repeatable: "name:password" or
        /// "name:password:sudo".
        #[arg(long = "extra-user")]
        extra_user: Vec<String>,
        /// Override the hostname.
        #[arg(long)]
        hostname: Option<String>,
        /// "none" (default), "full" (LUKS2 the whole root), or "home" (LUKS2 a
        /// separate /home only). "full"/"home" need --passphrase; erase-install
        /// only.
        #[arg(long, default_value = "none")]
        encrypt_mode: String,
        /// LUKS passphrase for --encrypt-mode full|home.
        #[arg(long)]
        passphrase: Option<String>,
        /// Root partition size in GiB when --encrypt-mode home (the rest of
        /// the disk becomes /home). Default 40.
        #[arg(long)]
        root_gib: Option<u32>,
        /// Put root on an LVM logical volume (composes with encryption/RAID).
        #[arg(long)]
        lvm: bool,
        /// Mirror root onto this second disk via mdadm RAID1.
        #[arg(long)]
        raid1_disk: Option<String>,
        /// Timezone (e.g. America/New_York), locale (e.g. en_US.UTF-8), keymap.
        #[arg(long)]
        timezone: Option<String>,
        #[arg(long)]
        locale: Option<String>,
        #[arg(long)]
        keymap: Option<String>,
        /// Set a root password (root is locked by default — login is via
        /// --user's wheel/sudo membership).
        #[arg(long)]
        root_password: Option<String>,
        /// Log the created (or manifest's primary) account in automatically.
        #[arg(long)]
        autologin: bool,
        /// Install the proprietary NVIDIA driver (nvidia-dkms).
        #[arg(long)]
        install_nvidia: bool,
        /// Install and enable CUPS printing.
        #[arg(long)]
        install_printing: bool,
        /// A local script to run inside the chroot after everything else.
        #[arg(long)]
        post_script: Option<String>,
        /// Static IPv4 for the install (CIDR, e.g. 192.168.1.50/24). Needs
        /// --gateway too. Omit for DHCP.
        #[arg(long)]
        static_ip: Option<String>,
        #[arg(long)]
        gateway: Option<String>,
        /// Comma-separated resolver IPs for --static-ip.
        #[arg(long)]
        dns: Option<String>,
        /// HTTP(S) proxy for the base install's own downloads, e.g.
        /// http://10.0.0.1:3128.
        #[arg(long)]
        proxy: Option<String>,
        /// Bring up a VLAN before installing: tag --vlan-id on --vlan-parent.
        #[arg(long)]
        vlan_id: Option<u16>,
        #[arg(long)]
        vlan_parent: Option<String>,
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
            let manifest = load_manifest(&file, answers.as_deref())?;
            install::run(&manifest, &Ctx::new(dry_run))
        }
        Command::Sync {
            file,
            dry_run,
            answers,
        } => {
            let manifest = load_manifest(&file, answers.as_deref())?;
            install::sync(&manifest, &Ctx::new(dry_run))
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
            config,
            disk,
            mode,
            filesystem,
            swap,
            swap_size_gib,
            alongside_gib,
            user,
            password,
            extra_user,
            hostname,
            encrypt_mode,
            passphrase,
            root_gib,
            lvm,
            raid1_disk,
            timezone,
            locale,
            keymap,
            root_password,
            autologin,
            install_nvidia,
            install_printing,
            post_script,
            static_ip,
            gateway,
            dns,
            proxy,
            vlan_id,
            vlan_parent,
            answers,
            dry_run,
            no_reboot,
        } => {
            let plan = if let Some(config) = config {
                // Preseed: an InstallPlan as JSON, bypassing every other flag.
                let raw = std::fs::read_to_string(&config)?;
                serde_json::from_str(&raw)
                    .with_context(|| format!("parsing preseed config {}", config.display()))?
            } else {
                let Some(file) = file else {
                    anyhow::bail!("provide a manifest file, or --config <preseed.json>");
                };
                let Some(disk) = disk else {
                    anyhow::bail!("--disk is required (unless using --config)");
                };
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
                // "name:password[:sudo]"
                let extra_users: Vec<ExtraUser> = extra_user
                    .iter()
                    .filter_map(|s| {
                        let mut parts = s.splitn(3, ':');
                        let username = parts.next()?.to_string();
                        let password = parts.next()?.to_string();
                        let sudo = parts.next() == Some("sudo");
                        Some(ExtraUser { username, password, sudo })
                    })
                    .collect();
                let static_ip = match (static_ip, gateway) {
                    (Some(address), Some(gw)) => {
                        Some(StaticIp { address, gateway: gw, dns: dns.unwrap_or_default() })
                    }
                    _ => None,
                };
                InstallPlan {
                    disk,
                    install_mode: mode,
                    alongside_gib,
                    filesystem,
                    swap,
                    swap_size_gib,
                    manifest: file.to_string_lossy().to_string(),
                    answers: answers_vec,
                    account,
                    extra_users,
                    hostname,
                    encrypt_mode,
                    encrypt_passphrase: passphrase.unwrap_or_default(),
                    root_gib,
                    lvm,
                    raid1_disk,
                    timezone,
                    locale,
                    keymap,
                    root_password,
                    autologin,
                    install_nvidia,
                    install_printing,
                    post_install_script: post_script,
                    static_ip,
                    proxy,
                    vlan_id,
                    vlan_parent,
                }
            };
            installer::execute(&plan, &Ctx::new(dry_run))?;
            if !dry_run && !no_reboot {
                installer::finish_and_reboot();
            }
            Ok(())
        }
        Command::Export | Command::Diff { .. } => {
            anyhow::bail!("not implemented yet — planned for Phase 5 (export/diff)")
        }
    }
}

/// Read a manifest, run its survey (using `answers` when unattended), inject the
/// answers, and fold in any conditional packages. Shared by `install` and
/// `sync`, which differ only in what they do with the resulting manifest.
fn load_manifest(file: &std::path::Path, answers: Option<&std::path::Path>) -> Result<Manifest> {
    let raw = std::fs::read_to_string(file)
        .with_context(|| format!("reading manifest at {}", file.display()))?;
    let answered = survey::collect(&raw, answers)?;
    let substituted = survey::substitute(&raw, &answered);
    let mut manifest = Manifest::from_str(&substituted)?;
    let extra = survey::conditional_packages(&manifest.conditional_packages, &answered);
    if !extra.is_empty() {
        println!("survey: +{} conditional package(s)", extra.len());
        manifest.packages.extend(extra);
    }
    Ok(manifest)
}
