//! Declarative file writing.
//!
//! Replaces the most common `post_install` hooks — `mkdir -p`, `echo >`,
//! `cat > file`. A `files` entry names a path and its content; the CLI creates
//! the parent directories and writes it, as the user for `~/...` paths or as
//! root for absolute system paths.

use crate::exec::Ctx;
use crate::manifest::FileSpec;
use anyhow::Result;
use std::path::{Path, PathBuf};

pub fn apply(files: &[FileSpec], ctx: &Ctx) -> Result<()> {
    for f in files {
        let (path, user_level) = resolve(&f.path, ctx);

        // An explicit owner means a chown, which needs root.
        if user_level && f.owner.is_none() {
            ctx.write_user(&path, &f.content)?;
            if let Some(mode) = &f.mode {
                ctx.run("chmod", &[mode, &path])?;
            }
        } else {
            // Capture which directories we're about to create, so `owner` can
            // claim the whole new tree (e.g. provisioning /home/x/.config/...),
            // not just the leaf file.
            let created = if f.owner.is_some() && !ctx.dry_run {
                topmost_created(Path::new(&path))
            } else {
                None
            };
            ctx.write_root(&path, &f.content)?;
            if let Some(mode) = &f.mode {
                ctx.sudo("chmod", &[mode, &path])?;
            }
            if let Some(owner) = &f.owner {
                match &created {
                    Some(top) => ctx.sudo("chown", &["-R", owner, &top.to_string_lossy()])?,
                    None => ctx.sudo("chown", &[owner, &path])?,
                }
            }
        }
    }
    Ok(())
}

/// The highest ancestor directory that does not yet exist — i.e. the topmost
/// directory `mkdir -p` will create. Used to chown an entire freshly-created
/// path tree to the file's owner.
fn topmost_created(path: &Path) -> Option<PathBuf> {
    let mut topmost = None;
    let mut cur = path.parent();
    while let Some(p) = cur {
        if p.exists() {
            break;
        }
        topmost = Some(p.to_path_buf());
        cur = p.parent();
    }
    topmost
}

/// Resolve `~/...` to the invoking user's home (a user-level write); anything
/// else is treated as an absolute system path (a root write).
fn resolve(path: &str, ctx: &Ctx) -> (String, bool) {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = if ctx.dry_run {
            "$HOME".to_string()
        } else {
            std::env::var("HOME").unwrap_or_else(|_| "/root".to_string())
        };
        (format!("{home}/{rest}"), true)
    } else {
        (path.to_string(), false)
    }
}
