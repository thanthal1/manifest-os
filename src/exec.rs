//! Thin wrapper around running external commands, with a `--dry-run` mode.
//!
//! In dry-run, every command is printed but nothing executes — this is how you
//! safely preview an install on any machine (including non-Arch dev boxes).
//!
//! Privilege model: the install is run by a normal user who has `sudo`.
//!   - `run`   — user-level (git clone, makepkg, paru)
//!   - `sudo`  — root (pacman, editing /etc/pacman.conf)
//!   - paru and makepkg must NEVER run as root, so they go through `run`.

use anyhow::{bail, Result};
use std::io::Write;
use std::process::{Command, Stdio};

/// Shared execution context threaded through the install pipeline.
pub struct Ctx {
    /// When true, commands are printed but never run.
    pub dry_run: bool,
}

impl Ctx {
    pub fn new(dry_run: bool) -> Self {
        Self { dry_run }
    }

    /// Run a program as the current user. Streams output to the terminal.
    pub fn run(&self, program: &str, args: &[&str]) -> Result<()> {
        self.exec(program, args, false)
    }

    /// Run a program as root via `sudo`.
    pub fn sudo(&self, program: &str, args: &[&str]) -> Result<()> {
        self.exec(program, args, true)
    }

    fn exec(&self, program: &str, args: &[&str], root: bool) -> Result<()> {
        let prefix = if root { "sudo " } else { "" };
        println!("  $ {prefix}{program} {}", args.join(" "));
        if self.dry_run {
            return Ok(());
        }
        let mut cmd = if root {
            let mut c = Command::new("sudo");
            c.arg(program);
            c
        } else {
            Command::new(program)
        };
        match cmd.args(args).status() {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => bail!("`{program}` exited with status {s}"),
            Err(e) => bail!("failed to launch `{program}`: {e} (is it installed?)"),
        }
    }

    /// Run a raw shell command line via `sh -c`. Used for pre/post hooks and
    /// multi-step bootstraps. Set `root` to wrap the whole line in `sudo sh -c`.
    pub fn shell(&self, line: &str, root: bool) -> Result<()> {
        let prefix = if root { "sudo " } else { "" };
        println!("  $ {prefix}{line}");
        if self.dry_run {
            return Ok(());
        }
        let mut cmd = if root {
            let mut c = Command::new("sudo");
            c.args(["sh", "-c"]);
            c
        } else {
            let mut c = Command::new("sh");
            c.arg("-c");
            c
        };
        match cmd.arg(line).status() {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => bail!("command exited with status {s}: {line}"),
            Err(e) => bail!("failed to run via sh: {e}"),
        }
    }

    /// Write a file as root, creating parent directories first. Used for
    /// greeter configs, env drop-ins and other desktop setup that isn't a
    /// package install.
    pub fn write_root(&self, path: &str, content: &str) -> Result<()> {
        println!("  > write {path} ({} bytes, root)", content.len());
        if self.dry_run {
            return Ok(());
        }
        if let Some(parent) = std::path::Path::new(path).parent() {
            let parent = parent.to_string_lossy();
            let status = Command::new("sudo").args(["mkdir", "-p", &parent]).status();
            if !matches!(status, Ok(s) if s.success()) {
                bail!("failed to create directory {parent}");
            }
        }
        let mut child = Command::new("sudo")
            .args(["tee", path])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to launch `sudo tee`: {e}"))?;
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(content.as_bytes())?;
        match child.wait() {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => bail!("writing {path} exited with status {s}"),
            Err(e) => bail!("failed writing {path}: {e}"),
        }
    }

    /// Run a command and capture its trimmed stdout. `root` runs it via sudo.
    /// In dry-run nothing executes and an empty string is returned, so callers
    /// must substitute a placeholder for preview output.
    pub fn output(&self, root: bool, program: &str, args: &[&str]) -> Result<String> {
        if self.dry_run {
            return Ok(String::new());
        }
        let mut cmd = if root {
            let mut c = Command::new("sudo");
            c.arg(program);
            c
        } else {
            Command::new(program)
        };
        let out = cmd
            .args(args)
            .output()
            .map_err(|e| anyhow::anyhow!("failed to launch `{program}`: {e}"))?;
        if !out.status.success() {
            bail!(
                "`{program}` failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Write a file as the current user (no sudo), creating parent dirs.
    pub fn write_user(&self, path: &str, content: &str) -> Result<()> {
        println!("  > write {path} ({} bytes)", content.len());
        if self.dry_run {
            return Ok(());
        }
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Set a user's password via `chpasswd`, feeding it over stdin so the
    /// password is NEVER printed to the log.
    pub fn set_password(&self, user: &str, password: &str) -> Result<()> {
        println!("  · setting password for {user}");
        if self.dry_run {
            return Ok(());
        }
        let mut child = Command::new("sudo")
            .arg("chpasswd")
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to launch chpasswd: {e}"))?;
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(format!("{user}:{password}\n").as_bytes())?;
        match child.wait() {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => bail!("chpasswd exited with status {s}"),
            Err(e) => bail!("chpasswd failed: {e}"),
        }
    }

    /// Run a detection command quietly and report whether it succeeded.
    ///
    /// In dry-run this does NOT execute and returns `false`, so the pipeline
    /// prints the full "would do X" path rather than silently skipping it.
    pub fn check(&self, program: &str, args: &[&str]) -> bool {
        if self.dry_run {
            return false;
        }
        Command::new(program)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}
