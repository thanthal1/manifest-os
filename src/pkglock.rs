//! Package version tracking + update rollback.
//!
//! Newest-by-default is best for security, so Manifest OS never holds packages
//! back on its own. But a `pacman -Syu` can still break a system, and the fix is
//! almost always "put the previous versions back". This module is that safety
//! net: every package transaction snapshots the **exact installed version** of
//! every package — official *and* AUR/foreign (`pacman -Q` lists both) — into a
//! small git repo, so any past state is nameable and restorable.
//!
//! Two halves:
//!   * **pure logic** (parse/render a lockfile, diff two version sets, name a
//!     change accurately, plan a downgrade) — unit-tested, no system access;
//!   * **runtime** (`snapshot`, `restore`) — capture `pacman -Q`, commit to the
//!     version-history repo, and downgrade via `pacman -U` from the local cache
//!     (falling back to the Arch Linux Archive).
//!
//! The version repo (`/var/lib/manifest-os/versions`) is deliberately *separate*
//! from the manifest rollback history ([`crate::history`]): package snapshots
//! land on every pacman run, and mixing them in would wreck `rollback N`'s
//! "N applies ago" counting.

use crate::exec::Ctx;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::process::Command;

/// Root-only git repo holding the version lockfile timeline.
pub const DIR: &str = "/var/lib/manifest-os/versions";
/// The single tracked file: `name version` per line, sorted.
pub const LOCK_FILE: &str = "packages.lock";
/// Pacman hook that runs [`snapshot`] after every package transaction.
const HOOK_PATH: &str = "/etc/pacman.d/hooks/96-manifest-versions.hook";
/// Where pacman keeps downloaded package files (the downgrade source of truth).
const CACHE_DIR: &str = "/var/cache/pacman/pkg";
/// Arch Linux Archive — historical package files for official-repo downgrades.
const ALA: &str = "https://archive.archlinux.org/packages";

// ---------------------------------------------------------------------------
// pure logic
// ---------------------------------------------------------------------------

/// Parse `name version` lines (a lockfile or raw `pacman -Q`) into name→version.
/// Blank lines and malformed rows are skipped.
pub fn parse(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() {
                return None;
            }
            let (name, ver) = l.split_once(char::is_whitespace)?;
            let ver = ver.trim();
            (!name.is_empty() && !ver.is_empty()).then(|| (name.to_string(), ver.to_string()))
        })
        .collect()
}

/// Render name→version as sorted `name version` lines (BTreeMap is ordered).
pub fn render(map: &BTreeMap<String, String>) -> String {
    let mut s = String::new();
    for (name, ver) in map {
        s.push_str(name);
        s.push(' ');
        s.push_str(ver);
        s.push('\n');
    }
    s
}

/// What moved between two version sets.
#[derive(Debug, Default, PartialEq)]
pub struct Change {
    /// `(name, old_version, new_version)` — same package, different version.
    pub upgraded: Vec<(String, String, String)>,
    /// `(name, version)` — present now, absent before.
    pub added: Vec<(String, String)>,
    /// `(name, version)` — present before, absent now.
    pub removed: Vec<(String, String)>,
}

impl Change {
    pub fn is_empty(&self) -> bool {
        self.upgraded.is_empty() && self.added.is_empty() && self.removed.is_empty()
    }
}

/// Diff `old` → `new`. "upgraded" covers any version change (up or down).
pub fn diff(old: &BTreeMap<String, String>, new: &BTreeMap<String, String>) -> Change {
    let mut c = Change::default();
    for (name, nv) in new {
        match old.get(name) {
            Some(ov) if ov != nv => c.upgraded.push((name.clone(), ov.clone(), nv.clone())),
            Some(_) => {}
            None => c.added.push((name.clone(), nv.clone())),
        }
    }
    for (name, ov) in old {
        if !new.contains_key(name) {
            c.removed.push((name.clone(), ov.clone()));
        }
    }
    c
}

