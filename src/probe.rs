//! Read-only system probing and the [`InstallPlan`] — the data the install
//! front-ends (the Ratatui TUI and the GTK GUI) collect and hand to
//! [`crate::installer::execute`]. Keeping it here (rather than in `tui.rs`) lets
//! both front-ends share the same disk/network/manifest discovery and the same
//! plan type, so the engine has exactly one input shape.

use serde::Deserialize;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

// Preseed-JSON defaults for InstallPlan — the same defaults `provision`'s CLI
// flags declare, so a partial preseed file behaves identically to omitting a
// flag (an empty `swap`/`filesystem` string would otherwise silently mean
// "no swap" / fall through to ext4 inconsistently).
fn default_install_mode() -> String {
    "erase".to_string()
}
fn default_filesystem() -> String {
    "ext4".to_string()
}
fn default_swap() -> String {
    "zram".to_string()
}

/// A whole disk the system can be installed onto.
pub struct Disk {
    pub name: String,
    pub size: String,
    pub model: String,
}

/// The daily-driver account a friendly install creates. Collected by the GUI
/// ("What's your name?" + a password); the password is only ever fed to
/// `chpasswd` over stdin, never written to disk or logged.
#[derive(Default, Deserialize)]
pub struct Account {
    #[serde(default)]
    pub full_name: String,
    pub username: String,
    pub password: String,
}

/// An additional (non-primary) user account. Kept separate from [`Account`] —
/// no `full_name`, and sudo is opt-in per user rather than assumed.
#[derive(Deserialize)]
pub struct ExtraUser {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub sudo: bool,
}

/// A static IPv4 configuration for the install (and, if requested, the
/// installed system). All install-time networking otherwise assumes DHCP.
#[derive(Default, Deserialize)]
pub struct StaticIp {
    /// CIDR, e.g. "192.168.1.50/24".
    pub address: String,
    pub gateway: String,
    /// Comma-separated resolver IPs, e.g. "1.1.1.1,8.8.8.8".
    #[serde(default)]
    pub dns: String,
}

