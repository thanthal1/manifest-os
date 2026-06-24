//! Bootloader installation and configuration.
//!
//! This is what turns "a kernel is installed" into "the machine boots it". It
//! supports the two bootloaders almost everyone on Arch uses:
//!
//!   * **systemd-boot** — UEFI only, no config language; we run `bootctl
//!     install` and hand-write `loader.conf` + one entry per kernel.
//!   * **GRUB** — UEFI or BIOS; we set `/etc/default/grub`, run `grub-install`
//!     for the detected firmware, then `grub-mkconfig`.
//!
//! It is designed to run in the **install/chroot context**, where `/` is the
//! target root and the ESP is mounted at `esp` (default `/boot`). It is opt-in:
//! nothing here runs unless the manifest has a `boot` block, because
//! reinstalling a bootloader on a healthy daily-driver is rarely what you want.
//!
//! NOTE: this is the one area that cannot be exercised in a container (no ESP,
//! no real disk, no EFI variables) — it must be tested in a VM.

use crate::exec::Ctx;
use crate::kernel::Kernel;
use crate::manifest::Boot;
use anyhow::{bail, Result};
use std::path::Path;

#[derive(Clone, Copy)]
enum Firmware {
    Uefi,
    Bios,
}

impl Firmware {
    fn label(self) -> &'static str {
        match self {
            Firmware::Uefi => "UEFI",
            Firmware::Bios => "BIOS",
        }
    }
}

pub fn apply(boot: &Boot, kernel: &Kernel, ctx: &Ctx) -> Result<()> {
    let fw = detect_firmware(ctx);
    println!("  · firmware: {}", fw.label());
    match boot.loader.as_str() {
        "systemd-boot" => systemd_boot(boot, kernel, fw, ctx),
        "grub" => grub(boot, fw, ctx),
        other => bail!("unknown bootloader `{other}` (systemd-boot|grub)"),
    }
}

/// UEFI systems expose `/sys/firmware/efi`. In dry-run we assume UEFI, which is
/// the overwhelmingly common case on modern hardware.
fn detect_firmware(ctx: &Ctx) -> Firmware {
    if ctx.dry_run {
        return Firmware::Uefi;
    }
    if Path::new("/sys/firmware/efi").exists() {
        Firmware::Uefi
    } else {
        Firmware::Bios
    }
}

// ---------------------------------------------------------------------------
// systemd-boot
// ---------------------------------------------------------------------------

fn systemd_boot(boot: &Boot, kernel: &Kernel, fw: Firmware, ctx: &Ctx) -> Result<()> {
    if let Firmware::Bios = fw {
        bail!("systemd-boot requires UEFI, but this system is BIOS — use `loader: \"grub\"`");
    }
    println!("  · bootloader: systemd-boot");
    ctx.sudo("bootctl", &["install"])?;

    let ucode = detect_ucode();
    if let Some(u) = ucode {
        ctx.sudo("pacman", &["-S", "--needed", "--noconfirm", u])?;
    }

    let entry_id = format!("manifest-{}", kernel.package);
    let timeout = boot.timeout.unwrap_or(3);
    let loader_conf = format!(
        "default {entry_id}.conf\ntimeout {timeout}\nconsole-mode keep\neditor no\n"
    );
    ctx.write_root(&format!("{}/loader/loader.conf", boot.esp), &loader_conf)?;

    let root = root_param(ctx)?;
    let mut options = format!("root={root} rw");
    for c in &boot.cmdline {
        options.push(' ');
        options.push_str(c);
    }

    let mut entry = format!(
        "title   Manifest OS ({})\nlinux   /vmlinuz-{}\n",
        kernel.display, kernel.package
    );
    // Microcode initrd must precede the main initramfs.
    if let Some(u) = ucode {
        entry.push_str(&format!("initrd  /{u}.img\n"));
    }
    entry.push_str(&format!(
        "initrd  /initramfs-{}.img\noptions {options}\n",
        kernel.package
    ));
    ctx.write_root(&format!("{}/loader/entries/{entry_id}.conf", boot.esp), &entry)?;
    println!("  · wrote loader entry `{entry_id}` (root={root})");
    Ok(())
}

