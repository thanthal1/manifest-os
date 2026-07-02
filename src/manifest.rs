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

    /// Bootloader installation + configuration. When present, the installer
    /// installs and configures the bootloader so a non-default kernel actually
    /// boots. Designed to run in the ISO's chroot context.
    pub boot: Option<Boot>,

    /// First-run questions a manifest author defines. Answers are injected
    /// wherever `{{id}}` appears and drive `conditional_packages`.
    #[serde(default)]
    pub survey: Vec<Question>,

    /// Package lists gated on survey answers.
    #[serde(default)]
    pub conditional_packages: Vec<ConditionalPackages>,

    /// Users to create (declarative — no useradd/sudoers shell needed).
    #[serde(default)]
    pub users: Vec<UserSpec>,

    /// Config files to write (declarative — no mkdir/echo/cat shell needed).
    #[serde(default)]
    pub files: Vec<FileSpec>,

    /// Desktop wallpaper, applied across whatever environment the manifest sets
    /// up (GNOME, KDE, Xfce, a window manager, …). Either a bare string source
    /// (`"wallpaper": "https://…/bg.jpg"`) or an object with a fit mode
    /// (`{ "source": "/path/bg.png", "mode": "fill" }`).
    pub wallpaper: Option<Wallpaper>,

    /// Custom keyboard shortcuts, applied across whatever environment the
    /// manifest sets up via that environment's own first-party mechanism
    /// (niri/Hyprland/Sway/i3 config, KDE's Custom Shortcuts, LXQt's global
    /// shortcuts daemon, or GNOME/Cinnamon/MATE/Xfce's custom-keybinding
    /// settings). See [`crate::keybindings`].
    #[serde(default)]
    pub keybindings: Vec<Keybinding>,

    /// Visual theme — GTK/widget theme, icons, cursor, fonts, dark preference —
    /// applied across whatever environment the manifest sets up. The theme
    /// packages themselves go in `packages`; this block only *selects* them.
    /// See [`crate::theming`].
    pub theme: Option<Theme>,

    /// Shell commands run *before* package installation. Escape hatch only.
    #[serde(default)]
    pub pre_install: Vec<String>,

    /// Shell commands run *after* everything else. Escape hatch only.
    #[serde(default)]
    pub post_install: Vec<String>,
}

/// A first-run survey question.
#[derive(Debug, Deserialize)]
pub struct Question {
    pub id: String,
    /// "text" | "secret" | "boolean" | "select" | "multiselect" | "number" | "path"
    #[serde(rename = "type")]
    pub qtype: String,
    pub label: String,
    #[serde(default)]
    pub required: bool,
    /// Default answer (any JSON scalar). Used when unattended.
    pub default: Option<serde_json::Value>,
    /// Choices for select / multiselect.
    #[serde(default)]
    pub options: Vec<String>,
}

/// A package list applied only when its condition holds.
#[derive(Debug, Deserialize)]
pub struct ConditionalPackages {
    /// e.g. "install_gaming == true" or "gpu == nvidia".
    #[serde(rename = "if")]
    pub condition: String,
    pub packages: Vec<String>,
}

/// A user account to create.
#[derive(Debug, Deserialize)]
pub struct UserSpec {
    pub name: String,
    /// Supplementary groups, e.g. ["wheel", "video", "input"].
    #[serde(default)]
    pub groups: Vec<String>,
    /// Login shell, e.g. "/bin/zsh". Defaults to the system default.
    pub shell: Option<String>,
    /// Grant passwordless-prompt sudo via a /etc/sudoers.d drop-in.
    #[serde(default)]
    pub sudo: bool,
    /// Initial password. Sensitive — never logged. Prefer a survey `secret`.
    pub password: Option<String>,
}

/// A file to write verbatim.
#[derive(Debug, Deserialize)]
pub struct FileSpec {
    /// Destination. `~/...` writes to the invoking user's home; an absolute
    /// path (e.g. /etc/...) is written as root.
    pub path: String,
    #[serde(default)]
    pub content: String,
    /// Octal permission string, e.g. "644" or "0440".
    pub mode: Option<String>,
    /// chown target, e.g. "root:root" or "alice". Implies a root-owned write.
    pub owner: Option<String>,
}

/// A wallpaper source, accepted as either a bare string or an object with a
/// fit `mode`. See [`crate::wallpaper`].
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Wallpaper {
    /// `"wallpaper": "https://…/bg.jpg"`
    Simple(String),
    /// `"wallpaper": { "source": "/path/bg.png", "mode": "fill" }`
    Detailed {
        source: String,
        mode: Option<String>,
    },
}

