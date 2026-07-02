//! Git-based manifest history and rollback.
//!
//! Every `install`/`sync` records the applied manifest into a small git repo
//! at `/var/lib/manifest-os/history` (one tracked file, `manifest.json`,
//! overwritten and committed each time). Because the manifest *is* the system's
//! declared state, that history is a full audit log — and reverting a commit
//! reverts the system: [`rollback`] loads a previous manifest and re-applies it
//! with [`install::sync`], so undoing a bad edit is one command.
//!
//! Secrets are scrubbed before committing (user passwords blanked — rollback
//! doesn't reset existing accounts anyway), and the repo lives in a root-only
//! `0700` directory. Manifests can still carry sensitive values elsewhere (a
//! password baked into a `files` entry, say), so the directory permissions are
//! the real protection, not the scrub.

use crate::exec::Ctx;
use crate::install;
use crate::manifest::Manifest;
use anyhow::{bail, Context, Result};
use std::process::Command;

const DIR: &str = "/var/lib/manifest-os/history";
const FILE: &str = "manifest.json";

/// Commit the just-applied manifest as a new history entry. Best-effort: a
/// failure (git missing, no sudo, …) warns but never fails the apply that
/// produced it — history is a convenience, not a prerequisite.
pub fn record(manifest_json: &str, name: &str, ctx: &Ctx) {
    if ctx.dry_run {
        println!("  · would record this manifest to the rollback history");
        return;
    }
    let label = if name.is_empty() { "(unnamed)" } else { name };
    let stamp = ctx.output(false, "date", &["+%Y-%m-%d %H:%M:%S"]).unwrap_or_default();
    let msg = format!("apply {label} — {stamp}");
    if let Err(e) = commit(manifest_json, &msg, ctx) {
        println!("  · note: couldn't record rollback history ({e:#})");
    }
}

/// Print the applied-manifest history, newest first.
pub fn show() -> Result<()> {
    match capture_git(&["log", "--format=%h  %ci  %s"]) {
        Ok(out) if !out.trim().is_empty() => {
            println!("Applied manifests (newest first):\n");
            println!("{}", out.trim_end());
            println!("\nRoll back with `manifest rollback [<ref>]` (default: the previous one).");
            Ok(())
        }
        _ => {
            println!("No manifest history yet — it starts on your first install or sync.");
            Ok(())
        }
    }
}

