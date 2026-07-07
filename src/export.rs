//! `manifest export` — capture a running system into a manifest.json.
//!
//! The inverse of install: scan the live system and write out a manifest that
//! would reproduce it. Captures explicitly-installed packages (minus the base
//! system, the detected desktop's own package set, and the kernel — all of
//! which the manifest re-adds implicitly), the desktop + login manager, system
//! settings (hostname, locale, timezone, keymap, kernel), enabled repos, real
//! user accounts, and user-enabled services.
//!
//! With `--install-hook` it also drops a pacman hook so the exported manifest
//! is regenerated after every package install/remove/upgrade — the system's
//! declared state then tracks reality automatically.
//!
//! Everything read here is world-readable (`/etc/*`, the pacman database), so
//! capture runs as a normal user; only `--install-hook` (writing under `/etc`)
//! needs root.

use crate::exec::Ctx;
use crate::{desktop, kernel};
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::path::Path;
use std::process::Command;

/// Where `--install-hook` writes the always-current system manifest.
const SYSTEM_MANIFEST: &str = "/etc/manifest-os/system.json";
const HOOK_PATH: &str = "/etc/pacman.d/hooks/95-manifest-export.hook";

pub fn run(output: Option<&Path>, install_hook: bool, ctx: &Ctx) -> Result<()> {
    let manifest = capture();
    let json = serde_json::to_string_pretty(&manifest)? + "\n";

    if install_hook {
        install_the_hook(ctx)?;
        // The hook regenerates SYSTEM_MANIFEST; seed it now.
        ctx.write_root(SYSTEM_MANIFEST, &json)?;
        println!("✓ Exported to {SYSTEM_MANIFEST} and installed the auto-update pacman hook.");
        println!("  It will refresh after every package install/remove/upgrade.");
        return Ok(());
    }

    match output {
        Some(path) => {
            std::fs::write(path, &json)
                .with_context(|| format!("writing manifest to {}", path.display()))?;
            println!("✓ Exported the current system to {}", path.display());
        }
        None => print!("{json}"),
    }
    Ok(())
}

/// The current system captured as pretty-printed manifest JSON. Read-only (no
/// root needed) — used by the desktop app to snapshot and diff.
pub fn capture_json() -> String {
    serde_json::to_string_pretty(&capture()).unwrap_or_default() + "\n"
}

/// The current system captured as a parsed [`Manifest`], for diffing against
/// another manifest.
pub fn capture_manifest() -> crate::manifest::Manifest {
    crate::manifest::Manifest::from_str(&capture_json())
        .expect("captured manifest is always schema-valid")
}

/// Build the manifest describing the current system.
fn capture() -> Value {
    let mut m = Map::new();
    m.insert("schema_version".into(), json!("1.0.0"));

    let mut meta = Map::new();
    meta.insert("name".into(), json!(hostname().unwrap_or_else(|| "Exported system".into())));
    meta.insert("description".into(), json!("Captured from a running system by `manifest export`."));
    m.insert("meta".into(), Value::Object(meta));

    // system block.
    let mut system = Map::new();
    if let Some(v) = hostname() {
        system.insert("hostname".into(), json!(v));
    }
    if let Some(v) = kv("/etc/locale.conf", "LANG") {
        system.insert("locale".into(), json!(v));
    }
    if let Some(v) = timezone() {
        system.insert("timezone".into(), json!(v));
    }
    if let Some(v) = kv("/etc/vconsole.conf", "KEYMAP") {
        system.insert("keymap".into(), json!(v));
    }
    let installed = installed_explicit();
    if let Some(k) = detect_kernel(&installed) {
        system.insert("kernel".into(), json!(k));
    }
    if !system.is_empty() {
        m.insert("system".into(), Value::Object(system));
    }

    // repos.
    let mut repos = Map::new();
    if pacman_conf_has("[multilib]") {
        repos.insert("multilib".into(), json!(true));
    }
    if pacman_conf_has("[cachyos]") {
        repos.insert("cachyos".into(), json!(true));
    }
    if !repos.is_empty() {
        m.insert("repos".into(), Value::Object(repos));
    }

    // desktop + display manager.
    let is_installed = |p: &str| is_pkg_installed(p);
    let desktop = desktop::detect_installed(is_installed);
    if let Some(d) = desktop {
        m.insert("desktop".into(), json!(d));
        if let Some(dm) = active_dm_key() {
            // Only record it when it isn't the recipe's own default (keeps the
            // manifest minimal; the default is implied by `desktop`).
            let default_dm = desktop::recipe(d).and_then(|r| r.default_dm);
            if Some(dm.as_str()) != default_dm {
                m.insert("display_manager".into(), json!(dm));
            }
        }
    }

    // packages: explicit installs minus base + desktop-implied + kernel.
    let mut skip = base_packages();
    if let Some(d) = desktop {
        skip.extend(desktop::implied_packages(d));
    }
    for k in kernel::catalog() {
        skip.push(k.package.to_string());
        skip.push(k.headers.to_string());
    }
    let packages: Vec<String> = installed
        .iter()
        .filter(|p| !skip.contains(p))
        .cloned()
        .collect();
    if !packages.is_empty() {
        m.insert("packages".into(), json!(packages));
    }

    // user-enabled services (minus system/base + the display manager).
    let services = enabled_services();
    if !services.is_empty() {
        m.insert("services".into(), json!({ "system": services }));
    }

    // real user accounts.
    let users = real_users();
    if !users.is_empty() {
        m.insert("users".into(), json!(users));
    }

    Value::Object(m)
}

