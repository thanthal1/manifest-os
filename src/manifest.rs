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

    /// Dotfiles repo(s) to clone and place. Accepts a single object or a list —
    /// a list lets one manifest map several repo dirs (or several repos) to
    /// different targets (e.g. `config/`→`~/.config`, a wallpaper repo→
    /// `~/Pictures`). Each repo is cloned once even if listed twice.
    #[serde(default, deserialize_with = "de_dotfiles")]
    pub dotfiles: Vec<Dotfiles>,

    /// Optional desktop environment or window manager to set up automatically.
    /// One of the recipe keys in `desktop.rs` (e.g. "gnome", "plasma",
    /// "hyprland", "sway", "niri", "i3", "xfce"). The installer expands this
    /// into packages, a display manager, portals, a polkit agent, services and
    /// any required greeter/session config.
    pub desktop: Option<String>,

    /// Override the display manager the desktop recipe picks by default.
    /// One of: "gdm", "sddm", "lightdm", "greetd", "ly", "cosmic-greeter".
    pub display_manager: Option<String>,

    /// Bootloader installation + configuration. When present, the installer
    /// installs and configures the bootloader so a non-default kernel actually
    /// boots. Designed to run in the ISO's chroot context.
    pub boot: Option<Boot>,

    /// Author-defined constants injected wherever `{{id}}` appears — a fixed
    /// accent colour, a username, a repeated path. Unlike `survey`, they need
    /// no prompting; unlike a hardcoded literal, they're written once and reused
    /// everywhere. A survey answer with the same id overrides its variable. See
    /// [`crate::survey`].
    #[serde(default)]
    pub variables: std::collections::BTreeMap<String, serde_json::Value>,

    /// First-run questions a manifest author defines. Answers are injected
    /// wherever `{{id}}` appears and drive `conditional_packages`.
    #[serde(default)]
    pub survey: Vec<Question>,

    /// Post-install **settings** the author exposes for later tweaking in the
    /// System Snapshots app — a curated subset of the manifest turned into a
    /// friendly control panel. Each entry's `id` is a [`variables`](Self::variables)
    /// key: the control shows that variable's current value, and saving rewrites
    /// it and re-applies (so `{{id}}` updates everywhere). Same typed/validated
    /// shape as `survey`. This is how a good manifest doubles as a settings app.
    #[serde(default)]
    pub settings: Vec<Question>,

    /// Package lists gated on survey answers (the original, string-condition
    /// form). Prefer `conditional` for anything beyond packages.
    #[serde(default)]
    pub conditional_packages: Vec<ConditionalPackages>,

    /// Conditional overlays — slices of manifest (packages, files, services,
    /// flatpaks, hooks, desktop, theme) each applied only when their `when`
    /// holds. This is the general `when` mechanism. See [`Conditional`] and
    /// [`crate::conditions`].
    #[serde(default)]
    pub conditional: Vec<Conditional>,

    /// Hardware-fact overrides for `when`/`{{}}`. Standard facts (`gpu`, `cpu`,
    /// `virt`, `is_vm`, `firmware`) are auto-detected with no config; an entry
    /// here pins one to a literal (anything but `"auto"`) — handy for testing a
    /// manifest as if on other hardware. `"auto"` just means "detect it".
    #[serde(default)]
    pub detect: std::collections::BTreeMap<String, String>,

    /// Users to create (declarative — no useradd/sudoers shell needed).
    #[serde(default)]
    pub users: Vec<UserSpec>,

    /// Config files to write (declarative — no mkdir/echo/cat shell needed).
    #[serde(default)]
    pub files: Vec<FileSpec>,

    /// Config *fragments* inserted into existing files without replacing them
    /// — e.g. drop a waybar launch bind into the `binds` section of someone's
    /// niri/Hyprland config. Each snippet is wrapped in comment markers so
    /// re-applying updates it in place. See [`crate::snippets`].
    #[serde(default)]
    pub snippets: Vec<Snippet>,

    /// Flatpak remotes and apps to install system-wide.
    pub flatpak: Option<Flatpak>,

    /// Default applications and MIME associations for the primary user.
    pub defaults: Option<Defaults>,

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

    /// Display settings — currently the UI `scale`. Applied across whatever
    /// environment the manifest sets up (GTK/Qt app scaling everywhere, plus
    /// each full DE's native setting). See [`crate::scaling`].
    pub display: Option<Display>,

    /// Login screen (display-manager greeter) appearance. Omit it and you get
    /// the bundled `manifest` SDDM theme / a sensible tuigreet colour scheme;
    /// tweak its colours here, or name another installed SDDM theme to skip
    /// ours entirely. See [`Login`] and [`crate::desktop`].
    pub login: Option<Login>,

    /// Shell commands run *before* package installation. Escape hatch only.
    #[serde(default)]
    pub pre_install: Vec<String>,

    /// Shell commands run *after* everything else. Escape hatch only.
    #[serde(default)]
    pub post_install: Vec<String>,

    /// Inline **plugin** definitions carried by the manifest itself, so a shared
    /// manifest that uses a custom block (`docker`, `tailscale`, `ollama`, …) is
    /// fully self-contained and reviewable — no out-of-band install. Each entry
    /// is a plugin descriptor (see [`crate::plugins::Plugin`]); it declares how
    /// its block expands into core primitives. Inline definitions override any
    /// same-named plugin found in a plugins directory. Applied and stripped by
    /// [`crate::plugins::expand`] before the rest of the pipeline ever runs, so
    /// the core engine never needs to know what these blocks mean.
    #[serde(default)]
    pub plugins: Vec<serde_json::Value>,

    /// Every top-level block the core schema doesn't recognize, captured here by
    /// serde rather than silently dropped. A **plugin** turns each of these into
    /// core primitives (see [`plugins`](Self::plugins)); after
    /// [`crate::plugins::expand`] runs, this is empty. Anything left with no
    /// plugin to claim it is an error (a typo or an unknown block), surfaced by
    /// `verify`/`install`.
    #[serde(flatten)]
    pub extensions: std::collections::BTreeMap<String, serde_json::Value>,
}

