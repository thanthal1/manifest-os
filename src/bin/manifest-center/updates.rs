//! Read-only view of the package-version history (`manifest pkglock`).
//!
//! The snapshots live in the CLI's root-owned version repo
//! (`/var/lib/manifest-os/versions`), written by a pacman hook after every
//! transaction. The lockfile holds no secrets (just `pacman -Q` output), so the
//! repo is world-readable and this lists it with **no privileges** — only
//! *restoring* a version set or toggling the pin changes the system, and those
//! go through `pkexec` from [`crate::run_privileged`].

use std::process::Command;

/// One recorded version snapshot, for the list.
pub struct VerSnap {
    pub id: String,
    pub date: String,
    pub label: String,
}

/// Recorded version snapshots, newest first. Empty when tracking hasn't started
/// yet or on a non-Arch box.
pub fn list() -> Vec<VerSnap> {
    // `-c safe.directory` so reading a root-owned repo as the user doesn't trip
    // git's dubious-ownership guard.
    let out = Command::new("git")
        .args(["-c", &format!("safe.directory={}", manifest::pkglock::DIR)])
        .args(["-C", manifest::pkglock::DIR])
        .args(["log", "--format=%h\t%cd\t%s", "--date=format:%Y-%m-%d %H:%M"])
        .output();
    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut p = l.splitn(3, '\t');
            Some(VerSnap {
                id: p.next()?.to_string(),
                date: p.next()?.to_string(),
                label: p.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// Whether the version pin is currently on (reads the world-readable
/// pacman.conf — no root needed).
pub fn pinned() -> bool {
    manifest::pkglock::pin_status()
}