// ---------------------------------------------------------------------------
// system readers
// ---------------------------------------------------------------------------

fn hostname() -> Option<String> {
    read_trim("/etc/hostname").filter(|s| !s.is_empty())
}

/// Value of `KEY=value` in a simple shell-style config file (quotes stripped).
fn kv(path: &str, key: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    parse_kv(&content, key)
}

fn parse_kv(content: &str, key: &str) -> Option<String> {
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix(key).and_then(|r| r.trim_start().strip_prefix('=')) {
            let v = rest.trim().trim_matches('"').trim_matches('\'').trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// The IANA timezone from the `/etc/localtime` symlink (everything after
/// `zoneinfo/`), e.g. `America/New_York`.
fn timezone() -> Option<String> {
    let target = std::fs::read_link("/etc/localtime").ok()?;
    let s = target.to_string_lossy();
    s.split_once("zoneinfo/").map(|(_, tz)| tz.to_string())
}

fn read_trim(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// The active login manager as a manifest `display_manager` key, mapped from
/// the enabled unit (`gdm.service` → `gdm`, `ly@tty2.service` → `ly`).
fn active_dm_key() -> Option<String> {
    let unit = desktop::active_dm_unit()?;
    let base = unit.strip_suffix(".service").unwrap_or(&unit);
    base.split('@').next().filter(|s| !s.is_empty()).map(String::from)
}

// ---------------------------------------------------------------------------
// pacman readers
// ---------------------------------------------------------------------------

/// Explicitly-installed packages (`pacman -Qqe`), sorted.
fn installed_explicit() -> Vec<String> {
    let mut v = lines_of(&pacman(&["-Qqe"]));
    v.sort();
    v
}

fn is_pkg_installed(pkg: &str) -> bool {
    Command::new("pacman")
        .args(["-Qq", pkg])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Packages the installer/base system provides implicitly — the members of the
/// `base` and `base-devel` groups plus the fixed extras pacstrap/paru add. The
/// user didn't "choose" these, so they're excluded from the exported list.
fn base_packages() -> Vec<String> {
    let mut v = lines_of(&pacman(&["-Sqg", "base", "base-devel"]));
    for p in ["base", "base-devel", "linux-firmware", "mkinitcpio", "sudo", "git", "paru"] {
        v.push(p.to_string());
    }
    v
}

fn pacman_conf_has(section: &str) -> bool {
    std::fs::read_to_string("/etc/pacman.conf")
        .map(|c| c.lines().any(|l| l.trim_start().starts_with(section)))
        .unwrap_or(false)
}

/// The non-default kernel that's installed, if any (so a stock-`linux` system
/// exports no `kernel` field — it's the default). Prefers a non-`linux`
/// kernel when several are present.
fn detect_kernel(installed: &[String]) -> Option<String> {
    // Only stock `linux` installed → it's the default, omit the field.
    kernel::catalog()
        .iter()
        .filter(|k| k.key != kernel::DEFAULT_KEY)
        .find(|k| installed.iter().any(|p| p == k.package))
        .map(|k| k.key.to_string())
}

fn pacman(args: &[&str]) -> String {
    Command::new("pacman")
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

fn lines_of(s: &str) -> Vec<String> {
    s.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
}

// ---------------------------------------------------------------------------
// services + users
// ---------------------------------------------------------------------------

/// Enabled system services worth recording — everything `systemctl` reports as
/// enabled, minus base/system units and the display manager (those come from
/// the base install or the `desktop` recipe, not a user choice).
fn enabled_services() -> Vec<String> {
    let out = Command::new("systemctl")
        .args(["list-unit-files", "--state=enabled", "--type=service", "--no-legend", "--plain"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    let dm = desktop::active_dm_unit();
    out.lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|u| {
            !u.starts_with("systemd-")
                && !u.starts_with("getty@")
                && !u.starts_with("user@")
                && !u.starts_with("dbus")
                && !u.starts_with("polkit")
                && !u.starts_with("NetworkManager")
                && !u.starts_with("wpa_supplicant")
                && !u.starts_with("ModemManager")
                && !u.starts_with("reflector")
                && Some(*u) != dm.as_deref()
        })
        .map(|u| u.trim_end_matches(".service").to_string())
        .collect()
}

/// Real (human) accounts: UID 1000–59999 from `/etc/passwd`, with their
/// supplementary groups. Passwords are never captured.
fn real_users() -> Vec<Value> {
    let passwd = match std::fs::read_to_string("/etc/passwd") {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    let mut users = Vec::new();
    for line in passwd.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 7 {
            continue;
        }
        let (name, uid, shell) = (f[0], f[2].parse::<u32>().unwrap_or(0), f[6]);
        if !(1000..60000).contains(&uid) {
            continue;
        }
        let groups = user_groups(name);
        let mut u = Map::new();
        u.insert("name".into(), json!(name));
        if !groups.is_empty() {
            u.insert("groups".into(), json!(groups));
        }
        if groups.iter().any(|g| g == "wheel") {
            u.insert("sudo".into(), json!(true));
        }
        if !shell.is_empty() && shell != "/bin/bash" {
            u.insert("shell".into(), json!(shell));
        }
        users.push(Value::Object(u));
    }
    users
}

/// Supplementary groups for `user` (via `id -nG`), excluding the user's own
/// primary group (conventionally same as the username).
fn user_groups(user: &str) -> Vec<String> {
    let out = Command::new("id")
        .args(["-nG", user])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    out.split_whitespace().filter(|g| *g != user).map(String::from).collect()
}

// ---------------------------------------------------------------------------
// pacman hook (auto-update)
// ---------------------------------------------------------------------------

/// Turn on automatic package tracking: install the pacman hook and seed the
/// system manifest. Called at the end of an install so a fresh system keeps
/// `/etc/manifest-os/system.json` current from first boot. Idempotent.
pub fn enable_tracking(ctx: &Ctx) -> Result<()> {
    if ctx.dry_run {
        println!("  · would install a pacman hook keeping {SYSTEM_MANIFEST} in sync with packages");
        return Ok(());
    }
    install_the_hook(ctx)?;
    let json = serde_json::to_string_pretty(&capture())? + "\n";
    ctx.write_root(SYSTEM_MANIFEST, &json)?;
    println!("  · package tracking on — {SYSTEM_MANIFEST} refreshes on every pacman change");
    Ok(())
}

fn install_the_hook(ctx: &Ctx) -> Result<()> {
    // Point the hook at *this* binary, wherever it lives, so it works even if
    // `manifest` isn't on the standard path.
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "manifest".to_string());
    let hook = format!(
        "# Managed by Manifest OS — keep {SYSTEM_MANIFEST} in sync with installed packages.\n\
         [Trigger]\n\
         Operation = Install\n\
         Operation = Remove\n\
         Operation = Upgrade\n\
         Type = Package\n\
         Target = *\n\n\
         [Action]\n\
         Description = Updating the Manifest OS system manifest...\n\
         When = PostTransaction\n\
         Exec = {exe} export --output {SYSTEM_MANIFEST}\n"
    );
    ctx.write_root(HOOK_PATH, &hook)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kv_config_values() {
        assert_eq!(parse_kv("LANG=en_US.UTF-8\n", "LANG").as_deref(), Some("en_US.UTF-8"));
        assert_eq!(parse_kv("KEYMAP=\"us\"\n", "KEYMAP").as_deref(), Some("us"));
        assert_eq!(parse_kv("# LANG=commented\nLANG=de_DE.UTF-8", "LANG").as_deref(), Some("de_DE.UTF-8"));
        assert_eq!(parse_kv("OTHER=x\n", "LANG"), None);
        assert_eq!(parse_kv("KEYMAP=\n", "KEYMAP"), None);
    }

    #[test]
    fn captured_manifest_is_valid_and_minimal() {
        // capture() reads the live system (empty/omitted fields on a non-Arch
        // dev box), but must always yield a schema-valid manifest object.
        let v = capture();
        assert_eq!(v["schema_version"], json!("1.0.0"));
        assert!(v.get("meta").is_some());
        let s = serde_json::to_string(&v).unwrap();
        assert!(crate::manifest::Manifest::from_str(&s).is_ok());
    }

    #[test]
    fn base_denylist_includes_group_members_and_fixed_extras() {
        // On a dev box `pacman -Sqg` returns nothing, but the fixed extras are
        // always present so the filter never lets base/sudo/git leak through.
        let base = base_packages();
        for p in ["base", "base-devel", "sudo", "git", "paru", "linux-firmware"] {
            assert!(base.iter().any(|b| b == p), "missing {p}");
        }
    }
}