/// The `root=` parameter for a kernel entry: prefer a stable PARTUUID over the
/// kernel-assigned device name.
fn root_param(ctx: &Ctx) -> Result<String> {
    if ctx.dry_run {
        return Ok("PARTUUID=<resolved-at-install>".to_string());
    }
    let dev = ctx.output(false, "findmnt", &["-no", "SOURCE", "/"])?;
    if dev.is_empty() {
        bail!("could not determine the root device (findmnt returned nothing)");
    }
    let partuuid = ctx
        .output(true, "blkid", &["-s", "PARTUUID", "-o", "value", &dev])
        .unwrap_or_default();
    Ok(if partuuid.is_empty() {
        dev
    } else {
        format!("PARTUUID={partuuid}")
    })
}

// ---------------------------------------------------------------------------
// GRUB
// ---------------------------------------------------------------------------

fn grub(boot: &Boot, fw: Firmware, ctx: &Ctx) -> Result<()> {
    println!("  · bootloader: grub");
    let mut pkgs = vec!["-S", "--needed", "--noconfirm", "grub"];
    if let Firmware::Uefi = fw {
        pkgs.push("efibootmgr");
    }
    ctx.sudo("pacman", &pkgs)?;

    if let Some(t) = boot.timeout {
        ctx.sudo(
            "sed",
            &["-i", &format!("s|^GRUB_TIMEOUT=.*|GRUB_TIMEOUT={t}|"), "/etc/default/grub"],
        )?;
    }
    if !boot.cmdline.is_empty() {
        let line = boot.cmdline.join(" ");
        ctx.sudo(
            "sed",
            &[
                "-i",
                &format!("s|^GRUB_CMDLINE_LINUX_DEFAULT=.*|GRUB_CMDLINE_LINUX_DEFAULT=\"{line}\"|"),
                "/etc/default/grub",
            ],
        )?;
    }

    match fw {
        Firmware::Uefi => ctx.sudo(
            "grub-install",
            &[
                "--target=x86_64-efi",
                &format!("--efi-directory={}", boot.esp),
                "--bootloader-id=GRUB",
            ],
        )?,
        Firmware::Bios => {
            let disk = root_disk(ctx)?;
            ctx.sudo("grub-install", &["--target=i386-pc", &disk])?;
        }
    }
    ctx.sudo("grub-mkconfig", &["-o", "/boot/grub/grub.cfg"])?;
    Ok(())
}

/// The whole disk holding `/`, for BIOS `grub-install` (which targets a disk,
/// not a partition). e.g. root `/dev/sda2` -> `/dev/sda`.
fn root_disk(ctx: &Ctx) -> Result<String> {
    if ctx.dry_run {
        return Ok("/dev/<disk>".to_string());
    }
    let dev = ctx.output(false, "findmnt", &["-no", "SOURCE", "/"])?;
    let pkname = ctx.output(true, "lsblk", &["-no", "PKNAME", &dev]).unwrap_or_default();
    let pkname = pkname.lines().next().unwrap_or("").trim();
    if pkname.is_empty() {
        bail!("could not determine the boot disk for BIOS grub-install");
    }
    Ok(format!("/dev/{pkname}"))
}

/// Detect the CPU microcode package from `/proc/cpuinfo`. Safe to read in any
/// mode; returns `None` off-Linux or on unknown vendors.
fn detect_ucode() -> Option<&'static str> {
    let cpu = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    if cpu.contains("GenuineIntel") {
        Some("intel-ucode")
    } else if cpu.contains("AuthenticAMD") {
        Some("amd-ucode")
    } else {
        None
    }
}
