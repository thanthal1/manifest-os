//! The install executor — turns an [`InstallPlan`] from the TUI into a real
//! system: partition → format → mount → pacstrap → `manifest install`.
//!
//! This is the disk/base layer the manifest deliberately does NOT own. It
//! mirrors, exactly, the steps proven by hand during development:
//!   sfdisk → mkfs → mount → pacstrap base → genfstab → bootstrap user →
//!   run `manifest install <chosen>` inside arch-chroot (which then does repos,
//!   paru, packages, the desktop, users, files and the bootloader).
//!
//! It is destructive: it erases the selected disk. The TUI confirms first.

use crate::exec::Ctx;
use crate::tui::InstallPlan;
use anyhow::{bail, Context, Result};
use std::path::Path;

/// Manifest OS identity — replaces the upstream (Arch) os-release so fastfetch,
/// login banners, lsb_release etc. report Manifest OS.
const OS_RELEASE: &str = r#"NAME="Manifest OS"
PRETTY_NAME="Manifest OS"
ID=manifestos
ID_LIKE=arch
BUILD_ID=rolling
ANSI_COLOR="38;2;203;166;247"
HOME_URL="https://manifest.os/"
DOCUMENTATION_URL="https://manifest.os/spec"
SUPPORT_URL="https://manifest.os"
LOGO=manifestos
"#;

/// System-wide fastfetch config: use the Manifest OS logo.
const FASTFETCH_CONF: &str = r#"{
  "logo": { "type": "file", "source": "/usr/share/manifest-os/logo.txt", "padding": { "top": 1, "left": 2 } },
  "display": { "separator": "  " },
  "modules": [ "title", "separator", "os", "kernel", "wm", "packages", "shell", "memory", "break", "colors" ]
}
"#;

/// The Manifest OS logo, embedded at build time.
const LOGO: &str = include_str!("logo.txt");

pub fn execute(plan: &InstallPlan, ctx: &Ctx) -> Result<()> {
    if plan.disk.is_empty() {
        bail!("no disk selected");
    }
    let uefi = Path::new("/sys/firmware/efi").exists();
    println!(
        "\n→ Installing Manifest OS to {} ({})\n",
        plan.disk,
        if uefi { "UEFI" } else { "BIOS" }
    );

    ensure_keyring(ctx)?;
    partition(&plan.disk, uefi, ctx)?;
    let (root_part, esp_part) = partition_names(&plan.disk, uefi);
    format_disks(&root_part, esp_part.as_deref(), &plan.filesystem, ctx)?;
    mount(&root_part, esp_part.as_deref(), ctx)?;

    pacstrap(ctx)?;
    ctx.shell("genfstab -U /mnt >> /mnt/etc/fstab", true)?;
    brand_system(ctx)?;
    create_bootstrap_user(ctx)?;

    let manifest_in_root = stage_manifest(&plan.manifest, ctx)?;
    stage_binary(ctx)?;
    run_manifest(&manifest_in_root, ctx)?;

    println!("\n✓ Manifest OS installed. Remove the media and reboot.");
    Ok(())
}

/// Make sure the live keyring is populated so pacstrap can verify signatures.
/// On some boots `pacman-init` hasn't run (e.g. mangled enablement), leaving an
/// empty keyring; init+populate is idempotent and cheap.
fn ensure_keyring(ctx: &Ctx) -> Result<()> {
    step("Preparing package keyring");
    ctx.sudo("pacman-key", &["--init"])?;
    ctx.sudo("pacman-key", &["--populate", "archlinux"])?;
    Ok(())
}

/// Wipe and partition the disk. BIOS gets one Linux partition on an MBR; UEFI
/// gets a GPT with an ESP plus a root partition.
fn partition(disk: &str, uefi: bool, ctx: &Ctx) -> Result<()> {
    step("Partitioning");
    let layout = if uefi {
        "label: gpt\n,550M,U\n,,L\n"
    } else {
        "label: dos\n,,L,*\n"
    };
    ctx.shell(&format!("printf '{layout}' | sfdisk --force {disk}"), true)
}

/// Partition device paths, accounting for the `p` separator on nvme/mmc.
fn partition_names(disk: &str, uefi: bool) -> (String, Option<String>) {
    let sep = if disk.chars().last().map(|c| c.is_ascii_digit()).unwrap_or(false) {
        "p"
    } else {
        ""
    };
    if uefi {
        (format!("{disk}{sep}2"), Some(format!("{disk}{sep}1")))
    } else {
        (format!("{disk}{sep}1"), None)
    }
}

