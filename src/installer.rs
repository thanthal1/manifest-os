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

    // Fail fast — before we touch (and would wipe/shrink) the disk — if we can't
    // reach the package mirrors. pacstrap needs the network; this turns a cryptic
    // "pacstrap exited 1" into a clear message, with the disk still untouched.
    ensure_online(ctx)?;
    fix_clock(ctx);
    ensure_keyring(ctx)?;
    let alongside = plan.install_mode == "alongside";
    // Free the target disk before partitioning: a leftover /mnt mount, active
    // swap, or auto-mounted partitions (often from a previous failed attempt in
    // the same live session) keep the kernel from re-reading the partition table,
    // so sfdisk/sgdisk fail with "Device or resource busy" / atomic-commit errors.
    free_disk(&plan.disk, ctx);
    // When encrypting, the raw partition that holds the LUKS container — kept so
    // we can write crypttab + the cryptdevice= kernel param by its UUID.
    let mut luks_part: Option<String> = None;
    let parts = if alongside {
        if plan.encrypt {
            bail!("disk encryption is only available for a full-disk (erase) install, not alongside an existing OS");
        }
        // Dual boot: shrink Windows and install in the freed space, reusing its
        // ESP. carve_alongside formats only the new root (never the ESP/Windows).
        carve_alongside(plan, ctx)?
    } else {
        // Erase: wipe and lay out the whole disk. A dedicated swap partition
        // (size in GiB) is the only choice that changes the layout; default to
        // 2 GiB if "partition" was chosen without one.
        let swap_part_gib: Option<u32> = if plan.swap == "partition" {
            Some(plan.swap_size_gib.filter(|&g| g > 0).unwrap_or(2))
        } else {
            None
        };
        partition(&plan.disk, uefi, swap_part_gib, ctx)?;
        let mut parts = partition_names(&plan.disk, uefi, swap_part_gib.is_some());
        format_aux(&parts, ctx)?; // ESP + swap (the root is handled below)
        if plan.encrypt {
            setup_luks(&parts.root, &plan.encrypt_passphrase, ctx)?;
            luks_part = Some(parts.root.clone());
            parts.root = LUKS_MAPPER.to_string(); // format + mount the unlocked mapper
        }
        mkfs_root(&parts.root, &plan.filesystem, ctx)?;
        parts
    };
    mount(&parts, ctx)?;
    setup_install_zram(ctx)?; // always-on: keeps low-memory machines off the OOM killer

    pacstrap(ctx)?;
    ctx.shell("genfstab -U /mnt >> /mnt/etc/fstab", true)?;
    install_fs_tools(&plan.filesystem, ctx)?;
    setup_persistent_swap(plan, &parts, ctx)?;
    let luks_systemd = luks_part.is_some() && initramfs_uses_systemd(ctx);
    if let Some(lp) = &luks_part {
        configure_encryption(lp, luks_systemd, ctx)?;
    }
    brand_system(ctx)?;
    create_bootstrap_user(ctx)?;

    let manifest_in_root = stage_manifest(&plan.manifest, ctx)?;
    ensure_boot_block(ctx)?;
    apply_system_overrides(plan, ctx)?;
    if let Some(lp) = &luks_part {
        inject_crypt_cmdline(lp, luks_systemd, ctx)?;
    }
    personalize_manifest(plan, ctx)?;
    let answers = write_answers(plan, ctx)?;
    stage_binary(ctx)?;
    run_manifest(&manifest_in_root, answers.as_deref(), ctx)?;
    create_account(plan, ctx)?;
    configure_root_password(plan, ctx)?;
    if plan.install_nvidia {
        install_nvidia_driver(ctx)?;
    }
    configure_autologin(plan, ctx)?;
    if alongside {
        enable_dual_boot(ctx);
    }
    finalize_boot(uefi, ctx);
    save_install_log(ctx);

    println!("\n✓ Manifest OS installed.");
    Ok(())
}