/// Re-apply a previous manifest, undoing later edits. `reference` is a git ref:
/// a bare integer N means "N applies ago" (`HEAD~N`), anything else is passed
/// through (a short hash, `HEAD~2`, …). Defaults to the previous manifest.
/// The rolled-back manifest is re-synced and then itself recorded, so history
/// is append-only and you can always roll forward again.
pub fn rollback(reference: Option<&str>, dry_run: bool) -> Result<()> {
    let refspec = match reference {
        None => "HEAD~1".to_string(),
        Some(r) if !r.is_empty() && r.chars().all(|c| c.is_ascii_digit()) => format!("HEAD~{r}"),
        Some(r) => r.to_string(),
    };

    let json = capture_git(&["show", &format!("{refspec}:{FILE}")]).with_context(|| {
        format!("no recorded manifest at `{refspec}` — run `manifest history` to see what's available")
    })?;
    let manifest = Manifest::from_str(&json)?;
    let label = if manifest.meta.name.is_empty() { "(unnamed)".into() } else { manifest.meta.name.clone() };

    println!("↩ Rolling back to {refspec}: \"{label}\"\n");
    let ctx = Ctx::new(dry_run);
    install::sync(&manifest, &ctx)?;

    if !dry_run {
        let stamp = ctx.output(false, "date", &["+%Y-%m-%d %H:%M:%S"]).unwrap_or_default();
        let msg = format!("rollback to {refspec} ({label}) — {stamp}");
        if let Err(e) = commit(&json, &msg, &ctx) {
            println!("  · note: couldn't record the rollback ({e:#})");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------

/// Write the scrubbed manifest and commit it (only if it actually changed from
/// the last recorded state). Assumes a non-dry-run `ctx`.
fn commit(manifest_json: &str, message: &str, ctx: &Ctx) -> Result<()> {
    ensure_repo(ctx)?;
    let scrubbed = scrub_secrets(manifest_json)?;
    ctx.write_root(&format!("{DIR}/{FILE}"), &scrubbed)?;
    ctx.sudo("git", &["-C", DIR, "add", FILE])?;
    // `diff --cached --quiet` exits 0 when nothing is staged-different.
    if ctx.check("sudo", &["git", "-C", DIR, "diff", "--cached", "--quiet"]) {
        println!("  · manifest unchanged — nothing new to record");
        return Ok(());
    }
    ctx.sudo("git", &["-C", DIR, "commit", "-q", "-m", message])?;
    println!("  · recorded to manifest history (undo with `manifest rollback`)");
    Ok(())
}

/// Create the history dir (root-only) and initialize the git repo + identity
/// if absent. Idempotent.
fn ensure_repo(ctx: &Ctx) -> Result<()> {
    ctx.sudo("mkdir", &["-p", DIR])?;
    ctx.sudo("chmod", &["700", DIR])?;
    if !ctx.check("sudo", &["test", "-d", &format!("{DIR}/.git")]) {
        ctx.sudo("git", &["-C", DIR, "init", "-q"])?;
        ctx.sudo("git", &["-C", DIR, "config", "user.name", "Manifest OS"])?;
        ctx.sudo("git", &["-C", DIR, "config", "user.email", "manifest-os@localhost"])?;
    }
    Ok(())
}

/// Blank out user passwords in a manifest's JSON before it's committed. Also
/// the semantically-correct thing for rollback: re-applying a manifest to an
/// existing system shouldn't reset already-created accounts' passwords.
fn scrub_secrets(json: &str) -> Result<String> {
    let mut v: serde_json::Value = serde_json::from_str(json).context("parsing manifest to scrub")?;
    if let Some(users) = v.get_mut("users").and_then(|u| u.as_array_mut()) {
        for u in users.iter_mut() {
            if let Some(obj) = u.as_object_mut() {
                if obj.contains_key("password") {
                    obj.insert("password".to_string(), serde_json::Value::Null);
                }
            }
        }
    }
    Ok(serde_json::to_string_pretty(&v)? + "\n")
}

/// Run `sudo git -C DIR <args>` and capture stdout. Read-only helper, so it
/// runs regardless of dry-run (rollback --dry-run still needs to read the
/// target manifest to preview it).
fn capture_git(args: &[&str]) -> Result<String> {
    let out = Command::new("sudo")
        .arg("git")
        .args(["-C", DIR])
        .args(args)
        .output()
        .context("failed to run git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_blanks_user_passwords_only() {
        let json = r#"{
            "schema_version": "1.0.0",
            "users": [
                {"name": "alice", "password": "hunter2", "sudo": true},
                {"name": "bob"}
            ],
            "packages": ["git"]
        }"#;
        let out = scrub_secrets(json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["users"][0]["password"].is_null());
        assert_eq!(v["users"][0]["name"], "alice");
        assert_eq!(v["users"][0]["sudo"], true);
        // A user that never had a password stays as-is (no key injected).
        assert!(v["users"][1].get("password").is_none());
        // Non-secret fields untouched.
        assert_eq!(v["packages"][0], "git");
    }

    #[test]
    fn scrub_is_a_noop_without_users() {
        let json = r#"{"schema_version":"1.0.0","packages":["vim"]}"#;
        let v: serde_json::Value = serde_json::from_str(&scrub_secrets(json).unwrap()).unwrap();
        assert_eq!(v["packages"][0], "vim");
    }

    // The ref-resolution rule rollback() uses, factored for testing.
    fn refspec(reference: Option<&str>) -> String {
        match reference {
            None => "HEAD~1".to_string(),
            Some(r) if !r.is_empty() && r.chars().all(|c| c.is_ascii_digit()) => format!("HEAD~{r}"),
            Some(r) => r.to_string(),
        }
    }

    #[test]
    fn rollback_ref_defaults_and_number_shorthand() {
        assert_eq!(refspec(None), "HEAD~1");
        assert_eq!(refspec(Some("3")), "HEAD~3");
        assert_eq!(refspec(Some("HEAD~2")), "HEAD~2");
        assert_eq!(refspec(Some("a1b2c3d")), "a1b2c3d");
    }
}