fn format_disks(root: &str, esp: Option<&str>, fs: &str, ctx: &Ctx) -> Result<()> {
    step("Formatting");
    if let Some(esp) = esp {
        ctx.sudo("mkfs.fat", &["-F32", esp])?;
    }
    match fs {
        "btrfs" => ctx.sudo("mkfs.btrfs", &["-f", root])?,
        _ => ctx.sudo("mkfs.ext4", &["-F", root])?,
    }
    Ok(())
}

fn mount(root: &str, esp: Option<&str>, ctx: &Ctx) -> Result<()> {
    ctx.sudo("mount", &[root, "/mnt"])?;
    if let Some(esp) = esp {
        ctx.sudo("mkdir", &["-p", "/mnt/boot"])?;
        ctx.sudo("mount", &[esp, "/mnt/boot"])?;
    }
    Ok(())
}

fn pacstrap(ctx: &Ctx) -> Result<()> {
    step("Installing base system (pacstrap)");
    // `mkinitcpio` is named explicitly: `base` pulls a virtual `initramfs`
    // with three providers, which otherwise triggers a prompt that fails
    // non-interactively.
    ctx.sudo(
        "pacstrap",
        &["-K", "/mnt", "base", "linux", "linux-firmware", "mkinitcpio", "sudo"],
    )
}

/// Write the Manifest OS identity into the new root: os-release (so fastfetch
/// & friends say "Manifest OS"), the logo, and a fastfetch config that uses it.
fn brand_system(ctx: &Ctx) -> Result<()> {
    step("Branding the system (Manifest OS)");
    // /etc/os-release is a symlink to /usr/lib/os-release; write the target.
    ctx.write_root("/mnt/usr/lib/os-release", OS_RELEASE)?;
    ctx.write_root("/mnt/usr/share/manifest-os/logo.txt", LOGO)?;
    ctx.write_root("/mnt/etc/xdg/fastfetch/config.jsonc", FASTFETCH_CONF)?;
    Ok(())
}

/// A throwaway sudo user inside the new root, so `manifest install` can run as
/// non-root (paru/makepkg refuse root). The manifest's own `users` block
/// creates the real daily account.
fn create_bootstrap_user(ctx: &Ctx) -> Result<()> {
    step("Preparing installer account");
    ctx.shell(
        "arch-chroot /mnt bash -c 'id installer >/dev/null 2>&1 || useradd -m -G wheel installer; \
         echo \"installer ALL=(ALL) NOPASSWD: ALL\" > /etc/sudoers.d/00-installer'",
        true,
    )
}

/// Place the chosen manifest somewhere the install runs from. Uses /etc (root-
/// owned, world-readable) so the non-root installer account can read it — NOT
/// /root, which it cannot. Accepts a bundled name, a local path, or an http(s)
/// URL.
fn stage_manifest(choice: &str, ctx: &Ctx) -> Result<String> {
    step("Staging manifest");
    let dest = "/mnt/etc/manifest-install.json";
    if choice.starts_with("http://") || choice.starts_with("https://") {
        ctx.sudo("curl", &["-fsSL", "-o", dest, choice])?;
    } else {
        // A bundled name resolves to the examples shipped on the ISO.
        let src = if Path::new(choice).is_file() {
            choice.to_string()
        } else {
            format!("/usr/share/manifest-os/examples/{choice}.json")
        };
        ctx.sudo("cp", &[&src, dest])?;
    }
    Ok("/etc/manifest-install.json".to_string())
}

/// Copy this very binary into the new root so it can run inside the chroot.
fn stage_binary(ctx: &Ctx) -> Result<()> {
    let exe = std::env::current_exe().context("locating the manifest binary")?;
    let exe = exe.to_string_lossy();
    ctx.sudo("install", &["-Dm755", &exe, "/mnt/usr/local/bin/manifest"])
}

/// Run the manifest inside the new root, as the bootstrap user.
fn run_manifest(manifest_in_root: &str, ctx: &Ctx) -> Result<()> {
    step("Applying the manifest");
    ctx.shell(
        &format!("arch-chroot /mnt runuser -l installer -c 'manifest install {manifest_in_root}'"),
        true,
    )
}

fn step(title: &str) {
    println!("\n[{title}]");
}