/// When a friendly install created an account, make the manifest's primary user
/// *be* that account: rename the first declared user, repoint its `/home/<old>`
/// file paths and `<old>:<old>` owners, and drop its password (set securely by
/// [`create_account`]). This is why the chosen account gets the manifest's
/// riced desktop instead of a bare one. Best-effort; never fail the install.
fn personalize_manifest(plan: &InstallPlan, ctx: &Ctx) -> Result<()> {
    let Some(acct) = plan.account.as_ref() else {
        return Ok(());
    };
    let new_user = sanitize_username(&acct.username);
    if new_user.is_empty() {
        return Ok(());
    }
    step("Personalizing the manifest for your account");
    if ctx.dry_run {
        println!("  · would rename the manifest's user to `{new_user}`");
        return Ok(());
    }
    let path = "/mnt/etc/manifest-install.json";
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    let mut doc: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(d) => d,
        Err(_) => return Ok(()),
    };
    // The manifest's primary user (first entry), if any.
    let old_user = doc
        .get("users")
        .and_then(|u| u.as_array())
        .and_then(|a| a.first())
        .and_then(|u| u.get("name"))
        .and_then(|n| n.as_str())
        .map(str::to_string);
    let Some(old_user) = old_user else {
        // No declared user — create_account makes the account from scratch.
        return Ok(());
    };
    if old_user == new_user {
        return Ok(());
    }
    if let Some(u) = doc.get_mut("users").and_then(|u| u.as_array_mut()).and_then(|a| a.first_mut()) {
        if let Some(obj) = u.as_object_mut() {
            obj.insert("name".into(), serde_json::Value::String(new_user.clone()));
            obj.remove("password"); // create_account sets it over stdin
        }
    }
    // Repoint /home/<old>/… file paths and <old>:<old> owners onto the new user.
    if let Some(files) = doc.get_mut("files").and_then(|f| f.as_array_mut()) {
        let old_home = format!("/home/{old_user}/");
        let new_home = format!("/home/{new_user}/");
        let old_owner = format!("{old_user}:{old_user}");
        for f in files.iter_mut().filter_map(|f| f.as_object_mut()) {
            if let Some(p) = f.get("path").and_then(|p| p.as_str()) {
                if let Some(rest) = p.strip_prefix(&old_home) {
                    f.insert("path".into(), serde_json::Value::String(format!("{new_home}{rest}")));
                }
            }
            if let Some(o) = f.get("owner").and_then(|o| o.as_str()) {
                if o == old_owner {
                    f.insert("owner".into(), serde_json::Value::String(format!("{new_user}:{new_user}")));
                }
            }
        }
    }
    let out = serde_json::to_string_pretty(&doc).unwrap_or(raw);
    let _ = std::fs::write(path, out);
    println!("  · manifest user `{old_user}` → `{new_user}` (your account gets its setup)");
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

/// Set root's password when explicitly requested. Root is locked by default —
/// the created account's wheel/sudo membership is the intended way in — so this
/// is strictly opt-in for people who want direct root login. Fed to chpasswd
/// over stdin; never logged or written to disk.
fn configure_root_password(plan: &InstallPlan, ctx: &Ctx) -> Result<()> {
    let Some(pw) = plan.root_password.as_ref().filter(|p| !p.is_empty()) else {
        return Ok(());
    };
    step("Setting the root password");
    ctx.set_password_chroot("/mnt", "root", pw)?;
    println!("  · root password set");
    Ok(())
}

/// The username to auto-login: the account the front-end created, or (for a
/// CLI/TUI install with no account) the manifest's own primary user.
fn autologin_user(plan: &InstallPlan) -> Option<String> {
    if let Some(acct) = &plan.account {
        let u = sanitize_username(&acct.username);
        if !u.is_empty() {
            return Some(u);
        }
    }
    let raw = std::fs::read_to_string("/mnt/etc/manifest-install.json").ok()?;
    let doc: serde_json::Value = serde_json::from_str(&raw).ok()?;
    doc.get("users")?.as_array()?.first()?.get("name")?.as_str().map(str::to_string)
}

/// Skip the login screen for the created account. Detects whichever display
/// manager the manifest set up — via the `display-manager.service` symlink
/// `systemctl enable` creates — and writes its native autologin config; a bare
/// window manager (no DM) falls back to a getty autologin on tty1. Best-effort;
/// never fails the install.
fn configure_autologin(plan: &InstallPlan, ctx: &Ctx) -> Result<()> {
    if !plan.autologin {
        return Ok(());
    }
    if ctx.dry_run {
        println!("\n[Configuring auto-login]\n  · would configure auto-login for the created account");
        return Ok(());
    }
    let Some(user) = autologin_user(plan) else {
        println!("\n[Configuring auto-login]\n  · no account found to log in — skipped");
        return Ok(());
    };
    step("Configuring auto-login");
    let dm = ctx
        .output(
            false,
            "sh",
            &["-c", "basename \"$(readlink -f /mnt/etc/systemd/system/display-manager.service)\" 2>/dev/null"],
        )
        .unwrap_or_default();
    match dm.trim() {
        "gdm.service" => ctx.write_root(
            "/mnt/etc/gdm/custom.conf",
            &format!("[daemon]\nAutomaticLoginEnable=True\nAutomaticLogin={user}\n"),
        )?,
        "sddm.service" => ctx.write_root(
            "/mnt/etc/sddm.conf.d/10-manifest-autologin.conf",
            &format!("[Autologin]\nUser={user}\nSession=\n"),
        )?,
        "lightdm.service" => ctx.write_root(
            "/mnt/etc/lightdm/lightdm.conf.d/60-manifest-autologin.conf",
            &format!("[Seat:*]\nautologin-user={user}\nautologin-user-timeout=0\n"),
        )?,
        "greetd.service" => {
            // greetd has no separate "autologin" flag — an `[initial_session]`
            // block runs the session directly on the first VT, no greeter at
            // all. Re-use the desktop catalog to know what to run.
            let desktop_key = std::fs::read_to_string("/mnt/etc/manifest-install.json")
                .ok()
                .and_then(|r| serde_json::from_str::<serde_json::Value>(&r).ok())
                .and_then(|d| d.get("desktop").and_then(|v| v.as_str()).map(str::to_string));
            let session_exec = desktop_key
                .as_deref()
                .and_then(crate::desktop::recipe)
                .map(|r| r.session_exec)
                .filter(|s| !s.is_empty());
            match session_exec {
                Some(cmd) => {
                    let toml = format!(
                        "[terminal]\nvt = 1\n\n[default_session]\ncommand = \"tuigreet --time --remember --cmd {cmd}\"\nuser = \"greeter\"\n\n[initial_session]\ncommand = \"{cmd}\"\nuser = \"{user}\"\n"
                    );
                    ctx.write_root("/mnt/etc/greetd/config.toml", &toml)?;
                }
                None => println!("  · couldn't determine the session command for greetd — skipped"),
            }
        }
        _ => {
            // No known DM (or a bare WM launched from .bash_profile): autologin
            // the tty itself, which is what a WM session normally starts from.
            let unit_dir = "/mnt/etc/systemd/system/getty@tty1.service.d";
            ctx.sudo("mkdir", &["-p", unit_dir])?;
            ctx.write_root(
                &format!("{unit_dir}/autologin.conf"),
                &format!(
                    "[Service]\nExecStart=\nExecStart=-/usr/bin/agetty --autologin {user} --noclear %I $TERM\n"
                ),
            )?;
        }
    }
    println!("  · {user} will log in automatically");
    Ok(())
}

