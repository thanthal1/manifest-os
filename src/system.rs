//! Base system configuration: hostname, timezone, locale and console keymap.
//!
//! These are applied with **file operations**, not `hostnamectl` /
//! `timedatectl` / `localectl`. The systemd tools need a running system bus and
//! fail inside `arch-chroot` (and in a container) — but the ISO installer runs
//! exactly there, in a chroot over the freshly pacstrapped root. Writing the
//! files directly works in both a live system and a chroot, which keeps this
//! logic reusable by the future ISO layer unchanged.
//!
//! Every step is idempotent: re-running overwrites the same files / re-checks
//! the same `locale.gen` line.

use crate::exec::Ctx;
use crate::manifest::System;
use anyhow::{bail, Result};

pub fn apply(system: &System, ctx: &Ctx) -> Result<()> {
    if let Some(tz) = &system.timezone {
        set_timezone(tz, ctx)?;
    }
    if let Some(locale) = &system.locale {
        set_locale(locale, ctx)?;
    }
    if let Some(keymap) = &system.keymap {
        set_keymap(keymap, ctx)?;
    }
    if let Some(hostname) = &system.hostname {
        set_hostname(hostname, ctx)?;
    }
    Ok(())
}

/// Symlink /etc/localtime to the zoneinfo entry, then sync the hardware clock.
fn set_timezone(tz: &str, ctx: &Ctx) -> Result<()> {
    println!("  · timezone: {tz}");
    let target = format!("/usr/share/zoneinfo/{tz}");
    // Validate on a real run (skipped in dry-run, where checks return false).
    if !ctx.dry_run && !ctx.check("test", &["-e", &target]) {
        bail!("unknown timezone `{tz}` — no such file {target}");
    }
    ctx.sudo("ln", &["-sf", &target, "/etc/localtime"])?;
    // RTC sync — best-effort. Fails where there is no hardware clock (some
    // containers/chroots); that must not abort the install.
    if let Err(e) = ctx.sudo("hwclock", &["--systohc"]) {
        println!("  ! hwclock skipped (no hardware clock?): {e}");
    }
    Ok(())
}

/// Enable the locale in /etc/locale.gen, generate it, and set it as LANG.
fn set_locale(locale: &str, ctx: &Ctx) -> Result<()> {
    println!("  · locale: {locale}");
    // Uncomment the matching line in /etc/locale.gen (e.g. `#en_US.UTF-8 UTF-8`).
    // No-op on re-run once the line is already uncommented.
    ctx.sudo(
        "sed",
        &["-i", &format!("/^#{locale} /s/^#//"), "/etc/locale.gen"],
    )?;
    ctx.sudo("locale-gen", &[])?;
    ctx.write_root("/etc/locale.conf", &format!("LANG={locale}\n"))?;
    Ok(())
}

/// Set the console (TTY) keymap.
fn set_keymap(keymap: &str, ctx: &Ctx) -> Result<()> {
    println!("  · keymap: {keymap}");
    ctx.write_root("/etc/vconsole.conf", &format!("KEYMAP={keymap}\n"))
}

/// Write /etc/hostname and the canonical /etc/hosts loopback block.
fn set_hostname(hostname: &str, ctx: &Ctx) -> Result<()> {
    println!("  · hostname: {hostname}");
    ctx.write_root("/etc/hostname", &format!("{hostname}\n"))?;
    let hosts = format!(
        "127.0.0.1\tlocalhost\n\
         ::1\t\tlocalhost\n\
         127.0.1.1\t{hostname}.localdomain {hostname}\n"
    );
    ctx.write_root("/etc/hosts", &hosts)
}
