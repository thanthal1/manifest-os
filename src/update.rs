//! `manifest update` — one command to update **everything** on the system, across
//! every package source ManifestOS manages: the Arch host (official repos + AUR),
//! each foreign-distro **stratum** via its own package manager, Flatpak apps, and
//! the Waydroid Android image. A thin orchestrator, like the rest of the engine —
//! it just drives each ecosystem's standard update tool.

use crate::exec::Ctx;
use anyhow::Result;
use std::path::Path;

const STRATA_ROOT: &str = "/strata";
const LIBEXEC_DIR: &str = "/strata/.libexec";

pub fn run(ctx: &Ctx) -> Result<()> {
    update_host(ctx)?;
    update_strata(ctx)?;
    update_flatpak(ctx)?;
    update_android(ctx)?;
    println!("✓ update complete");
    Ok(())
}

/// The Arch host: `paru -Syu` when paru is present (covers official repos **and**
/// the AUR in one pass), else plain `pacman -Syu`.
fn update_host(ctx: &Ctx) -> Result<()> {
    println!("== host (Arch) ==");
    if ctx.check("sh", &["-c", "command -v paru"]) {
        // paru runs as the user and escalates for the pacman half itself.
        ctx.shell("paru -Syu --noconfirm", false)
    } else {
        ctx.shell("pacman -Syu --noconfirm", true)
    }
}

/// Each installed stratum, updated with **its own** package manager through the
/// root enter-helper. Distro is read from the rootfs's `os-release`, so this needs
/// no manifest and works for anything bootstrapped by hand too.
fn update_strata(ctx: &Ctx) -> Result<()> {
    let strata = installed_strata();
    if strata.is_empty() {
        return Ok(());
    }
    for (name, distro) in strata {
        let Some(cmd) = upgrade_cmd(&distro) else {
            println!("== stratum '{name}' ({distro}) — no known updater, skipping ==");
            continue;
        };
        println!("== stratum '{name}' ({distro}) ==");
        // The root-mode enter-helper needs root to unshare+chroot; `root: true`
        // prefixes sudo. The stratum's own pkg manager then runs inside it.
        let helper = format!("{LIBEXEC_DIR}/enter-{name}");
        ctx.shell(&format!("{helper} root sh -c {}", shq(cmd)), true)?;
    }
    Ok(())
}

/// System Flatpak apps, if flatpak is installed.
fn update_flatpak(ctx: &Ctx) -> Result<()> {
    if !ctx.check("flatpak", &["--version"]) {
        return Ok(());
    }
    println!("== flatpak ==");
    ctx.sudo("flatpak", &["update", "--system", "-y", "--noninteractive"])
}

/// The Waydroid Android image, if Waydroid is set up. `waydroid upgrade` only
/// pulls a new image when there is one; best-effort so a failure (e.g. no session
/// available) never fails the whole update.
fn update_android(ctx: &Ctx) -> Result<()> {
    if !ctx.check("sh", &["-c", "command -v waydroid && test -f /var/lib/waydroid/waydroid.cfg"]) {
        return Ok(());
    }
    println!("== android (Waydroid image) ==");
    ctx.shell("waydroid upgrade 2>/dev/null || true", true)
}

/// Discover installed strata: subdirectories of `/strata` (skipping the `.bin` /
/// `.libexec` control dirs) that have a matching enter-helper, paired with the
/// distro `ID` from their `os-release`.
fn installed_strata() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(STRATA_ROOT) else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue; // .bin / .libexec
        }
        let root = e.path();
        if !root.is_dir() {
            continue;
        }
        if !Path::new(&format!("{LIBEXEC_DIR}/enter-{name}")).exists() {
            continue; // not a real, exposed stratum
        }
        let distro = read_distro_id(&root).unwrap_or_default();
        out.push((name, distro));
    }
    out.sort();
    out
}

/// Read `ID=` from a rootfs's `etc/os-release`.
fn read_distro_id(root: &Path) -> Option<String> {
    let os = std::fs::read_to_string(root.join("etc/os-release")).ok()?;
    os.lines().find_map(|l| {
        l.strip_prefix("ID=")
            .map(|v| v.trim().trim_matches('"').to_ascii_lowercase())
    })
}

/// The in-stratum "update + upgrade" command for a distro. Pure — unit-tested.
fn upgrade_cmd(distro: &str) -> Option<&'static str> {
    match distro {
        "debian" | "ubuntu" => {
            Some("apt-get update && DEBIAN_FRONTEND=noninteractive apt-get -y upgrade")
        }
        "fedora" => Some("dnf -y upgrade"),
        "alpine" => Some("apk update && apk upgrade"),
        _ => None,
    }
}

/// Minimal single-quote shell escaping.
fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_cmd_covers_the_four_distros() {
        assert!(upgrade_cmd("debian").unwrap().contains("apt-get"));
        assert!(upgrade_cmd("ubuntu").unwrap().contains("apt-get"));
        assert_eq!(upgrade_cmd("fedora"), Some("dnf -y upgrade"));
        assert!(upgrade_cmd("alpine").unwrap().contains("apk upgrade"));
        assert_eq!(upgrade_cmd("gentoo"), None);
    }

    #[test]
    fn shq_wraps_and_escapes() {
        assert_eq!(shq("apk update && apk upgrade"), "'apk update && apk upgrade'");
        assert_eq!(shq("a'b"), "'a'\\''b'");
    }

    #[test]
    fn read_distro_id_parses_os_release() {
        let dir = std::env::temp_dir().join(format!("mos-upd-test-{}", std::process::id()));
        let etc = dir.join("etc");
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::write(etc.join("os-release"), "NAME=\"Debian\"\nID=debian\nVERSION_ID=\"12\"\n").unwrap();
        assert_eq!(read_distro_id(&dir).as_deref(), Some("debian"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