/// Install the proprietary NVIDIA driver: nvidia-dkms (rebuilds against future
/// kernel updates automatically, unlike the kernel-version-pinned `nvidia`
/// package) + nvidia-utils, plus the early-KMS + modeset setup NVIDIA's own
/// docs recommend for a flicker-free boot on Wayland/Xorg. Runs after the
/// manifest so the target kernel/headers already match. Best-effort — a failed
/// driver build shouldn't sink an otherwise-complete install.
fn install_nvidia_driver(ctx: &Ctx) -> Result<()> {
    step("Installing the NVIDIA driver");
    if ctx
        .shell(
            "arch-chroot /mnt pacman -S --needed --noconfirm nvidia-dkms nvidia-utils nvidia-settings libva-nvidia-driver",
            true,
        )
        .is_err()
    {
        println!("  · warning: NVIDIA driver install failed — you can install nvidia-dkms manually later");
        return Ok(());
    }
    ctx.write_root(
        "/mnt/etc/modprobe.d/nvidia.conf",
        "# Managed by Manifest OS\noptions nvidia_drm modeset=1\noptions nvidia_drm fbdev=1\n",
    )?;
    // Early KMS: load the NVIDIA modules from the initramfs, before any display
    // server starts.
    ctx.shell(
        "sed -i '/^MODULES=/{/nvidia_drm/!s/MODULES=(/MODULES=(nvidia nvidia_modeset nvidia_uvm nvidia_drm /}' /mnt/etc/mkinitcpio.conf",
        true,
    )?;
    ctx.shell("arch-chroot /mnt mkinitcpio -P", true)?;
    println!("  · NVIDIA proprietary driver installed (nvidia-dkms)");
    Ok(())
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

/// Confirm we can reach the package mirrors before pacstrap, and nudge the clock
/// — a wrong RTC (common on real hardware with a dead CMOS battery) makes pacman
/// reject signatures. Both are top causes of an otherwise-mysterious pacstrap
/// failure that "works in a VM" (where NAT + host time hide them).
fn ensure_online(ctx: &Ctx) -> Result<()> {
    step("Checking internet connection");
    if ctx.dry_run {
        return Ok(());
    }
    if !crate::probe::is_online() {
        bail!(
            "No internet connection. Connect to Wi-Fi or plug in an Ethernet cable, then try again — \
             the base system is downloaded during install."
        );
    }
    println!("  · online");
    Ok(())
}

/// Correct the system clock before pacstrap. A machine with a dead CMOS battery
/// boots with a wildly wrong date, and pacman then rejects every package
/// signature as not-yet-valid — the classic pacstrap failure that "works in a VM
/// but not on hardware". `timedatectl set-ntp` alone isn't enough: it may not
/// converge before pacstrap, or NTP may be blocked. So we set a correct-enough
/// time *immediately* from an HTTPS `Date:` header (accurate to ~1s, which is far
/// more than signatures need), then enable NTP for ongoing accuracy and write it
/// back to the RTC. Requires network, so it runs after `ensure_online`.
/// Best-effort — never fails the install.
fn fix_clock(ctx: &Ctx) {
    if ctx.dry_run {
        return;
    }
    step("Setting the clock");
    // Set an accurate-enough time once from an HTTPS Date header, then DISABLE
    // continuous NTP for the duration of the install. Leaving NTP on lets it
    // *step the clock backward* between `pacman-key --init` (which dates a new
    // local master key at "now") and `--populate` (which signs it) — making that
    // key look "created N seconds in the future" and failing the keyring. A
    // stable clock for the few install minutes avoids the whole race; the
    // installed system does its own time sync on first boot.
    let _ = ctx.shell(
        "timedatectl set-ntp false 2>/dev/null || true; \
         for url in https://archlinux.org https://www.cloudflare.com https://www.google.com; do \
            d=$(curl -sI --max-time 8 \"$url\" 2>/dev/null | grep -i '^date:' | head -n1 | cut -d' ' -f2-); \
            if [ -n \"$d\" ]; then date -s \"$d\" >/dev/null 2>&1 && break; fi; \
         done; \
         hwclock --systohc 2>/dev/null || true",
        true,
    );
    let _ = ctx.shell("printf '  · clock set to '; date -u '+%Y-%m-%d %H:%M:%S UTC'", true);
}

/// Release the target disk so the kernel can re-read its partition table.
/// Unmounts a leftover `/mnt` (e.g. from a previous failed attempt in the same
/// live session), turns off swap, and unmounts any auto-mounted partitions that
/// live on this disk. Without this, partitioning fails with "Device or resource
/// busy" / "atomic commit" errors. Best-effort; never fails the install.
fn free_disk(disk: &str, ctx: &Ctx) {
    step("Releasing the disk");
    let script = format!(
        "umount -R /mnt 2>/dev/null || true; \
         swapoff -a 2>/dev/null || true; \
         for p in $(lsblk -lnpo NAME {disk} 2>/dev/null | tail -n +2); do \
            swapoff \"$p\" 2>/dev/null || true; \
            umount -fR \"$p\" 2>/dev/null || true; \
         done; \
         udevadm settle 2>/dev/null || true"
    );
    let _ = ctx.shell(&script, true);
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

/// The partitions the install will use, by device path.
struct Parts {
    root: String,
    esp: Option<String>,
    swap: Option<String>,
}

/// Wipe and partition the disk. Layout depends on firmware and whether a
/// dedicated swap partition was requested:
///   BIOS:  [swap] root(*)
///   UEFI:  ESP(550M) [swap] root
fn partition(disk: &str, uefi: bool, swap_gib: Option<u32>, ctx: &Ctx) -> Result<()> {
    step("Partitioning");
    let layout = match (uefi, swap_gib) {
        (true, Some(g)) => format!("label: gpt\n,550M,U\n,{g}G,S\n,,L\n"),
        (true, None) => "label: gpt\n,550M,U\n,,L\n".to_string(),
        (false, Some(g)) => format!("label: dos\n,{g}G,S\n,,L,*\n"),
        (false, None) => "label: dos\n,,L,*\n".to_string(),
    };
    // wipefs clears stale FS/RAID/LVM signatures that would make the kernel hold
    // references to old partitions; `--wipe always` does the same during the
    // write; partprobe + udevadm settle make the new partitions appear before we
    // format them. `set -e` so a real sfdisk failure still propagates.
    ctx.shell(
        &format!(
            "set -e\n\
             wipefs -af {disk} >/dev/null 2>&1 || true\n\
             printf '{layout}' | sfdisk --force --wipe always {disk}\n\
             partprobe {disk} >/dev/null 2>&1 || true\n\
             udevadm settle >/dev/null 2>&1 || true"
        ),
        true,
    )
}

/// Partition device paths, accounting for the `p` separator on nvme/mmc and the
/// order produced by [`partition`].
fn partition_names(disk: &str, uefi: bool, has_swap: bool) -> Parts {
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
    let p = |n: u32| format!("{disk}{sep}{n}");
    match (uefi, has_swap) {
        (true, true) => Parts { esp: Some(p(1)), swap: Some(p(2)), root: p(3) },
        (true, false) => Parts { esp: Some(p(1)), swap: None, root: p(2) },
        (false, true) => Parts { esp: None, swap: Some(p(1)), root: p(2) },
        (false, false) => Parts { esp: None, swap: None, root: p(1) },
    }
}

/// Shrink the largest filesystem of an existing OS (Windows, another Linux, …)
/// and create a Manifest OS root in the freed space, **reusing** that OS's
/// existing ESP — i.e. set up a dual boot instead of erasing the disk. Returns
/// the partitions to install onto.
///
/// Order matters and is the safe one: shrink the *filesystem* first (with the
/// right tool for its type), then the *partition* to match, then carve our
/// partition out of the freed tail. The partition is recreated at the SAME start
/// sector with its original type + unique GUID preserved, so the other OS's boot
/// references stay valid. NTFS is also gated on a clean `--no-action` pass so we
/// never resize a hibernated / Fast-Startup Windows.
fn carve_alongside(plan: &InstallPlan, ctx: &Ctx) -> Result<Parts> {
    let os = crate::probe::detect_existing_os()
        .context("no existing OS was detected to install alongside")?;
    step("Making room alongside the existing system");
    println!(
        "  · {} is on {} ({} GiB {}); reusing its ESP {}",
        os.label, os.shrink_part, os.shrink_size_gib, os.shrink_fstype, os.esp
    );

    let gib = 1u64 << 30;
    let give = (plan.alongside_gib.filter(|&g| g >= 15).unwrap_or(40) as u64) * gib;

    if ctx.dry_run {
        println!("  · would shrink {} and create a {} GiB Manifest OS partition", os.shrink_part, give / gib);
        return Ok(Parts { root: format!("{}-new", os.disk), esp: Some(os.esp), swap: None });
    }

    let part_bytes = disk_bytes(&os.shrink_part, ctx);
    if part_bytes < give + 20 * gib {
        bail!(
            "not enough room: {} is only {} GiB — free up space in it first",
            os.shrink_part,
            part_bytes / gib
        );
    }
    let new_fs = part_bytes - give;

    // 1) Shrink the existing filesystem with the right tool for its type.
    shrink_filesystem(&os.shrink_part, &os.shrink_fstype, new_fs, &os.label, ctx)?;

    // 2) Shrink the partition to match. Recreate it at the SAME start sector with
    //    its original type + unique GUID preserved, so the other OS's bootloader
    //    references stay valid. (parted's resizepart refuses to shrink
    //    non-interactively; sgdisk never prompts.) End sector = shrunk filesystem
    //    + 1 MiB slack, so the partition is never smaller than its filesystem.
    let num: String = os
        .shrink_part
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let base = os.shrink_part.trim_start_matches("/dev/");
    let start_sec = sysfs_u64(&format!("/sys/class/block/{base}/start"));
    let new_end_sec = start_sec + new_fs / 512 + 2048;
    let shrink = format!(
        "info=$(sgdisk -i {num} {disk}); \
         tguid=$(echo \"$info\" | sed -n 's/.*GUID code: \\([0-9A-Fa-f-]*\\).*/\\1/p'); \
         uguid=$(echo \"$info\" | sed -n 's/.*unique GUID: //p' | tr -d ' \\r'); \
         sgdisk -d {num} -n {num}:{start}:{end} ${{tguid:+-t {num}:$tguid}} ${{uguid:+-u {num}:$uguid}} {disk}",
        num = num,
        disk = os.disk,
        start = start_sec,
        end = new_end_sec
    );
    ctx.shell(&shrink, true).context("resizing the existing partition failed")?;
    let _ = ctx.shell(&format!("partprobe {}", os.disk), true);

    // 3) Create our root in the freed (now largest free) space; identify the new
    //    partition by diffing the table before/after so we never guess wrong.
    let before = list_parts(&os.disk, ctx);
    ctx.shell(&format!("sgdisk -n 0:0:0 -t 0:8300 {}", os.disk), true)
        .context("creating the Manifest OS partition failed")?;
    let _ = ctx.shell(&format!("partprobe {}", os.disk), true);
    let after = list_parts(&os.disk, ctx);
    let root = after
        .into_iter()
        .find(|p| !before.contains(p))
        .context("could not locate the new Manifest OS partition after creating it")?;

    // 4) Format ONLY our new root — never the ESP, never the existing OS.
    mkfs_root(&root, &plan.filesystem, ctx)?;
    println!("  · created Manifest OS on {} ({} GiB) — {} left intact", root, give / gib, os.label);
    Ok(Parts { root, esp: Some(os.esp), swap: None })
}

/// Shrink an existing filesystem to `new_size` bytes, in place, picking the tool
/// for its type. NTFS gets a `--no-action` safety pass first (fails on a dirty /
/// hibernated volume). ext* are fsck'd then resized; btrfs resizes mounted.
fn shrink_filesystem(part: &str, fstype: &str, new_size: u64, label: &str, ctx: &Ctx) -> Result<()> {
    match fstype {
        "ntfs" => {
            if ctx
                .shell(&format!("ntfsresize -f --no-action --size {new_size} {part}"), true)
                .is_err()
            {
                bail!(
                    "couldn't safely resize {label} on {part} — boot it, turn off Fast Startup, fully shut down, then try again"
                );
            }
            ctx.shell(&format!("echo y | ntfsresize -f --size {new_size} {part}"), true)
                .context("shrinking the NTFS filesystem failed")?;
        }
        "ext2" | "ext3" | "ext4" => {
            // resize2fs needs a clean, unmounted fs; e2fsck exit ≤2 means OK/fixed.
            let mib = new_size / (1 << 20);
            ctx.shell(
                &format!(
                    "e2fsck -fy {part}; rc=$?; [ $rc -le 2 ] || exit $rc; resize2fs {part} {mib}M"
                ),
                true,
            )
            .with_context(|| format!("shrinking the {fstype} filesystem on {part} failed"))?;
        }
        "btrfs" => {
            // btrfs resizes while mounted.
            let mnt = "/run/manifest-shrink";
            ctx.shell(
                &format!(
                    "mkdir -p {mnt} && mount {part} {mnt} && \
                     btrfs filesystem resize {new_size} {mnt}; rc=$?; umount {mnt}; exit $rc"
                ),
                true,
            )
            .with_context(|| format!("shrinking the btrfs filesystem on {part} failed"))?;
        }
        other => bail!("can't shrink a {other} filesystem to make room — free up space manually first"),
    }
    Ok(())
}

/// After a dual-boot install, make the other OS appear in the GRUB menu: install
/// os-prober, enable it, and regenerate the config. Best-effort — a daily-driver
/// that chose a non-GRUB loader simply won't get the extra entry.
fn enable_dual_boot(ctx: &Ctx) {
    step("Adding the other system to the boot menu");
    let script = "arch-chroot /mnt bash -c '\
        command -v grub-mkconfig >/dev/null 2>&1 || exit 0; \
        pacman -S --needed --noconfirm os-prober || exit 0; \
        if grep -q \"^#*GRUB_DISABLE_OS_PROBER\" /etc/default/grub; then \
            sed -i \"s/^#*GRUB_DISABLE_OS_PROBER=.*/GRUB_DISABLE_OS_PROBER=false/\" /etc/default/grub; \
        else echo GRUB_DISABLE_OS_PROBER=false >> /etc/default/grub; fi; \
        grub-mkconfig -o /boot/grub/grub.cfg'";
    let _ = ctx.shell(script, true);
}

/// Size of a block device in bytes (0 if it can't be read).
fn disk_bytes(dev: &str, ctx: &Ctx) -> u64 {
    ctx.output(false, "lsblk", &["-bno", "SIZE", dev])
        .ok()
        .and_then(|s| s.lines().next().map(str::trim).map(str::to_string))
        .and_then(|l| l.parse().ok())
        .unwrap_or(0)
}

/// Read a small unsigned integer out of a sysfs file (0 on any error).
fn sysfs_u64(path: &str) -> u64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// The partition device paths currently on a disk (e.g. `/dev/sda1`, `/dev/sda3`).
fn list_parts(disk: &str, ctx: &Ctx) -> Vec<String> {
    ctx.output(false, "lsblk", &["-lnpo", "NAME,TYPE", disk])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let name = it.next()?;
            if it.next()? == "part" {
                Some(name.to_string())
            } else {
                None
            }
        })
        .collect()
}