/// What a front-end collected — handed back to the caller to execute. Also the
/// shape of a `manifest provision --config file.json` preseed file, so an
/// unattended install can skip the wizard/flags entirely; every field short of
/// `disk`/`manifest` has a sane empty/off default so a partial preseed file
/// still parses.
#[derive(Default, Deserialize)]
pub struct InstallPlan {
    pub disk: String,
    /// `"erase"` (wipe the whole disk) or `"alongside"` (shrink an existing OS
    /// and dual-boot).
    #[serde(default = "default_install_mode")]
    pub install_mode: String,
    /// For `alongside`: how many GiB to carve out for Manifest OS (None = a
    /// sensible default).
    #[serde(default)]
    pub alongside_gib: Option<u32>,
    #[serde(default = "default_filesystem")]
    pub filesystem: String,
    /// Persistent swap for the *installed* system, one of:
    /// `"none"`, `"zram"` (compressed RAM swap via zram-generator),
    /// `"swapfile"` (a file on root), or `"partition"` (a dedicated partition).
    /// Independent of the always-on install-time zram that keeps low-memory
    /// machines from OOMing during pacstrap/AUR builds.
    #[serde(default = "default_swap")]
    pub swap: String,
    /// Size in GiB for `swapfile`/`partition` swap (ignored otherwise).
    #[serde(default)]
    pub swap_size_gib: Option<u32>,
    /// A bundled example name, a local path, or an `http(s)` URL.
    #[serde(default)]
    pub manifest: String,
    /// Answers to the manifest's `survey` questions, as `(id, value)` pairs.
    /// Written to an `--answers` file so `{{id}}` tokens resolve during install.
    #[serde(default)]
    pub answers: Vec<(String, String)>,
    /// Daily-driver account to create (None = use whatever the manifest declares).
    #[serde(default)]
    pub account: Option<Account>,
    /// Hostname override (None = use the manifest's, or the default).
    #[serde(default)]
    pub hostname: Option<String>,
    /// `"none"` | `"full"` (LUKS2 the whole root) | `"home"` (LUKS2 a separate
    /// `/home` partition only, root stays plaintext). "full"/"home" are
    /// mutually exclusive and erase-install only.
    #[serde(default)]
    pub encrypt_mode: String,
    /// LUKS passphrase (sensitive — fed to cryptsetup over stdin, never logged).
    #[serde(default)]
    pub encrypt_passphrase: String,
    /// Root partition size in GiB when `encrypt_mode == "home"` (the rest of
    /// the disk becomes /home). Ignored otherwise. None = a sensible default.
    #[serde(default)]
    pub root_gib: Option<u32>,
    /// Put the root filesystem on an LVM logical volume (a single LV filling
    /// one volume group) instead of directly on the partition/mapper —
    /// composes with encryption (LVM-on-LUKS) and RAID (LVM-on-RAID).
    #[serde(default)]
    pub lvm: bool,
    /// A second disk to mirror the root onto via mdadm RAID1. Only the root
    /// is mirrored — the ESP and any swap partition stay on the primary disk
    /// only (the common, simpler tradeoff most installers make).
    #[serde(default)]
    pub raid1_disk: Option<String>,
    /// Timezone / locale / console keymap overrides (None = the manifest's).
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub locale: Option<String>,
    #[serde(default)]
    pub keymap: Option<String>,
    /// Set a root password (sensitive, stdin-only). None = leave root locked
    /// (the wheel/sudo account is the way in — the safer default).
    #[serde(default)]
    pub root_password: Option<String>,
    /// Log the created account in automatically (skip the login screen).
    #[serde(default)]
    pub autologin: bool,
    /// Install the proprietary NVIDIA driver (offered when an NVIDIA GPU is seen).
    #[serde(default)]
    pub install_nvidia: bool,
    /// Additional accounts beyond the primary one, each with its own sudo choice.
    #[serde(default)]
    pub extra_users: Vec<ExtraUser>,
    /// Install and enable CUPS (printing).
    #[serde(default)]
    pub install_printing: bool,
    /// Skip staging the System Snapshots desktop app (`manifest-center`) into
    /// the installed system. Off by default (the app is installed); turned on
    /// for headless/server installs that don't want a GUI app. Inverted so the
    /// type default (false) keeps the friendly behaviour.
    #[serde(default)]
    pub skip_desktop_app: bool,
    /// A local script (on the install medium) to run inside the chroot after
    /// everything else — the escape hatch for one-off customization beyond
    /// what the manifest declares.
    #[serde(default)]
    pub post_install_script: Option<String>,
    /// Static IPv4 for the install itself (and persisted into the target via a
    /// systemd-networkd profile). None = DHCP, the default.
    #[serde(default)]
    pub static_ip: Option<StaticIp>,
    /// HTTP(S) proxy for the install's downloads (pacman + curl), e.g.
    /// "http://10.0.0.1:3128". Applied as `http_proxy`/`https_proxy`.
    #[serde(default)]
    pub proxy: Option<String>,
    /// A VLAN to bring up before installing: tag `vlan_id` on `vlan_parent`
    /// (e.g. id 100 on "eth0" -> "eth0.100"). Both must be set together.
    #[serde(default)]
    pub vlan_id: Option<u16>,
    #[serde(default)]
    pub vlan_parent: Option<String>,
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

pub fn is_online() -> bool {
    Command::new("ping")
        .args(["-c", "1", "-W", "2", "archlinux.org"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn wifi_device() -> Option<String> {
    let entries = std::fs::read_dir("/sys/class/net").ok()?;
    for e in entries.flatten() {
        if e.path().join("wireless").exists() {
            return Some(e.file_name().to_string_lossy().to_string());
        }
    }
    None
}

/// The first non-loopback network interface, sorted (so e.g. `eth0` beats
/// `wlan0` deterministically) — the default target for static-IP config when
/// nothing more specific is asked.
pub fn primary_iface() -> Option<String> {
    let entries = std::fs::read_dir("/sys/class/net").ok()?;
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n != "lo")
        .collect();
    names.sort();
    names.into_iter().next()
}

/// Every IANA timezone name systemd knows about, for a searchable picker.
/// Falls back to a short common list if `timedatectl` isn't available.
pub fn list_timezones() -> Vec<String> {
    let out = Command::new("timedatectl").arg("list-timezones").output();
    match out {
        Ok(o) if o.status.success() => {
            let v: Vec<String> = String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            if !v.is_empty() {
                return v;
            }
        }
        _ => {}
    }
    ["UTC", "America/New_York", "America/Chicago", "America/Denver", "America/Los_Angeles", "Europe/London", "Europe/Berlin", "Europe/Paris", "Asia/Tokyo", "Asia/Shanghai", "Australia/Sydney"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// A curated set of common locales — not exhaustive (there's no reliable way
/// to enumerate every generatable locale from the live ISO, since it isn't
/// what will be generated on the *target*), but covers the common case; the
/// GUI also keeps a manual-entry field for anything else.
pub const COMMON_LOCALES: &[&str] = &[
    "en_US.UTF-8", "en_GB.UTF-8", "en_CA.UTF-8", "en_AU.UTF-8", "en_IE.UTF-8",
    "de_DE.UTF-8", "fr_FR.UTF-8", "es_ES.UTF-8", "es_MX.UTF-8", "it_IT.UTF-8",
    "pt_BR.UTF-8", "pt_PT.UTF-8", "nl_NL.UTF-8", "pl_PL.UTF-8", "ru_RU.UTF-8",
    "ja_JP.UTF-8", "ko_KR.UTF-8", "zh_CN.UTF-8", "zh_TW.UTF-8", "sv_SE.UTF-8",
    "da_DK.UTF-8", "fi_FI.UTF-8", "nb_NO.UTF-8", "tr_TR.UTF-8", "ar_SA.UTF-8",
    "hi_IN.UTF-8", "el_GR.UTF-8", "cs_CZ.UTF-8", "uk_UA.UTF-8", "he_IL.UTF-8",
];

/// Strip ANSI escape sequences (iwctl colorizes its table output).
fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn scan_wifi(dev: &str) -> Vec<String> {
    let _ = Command::new("iwctl").args(["station", dev, "scan"]).output();
    // iwd's scan takes a few seconds to populate all networks.
    std::thread::sleep(Duration::from_secs(4));
    let out = match Command::new("iwctl").args(["station", dev, "get-networks"]).output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut nets = Vec::new();
    for raw in text.lines() {
        let line = strip_ansi(raw);
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.contains("Available networks")
            || trimmed.contains("Network name")
            || trimmed.chars().all(|c| c == '-' || c.is_whitespace())
        {
            continue;
        }
        // Drop the leading "> " connected-marker column, then the SSID runs up
        // to the 2+ space gap before the Security column (keeps SSIDs w/ spaces).
        let body = line.trim_start();
        let body = body.strip_prefix('>').unwrap_or(body).trim_start();
        let ssid = body.split("  ").next().unwrap_or("").trim();
        if !ssid.is_empty() && !nets.iter().any(|n| n == ssid) {
            nets.push(ssid.to_string());
        }
    }
    nets.truncate(20);
    nets
}

/// Connect to `ssid`, then verify: poll for connectivity until online or
/// timeout. Returns `(online, human_status)` so either front-end can show the
/// result (and surface a likely wrong-password cause).
pub fn connect_wifi(dev: &str, ssid: &str, passphrase: &str) -> (bool, String) {
    let connected = Command::new("iwctl")
        .args(["--passphrase", passphrase, "station", dev, "connect", ssid])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    // Wait for association + DHCP (networkd), polling connectivity.
    let mut online = false;
    for _ in 0..6 {
        std::thread::sleep(Duration::from_secs(2));
        if is_online() {
            online = true;
            break;
        }
    }
    let status = if online {
        format!("✓ Connected to {ssid}")
    } else if !connected {
        format!("✗ Couldn't connect to {ssid} — wrong password?")
    } else {
        format!("✗ Joined {ssid} but no internet yet — wait a moment or retry")
    };
    (online, status)
}

// ---------------------------------------------------------------------------
// Disks
// ---------------------------------------------------------------------------

pub fn list_disks() -> Vec<Disk> {
    let out = Command::new("lsblk").args(["-dpno", "NAME,SIZE,TYPE,MODEL"]).output();
    let Ok(out) = out else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let name = it.next()?.to_string();
            let size = it.next()?.to_string();
            let kind = it.next()?;
            if kind != "disk" {
                return None;
            }
            let model = it.collect::<Vec<_>>().join(" ");
            Some(Disk { name, size, model: if model.is_empty() { "disk".into() } else { model } })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Hardware
// ---------------------------------------------------------------------------

/// Whether an NVIDIA GPU is present (`lspci`'s VGA/3D controller class, vendor
/// "NVIDIA"). Used to offer the proprietary driver only when it's relevant.
pub fn has_nvidia_gpu() -> bool {
    let Ok(out) = Command::new("lspci").arg("-mm").output() else {
        return false;
    };
    String::from_utf8_lossy(&out.stdout).lines().any(|l| {
        let lower = l.to_ascii_lowercase();
        (lower.contains("vga compatible controller") || lower.contains("3d controller"))
            && lower.contains("nvidia")
    })
}

// ---------------------------------------------------------------------------
// Manifests
// ---------------------------------------------------------------------------

/// Manifests offered in the picker: the ISO's bundled examples plus any found
/// on removable media (a `manifests/` folder or loose `*.json`).
pub fn bundled_manifests() -> Vec<String> {
    let mut v = json_files_in("/usr/share/manifest-os/examples");
    v.extend(scan_removable_manifests());
    v.sort();
    v.dedup();
    if v.is_empty() {
        v = vec![
            "tokyonight-aurora".into(),
            "catppuccin-plasma".into(),
            "niri-rice".into(),
            "sway-pro".into(),
        ];
    }
    v
}

fn json_files_in(dir: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            (p.extension().map(|x| x == "json").unwrap_or(false))
                .then(|| p.to_string_lossy().to_string())
        })
        .collect()
}

/// Scan removable drives for manifests. Each removable partition is mounted
/// read-only (the installer runs as root on the ISO), then `manifests/*.json`
/// and any loose `*.json` at its root are collected.
fn scan_removable_manifests() -> Vec<String> {
    let mut found = Vec::new();
    let Ok(out) = Command::new("lsblk")
        .args(["-P", "-p", "-o", "NAME,TYPE,RM,FSTYPE,MOUNTPOINT"])
        .output()
    else {
        return found;
    };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let val = |k: &str| -> String {
            line.split(&format!("{k}=\""))
                .nth(1)
                .and_then(|s| s.split('"').next())
                .unwrap_or("")
                .to_string()
        };
        if val("TYPE") != "part" || val("RM") != "1" || val("FSTYPE").is_empty() {
            continue;
        }
        let name = val("NAME");
        let mut mp = val("MOUNTPOINT");
        if mp.is_empty() {
            let dir = format!("/run/manifest-usb/{}", name.replace('/', "_"));
            let _ = Command::new("mkdir").args(["-p", &dir]).status();
            let ok = Command::new("mount")
                .args(["-o", "ro", &name, &dir])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok {
                continue;
            }
            mp = dir;
        }
        found.extend(json_files_in(&format!("{mp}/manifests")));
        found.extend(json_files_in(&mp));
    }
    found
}

/// An existing OS install we could dual-boot alongside — Windows, another Linux,
/// anything. Carries the pieces a side-by-side install needs.
pub struct ExistingOs {
    /// The whole disk the OS lives on (e.g. `/dev/sda`).
    pub disk: String,
    /// The existing EFI System Partition to reuse (not reformat).
    pub esp: String,
    /// The partition to shrink to make room (its largest resizable filesystem).
    pub shrink_part: String,
    /// That partition's filesystem (`ntfs`, `ext4`, `btrfs`, …) — picks the
    /// shrink tool.
    pub shrink_fstype: String,
    /// Its current size, in GiB.
    pub shrink_size_gib: u32,
    /// A friendly name for the menus: `Windows`, `Ubuntu`, `an existing system`…
    pub label: String,
}

/// Detect an existing OS we could install alongside: a disk that has an EFI
/// System Partition (an OS already boots via UEFI) **and** a resizable
/// filesystem we can shrink for room. Works for Windows or another Linux. A
/// blank/unpartitioned disk returns `None`, so callers just offer a fresh
/// whole-disk install. Read-only (mounts partitions briefly to peek).
pub fn detect_existing_os() -> Option<ExistingOs> {
    let out = Command::new("lsblk")
        .args(["-P", "-p", "-b", "-o", "NAME,TYPE,FSTYPE,SIZE,PKNAME"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let val = |line: &str, k: &str| -> String {
        line.split(&format!("{k}=\""))
            .nth(1)
            .and_then(|s| s.split('"').next())
            .unwrap_or("")
            .to_string()
    };
    struct P {
        name: String,
        fstype: String,
        size: u64,
        disk: String,
    }
    let mut parts = Vec::new();
    for line in text.lines() {
        if val(line, "TYPE") != "part" {
            continue;
        }
        let pk = val(line, "PKNAME");
        let disk = if pk.starts_with("/dev/") { pk } else { format!("/dev/{pk}") };
        parts.push(P {
            name: val(line, "NAME"),
            fstype: val(line, "FSTYPE"),
            size: val(line, "SIZE").parse().unwrap_or(0),
            disk,
        });
    }
    let disks: Vec<String> = {
        let mut d: Vec<String> = parts.iter().map(|p| p.disk.clone()).collect();
        d.sort();
        d.dedup();
        d
    };
    for disk in disks {
        let on = |p: &&P| p.disk == disk;
        // An in-use ESP (a vfat partition that actually holds an /EFI tree).
        let esp = parts.iter().filter(on).find(|p| p.fstype == "vfat" && is_esp(&p.name));
        // The largest filesystem we know how to shrink.
        let shrink = parts
            .iter()
            .filter(on)
            .filter(|p| is_shrinkable(&p.fstype))
            .max_by_key(|p| p.size);
        if let (Some(esp), Some(shrink)) = (esp, shrink) {
            return Some(ExistingOs {
                disk: disk.clone(),
                esp: esp.name.clone(),
                shrink_part: shrink.name.clone(),
                shrink_fstype: shrink.fstype.clone(),
                shrink_size_gib: (shrink.size / (1 << 30)) as u32,
                label: os_label(&esp.name, &shrink.name),
            });
        }
    }
    None
}

/// Filesystems the installer can shrink to make room for a side-by-side install.
fn is_shrinkable(fs: &str) -> bool {
    matches!(fs, "ntfs" | "ext2" | "ext3" | "ext4" | "btrfs")
}

/// Mount a partition read-only and run `peek` against the mountpoint, then
/// unmount. Returns `peek`'s value (or its default on a failed mount).
fn with_ro_mount<T: Default>(part: &str, peek: impl FnOnce(&std::path::Path) -> T) -> T {
    let dir = "/run/manifest-osprobe";
    let _ = Command::new("mkdir").args(["-p", dir]).status();
    let mounted = Command::new("mount")
        .args(["-o", "ro", part, dir])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !mounted {
        return T::default();
    }
    let out = peek(std::path::Path::new(dir));
    let _ = Command::new("umount").arg(dir).status();
    out
}

/// Whether a vfat partition is actually an EFI System Partition in use (has an
/// `/EFI` directory), as opposed to a plain FAT data partition.
fn is_esp(part: &str) -> bool {
    with_ro_mount(part, |root| root.join("EFI").is_dir())
}

/// A friendly name for an existing install: `Windows` if its ESP holds the
/// Windows boot manager, else the distro `NAME` from the shrink partition's
/// os-release, else a generic fallback.
fn os_label(esp: &str, shrink: &str) -> String {
    let windows = with_ro_mount(esp, |root| root.join("EFI/Microsoft/Boot/bootmgfw.efi").exists());
    if windows {
        return "Windows".to_string();
    }
    let name = with_ro_mount(shrink, |root| {
        for rel in ["etc/os-release", "usr/lib/os-release"] {
            if let Ok(txt) = std::fs::read_to_string(root.join(rel)) {
                for line in txt.lines() {
                    if let Some(v) = line.strip_prefix("NAME=") {
                        return v.trim().trim_matches('"').to_string();
                    }
                }
            }
        }
        String::new()
    });
    if name.is_empty() {
        "an existing system".to_string()
    } else {
        name
    }
}

/// Mountpoints of *writable* removable partitions, mounting any that aren't
/// mounted yet. Used to drop the install log onto a USB the user can read after
/// a failure. We deliberately do NOT exclude the boot medium: its read-only
/// ISO9660 partition is skipped by the filesystem filter below, while its FAT
/// partition (an ISO-mode flash, or one freed by copytoram) is exactly where a
/// single-USB user expects the log. We only ever *create* a `logs/` folder; the
/// squashfs the live system reads lives in the ISO9660 part we never touch.
///
/// `target_disk` (e.g. `/dev/sda`) is the disk being installed to — its
/// partitions are always skipped, even in the fallback pass below, so we never
/// write into (or race) whatever the installer is doing to it.
///
/// lsblk's `RM` (removable) flag is what we'd prefer to gate on, but it's not
/// reliable on real hardware: plenty of USB controllers/BIOSes report `RM=0`
/// for an honest-to-god USB stick (VirtualBox's virtual media are *also*
/// always `RM=0`, which is why this had never been exercised against real
/// hardware — see HANDOFF.md). So: prefer `RM=1` drives first, and only if
/// that finds nothing, fall back to any other non-target, non-system-mounted
/// writable partition.
pub fn writable_removable_mounts(target_disk: &str) -> Vec<String> {
    let mut out = Vec::new();

    // If the boot medium is a writable FAT (Rufus "ISO" mode) still mounted
    // read-only at bootmnt, flip it read-write so the log lands on the install
    // USB. Harmlessly fails on a read-only ISO9660 or when copytoram already
    // unmounted it (the scan below then mounts that partition fresh).
    // stderr is silenced: when bootmnt doesn't exist (copytoram, or not an
    // archiso boot), mount prints a scary "mount point does not exist" that
    // isn't an error here — this whole step is best-effort.
    if Path::new("/run/archiso/bootmnt").exists()
        && Command::new("mount")
            .args(["-o", "remount,rw", "/run/archiso/bootmnt"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    {
        out.push("/run/archiso/bootmnt".to_string());
    }
    let Ok(o) = Command::new("lsblk")
        .args(["-P", "-p", "-o", "NAME,TYPE,RM,FSTYPE,MOUNTPOINT"])
        .output()
    else {
        return out;
    };
    let text = String::from_utf8_lossy(&o.stdout).to_string();

    collect_writable_partitions(&text, target_disk, true, &mut out);
    if out.is_empty() {
        collect_writable_partitions(&text, target_disk, false, &mut out);
    }
    out
}

/// One pass over `lsblk -P` output, appending usable mountpoints to `out`.
/// `require_removable` gates on `RM=1`; when false (the fallback pass), any
/// partition not on `target_disk` and not already mounted at a system path is
/// accepted instead.
fn collect_writable_partitions(lsblk_out: &str, target_disk: &str, require_removable: bool, out: &mut Vec<String>) {
    for line in lsblk_out.lines() {
        let val = |k: &str| -> String {
            line.split(&format!("{k}=\""))
                .nth(1)
                .and_then(|s| s.split('"').next())
                .unwrap_or("")
                .to_string()
        };
        if val("TYPE") != "part" {
            continue;
        }
        let name = val("NAME");
        if !target_disk.is_empty() && name.starts_with(target_disk) {
            continue; // never touch the disk we're installing to
        }
        if require_removable && val("RM") != "1" {
            continue;
        }
        // Only filesystems we can actually write a log onto (this skips the
        // boot medium's read-only ISO9660 partition).
        if !matches!(
            val("FSTYPE").as_str(),
            "vfat" | "exfat" | "ext4" | "ext3" | "ext2" | "ntfs" | "f2fs"
        ) {
            continue;
        }
        let mp = val("MOUNTPOINT");
        if !mp.is_empty() {
            // Already mounted somewhere usable (bootmnt is handled above).
            if mp.starts_with("/run/archiso") || out.contains(&mp) {
                continue;
            }
            if !require_removable && is_system_mount(&mp) {
                continue; // fallback pass: don't write into the live root/etc.
            }
            out.push(mp);
        } else {
            // Not mounted (e.g. copytoram freed the boot USB) — mount it rw.
            let dir = format!("/run/manifest-logs/{}", name.replace('/', "_"));
            let _ = Command::new("mkdir").args(["-p", &dir]).status();
            let ok = Command::new("mount")
                .args(["-o", "rw", &name, &dir])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                out.push(dir);
            }
        }
    }
}

/// A mountpoint that's part of the live system itself, never a log-drop target
/// (only relevant to the fallback, non-removable-gated pass).
fn is_system_mount(mp: &str) -> bool {
    mp == "/" || mp.starts_with("/mnt") || matches!(mp, "/usr" | "/etc" | "/var" | "/boot")
}

/// Friendly display name for a manifest source — its `meta.name` if we can read
/// it, else the file stem. Used by the GUI to show "looks" instead of paths.
pub fn manifest_display_name(source: &str) -> String {
    if let Ok(raw) = std::fs::read_to_string(source) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(name) = v.get("meta").and_then(|m| m.get("name")).and_then(|n| n.as_str()) {
                return name.to_string();
            }
        }
    }
    std::path::Path::new(source)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| source.to_string())
}

/// Parse the `survey` questions out of a manifest source (a bundled name or a
/// local path; URLs aren't fetched here). Lets the GUI ask a manifest's
/// author-defined questions and inject the answers.
pub fn manifest_survey(source: &str) -> Vec<crate::manifest::Question> {
    let path = if std::path::Path::new(source).is_file() {
        source.to_string()
    } else if source.starts_with("http://") || source.starts_with("https://") {
        return Vec::new();
    } else {
        format!("/usr/share/manifest-os/examples/{source}.json")
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    #[derive(serde::Deserialize)]
    struct SurveyOnly {
        #[serde(default)]
        survey: Vec<crate::manifest::Question>,
    }
    serde_json::from_str::<SurveyOnly>(&raw).map(|s| s.survey).unwrap_or_default()
}

/// Friendly one-line description for a manifest source (its `meta.description`).
pub fn manifest_description(source: &str) -> Option<String> {
    let raw = std::fs::read_to_string(source).ok()?;
    let v = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
    v.get("meta")
        .and_then(|m| m.get("description"))
        .and_then(|d| d.as_str())
        .map(|s| s.to_string())
}
