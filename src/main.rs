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
use manifest::{desktop, diff, export, history, install, installer, kernel, survey, tui};
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
// One instance, parsed once at startup — the size gap between `Provision` and
// the small variants doesn't matter, and boxing would clutter the flag structs.
#[allow(clippy::large_enum_variant)]
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
    /// Capture the running system into a manifest.json: explicitly-installed
    /// packages, desktop, system settings, repos, users and services. Prints to
    /// stdout by default.
    Export {
        /// Write the manifest to this file instead of stdout.
        #[arg(long, short)]
        output: Option<PathBuf>,
        /// Also install a pacman hook that regenerates the system manifest
        /// (/etc/manifest-os/system.json) after every package change.
        #[arg(long)]
        install_hook: bool,
    },
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
    /// Targeted re-apply for settings / config-only edits: regenerate the
    /// config the manifest declares (theme, wallpaper, scale, keybindings,
    /// files, snippets, defaults) without the slow steps — no `-Syu`, no paru,
    /// no package installs. Checks the diff first and falls back to a full
    /// `sync` automatically if the edit actually changed packages, the desktop,
    /// users or services. This is what the Settings app uses.
    Reconfigure {
        /// Path to a manifest.json.
        file: PathBuf,
        /// Print every step without executing anything.
        #[arg(long)]
        dry_run: bool,
        /// JSON object of survey answers ({"id": value}) for unattended runs.
        #[arg(long)]
        answers: Option<PathBuf>,
    },
    /// Preview what applying a manifest would change, compared to the
    /// last-applied one. Read-only — makes no changes.
    Diff {
        /// Path to a manifest.json.
        file: PathBuf,
        /// JSON object of survey answers ({"id": value}) for unattended diffs.
        #[arg(long)]
        answers: Option<PathBuf>,
    },
    /// List the manifests applied to this system (the rollback history).
    History,
    /// Undo a manifest change: re-apply a previously-recorded manifest.
    /// Defaults to the one before the current; pass a git ref or "N applies
    /// ago" as a bare number (e.g. `manifest rollback 2`).
    Rollback {
        /// Which recorded manifest to restore (default: the previous one).
        reference: Option<String>,
        /// Preview the rollback without changing anything.
        #[arg(long)]
        dry_run: bool,
    },
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
        /// Don't install the System Snapshots desktop app (for headless/server
        /// installs with no GUI).
        #[arg(long)]
        no_desktop_app: bool,
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
            let (manifest, json) = load_manifest(&file, answers.as_deref())?;
            let ctx = Ctx::new(dry_run);
            install::run(&manifest, &ctx)?;
            history::record(&json, &manifest.meta.name, &ctx);
            Ok(())
        }
        Command::Sync {
            file,
            dry_run,
            answers,
        } => {
            let (mut manifest, json) = load_manifest(&file, answers.as_deref())?;
            // Applying a manifest (incl. a shared setup, via the desktop app)
            // should never drop what you already have: fold this system's
            // explicitly-installed packages into the manifest so it stays a
            // complete record and a future prune won't remove them. Sync-only —
            // a fresh install has no meaningful "existing" packages.
            let json = fold_existing_packages(&mut manifest, json);
            let ctx = Ctx::new(dry_run);
            install::sync(&manifest, &ctx)?;
            history::record(&json, &manifest.meta.name, &ctx);
            Ok(())
        }
        Command::Reconfigure {
            file,
            dry_run,
            answers,
        } => {
            let (mut manifest, json) = load_manifest(&file, answers.as_deref())?;
            let json = fold_existing_packages(&mut manifest, json);
            let ctx = Ctx::new(dry_run);
            // Use the diff against the last-applied manifest to decide: if only
            // config/variables changed, do the fast targeted re-apply; if the
            // edit changed packages/desktop/users/services, fall back to a full
            // sync so nothing new is left uninstalled.
            let current = history::current();
            if diff::requires_full_apply(&manifest, current.as_ref()) {
                println!("→ this edit changes packages/desktop/services — running a full sync\n");
                install::sync(&manifest, &ctx)?;
            } else {
                install::reconfigure(&manifest, &ctx)?;
            }
            history::record(&json, &manifest.meta.name, &ctx);
            Ok(())
        }
        Command::Diff { file, answers } => {
            let (manifest, _) = load_manifest(&file, answers.as_deref())?;
            diff::run(&manifest, history::current().as_ref());
            Ok(())
        }
        Command::History => history::show(),
        Command::Rollback { reference, dry_run } => {
            history::rollback(reference.as_deref(), dry_run)
        }
        Command::Verify { file } => {
            let raw = std::fs::read_to_string(&file)
                .with_context(|| format!("reading manifest at {}", file.display()))?;
            // Expand plugin blocks first, so an unknown block is a verify error
            // and the reported package count includes what plugins contribute.
            let expanded = manifest::plugins::expand(&raw)?;
            let manifest = Manifest::from_str(&expanded)?;
            let kernel = kernel::resolve(manifest.system.kernel.as_deref())?;
            let default_note = if manifest.system.kernel.is_none() {
                " (default)"
            } else {
                ""
            };
            let plugin_note = if expanded != raw {
                " (plugins expanded)"
            } else {
                ""
            };
            println!(
                "✓ valid — schema v{}, {} package(s), kernel: {}{}{}",
                manifest.schema_version,
                manifest.packages.len(),
                kernel.package,
                default_note,
                plugin_note,
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
            no_desktop_app,
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
                    (None, None) => None,
                    // Don't silently finish an install missing its admin account.
                    _ => anyhow::bail!("--user and --password must be given together"),
                };
                // "name:password[:sudo]"
                let mut extra_users: Vec<ExtraUser> = Vec::new();
                for s in &extra_user {
                    let mut parts = s.splitn(3, ':');
                    match (parts.next(), parts.next()) {
                        (Some(username), Some(password)) if !username.is_empty() => {
                            extra_users.push(ExtraUser {
                                username: username.to_string(),
                                password: password.to_string(),
                                sudo: parts.next() == Some("sudo"),
                            });
                        }
                        _ => anyhow::bail!(
                            "bad --extra-user `{s}` (expected name:password or name:password:sudo)"
                        ),
                    }
                }
                let static_ip = match (static_ip, gateway) {
                    (Some(address), Some(gw)) => {
                        Some(StaticIp { address, gateway: gw, dns: dns.unwrap_or_default() })
                    }
                    (None, None) => None,
                    _ => anyhow::bail!("--static-ip and --gateway must be given together"),
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
                    skip_desktop_app: no_desktop_app,
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
        Command::Export {
            output,
            install_hook,
        } => export::run(output.as_deref(), install_hook, &Ctx::new(false)),
    }
}