/// The unlocked LUKS root device (a fixed name; only one encrypted root exists).
const LUKS_MAPPER: &str = "/dev/mapper/cryptroot";

/// Install the userspace tools a non-ext4 root needs in the *target* — without
/// them, mkinitcpio's fsck hook can't find `fsck.xfs`/`btrfs` and boot-time
/// fsck/mount fail. ext4's e2fsprogs is already in `base`.
fn install_fs_tools(fs: &str, ctx: &Ctx) -> Result<()> {
    let pkg = match fs {
        "xfs" => "xfsprogs",
        "btrfs" => "btrfs-progs",
        _ => return Ok(()),
    };
    step("Installing filesystem tools");
    ctx.shell(&format!("arch-chroot /mnt pacman -S --needed --noconfirm {pkg}"), true)
}

/// Format the ESP and swap. The root is formatted separately (after optional
/// LUKS) by [`mkfs_root`].
fn format_aux(parts: &Parts, ctx: &Ctx) -> Result<()> {
    step("Formatting");
    if let Some(esp) = &parts.esp {
        ctx.sudo("mkfs.fat", &["-F32", esp])?;
    }
    if let Some(sw) = &parts.swap {
        ctx.sudo("mkswap", &[sw])?;
    }
    Ok(())
}

/// Make the root filesystem on `dev` (a partition, or the LUKS mapper).
fn mkfs_root(dev: &str, fs: &str, ctx: &Ctx) -> Result<()> {
    match fs {
        "btrfs" => ctx.sudo("mkfs.btrfs", &["-f", dev]),
        "xfs" => ctx.sudo("mkfs.xfs", &["-f", dev]),
        _ => ctx.sudo("mkfs.ext4", &["-F", dev]),
    }
}

