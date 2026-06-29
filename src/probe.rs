//! Read-only system probing and the [`InstallPlan`] — the data the install
//! front-ends (the Ratatui TUI and the GTK GUI) collect and hand to
//! [`crate::installer::execute`]. Keeping it here (rather than in `tui.rs`) lets
//! both front-ends share the same disk/network/manifest discovery and the same
//! plan type, so the engine has exactly one input shape.

use std::process::Command;
use std::time::Duration;

/// A whole disk the system can be installed onto.
pub struct Disk {
    pub name: String,
    pub size: String,
    pub model: String,
}

/// The daily-driver account a friendly install creates. Collected by the GUI
/// ("What's your name?" + a password); the password is only ever fed to
/// `chpasswd` over stdin, never written to disk or logged.
pub struct Account {
    pub full_name: String,
    pub username: String,
    pub password: String,
}

/// What a front-end collected — handed back to the caller to execute.
pub struct InstallPlan {
    pub disk: String,
    pub filesystem: String,
    /// Persistent swap for the *installed* system, one of:
    /// `"none"`, `"zram"` (compressed RAM swap via zram-generator),
    /// `"swapfile"` (a file on root), or `"partition"` (a dedicated partition).
    /// Independent of the always-on install-time zram that keeps low-memory
    /// machines from OOMing during pacstrap/AUR builds.
    pub swap: String,
    /// Size in GiB for `swapfile`/`partition` swap (ignored otherwise).
    pub swap_size_gib: Option<u32>,
    /// A bundled example name, a local path, or an `http(s)` URL.
    pub manifest: String,
    /// Daily-driver account to create (None = use whatever the manifest declares).
    pub account: Option<Account>,
    /// Hostname override (None = use the manifest's, or the default).
    pub hostname: Option<String>,
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
        v = vec!["niri-rice".into(), "hyprland-rice".into(), "gnome".into(), "minimal".into()];
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

/// Friendly one-line description for a manifest source (its `meta.description`).
pub fn manifest_description(source: &str) -> Option<String> {
    let raw = std::fs::read_to_string(source).ok()?;
    let v = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
    v.get("meta")
        .and_then(|m| m.get("description"))
        .and_then(|d| d.as_str())
        .map(|s| s.to_string())
}
