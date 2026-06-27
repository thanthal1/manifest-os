//! Declarative file writing.
//!
//! Replaces the most common `post_install` hooks — `mkdir -p`, `echo >`,
//! `cat > file`. A `files` entry names a path and its content; the CLI creates
//! the parent directories and writes it, as the user for `~/...` paths or as
//! root for absolute system paths.

use crate::exec::Ctx;
use crate::manifest::FileSpec;
use anyhow::Result;

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
            ctx.write_root(&path, &f.content)?;
            if let Some(mode) = &f.mode {
                ctx.sudo("chmod", &[mode, &path])?;
            }
            if let Some(owner) = &f.owner {
                ctx.sudo("chown", &[owner, &path])?;
            }
        }
    }
    Ok(())
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