/// Create the LUKS2 container on `part` and open it as `cryptroot`. The
/// passphrase is fed over stdin (never logged).
fn setup_luks(part: &str, passphrase: &str, ctx: &Ctx) -> Result<()> {
    step("Encrypting the disk (LUKS)");
    if passphrase.trim().is_empty() {
        bail!("encryption is on but no passphrase was provided");
    }
    ctx.cryptsetup(
        &["luksFormat", "--type", "luks2", "--batch-mode", "--key-file=-", part],
        passphrase,
    )?;
    ctx.cryptsetup(&["open", "--key-file=-", part, "cryptroot"], passphrase)?;
    println!("  · root encrypted with LUKS2 (unlocked as cryptroot)");
    Ok(())
}

/// Whether the target's initramfs is systemd-based (HOOKS includes `systemd`),
/// which changes the encrypt hook (`sd-encrypt`) and cmdline (`rd.luks.name`).
fn initramfs_uses_systemd(ctx: &Ctx) -> bool {
    if ctx.dry_run {
        return false;
    }
    std::fs::read_to_string("/mnt/etc/mkinitcpio.conf")
        .map(|c| {
            c.lines()
                .any(|l| l.trim_start().starts_with("HOOKS=") && l.contains("systemd"))
        })
        .unwrap_or(false)
}