/// Read a manifest, run its survey (using `answers` when unattended), inject the
/// answers, and fold in any conditional packages. Returns the parsed manifest
/// plus the final substituted JSON (recorded into the rollback history so a
/// re-apply reproduces exactly this state). Shared by `install` and `sync`.
fn load_manifest(
    file: &std::path::Path,
    answers: Option<&std::path::Path>,
) -> Result<(Manifest, String)> {
    let raw = std::fs::read_to_string(file)
        .with_context(|| format!("reading manifest at {}", file.display()))?;
    let mut answered = survey::collect(&raw, answers)?;
    // Let auto-detected hardware facts (`{{gpu}}`, `{{scale}}`, …) fill tokens
    // too, at lower priority than survey answers / variables. Detect without
    // manifest `detect` overrides here — those aren't parsed yet and only
    // matter to `when`, which re-detects below.
    answered.add_base_facts(
        manifest::conditions::Facts::detect(&std::collections::BTreeMap::new()).pairs(),
    );
    let substituted = survey::substitute(&raw, &answered);
    // Expand any plugin blocks (`docker`, `tailscale`, …) into core primitives
    // before parsing, so nothing downstream — including the recorded/rollback
    // JSON — needs to know a plugin was ever involved.
    let substituted = manifest::plugins::expand(&substituted)?;
    let mut manifest = Manifest::from_str(&substituted)?;
    let extra = survey::conditional_packages(&manifest.conditional_packages, &answered);
    let mut recorded = substituted.clone();
    if !extra.is_empty() {
        println!("survey: +{} conditional package(s)", extra.len());
        manifest.packages.extend(extra.iter().cloned());
        // Fold the survey-gated packages into the JSON we record, so a rollback
        // restores the exact package set without re-running the survey.
        recorded = merge_conditional_packages(&substituted, &extra).unwrap_or(substituted);
    }

    // Resolve `when` conditions: fold matching `conditional` overlays in and
    // drop `when`-gated files that don't apply, evaluated against survey/
    // variable answers plus auto-detected hardware (gpu/cpu/virt/firmware). The
    // recorded JSON keeps the raw conditionals so a rollback re-evaluates them.
    let mut facts = manifest::conditions::Facts::detect(&manifest.detect);
    facts.overlay(answered.pairs());
    manifest::conditions::resolve(&mut manifest, &facts);

    Ok((manifest, recorded))
}

/// Fold the packages already explicitly installed on this system into the
/// manifest (and the JSON that gets recorded), so a sync never forgets what's
/// already here. Uses the same capture as `manifest export` — explicit installs
/// minus the base system, the desktop recipe's own packages and the kernel — so
/// only *your* chosen apps are added, not recipe-implied ones. On a non-Arch box
/// (no pacman) capture yields nothing, so this is a no-op.
fn fold_existing_packages(manifest: &mut Manifest, json: String) -> String {
    let existing = export::capture_manifest().packages;
    let extra: Vec<String> = existing
        .into_iter()
        .filter(|p| !manifest.packages.contains(p))
        .collect();
    if extra.is_empty() {
        return json;
    }
    println!("sync: keeping {} package(s) already installed on this system", extra.len());
    manifest.packages.extend(extra.iter().cloned());
    merge_conditional_packages(&json, &extra).unwrap_or(json)
}

/// Return `json` with `extra` appended to its top-level `packages` array.
fn merge_conditional_packages(json: &str, extra: &[String]) -> Result<String> {
    let mut v: serde_json::Value = serde_json::from_str(json)?;
    let pkgs = v
        .as_object_mut()
        .context("manifest is not a JSON object")?
        .entry("packages")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    if let Some(arr) = pkgs.as_array_mut() {
        for p in extra {
            arr.push(serde_json::Value::String(p.clone()));
        }
    }
    Ok(serde_json::to_string_pretty(&v)? + "\n")
}
