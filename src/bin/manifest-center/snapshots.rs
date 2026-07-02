//! The desktop app's snapshot store — a small git repo in the user's home.
//!
//! Kept deliberately separate from the CLI's root-owned history
//! (`/var/lib/manifest-os/history`): this one lives in
//! `~/.local/share/manifest-os/snapshots`, owned by the user, so saving and
//! browsing snapshots needs **no root** (capturing the system and committing
//! are all read-only or user-owned). Only *restoring* a snapshot changes the
//! system, and that goes through `pkexec` — the one place a password is asked.

use std::path::PathBuf;
use std::process::Command;

const FILE: &str = "system.json";

/// One saved snapshot, for the list.
pub struct Snap {
    pub id: String,
    pub date: String,
    pub label: String,
}

fn dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    PathBuf::from(home).join(".local/share/manifest-os/snapshots")
}

fn git(args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("git").arg("-C").arg(dir()).args(args).output()
}

/// Create the repo + identity on first use. Idempotent.
fn ensure_repo() -> std::io::Result<()> {
    std::fs::create_dir_all(dir())?;
    if !dir().join(".git").is_dir() {
        git(&["init", "-q"])?;
        git(&["config", "user.name", "Manifest OS"])?;
        git(&["config", "user.email", "manifest-os@localhost"])?;
    }
    Ok(())
}

/// Capture the current system and commit it as a new snapshot. No root needed.
/// Returns an error string on failure (for a toast).
pub fn save(label: &str) -> Result<(), String> {
    ensure_repo().map_err(|e| e.to_string())?;
    let json = manifest::export::capture_json();
    std::fs::write(dir().join(FILE), json).map_err(|e| e.to_string())?;
    git(&["add", FILE]).map_err(|e| e.to_string())?;
    // Nothing changed since the last snapshot? Still make a commit so the user
    // gets a dated restore point (allow-empty via a message-only amend-free path).
    let name = if label.trim().is_empty() { "Snapshot".to_string() } else { label.trim().to_string() };
    let out = git(&["commit", "-q", "--allow-empty", "-m", &name]).map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(())
}

/// Every saved snapshot, newest first.
pub fn list() -> Vec<Snap> {
    let out = match git(&["log", "--format=%h\t%cd\t%s", "--date=format:%Y-%m-%d %H:%M"]) {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut p = l.splitn(3, '\t');
            Some(Snap {
                id: p.next()?.to_string(),
                date: p.next()?.to_string(),
                label: p.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// The captured manifest JSON stored in snapshot `id`.
pub fn json_at(id: &str) -> Result<String, String> {
    let out = git(&["show", &format!("{id}:{FILE}")]).map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}