/// After the base system exists, add the right LUKS initramfs hook and
/// regenerate. The root is unlocked from the kernel cmdline (see
/// [`inject_crypt_cmdline`]) — NOT /etc/crypttab, which is only for secondary
/// volumes and would make systemd try to re-unlock the root post-boot.
fn configure_encryption(luks_part: &str, systemd: bool, ctx: &Ctx) -> Result<()> {
    let _ = luks_part;
    step("Configuring encryption (initramfs)");
    // cryptsetup isn't in `base`, but the encrypt hooks need it.
    ctx.shell("arch-chroot /mnt pacman -S --needed --noconfirm cryptsetup", true)?;
    // Insert the correct hook just before `filesystems` (idempotent). A
    // systemd-based initramfs uses `sd-encrypt`; the classic udev base, `encrypt`.
    let sed = if systemd {
        "sed -i '/^HOOKS=/{/sd-encrypt/!s/filesystems/sd-encrypt filesystems/}' /mnt/etc/mkinitcpio.conf"
    } else {
        "sed -i '/^HOOKS=/{/encrypt/!s/filesystems/encrypt filesystems/}' /mnt/etc/mkinitcpio.conf"
    };
    ctx.shell(sed, true)?;
    ctx.shell("arch-chroot /mnt mkinitcpio -P", true)?;
    Ok(())
}

