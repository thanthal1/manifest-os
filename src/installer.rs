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
use crate::probe::InstallPlan;
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
    setup_install_swap(&plan.swap, ctx)?;

    pacstrap(ctx)?;
    ctx.shell("genfstab -U /mnt >> /mnt/etc/fstab", true)?;
    brand_system(ctx)?;
    create_bootstrap_user(ctx)?;

    let manifest_in_root = stage_manifest(&plan.manifest, ctx)?;
    ensure_boot_block(ctx)?;
    stage_binary(ctx)?;
    run_manifest(&manifest_in_root, ctx)?;
    create_account(plan, ctx)?;
    finalize_boot(uefi, ctx);

    println!("\n✓ Manifest OS installed.");
    Ok(())
}

/// Create the daily-driver account a friendly (GUI) install collected, inside
/// the new system: add the user to `wheel`, enable wheel-sudo, and set the
/// password via stdin (never logged or written to disk). No-op for CLI/TUI
/// installs that pass no account — there the manifest declares its own users.
fn create_account(plan: &InstallPlan, ctx: &Ctx) -> Result<()> {
    let Some(acct) = plan.account.as_ref() else {
        return Ok(());
    };
    let user = sanitize_username(&acct.username);
    if user.is_empty() {
        bail!("the account username is empty or invalid");
    }
    step("Creating your account");
    // useradd + wheel-sudo. These lines carry no secret, so logging is fine; the
    // sanitized username contains no shell metacharacters.
    ctx.shell(
        &format!(
            "arch-chroot /mnt bash -c 'id {user} >/dev/null 2>&1 || \
             useradd -m -G wheel -s /bin/bash {user}'"
        ),
        true,
    )?;
    ctx.write_root(
        "/mnt/etc/sudoers.d/10-wheel",
        "# Managed by Manifest OS — let the wheel group use sudo\n%wheel ALL=(ALL:ALL) ALL\n",
    )?;
    ctx.set_password_chroot("/mnt", &user, &acct.password)?;
    println!("  · created administrator account `{user}`");
    Ok(())
}

/// Keep only safe username characters (lowercase letters, digits, `_`, `-`) — a
/// conservative subset of useradd's NAME_REGEX, also making the name safe to
/// interpolate into the chroot command above.
fn sanitize_username(raw: &str) -> String {
    raw.trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_' || *c == '-')
        .collect()
}

/// Sync, unmount the target, and reboot — no prompt. For the GUI, which shows
/// its own firmware-appropriate "you can remove the media" guidance on screen
/// (it has no stdin to prompt on, unlike [`finish_and_reboot`]).
pub fn reboot() {
    use std::process::Command;
    let _ = Command::new("sync").status();
    let _ = Command::new("umount").args(["-R", "/mnt"]).status();
    if Command::new("systemctl").arg("reboot").status().is_err() {
        let _ = Command::new("reboot").status();
    }
}

/// Whether the live system booted via UEFI (vs legacy BIOS) — lets the GUI's
/// final screen tell the user whether the install media can stay or must come out.
pub fn is_uefi() -> bool {
    Path::new("/sys/firmware/efi").exists()
}