/// A first-run survey question.
#[derive(Debug, Deserialize)]
pub struct Question {
    pub id: String,
    /// "text" | "secret" | "boolean" | "select" | "multiselect" | "number" |
    /// "path" | "color"
    #[serde(rename = "type")]
    pub qtype: String,
    pub label: String,
    /// Optional one-line helper text (shown under the control in the survey /
    /// Settings UI).
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
    /// Default answer (any JSON scalar). Used when unattended.
    pub default: Option<serde_json::Value>,
    /// Choices for select / multiselect (also enforced as an enum: a select
    /// answer must be one of these).
    #[serde(default)]
    pub options: Vec<String>,

    // ---- validation (all optional) ------------------------------------
    /// A regex the answer must fully match (anchored). Applies to text/secret/
    /// path answers, e.g. `"^[a-z_][a-z0-9_-]*$"` for a username.
    pub pattern: Option<String>,
    /// Lower bound: a `number` answer's value, or a text answer's length.
    pub min: Option<f64>,
    /// Upper bound: a `number` answer's value, or a text answer's length.
    pub max: Option<f64>,
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

/// A file to write. The content is either inline (`content`) or pulled from a
/// hosted URL / local path (`source`) — the latter lets a manifest reference a
/// hosted config (e.g. a swaylock theme in a GitHub repo) instead of inlining
/// it, which pairs with a `settings` dropdown + `conditional` to offer a
/// picker of hosted styles.
#[derive(Debug, Clone, Deserialize)]
pub struct FileSpec {
    /// Destination. `~/...` writes to the invoking user's home; an absolute
    /// path (e.g. /etc/...) is written as root.
    pub path: String,
    #[serde(default)]
    pub content: String,
    /// Fetch the content from here instead of `content`: an `http(s)://` URL
    /// (curl'd) or a local path (copied). Wins over `content` when set.
    pub source: Option<String>,
    /// Octal permission string, e.g. "644" or "0440".
    pub mode: Option<String>,
    /// chown target, e.g. "root:root" or "alice". Implies a root-owned write.
    pub owner: Option<String>,
    /// Only write this file when the condition holds — e.g. an nvidia Xorg
    /// snippet gated on `{ "gpu": "nvidia" }`. See [`Condition`].
    pub when: Option<Condition>,
}

/// A `when` condition, evaluated against the run's [facts](crate::conditions)
/// (survey answers, `variables`, and auto-detected hardware: `gpu`, `cpu`,
/// `virt`, `is_vm`, `firmware`). Two forms:
/// ```json
/// "when": { "gpu": "nvidia" }                 // object: every key must match
/// "when": { "gpu": ["nvidia", "amd"] }        // array value: match any
/// "when": "install_gaming == true"            // legacy string expression
/// ```
/// An object with several keys is an AND; an array value is an OR for that key.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Condition {
    /// `{ "gpu": "nvidia", "is_vm": false }` — all keys must match.
    Match(std::collections::BTreeMap<String, serde_json::Value>),
    /// `"gpu == nvidia"` / `"gpu != nvidia"` — legacy expression form.
    Expr(String),
}

