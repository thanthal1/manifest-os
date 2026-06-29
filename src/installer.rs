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
    let alongside = plan.install_mode == "alongside";
    let parts = if alongside {
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
        let parts = partition_names(&plan.disk, uefi, swap_part_gib.is_some());
        format_disks(&parts, &plan.filesystem, ctx)?;
        parts
    };
    mount(&parts, ctx)?;
    setup_install_zram(ctx)?; // always-on: keeps low-memory machines off the OOM killer

    pacstrap(ctx)?;
    ctx.shell("genfstab -U /mnt >> /mnt/etc/fstab", true)?;
    setup_persistent_swap(plan, &parts, ctx)?;
    brand_system(ctx)?;
    create_bootstrap_user(ctx)?;

    let manifest_in_root = stage_manifest(&plan.manifest, ctx)?;
    ensure_boot_block(ctx)?;
    personalize_manifest(plan, ctx)?;
    let answers = write_answers(plan, ctx)?;
    stage_binary(ctx)?;
    run_manifest(&manifest_in_root, answers.as_deref(), ctx)?;
    create_account(plan, ctx)?;
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
    ctx.shell(&format!("printf '{layout}' | sfdisk --force {disk}"), true)
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

/// Shrink the detected Windows partition and create a Manifest OS root in the
/// freed space, **reusing** Windows' existing ESP — i.e. set up a dual boot
/// instead of erasing the disk. Returns the partitions to install onto.
///
/// Order matters and is the safe one: shrink the NTFS *filesystem* first, then
/// the *partition* to match (parted keeps the start sector + GUID so Windows'
/// boot references stay valid), then carve our partition out of the freed tail.
/// It refuses to touch a Windows volume that isn't cleanly resizable (a
/// hibernated / Fast-Startup Windows leaves it "dirty").
fn carve_alongside(plan: &InstallPlan, ctx: &Ctx) -> Result<Parts> {
    let win = crate::probe::detect_windows()
        .context("no Windows install was detected to install alongside")?;
    step("Making room alongside Windows");
    println!(
        "  · Windows is on {} ({} GiB NTFS); reusing its ESP {}",
        win.windows_part, win.windows_size_gib, win.esp
    );

    let gib = 1u64 << 30;
    let give = (plan.alongside_gib.filter(|&g| g >= 15).unwrap_or(40) as u64) * gib;

    if ctx.dry_run {
        println!("  · would shrink {} and create a {} GiB Manifest OS partition", win.windows_part, give / gib);
        return Ok(Parts { root: format!("{}-new", win.disk), esp: Some(win.esp), swap: None });
    }

    let part_bytes = disk_bytes(&win.windows_part, ctx);
    if part_bytes < give + 20 * gib {
        bail!(
            "not enough room: the Windows partition is only {} GiB — free up space in Windows first",
            part_bytes / gib
        );
    }
    let new_ntfs = part_bytes - give;

    // 1) Shrink the NTFS filesystem. The --no-action pass is the safety gate: it
    //    fails on a dirty/hibernated volume, so we never resize one.
    if ctx
        .shell(&format!("ntfsresize -f --no-action --size {new_ntfs} {}", win.windows_part), true)
        .is_err()
    {
        bail!(
            "couldn't safely resize the Windows filesystem on {} — boot Windows, turn off Fast Startup, fully shut it down, then try again",
            win.windows_part
        );
    }
    ctx.shell(&format!("echo y | ntfsresize -f --size {new_ntfs} {}", win.windows_part), true)
        .context("shrinking the Windows filesystem failed")?;

    // 2) Shrink the partition to match (start sector + GUID preserved).
    let num: String = win
        .windows_part
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let base = win.windows_part.trim_start_matches("/dev/");
    let start_sec = sysfs_u64(&format!("/sys/class/block/{base}/start"));
    // End sector: the shrunk filesystem plus 1 MiB of slack, so the partition is
    // never smaller than its filesystem. We recreate the partition at the SAME
    // start sector and preserve its type (0700, Microsoft basic data) and unique
    // GUID, so Windows' boot references stay valid. (parted's resizepart refuses
    // to shrink non-interactively; sgdisk never prompts.)
    let new_end_sec = start_sec + new_ntfs / 512 + 2048;
    let shrink = format!(
        "guid=$(sgdisk -i {num} {disk} | sed -n 's/.*unique GUID: //p' | tr -d ' \\r'); \
         sgdisk -d {num} -n {num}:{start}:{end} -t {num}:0700 ${{guid:+-u {num}:$guid}} {disk}",
        num = num,
        disk = win.disk,
        start = start_sec,
        end = new_end_sec
    );
    ctx.shell(&shrink, true).context("resizing the Windows partition failed")?;
    let _ = ctx.shell(&format!("partprobe {}", win.disk), true);

    // 3) Create our root in the freed (now largest free) space; identify the new
    //    partition by diffing the table before/after so we never guess wrong.
    let before = list_parts(&win.disk, ctx);
    ctx.shell(&format!("sgdisk -n 0:0:0 -t 0:8300 {}", win.disk), true)
        .context("creating the Manifest OS partition failed")?;
    let _ = ctx.shell(&format!("partprobe {}", win.disk), true);
    let after = list_parts(&win.disk, ctx);
    let root = after
        .into_iter()
        .find(|p| !before.contains(p))
        .context("could not locate the new Manifest OS partition after creating it")?;

    // 4) Format ONLY our new root — never the ESP, never Windows.
    match plan.filesystem.as_str() {
        "btrfs" => ctx.sudo("mkfs.btrfs", &["-f", &root])?,
        _ => ctx.sudo("mkfs.ext4", &["-F", &root])?,
    }
    println!("  · created Manifest OS on {} ({} GiB) — Windows left intact", root, give / gib);
    Ok(Parts { root, esp: Some(win.esp), swap: None })
}

/// After a dual-boot install, make Windows appear in the GRUB menu: install
/// os-prober, enable it, and regenerate the config. Best-effort — a daily-driver
/// that chose a non-GRUB loader simply won't get the extra entry.
fn enable_dual_boot(ctx: &Ctx) {
    step("Adding Windows to the boot menu");
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

fn format_disks(parts: &Parts, fs: &str, ctx: &Ctx) -> Result<()> {
    step("Formatting");
    if let Some(esp) = &parts.esp {
        ctx.sudo("mkfs.fat", &["-F32", esp])?;
    }
    match fs {
        "btrfs" => ctx.sudo("mkfs.btrfs", &["-f", &parts.root])?,
        _ => ctx.sudo("mkfs.ext4", &["-F", &parts.root])?,
    }
    if let Some(sw) = &parts.swap {
        ctx.sudo("mkswap", &[sw])?;
    }
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
}
