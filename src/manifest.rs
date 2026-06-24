//! The `manifest.json` schema (v1.0.0) and its deserialization.
//!
//! The manifest is the single source of truth: packages, kernel, repos,
//! services, dotfiles and pre/post hooks. Fields are deliberately permissive —
//! almost everything is optional so a minimal manifest (just a package list)
//! is valid and useful.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

/// A fully parsed manifest.
#[derive(Debug, Deserialize)]
pub struct Manifest {
    /// Semantic version of the schema this manifest targets, e.g. "1.0.0".
    /// The core CLI reads this to decide which parser/behavior applies.
    pub schema_version: String,

    #[serde(default)]
    pub meta: Meta,

    #[serde(default)]
    pub system: System,

    #[serde(default)]
    pub repos: Repos,

    /// Packages installed via paru (official repos + AUR, transparently).
    #[serde(default)]
    pub packages: Vec<String>,

    #[serde(default)]
    pub services: Services,

    pub dotfiles: Option<Dotfiles>,

    /// Optional desktop environment or window manager to set up automatically.
    /// One of the recipe keys in `desktop.rs` (e.g. "gnome", "plasma",
    /// "hyprland", "sway", "niri", "i3", "xfce"). The installer expands this
    /// into packages, a display manager, portals, a polkit agent, services and
    /// any required greeter/session config.
    pub desktop: Option<String>,

    /// Override the display manager the desktop recipe picks by default.
    /// One of: "gdm", "sddm", "lightdm", "greetd", "ly".
    pub display_manager: Option<String>,

    /// Shell commands run *before* package installation.
    #[serde(default)]
    pub pre_install: Vec<String>,

    /// Shell commands run *after* services are enabled.
    #[serde(default)]
    pub post_install: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Meta {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub version: String,
    /// "free" | "paid" — catalog metadata, ignored by the installer.
    #[serde(default)]
    pub license: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct System {
    /// System hostname, written to /etc/hostname and /etc/hosts.
    pub hostname: Option<String>,
    /// LANG locale, e.g. "en_US.UTF-8". Generated and set as the system locale.
    pub locale: Option<String>,
    /// IANA timezone, e.g. "America/New_York". Symlinked into /etc/localtime.
    pub timezone: Option<String>,
    /// Console keymap for the TTY, e.g. "us", "uk". Written to /etc/vconsole.conf.
    pub keymap: Option<String>,
    /// One of: "linux", "linux-lts", "linux-zen", "linux-hardened", "cachy".
    pub kernel: Option<String>,
}

impl System {
    /// Whether any setting in this block needs applying.
    pub fn is_empty(&self) -> bool {
        self.hostname.is_none()
            && self.locale.is_none()
            && self.timezone.is_none()
            && self.keymap.is_none()
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct Repos {
    #[serde(default)]
    pub multilib: bool,
    #[serde(default)]
    pub cachyos: bool,
    #[serde(default)]
    pub cachy_optimized_packages: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct Services {
    /// systemd system units (`systemctl enable`).
    #[serde(default)]
    pub system: Vec<String>,
    /// systemd user units (`systemctl --user enable`).
    #[serde(default)]
    pub user: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Dotfiles {
    pub source: String,
    #[serde(default = "default_branch")]
    pub branch: String,
    /// "symlink" | "copy" — how dotfiles are placed. Phase 1 clones only.
    #[serde(default)]
    pub method: String,
}

fn default_branch() -> String {
    "main".to_string()
}

impl Manifest {
    /// Load and parse a manifest from a JSON file on disk.
    pub fn from_path(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading manifest at {}", path.display()))?;
        Self::from_str(&raw)
    }

    /// Parse a manifest from a JSON string.
    pub fn from_str(raw: &str) -> Result<Self> {
        let manifest: Manifest =
            serde_json::from_str(raw).context("manifest is not valid JSON for schema v1.0.0")?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Cheap structural checks beyond what serde guarantees.
    fn validate(&self) -> Result<()> {
        if self.schema_version.trim().is_empty() {
            anyhow::bail!("`schema_version` is required and must be non-empty");
        }
        // Validate the kernel name up front (defaults to `linux` when unset).
        crate::kernel::resolve(self.system.kernel.as_deref())?;
        Ok(())
    }
}