/// A conditional overlay: a slice of manifest applied only when `when` holds.
/// This is how `when` reaches list-shaped sections (packages, services,
/// flatpak apps, hooks) that can't carry a per-item condition, plus a place to
/// gate a whole bundle of related config at once.
#[derive(Debug, Deserialize)]
pub struct Conditional {
    pub when: Condition,
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default)]
    pub files: Vec<FileSpec>,
    #[serde(default)]
    pub services: Services,
    #[serde(default)]
    pub snippets: Vec<Snippet>,
    #[serde(default)]
    pub keybindings: Vec<Keybinding>,
    #[serde(default)]
    pub pre_install: Vec<String>,
    #[serde(default)]
    pub post_install: Vec<String>,
    pub flatpak: Option<Flatpak>,
    /// Set the desktop only if the base manifest didn't already choose one.
    pub desktop: Option<String>,
    /// Set the theme only if the base manifest didn't already choose one.
    pub theme: Option<Theme>,
}

/// A config fragment to insert into an existing file:
/// ```json
/// {
///   "id": "waybar-bind",
///   "path": "~/.config/niri/config.kdl",
///   "section": "binds",
///   "content": "Mod+W { spawn \"waybar\"; }"
/// }
/// ```
/// `id` names the managed block (re-applying replaces it in place); `section`
/// targets a brace block (`binds { … }`) or INI `[section]` — omitted, the
/// snippet is appended to the end of the file. See [`crate::snippets`].
#[derive(Debug, Deserialize)]
pub struct Snippet {
    /// Unique name for this fragment's managed block.
    pub id: String,
    /// Target file. `~/...` resolves to the primary user's home.
    pub path: String,
    /// Optional section to insert into (brace block or INI `[section]`).
    pub section: Option<String>,
    pub content: String,
}

/// Flatpak setup: add remotes, then install app ids.
#[derive(Debug, Deserialize)]
pub struct Flatpak {
    #[serde(default)]
    pub remotes: Vec<FlatpakRemote>,
    #[serde(default)]
    pub apps: Vec<String>,
}

impl Flatpak {
    pub fn is_empty(&self) -> bool {
        self.remotes.is_empty() && self.apps.is_empty()
    }
}

/// A Flatpak remote, e.g. Flathub.
#[derive(Debug, Deserialize)]
pub struct FlatpakRemote {
    pub name: String,
    pub url: String,
}

/// User-level default app choices. `browser` expands to the standard browser
/// MIME handlers; `mime` maps arbitrary MIME types to desktop file ids.
#[derive(Debug, Deserialize)]
pub struct Defaults {
    pub browser: Option<String>,
    #[serde(default)]
    pub mime: std::collections::BTreeMap<String, String>,
}