/// Add the boot-cmdline parameter that unlocks the root in the initramfs:
/// `rd.luks.name=<uuid>=cryptroot` for a systemd initramfs, else
/// `cryptdevice=UUID=<uuid>:cryptroot`. (root=/dev/mapper/cryptroot is derived
/// from fstab by grub-mkconfig.)
fn inject_crypt_cmdline(luks_part: &str, systemd: bool, ctx: &Ctx) -> Result<()> {
    if ctx.dry_run {
        println!("  · would add the LUKS unlock parameter to the boot cmdline");
        return Ok(());
    }
    let uuid = ctx.output(false, "blkid", &["-s", "UUID", "-o", "value", luks_part])?;
    let uuid = uuid.trim();
    if uuid.is_empty() {
        return Ok(());
    }
    let param = if systemd {
        format!("rd.luks.name={uuid}=cryptroot")
    } else {
        format!("cryptdevice=UUID={uuid}:cryptroot")
    };
    let path = "/mnt/etc/manifest-install.json";
    let mut doc: serde_json::Value = match std::fs::read_to_string(path).ok().and_then(|r| serde_json::from_str(&r).ok()) {
        Some(d) => d,
        None => return Ok(()),
    };
    if let Some(boot) = doc.get_mut("boot").and_then(|b| b.as_object_mut()) {
        let present = boot
            .get("cmdline")
            .and_then(|c| c.as_array())
            .map(|a| a.iter().any(|v| v.as_str() == Some(param.as_str())))
            .unwrap_or(false);
        if !present {
            match boot.get_mut("cmdline").and_then(|c| c.as_array_mut()) {
                Some(arr) => arr.push(serde_json::Value::String(param.clone())),
                None => {
                    boot.insert("cmdline".into(), serde_json::json!([param]));
                }
            }
        }
    }
    let _ = std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap_or_default());
    println!("  · boot cmdline: {param}");
    Ok(())
}

/// Override the staged manifest's `system` block (timezone/locale/keymap/
/// hostname) with values the front-end collected, so an Advanced install can set
/// them without editing the manifest. Best-effort.
fn apply_system_overrides(plan: &InstallPlan, ctx: &Ctx) -> Result<()> {
    let tz = plan.timezone.as_deref().filter(|s| !s.trim().is_empty());
    let locale = plan.locale.as_deref().filter(|s| !s.trim().is_empty());
    let keymap = plan.keymap.as_deref().filter(|s| !s.trim().is_empty());
    let hostname = plan.hostname.as_deref().filter(|s| !s.trim().is_empty());
    if tz.is_none() && locale.is_none() && keymap.is_none() && hostname.is_none() {
        return Ok(());
    }
    step("Applying system settings");
    if ctx.dry_run {
        println!("  · would set timezone/locale/keymap/hostname from the installer");
        return Ok(());
    }
    let path = "/mnt/etc/manifest-install.json";
    let Some(mut doc) = std::fs::read_to_string(path).ok().and_then(|r| serde_json::from_str::<serde_json::Value>(&r).ok()) else {
        return Ok(());
    };
    let Some(obj) = doc.as_object_mut() else { return Ok(()) };
    let system = obj.entry("system").or_insert_with(|| serde_json::json!({}));
    if !system.is_object() {
        *system = serde_json::json!({});
    }
    let sys = system.as_object_mut().unwrap();
    if let Some(v) = tz { sys.insert("timezone".into(), serde_json::json!(v)); }
    if let Some(v) = locale { sys.insert("locale".into(), serde_json::json!(v)); }
    if let Some(v) = keymap { sys.insert("keymap".into(), serde_json::json!(v)); }
    if let Some(v) = hostname { sys.insert("hostname".into(), serde_json::json!(v)); }
    let _ = std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap_or_default());
    println!("  · system settings applied from the installer");
    Ok(())
}

fn mount(parts: &Parts, ctx: &Ctx) -> Result<()> {
    ctx.sudo("mount", &[&parts.root, "/mnt"])?;
    if let Some(esp) = &parts.esp {
        ctx.sudo("mkdir", &["-p", "/mnt/boot"])?;
        ctx.sudo("mount", &[esp, "/mnt/boot"])?;
    }
    Ok(())
}

