//! Dotfiles installation: clone a repo and place its files into `$HOME`.
//!
//! The repo is treated as a **mirror of `$HOME`**: a file at
//! `.config/nvim/init.lua` in the repo lands at `~/.config/nvim/init.lua`.
//! Placement is **per file**, not per top-level
//! directory, so we never replace a whole directory like `~/.config` with a
//! single symlink — we create the directories and link/copy the leaves.
//!
//! `method`:
//!   * `symlink` (default) — symlink each file back to the clone, so editing
//!     the repo updates the live config. The clone therefore lives in a
//!     **persistent** location (never `/tmp`, which is wiped on reboot).
//!   * `copy` — copy each file into `$HOME`; the clone is then disposable.

use crate::exec::Ctx;
use crate::manifest::Dotfiles;
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use std::process::Command;

enum Method {
    Symlink,
    Copy,
}

pub fn install(df: &Dotfiles, ctx: &Ctx) -> Result<()> {
    // The bundled examples ship a "point this at your own repo" placeholder;
    // installing one unedited must not kill the whole install at this stage.
    // A real (non-placeholder) URL that fails to clone is still fatal below —
    // dotfiles are load-bearing for a rice, silently skipping one would be worse.
    if df.source.contains("github.com/you/") {
        println!(
            "  · dotfiles source {} is a placeholder — edit the manifest to point at your own repo; skipping",
            df.source
        );
        return Ok(());
    }
    let method = match df.method.as_str() {
        "" | "symlink" => Method::Symlink,
        "copy" => Method::Copy,
        other => anyhow::bail!("unknown dotfiles method `{other}` (expected symlink|copy)"),
    };

    // Persistent clone location so symlinks survive a reboot.
    let home = if ctx.dry_run {
        "$HOME".to_string()
    } else {
        std::env::var("HOME").context("HOME is not set")?
    };
    let parent = format!("{home}/.local/share/manifest");
    let dest = format!("{parent}/dotfiles");

    // Re-clone cleanly each run so the repo is the source of truth.
    ctx.run("rm", &["-rf", &dest])?;
    ctx.run("mkdir", &["-p", &parent])?;
    ctx.run(
        "git",
        &["clone", "--branch", &df.branch, "--depth", "1", &df.source, &dest],
    )?;

    let verb = match method {
        Method::Symlink => "symlink",
        Method::Copy => "copy",
    };
    if ctx.dry_run {
        println!("  · would {verb} repo files into {home} (mirroring the tree)");
        return Ok(());
    }

    let repo_root = Path::new(&dest);
    let home_path = Path::new(&home);
    let mut count = 0usize;
    place_tree(repo_root, repo_root, home_path, &method, &mut count)?;
    println!("  · {verb}ed {count} file(s) into {home}");
    Ok(())
}

/// Recursively place every regular file under `dir` into `$HOME`, preserving
/// its path relative to the repo root.
fn place_tree(
    repo_root: &Path,
    dir: &Path,
    home: &Path,
    method: &Method,
    count: &mut usize,
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();

        // At the repo root, skip VCS internals and project meta files.
        if dir == repo_root
            && (name == ".git"
                || name == ".github"
                || name == ".gitignore"
                || name.starts_with("README")
                || name.starts_with("LICENSE"))
        {
            continue;
        }

        if path.is_dir() {
            place_tree(repo_root, &path, home, method, count)?;
        } else {
            let rel = path.strip_prefix(repo_root)?;
            let target = home.join(rel);
            if let Some(p) = target.parent() {
                fs::create_dir_all(p)?;
            }
            place_file(method, &path, &target)?;
            *count += 1;
        }
    }
    Ok(())
}

fn place_file(method: &Method, src: &Path, dst: &Path) -> Result<()> {
    let (src, dst) = (
        src.to_str().context("non-UTF-8 source path")?,
        dst.to_str().context("non-UTF-8 target path")?,
    );
    let (prog, args): (&str, [&str; 3]) = match method {
        // -f overwrite, -n don't dereference an existing dir symlink at dst.
        Method::Symlink => ("ln", ["-sfn", src, dst]),
        Method::Copy => ("cp", ["-f", src, dst]),
    };
    let status = Command::new(prog)
        .args(args)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run {prog}: {e}"))?;
    if !status.success() {
        anyhow::bail!("{prog} failed placing {dst}");
    }
    Ok(())
}