/// After a successful install, show a completion screen and reboot into the new
/// system, handling the install media as cleanly as the firmware allows:
///
///   * **UEFI (VM or hardware)** — `finalize_boot` made the new system the first
///     EFI boot entry, so the firmware boots it even with the install media still
///     attached. Fully hands-off; we just reboot.
///   * **Legacy BIOS (VM or hardware)** — there is no firmware boot-order API to
///     prefer the disk, and a live medium cannot eject itself while it is in use
///     (the OS reads its squashfs from it on demand). So the medium must be
///     removed by hand; we ask, wording it for a VM vs a USB.
pub fn finish_and_reboot() {
    use std::process::Command;

    let uefi = Path::new("/sys/firmware/efi").exists();
    let in_vm = Command::new("systemd-detect-virt")
        .args(["-q", "--vm"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    println!("\n  ╭───────────────────────────────────────────────╮");
    println!("  │   ✓  Manifest OS installed successfully!       │");
    println!("  ╰───────────────────────────────────────────────╯");

    // Flush, then cleanly unmount the freshly-installed disk before rebooting.
    let _ = Command::new("sync").status();
    let _ = Command::new("umount").args(["-R", "/mnt"]).status();

    if uefi {
        println!("\n  Set as the default boot entry — rebooting into Manifest OS.");
        println!("  (You can leave the install media attached.)");
    } else {
        use std::io::Write;
        let how = if in_vm {
            "detach the ISO from this VM's optical drive"
        } else {
            "unplug the install USB"
        };
        print!("\n  Installed. Please {how}, then press Enter to reboot. ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok();
    }

    // systemctl reboot on a booted system; reboot(8) as a fallback.
    if Command::new("systemctl").arg("reboot").status().is_err() {
        let _ = Command::new("reboot").status();
    }
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
    let sep = if disk
        .chars()
        .last()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
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

/// The TUI defaults to zram so low-memory machines have breathing room while
/// pacstrap and AUR builds run. This is install-time swap only; the installed
/// system can still declare its own persistent swap/zram policy later.
fn setup_install_swap(choice: &str, ctx: &Ctx) -> Result<()> {
    match choice {
        "zram" => {
            step("Preparing low-memory swap");
            let script = r#"
if swapon --show=NAME | grep -qx /dev/zram0; then
    echo "  · zram swap already active"
    exit 0
fi

modprobe zram num_devices=1 2>/dev/null || modprobe zram 2>/dev/null || true
if [ ! -e /sys/block/zram0/disksize ]; then
    echo "  · zram is unavailable on this kernel; continuing without install swap"
    exit 0
fi

mem_kb=$(awk '/MemTotal/ { print $2 }' /proc/meminfo)
size_mb=$((mem_kb / 1024 * 2))
[ "$size_mb" -lt 2048 ] && size_mb=2048
[ "$size_mb" -gt 8192 ] && size_mb=8192

swapoff /dev/zram0 2>/dev/null || true
echo 1 > /sys/block/zram0/reset 2>/dev/null || true
echo lz4 > /sys/block/zram0/comp_algorithm 2>/dev/null || true
echo "${size_mb}M" > /sys/block/zram0/disksize
mkswap /dev/zram0 >/dev/null
swapon -p 100 /dev/zram0
echo "  · enabled ${size_mb}M compressed zram swap"
"#;
            ctx.shell(script, true)
        }
        "none" => {
            println!("  · install swap disabled");
            Ok(())
        }
        other => {
            println!("  · unknown swap choice `{other}`; continuing without install swap");
            Ok(())
        }
    }
}

fn pacstrap(ctx: &Ctx) -> Result<()> {
    step("Installing base system (pacstrap)");
    // `mkinitcpio` is named explicitly: `base` pulls a virtual `initramfs`
    // with three providers, which otherwise triggers a prompt that fails
    // non-interactively.
    ctx.sudo(
        "pacstrap",
        &[
            "-K",
            "/mnt",
            "base",
            "linux",
            "linux-firmware",
            "mkinitcpio",
            "sudo",
        ],
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

/// Guarantee the installed system can boot. The manifest's `boot` block is
/// optional — it's an opt-in customization for re-applying to a daily-driver —
/// but a fresh disk install MUST have a bootloader, or the machine drops back to
/// the install media. If the staged manifest declares no bootloader, inject a
/// default GRUB block. GRUB auto-detects UEFI vs BIOS (see `boot.rs`) and boots
/// either, so it is the safe universal default. Best-effort: never fail the
/// install over this (the manifest may still carry its own loader).
fn ensure_boot_block(ctx: &Ctx) -> Result<()> {
    step("Ensuring a bootloader");
    if ctx.dry_run {
        println!("  · would add a default GRUB boot block if the manifest declares none");
        return Ok(());
    }
    let path = "/mnt/etc/manifest-install.json";
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(e) => {
            println!("  · skip: cannot read staged manifest ({e})");
            return Ok(());
        }
    };
    let mut doc: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(d) => d,
        Err(e) => {
            println!("  · skip: staged manifest is not plain JSON ({e})");
            return Ok(());
        }
    };
    match doc.as_object_mut() {
        Some(obj) if !obj.contains_key("boot") => {
            obj.insert("boot".to_string(), serde_json::json!({ "loader": "grub" }));
            let out = serde_json::to_string_pretty(&doc).context("re-serializing manifest")?;
            std::fs::write(path, out).with_context(|| format!("writing {path}"))?;
            println!("  · no bootloader declared — added a default GRUB boot block");
        }
        Some(_) => println!("  · manifest declares its own bootloader — leaving it"),
        None => println!("  · skip: staged manifest is not a JSON object"),
    }
    Ok(())
}

/// Make the installed system the firmware's preferred boot target, so a reboot
/// lands on it instead of the install media. On UEFI we move our boot entry to
/// the front of the EFI `BootOrder` (works even if the USB is left plugged in).
/// The VM optical-disc eject is handled at reboot time (`finish_and_reboot`),
/// and legacy BIOS has no firmware boot-order API, so neither is done here.
/// Best-effort: a failure here never fails the install.
fn finalize_boot(uefi: bool, ctx: &Ctx) {
    if !uefi {
        return;
    }
    step("Setting UEFI boot priority");
    // grub-install / bootctl created an entry labelled "GRUB" or "Linux Boot
    // Manager"; put its number first in BootOrder, ahead of the install media.
    let script = "command -v efibootmgr >/dev/null 2>&1 || exit 0\n\
        n=$(efibootmgr | sed -n 's/^Boot\\([0-9A-Fa-f]\\{4\\}\\)\\* \\(GRUB\\|Linux Boot Manager\\)$/\\1/p' | head -n1)\n\
        [ -z \"$n\" ] && exit 0\n\
        rest=$(efibootmgr | sed -n 's/^BootOrder: //p' | tr ',' '\\n' | grep -vix \"$n\" | paste -sd, -)\n\
        if [ -n \"$rest\" ]; then efibootmgr -o \"$n,$rest\" >/dev/null; else efibootmgr -o \"$n\" >/dev/null; fi\n\
        echo \"  · made boot entry $n the UEFI default\"";
    let _ = ctx.shell(script, true);
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
