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

/// A [`FileSpec`] for a file under a home directory (`rel_path` like
/// `.config/gtk-3.0/settings.ini`). With a declared primary user this becomes
/// an absolute `/home/<user>/...` path owned by that user — required during a
/// disk install, where the installer runs as a throwaway bootstrap account,
/// so `~` would be the *wrong* home. Without one it falls back to `~/...`
/// (correct when `manifest install` is run directly on an existing system).
pub fn home_spec(primary_user: Option<&str>, rel_path: &str, content: String) -> FileSpec {
    let (path, owner) = match primary_user {
        Some(user) => (format!("/home/{user}/{rel_path}"), Some(format!("{user}:{user}"))),
        None => (format!("~/{rel_path}"), None),
    };
    FileSpec { path, content, source: None, mode: None, owner, when: None }
}

pub fn apply(files: &[FileSpec], ctx: &Ctx) -> Result<()> {
    for f in files {
        let (path, user_level) = resolve(&f.path, ctx);
        // A chown (explicit owner) needs root; `~/…` with no owner stays user.
        let root = !user_level || f.owner.is_some();

        // Capture the topmost dir `mkdir -p` will create, so an `owner` can
        // claim the whole new tree (e.g. /home/x/.config/…), not just the leaf.
        let created = if f.owner.is_some() && !ctx.dry_run {
            topmost_created(Path::new(&path))
        } else {
            None
        };

        // Write the content: fetched from `source` if set, else inline.
        match &f.source {
            Some(src) => fetch_file(&path, src, root, ctx)?,
            None if root => ctx.write_root(&path, &f.content)?,
            None => ctx.write_user(&path, &f.content)?,
        }

        if let Some(mode) = &f.mode {
            if root {
                ctx.sudo("chmod", &[mode, &path])?;
            } else {
                ctx.run("chmod", &[mode, &path])?;
            }
        }
        if let Some(owner) = &f.owner {
            match &created {
                Some(top) => ctx.sudo("chown", &["-R", owner, &top.to_string_lossy()])?,
                None => ctx.sudo("chown", &[owner, &path])?,
            }
        }
    }
    Ok(())
}

/// Fetch a file's content from `source` — an `http(s)://` URL (curl) or a local
/// path (copy) — to `path`, creating parent dirs. `root` runs it via sudo.
fn fetch_file(path: &str, source: &str, root: bool, ctx: &Ctx) -> Result<()> {
    let get = if source.starts_with("http://") || source.starts_with("https://") {
        format!("curl -fsSL --retry 2 -o '{path}' '{source}'")
    } else {
        format!("cp -f '{source}' '{path}'")
    };
    ctx.shell(&format!("mkdir -p \"$(dirname '{path}')\" && {get}"), root)
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