/// The visual theme block:
/// ```json
/// "theme": {
///   "gtk": "Materia-dark",
///   "icons": "Papirus-Dark",
///   "cursor": "Adwaita",
///   "cursor_size": 24,
///   "font": "Noto Sans 11",
///   "monospace_font": "JetBrains Mono 11",
///   "dark": true
/// }
/// ```
/// Every field is optional — set only what the manifest cares about. Names are
/// the theme's installed directory name (what `ls /usr/share/themes`,
/// `/usr/share/icons` show); fonts are "Family Size" in GTK font syntax.
#[derive(Debug, Deserialize)]
pub struct Theme {
    /// GTK / widget theme name, e.g. "Adwaita-dark", "Materia".
    pub gtk: Option<String>,
    /// Icon theme name, e.g. "Papirus-Dark".
    pub icons: Option<String>,
    /// Cursor theme name, e.g. "Adwaita", "Bibata-Modern-Classic".
    pub cursor: Option<String>,
    /// Cursor size in pixels (commonly 24, 32, 48).
    pub cursor_size: Option<u32>,
    /// Interface font, e.g. "Noto Sans 11".
    pub font: Option<String>,
    /// Monospace font, e.g. "JetBrains Mono 11".
    pub monospace_font: Option<String>,
    /// Prefer dark variants app-wide (GNOME color-scheme, GTK prefer-dark,
    /// KDE BreezeDark color scheme).
    pub dark: Option<bool>,
}

impl Theme {
    /// Whether the block sets anything at all.
    pub fn is_empty(&self) -> bool {
        self.gtk.is_none()
            && self.icons.is_none()
            && self.cursor.is_none()
            && self.cursor_size.is_none()
            && self.font.is_none()
            && self.monospace_font.is_none()
            && self.dark.is_none()
    }
}

/// A single custom keyboard shortcut, in Manifest OS's universal
/// representation:
/// ```json
/// { "keys": "SUPER+Enter", "action": "terminal" }
/// { "keys": "SUPER+B", "command": "firefox" }
/// ```
/// `keys` is `+`-joined modifiers (SUPER/CTRL/ALT/SHIFT, case-insensitive,
/// with common aliases like WIN/META for SUPER) followed by a key name (a
/// single letter/digit, a common name like `Enter`/`Left`/`F5`, or a raw XF86
/// media-key name such as `XF86AudioRaiseVolume`).
///
/// Exactly one of `action` (a small built-in vocabulary resolved per
/// environment — see [`crate::keybindings`]) or `command` (a literal shell
/// command, used verbatim everywhere) should be set; `command` wins if both
/// are given.
#[derive(Debug, Deserialize)]
pub struct Keybinding {
    pub keys: String,
    pub action: Option<String>,
    pub command: Option<String>,
}

impl Wallpaper {
    /// The image source — an `http(s)://` URL or a local path.
    pub fn source(&self) -> &str {
        match self {
            Wallpaper::Simple(s) => s,
            Wallpaper::Detailed { source, .. } => source,
        }
    }

    /// Fit mode: `fill` (default), `fit`, `stretch`, `center`, or `tile`.
    pub fn mode(&self) -> &str {
        match self {
            Wallpaper::Detailed { mode: Some(m), .. } if !m.trim().is_empty() => m,
            _ => "fill",
        }
    }
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

#[derive(Debug, Deserialize)]
pub struct Boot {
    /// "systemd-boot" (UEFI only) or "grub" (UEFI or BIOS).
    pub loader: String,
    /// Extra kernel command-line parameters, e.g. ["quiet", "nvidia_drm.modeset=1"].
    #[serde(default)]
    pub cmdline: Vec<String>,
    /// Boot menu timeout in seconds.
    pub timeout: Option<u32>,
    /// EFI system partition mount point. Standard Arch layout mounts it at /boot.
    #[serde(default = "default_esp")]
    pub esp: String,
}

fn default_esp() -> String {
    "/boot".to_string()
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
        if let Some(boot) = &self.boot {
            if !matches!(boot.loader.as_str(), "systemd-boot" | "grub") {
                anyhow::bail!(
                    "unknown bootloader `{}` (expected `systemd-boot` or `grub`)",
                    boot.loader
                );
            }
        }
        Ok(())
    }
}