impl Defaults {
    pub fn is_empty(&self) -> bool {
        self.browser.is_none() && self.mime.is_empty()
    }
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

/// Login screen (display-manager greeter) appearance — see
/// [`crate::desktop::configure_login`].
///
/// For **SDDM** (GNOME/KDE/Hyprland/Sway when it's the DM): with no `theme`, or
/// `theme: "manifest"`, you get the bundled Manifest theme and can restyle it
/// with `accent`/`panel`/`background`/`font`. Set `theme` to any *other*
/// installed SDDM theme name (e.g. `"breeze"`, `"sugar-candy"` — install its
/// package in `packages`) and ours is not shipped or selected at all. For
/// **greetd/tuigreet** (Niri/Sway/i3 …), `tuigreet_theme` overrides the colour
/// spec. Everything here is optional.
#[derive(Debug, Clone, Deserialize)]
pub struct Login {
    /// SDDM theme name. Unset / `"manifest"` = the bundled theme (styled by the
    /// fields below); anything else selects that installed theme instead.
    pub theme: Option<String>,
    /// Bundled-theme accent colour (hex), e.g. `"#ff9b54"`.
    pub accent: Option<String>,
    /// Bundled-theme panel/card colour (hex).
    pub panel: Option<String>,
    /// Bundled-theme background image path, e.g.
    /// `"/usr/share/backgrounds/manifest/current"` (where `wallpaper` lands).
    pub background: Option<String>,
    /// Bundled-theme font family, e.g. `"Inter"`.
    pub font: Option<String>,
    /// Override the greetd/tuigreet colour spec (see
    /// [`crate::desktop::TUIGREET_THEME`] for the syntax).
    pub tuigreet_theme: Option<String>,
}

/// Display / scaling settings.
#[derive(Debug, Default, Deserialize)]
pub struct Display {
    /// UI scale factor: `1.0` = 100%, `1.5` = 150%, `2.0` = 200% (HiDPI). When
    /// unset the installer auto-detects a default from the panel (see the
    /// `scale` fact). Applied cross-desktop by [`crate::scaling`]. Accepts a
    /// number or a numeric string, so `"scale": "{{scale}}"` works after
    /// token substitution.
    #[serde(default, deserialize_with = "de_scale")]
    pub scale: Option<f64>,
}

/// Deserialize a scale as a number *or* a numeric string (empty → none), so a
/// substituted `{{scale}}` token — which lands as a JSON string — still parses.
fn de_scale<'de, D>(d: D) -> std::result::Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Number(n) => Ok(n.as_f64()),
        // A non-numeric string (empty, or an unresolved `{{token}}` when the raw
        // manifest is parsed before substitution — e.g. by `manifest verify`) is
        // treated as unset rather than an error.
        serde_json::Value::String(s) => Ok(s.trim().parse().ok()),
        _ => Err(serde::de::Error::custom("scale must be a number")),
    }
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
    /// "symlink" | "copy" — how dotfiles are placed.
    #[serde(default)]
    pub method: String,
    /// Place only this subdirectory of the repo, not the whole root — for repos
    /// that keep configs under e.g. `config/` rather than mirroring `$HOME`.
    pub subdir: Option<String>,
    /// Target base directory (default `$HOME`). Pair with `subdir` to map a
    /// repo layout onto the right place, e.g. `subdir:"config", into:"~/.config"`.
    pub into: Option<String>,
}

fn default_branch() -> String {
    "main".to_string()
}

/// Accept `dotfiles` as either a single object or an array of them.
fn de_dotfiles<'de, D>(d: D) -> std::result::Result<Vec<Dotfiles>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        Many(Vec<Dotfiles>),
        One(Dotfiles),
    }
    Ok(match Option::<OneOrMany>::deserialize(d)? {
        None => Vec::new(),
        Some(OneOrMany::One(o)) => vec![o],
        Some(OneOrMany::Many(m)) => m,
    })
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
    // Not the `FromStr` trait: that would force every caller to import the
    // trait, and our error handling is anyhow-based anyway.
    #[allow(clippy::should_implement_trait)]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scale_of(json: &str) -> Option<f64> {
        Manifest::from_str(json).unwrap().display.and_then(|d| d.scale)
    }

    #[test]
    fn display_scale_accepts_number_or_numeric_string() {
        assert_eq!(scale_of(r#"{"schema_version":"1.0.0","display":{"scale":2}}"#), Some(2.0));
        assert_eq!(scale_of(r#"{"schema_version":"1.0.0","display":{"scale":"1.5"}}"#), Some(1.5));
        // Empty (e.g. an unresolved token that fell through) → none, not an error.
        assert_eq!(scale_of(r#"{"schema_version":"1.0.0","display":{"scale":""}}"#), None);
        assert_eq!(scale_of(r#"{"schema_version":"1.0.0"}"#), None);
    }

    #[test]
    fn settings_block_reuses_the_question_shape() {
        let m = Manifest::from_str(
            r#"{"schema_version":"1.0.0","settings":[
                {"id":"scale","type":"number","label":"Scale","min":1,"max":3,
                 "description":"HiDPI"}]}"#,
        )
        .unwrap();
        assert_eq!(m.settings.len(), 1);
        assert_eq!(m.settings[0].id, "scale");
        assert_eq!(m.settings[0].description.as_deref(), Some("HiDPI"));
    }
}