/// A short, accurate one-line name for a snapshot, e.g.
/// `"23 upgraded, +2, -1 — linux 6.15.1→6.16, mesa 25.1→25.2, systemd 256→257"`.
/// Highlights a few notable packages (kernel/graphics/init first) so the entry
/// reads meaningfully in a list, then the counts.
pub fn summarize(c: &Change) -> String {
    if c.is_empty() {
        return "no version changes".to_string();
    }
    let mut parts = Vec::new();
    if !c.upgraded.is_empty() {
        parts.push(format!("{} upgraded", c.upgraded.len()));
    }
    if !c.added.is_empty() {
        parts.push(format!("+{}", c.added.len()));
    }
    if !c.removed.is_empty() {
        parts.push(format!("-{}", c.removed.len()));
    }
    let counts = parts.join(", ");

    // Pick up to 3 notable upgrades to spell out. Prefer headline packages, then
    // fall back to whatever's first (the list is already alphabetical).
    const NOTABLE: [&str; 6] = ["linux", "linux-cachyos", "mesa", "systemd", "nvidia", "glibc"];
    let mut highlights: Vec<&(String, String, String)> = c
        .upgraded
        .iter()
        .filter(|(n, _, _)| NOTABLE.iter().any(|h| n == h || n.starts_with("linux-")))
        .collect();
    for u in &c.upgraded {
        if highlights.len() >= 3 {
            break;
        }
        if !highlights.iter().any(|h| h.0 == u.0) {
            highlights.push(u);
        }
    }
    highlights.truncate(3);

    if highlights.is_empty() {
        return counts;
    }
    let detail = highlights
        .iter()
        .map(|(n, o, nw)| format!("{n} {}→{}", short_ver(o), short_ver(nw)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{counts} — {detail}")
}

/// Trim a pacman version to something readable: drop the `epoch:` prefix and the
/// `-pkgrel` suffix for display (e.g. `1:25.1.7-2` → `25.1.7`). The lockfile
/// still stores the full version; this is cosmetic, for the summary line only.
fn short_ver(v: &str) -> &str {
    let v = v.split_once(':').map(|(_, r)| r).unwrap_or(v);
    v.rsplit_once('-').map(|(l, _)| l).unwrap_or(v)
}

/// Packages to move to restore `target`: every package whose *current* version
/// differs from the target's and that still exists in the target set. Returned
/// as `(name, target_version)`. Packages absent from `target` (installed since)
/// are left alone — restoring versions shouldn't uninstall things.
pub fn downgrade_targets(
    current: &BTreeMap<String, String>,
    target: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    target
        .iter()
        .filter(|(name, tv)| current.get(*name).is_some_and(|cv| cv != *tv))
        .map(|(n, v)| (n.clone(), v.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// runtime — capture
// ---------------------------------------------------------------------------

/// The current installed version of every package (`pacman -Q`), incl. AUR /
/// foreign. Empty on a non-Arch box (no pacman) — callers treat that as "nothing
/// to snapshot".
pub fn capture() -> BTreeMap<String, String> {
    let out = Command::new("pacman")
        .arg("-Q")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    parse(&out)
}

// ---------------------------------------------------------------------------
// runtime — snapshot into the version-history git repo
// ---------------------------------------------------------------------------

/// Snapshot the current version set and commit it if it changed since the last
/// snapshot, naming the commit for what actually moved. Invoked by the pacman
/// hook after every transaction, so it must be quiet and cheap when nothing
/// changed. Best-effort: never fails the pacman transaction that triggered it.
pub fn snapshot(ctx: &Ctx) -> Result<()> {
    let current = capture();
    if current.is_empty() {
        return Ok(()); // non-Arch / dry dev box — nothing to record.
    }
    ensure_repo(ctx)?;

    let previous = read_lock_at("HEAD").map(|s| parse(&s)).unwrap_or_default();
    let change = diff(&previous, &current);
    if change.is_empty() && !previous.is_empty() {
        return Ok(()); // versions unchanged since last snapshot.
    }

    ctx.write_root(&format!("{DIR}/{LOCK_FILE}"), &render(&current))?;
    ctx.sudo("git", &["-C", DIR, "add", LOCK_FILE])?;
    if ctx.check("sudo", &["git", "-C", DIR, "diff", "--cached", "--quiet"]) {
        return Ok(());
    }
    let stamp = ctx.output(false, "date", &["+%Y-%m-%d %H:%M"]).unwrap_or_default();
    let name = if previous.is_empty() {
        format!("baseline — {} packages", current.len())
    } else {
        summarize(&change)
    };
    let msg = format!("{name} ({stamp})");
    ctx.sudo("git", &["-C", DIR, "commit", "-q", "-m", &msg])?;
    println!("  · package versions snapshotted — {name}");
    Ok(())
}

/// Install the pacman hook that snapshots versions after every transaction, and
/// seed an initial baseline. Called from [`crate::export::enable_tracking`] at
/// the end of an install so a fresh system tracks versions from first boot.
/// Idempotent. Best-effort: a failure here must not fail the install.
pub fn enable_hook(ctx: &Ctx) -> Result<()> {
    if ctx.dry_run {
        println!("  · would install a pacman hook snapshotting package versions for rollback");
        return Ok(());
    }
    // Point the hook at *this* binary wherever it lives (mirrors export's hook).
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "manifest".to_string());
    let hook = format!(
        "# Managed by Manifest OS — snapshot package versions for update rollback.\n\
         [Trigger]\n\
         Operation = Install\n\
         Operation = Remove\n\
         Operation = Upgrade\n\
         Type = Package\n\
         Target = *\n\n\
         [Action]\n\
         Description = Snapshotting package versions (Manifest OS)...\n\
         When = PostTransaction\n\
         Exec = {exe} snapshot-packages\n"
    );
    ctx.write_root(HOOK_PATH, &hook)?;
    // Seed the baseline now so there's a snapshot to roll back *to* even before
    // the first upgrade.
    snapshot(ctx)?;
    println!("  · package-version tracking on — snapshots on every pacman change");
    Ok(())
}

// ---------------------------------------------------------------------------
// runtime — restore (downgrade) to a recorded snapshot
// ---------------------------------------------------------------------------

/// Restore the exact package versions recorded at `reference` (default: the
/// snapshot before the current one). Resolves each downgrade's package file from
/// the local pacman cache, then the Arch Linux Archive, and applies them in one
/// `pacman -U`. Anything it can't find (a cache-cleared AUR build, usually) is
/// listed so the user knows what to rebuild.
pub fn restore(reference: Option<&str>, ctx: &Ctx) -> Result<()> {
    let refspec = match reference {
        None => "HEAD~1".to_string(),
        Some(r) if !r.is_empty() && r.chars().all(|c| c.is_ascii_digit()) => format!("HEAD~{r}"),
        Some(r) => r.to_string(),
    };
    let target = parse(&read_lock_at(&refspec).with_context(|| {
        format!("no version snapshot at `{refspec}` — see `manifest history --versions`")
    })?);
    if target.is_empty() {
        bail!("the snapshot at `{refspec}` is empty");
    }

    let current = capture();
    let targets = downgrade_targets(&current, &target);
    if targets.is_empty() {
        println!("✓ Already at the versions recorded in {refspec} — nothing to restore.");
        return Ok(());
    }

    println!("↩ Restoring {} package version(s) from {refspec}:\n", targets.len());
    let mut files = Vec::new();
    let mut missing = Vec::new();
    for (name, ver) in &targets {
        match resolve_package_file(name, ver, ctx) {
            Some(path) => {
                println!("  · {name} → {}", short_ver(ver));
                files.push(path);
            }
            None => missing.push(format!("{name} {ver}")),
        }
    }

    if !missing.is_empty() {
        println!(
            "\n  ! couldn't find package files for {} package(s) (cache cleared / AUR):",
            missing.len()
        );
        for m in &missing {
            println!("      {m}");
        }
        println!("    These stay at their current version — rebuild AUR ones manually if needed.");
    }
    if files.is_empty() {
        bail!("no package files available to restore — nothing changed");
    }

    let mut args = vec!["-U", "--noconfirm"];
    args.extend(files.iter().map(String::as_str));
    ctx.sudo("pacman", &args)?;
    println!("\n✓ Restored {} package(s) from {refspec}.", files.len());
    if !missing.is_empty() {
        println!("  ({} could not be restored — see above.)", missing.len());
    }
    Ok(())
}

/// Find a package file for `name`-`version`: the local pacman cache first (most
/// reliable — pacman keeps downloaded packages by default), then the Arch Linux
/// Archive for official-repo packages. Returns a local path ready for `pacman
/// -U` (Archive files are downloaded into the cache first). `None` when neither
/// has it (typically an AUR/foreign package whose cache was cleared).
fn resolve_package_file(name: &str, version: &str, ctx: &Ctx) -> Option<String> {
    // Local cache: match `<name>-<version>-<arch>.pkg.tar.*`, skipping `.sig`.
    if let Ok(entries) = std::fs::read_dir(CACHE_DIR) {
        let prefix = format!("{name}-{version}-");
        for e in entries.flatten() {
            let fname = e.file_name().to_string_lossy().to_string();
            if fname.starts_with(&prefix) && is_pkg_file(&fname) {
                return Some(format!("{CACHE_DIR}/{fname}"));
            }
        }
    }
    // Arch Linux Archive: only official-repo packages live there. Try the common
    // arch/extension combos; the first that downloads into the cache wins.
    let first = name.chars().next()?;
    for arch in ["x86_64", "any"] {
        for ext in ["zst", "xz"] {
            let file = format!("{name}-{version}-{arch}.pkg.tar.{ext}");
            let url = format!("{ALA}/{first}/{name}/{file}");
            let dest = format!("{CACHE_DIR}/{file}");
            // -f fails on 404 so a miss doesn't leave a 0-byte/HTML file behind.
            if ctx
                .sudo("curl", &["-fsSL", "-o", &dest, &url])
                .is_ok()
                && std::path::Path::new(&dest).exists()
            {
                return Some(dest);
            }
        }
    }
    None
}

fn is_pkg_file(name: &str) -> bool {
    (name.ends_with(".pkg.tar.zst") || name.ends_with(".pkg.tar.xz")) && !name.ends_with(".sig")
}

// ---------------------------------------------------------------------------
// runtime — the version-history git repo + listing
// ---------------------------------------------------------------------------

/// List recorded version snapshots, newest first (for `manifest history
/// --versions` and the GUI).
pub fn show() -> Result<()> {
    match capture_git(&["log", "--format=%h  %ci  %s"]) {
        Ok(out) if !out.trim().is_empty() => {
            println!("Package version snapshots (newest first):\n");
            println!("{}", out.trim_end());
            println!("\nRestore one with `manifest restore-versions [<ref>]` (default: the previous).");
            Ok(())
        }
        _ => {
            println!("No package version snapshots yet — they start on your next package change.");
            Ok(())
        }
    }
}

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

/// Read the lockfile at a git ref (`HEAD`, `HEAD~1`, a short hash…). `None` when
/// the repo or that revision doesn't exist yet.
fn read_lock_at(refspec: &str) -> Option<String> {
    capture_git(&["show", &format!("{refspec}:{LOCK_FILE}")]).ok()
}

fn capture_git(args: &[&str]) -> Result<String> {
    let out = Command::new("sudo")
        .arg("git")
        .args(["-C", DIR])
        .args(args)
        .output()
        .context("failed to run git")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(n, v)| (n.to_string(), v.to_string())).collect()
    }

    #[test]
    fn parse_and_render_round_trip() {
        let text = "firefox 1.2.3-1\nlinux 6.16.arch1-1\n";
        let m = parse(text);
        assert_eq!(m.get("firefox").map(String::as_str), Some("1.2.3-1"));
        assert_eq!(m.get("linux").map(String::as_str), Some("6.16.arch1-1"));
        // render is sorted; firefox < linux, so order matches.
        assert_eq!(render(&m), text);
    }

    #[test]
    fn parse_skips_blank_and_malformed() {
        let m = parse("\n  \nfoo 1.0\nbar\n\n");
        assert_eq!(m.len(), 1);
        assert_eq!(m.get("foo").map(String::as_str), Some("1.0"));
    }

    #[test]
    fn diff_classifies_upgrade_add_remove() {
        let old = map(&[("a", "1"), ("b", "1"), ("gone", "9")]);
        let new = map(&[("a", "2"), ("b", "1"), ("fresh", "1")]);
        let c = diff(&old, &new);
        assert_eq!(c.upgraded, vec![("a".into(), "1".into(), "2".into())]);
        assert_eq!(c.added, vec![("fresh".into(), "1".into())]);
        assert_eq!(c.removed, vec![("gone".into(), "9".into())]);
        assert!(!c.is_empty());
    }

    #[test]
    fn diff_of_identical_is_empty() {
        let m = map(&[("a", "1"), ("b", "2")]);
        assert!(diff(&m, &m).is_empty());
    }

    #[test]
    fn short_ver_strips_epoch_and_pkgrel() {
        assert_eq!(short_ver("1:25.1.7-2"), "25.1.7");
        assert_eq!(short_ver("6.16.arch1-1"), "6.16.arch1");
        assert_eq!(short_ver("1.0"), "1.0");
    }

    #[test]
    fn summarize_highlights_kernel_and_counts() {
        let old = map(&[("linux", "6.15.1-1"), ("zzz", "1-1"), ("old", "1-1")]);
        let new = map(&[("linux", "6.16-1"), ("zzz", "2-1"), ("new", "1-1")]);
        let s = summarize(&diff(&old, &new));
        assert!(s.contains("2 upgraded"), "{s}");
        assert!(s.contains("+1"), "{s}"); // new
        assert!(s.contains("-1"), "{s}"); // old removed
        assert!(s.contains("linux 6.15.1→6.16"), "{s}"); // kernel highlighted first
    }

    #[test]
    fn summarize_empty_change() {
        assert_eq!(summarize(&Change::default()), "no version changes");
    }

    #[test]
    fn downgrade_targets_only_changed_and_still_present() {
        let current = map(&[("a", "2"), ("b", "1"), ("since", "1")]);
        let target = map(&[("a", "1"), ("b", "1"), ("gone", "9")]);
        let t = downgrade_targets(&current, &target);
        // a: changed → include at target ver. b: same → skip. gone: not installed
        // now → skip (don't reinstall). since: not in target → left alone.
        assert_eq!(t, vec![("a".into(), "1".into())]);
    }

    #[test]
    fn is_pkg_file_accepts_pkgs_not_sigs() {
        assert!(is_pkg_file("firefox-1.2.3-1-x86_64.pkg.tar.zst"));
        assert!(is_pkg_file("foo-1-1-any.pkg.tar.xz"));
        assert!(!is_pkg_file("firefox-1.2.3-1-x86_64.pkg.tar.zst.sig"));
        assert!(!is_pkg_file("firefox.txt"));
    }
}