/// Always-on, transient zram swap for the *install itself*, so low-memory
/// machines have breathing room while pacstrap and AUR builds run (this is what
/// kept weak boxes off the OOM killer). It does not touch the installed system;
/// the persistent swap the user chose is configured by [`setup_persistent_swap`].
fn setup_install_zram(ctx: &Ctx) -> Result<()> {
    step("Preparing low-memory install swap");
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

/// Configure the *installed* system's persistent swap, per the plan. Runs after
/// genfstab so we can append our own entries.
///   * `partition` — `mkswap` already ran in [`format_disks`]; add it to fstab.
///   * `swapfile`  — create a sized file on root and add it to fstab.
///   * `zram`      — install zram-generator + a config (compressed RAM swap).
///   * `none`      — nothing.
fn setup_persistent_swap(plan: &InstallPlan, parts: &Parts, ctx: &Ctx) -> Result<()> {
    match plan.swap.as_str() {
        "partition" => {
            let Some(sw) = &parts.swap else { return Ok(()) };
            step("Configuring swap (partition)");
            ctx.shell(
                &format!(
                    "uuid=$(blkid -s UUID -o value {sw}) && \
                     echo \"UUID=$uuid none swap defaults 0 0\" >> /mnt/etc/fstab"
                ),
                true,
            )?;
            println!("  · swap partition {sw} added to fstab");
        }
        "swapfile" => {
            let gib = plan.swap_size_gib.filter(|&g| g > 0).unwrap_or(2);
            step("Configuring swap (file)");
            // btrfs needs a NOCOW swapfile; its mkswapfile handles that for us.
            let make = if plan.filesystem == "btrfs" {
                format!("btrfs filesystem mkswapfile --size {gib}g /mnt/swapfile")
            } else {
                format!(
                    "fallocate -l {gib}G /mnt/swapfile && chmod 600 /mnt/swapfile && \
                     mkswap /mnt/swapfile"
                )
            };
            ctx.shell(
                &format!("{make} && echo '/swapfile none swap defaults 0 0' >> /mnt/etc/fstab"),
                true,
            )?;
            println!("  · {gib} GiB swapfile created and added to fstab");
        }
        "zram" => {
            step("Configuring swap (zram)");
            ctx.shell(
                "arch-chroot /mnt pacman -S --needed --noconfirm zram-generator",
                true,
            )?;
            ctx.write_root(
                "/mnt/etc/systemd/zram-generator.conf",
                "# Managed by Manifest OS\n[zram0]\nzram-size = min(ram, 8192)\ncompression-algorithm = zstd\n",
            )?;
            println!("  · compressed RAM swap configured (zram-generator)");
        }
        _ => println!("  · no persistent swap"),
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
    // Stage the CLI `manifest` binary into the target — NOT whichever front-end
    // is running. The GUI (`manifest-gui`) is GTK-linked and cannot run inside
    // the minimal chroot (no libgtk), so `manifest install` there would fail to
    // load (exit 127). Prefer a `manifest` sibling of the current exe; fall back
    // to the current exe (the CLI/TUI case, where it already is `manifest`).
    let exe = std::env::current_exe().context("locating the manifest binary")?;
    let cli = exe.with_file_name("manifest");
    let src = if cli.exists() { cli } else { exe };
    let src = src.to_string_lossy();
    ctx.sudo("install", &["-Dm755", &src, "/mnt/usr/local/bin/manifest"])
}

/// Write the survey answers the front-end collected to a JSON object the chroot
/// install can read via `--answers`, so the manifest's `{{id}}` tokens resolve.
/// Returns the in-chroot path, or `None` when there are no answers. Lives in
/// `/etc` so the non-root installer account can read it.
fn write_answers(plan: &InstallPlan, ctx: &Ctx) -> Result<Option<String>> {
    if plan.answers.is_empty() {
        return Ok(None);
    }
    step("Recording your answers");
    let map: serde_json::Map<String, serde_json::Value> = plan
        .answers
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    let json = serde_json::to_string_pretty(&serde_json::Value::Object(map))?;
    ctx.write_root("/mnt/etc/manifest-answers.json", &json)?;
    Ok(Some("/etc/manifest-answers.json".to_string()))
}

/// Run the manifest inside the new root, as the bootstrap user, optionally
/// feeding it the survey answers file.
fn run_manifest(manifest_in_root: &str, answers: Option<&str>, ctx: &Ctx) -> Result<()> {
    step("Applying the manifest");
    let args = match answers {
        Some(a) => format!("install {manifest_in_root} --answers {a}"),
        None => format!("install {manifest_in_root}"),
    };
    let result = ctx.shell(
        &format!("arch-chroot /mnt runuser -l installer -c 'manifest {args}'"),
        true,
    );
    // The answers file may hold survey secrets; don't leave it on the new system.
    if answers.is_some() && !ctx.dry_run {
        let _ = std::fs::remove_file("/mnt/etc/manifest-answers.json");
    }
    result
}

/// Save the install log somewhere it survives a failure: the target's
/// `/var/log` and — since the boot ISO is read-only — any writable removable
/// drive's `logs/` folder. Best-effort; the live log lives at
/// `/tmp/manifest-install.log` (see the `.zlogin` launcher).
pub fn save_install_log(ctx: &Ctx) {
    if ctx.dry_run {
        return;
    }
    let src = "/tmp/manifest-install.log";
    if !Path::new(src).exists() {
        return;
    }
    step("Saving the install log");
    // 1) Onto the installed system (if the target is still mounted).
    if Path::new("/mnt/var/log").exists() {
        let _ = std::fs::copy(src, "/mnt/var/log/manifest-install.log");
        println!("  · /var/log/manifest-install.log (on the installed system)");
    }
    // 2) Onto a writable removable drive's logs/ folder (the USB the user has).
    let stamp = ctx
        .output(false, "date", &["+%Y%m%d-%H%M%S"])
        .unwrap_or_default();
    let stamp = if stamp.is_empty() { "install".into() } else { stamp };
    for mp in crate::probe::writable_removable_mounts() {
        let dir = format!("{mp}/logs");
        if std::fs::create_dir_all(&dir).is_ok() {
            let dest = format!("{dir}/manifest-install-{stamp}.log");
            if std::fs::copy(src, &dest).is_ok() {
                println!("  · {dest}");
            }
        }
    }
}

fn step(title: &str) {
    println!("\n[{title}]");
    // stdout is block-buffered when redirected to the install log; flush so the
    // GUI's live log shows each phase header as it starts, not in bursts.
    use std::io::Write;
    let _ = std::io::stdout().flush();
}
