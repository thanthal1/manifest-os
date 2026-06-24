//! Thin wrapper around running external commands, with a `--dry-run` mode.
//!
//! In dry-run, every command is printed but nothing executes — this is how you
//! safely preview an install on any machine (including non-Arch dev boxes).

use anyhow::{bail, Result};
use std::process::Command;

/// Shared execution context threaded through the install pipeline.
pub struct Ctx {
    /// When true, commands are printed but never run.
    pub dry_run: bool,
}

impl Ctx {
    pub fn new(dry_run: bool) -> Self {
        Self { dry_run }
    }

    /// Run a program with args. Streams stdout/stderr to the user's terminal.
    pub fn run(&self, program: &str, args: &[&str]) -> Result<()> {
        eprintln!("  $ {program} {}", args.join(" "));
        if self.dry_run {
            return Ok(());
        }
        let status = Command::new(program).args(args).status();
        match status {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => bail!("`{program}` exited with status {s}"),
            Err(e) => bail!("failed to launch `{program}`: {e} (is it installed?)"),
        }
    }

    /// Run a raw shell command line via `sh -c`. Used for pre/post hooks, which
    /// the manifest author writes as arbitrary shell.
    pub fn shell(&self, line: &str) -> Result<()> {
        eprintln!("  $ {line}");
        if self.dry_run {
            return Ok(());
        }
        let status = Command::new("sh").arg("-c").arg(line).status();
        match status {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => bail!("hook exited with status {s}: {line}"),
            Err(e) => bail!("failed to run hook via sh: {e}"),
        }
    }
}
