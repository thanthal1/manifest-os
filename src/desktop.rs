//! Desktop environment / window manager recipes.
//!
//! Setting up a desktop on Arch is far more than installing one package. Each
//! environment needs, in some combination:
//!
//!   * the core DE/WM packages
//!   * a **display manager** (login screen) — and they are not interchangeable
//!     without consequences; some want a specific one and several need a config
//!     file written before they work
//!   * **XDG desktop portals** (file pickers, screen sharing) with the right
//!     backend for the toolkit
//!   * a **polkit agent** so GUI apps can ask for privileges
//!   * for bare window managers: a **notification daemon**, **launcher**,
//!     **bar**, **wallpaper tool**, **terminal**, **lock/idle** and Xwayland
//!   * **session environment variables** (Wayland/Qt/Firefox hints)
//!   * **services** (NetworkManager, the DM itself)
//!
//! This module encodes that knowledge as data. The manifest only names the
//! environment; [`resolve`] expands it and [`apply`] performs the parts that
//! are not package installs (enabling the DM, writing greeter/session config).
//!
//! Package names are real Arch package names. AUR-only packages are flagged in
//! `aur` so the user (and a future auditor) knows what crosses into the AUR.

use crate::exec::Ctx;
use crate::manifest::Manifest;
use anyhow::{bail, Result};

/// Which display server a session targets.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Session {
    Wayland,
    #[default]
    X11,
    /// Ships both a Wayland and an Xorg session (GNOME, Plasma).
    Both,
}

/// A static description of how to stand up one environment.
#[derive(Default)]
pub struct Recipe {
    pub key: &'static str,
    pub display_name: &'static str,
    pub session: Session,
    /// The DE/WM itself plus anything mandatory for it to start at all.
    pub core: &'static [&'static str],
    /// Display manager key the recipe defaults to. Overridable per manifest.
    pub default_dm: Option<&'static str>,
    /// XDG portal backends to install alongside the base portal.
    pub portals: &'static [&'static str],
    /// Polkit agent package, if the environment doesn't bundle one.
    pub polkit: Option<&'static str>,
    /// Strongly-recommended companions (terminal, notifications, launcher,
    /// bar, wallpaper, applets). What turns a WM from "boots" into "usable".
    pub extras: &'static [&'static str],
    /// Extra system services beyond NetworkManager + the display manager.
    pub services: &'static [&'static str],
    /// Session environment variables written to a login env drop-in.
    pub env: &'static [(&'static str, &'static str)],
    /// The session command a greeter (greetd/tuigreet) should launch. Empty if
    /// the environment is normally started from a `.desktop` session file.
    pub session_exec: &'static str,
    /// Of all packages above, which live only in the AUR (informational).
    pub aur: &'static [&'static str],
    pub notes: &'static str,
}

/// Packages every graphical system needs regardless of environment: the portal
/// base, audio stack, networking, fonts and Xwayland-agnostic utilities.
const DESKTOP_BASE: &[&str] = &[
    "xdg-desktop-portal",
    "xdg-user-dirs",
    "xdg-utils",
    "pipewire",
    "pipewire-pulse",
    "pipewire-alsa",
    "wireplumber",
    "networkmanager",
    "noto-fonts",
    "noto-fonts-emoji",
    "ttf-dejavu",
];

/// System services every graphical system wants enabled.
const DESKTOP_BASE_SERVICES: &[&str] = &["NetworkManager"];

/// Look up a recipe by key (case-insensitive).
pub fn recipe(key: &str) -> Option<&'static Recipe> {
    let k = key.to_ascii_lowercase();
    CATALOG.iter().find(|r| r.key == k)
}

/// Every supported environment, for `manifest desktops`.
pub fn catalog() -> &'static [Recipe] {
    CATALOG
}

// ---------------------------------------------------------------------------
// Display managers
// ---------------------------------------------------------------------------

/// A login manager: its package, its systemd unit, and (for some) the config
/// that must be written before it will launch the chosen session.
pub struct DisplayManager {
    pub key: &'static str,
    pub package: &'static str,
    pub service: &'static str,
}

pub fn display_manager(key: &str) -> Option<DisplayManager> {
    let dm = match key.to_ascii_lowercase().as_str() {
        "gdm" => ("gdm", "gdm", "gdm.service"),
        "sddm" => ("sddm", "sddm", "sddm.service"),
        "lightdm" => ("lightdm", "lightdm", "lightdm.service"),
        "greetd" => ("greetd", "greetd", "greetd.service"),
        "ly" => ("ly", "ly", "ly.service"),
        "cosmic-greeter" => ("cosmic-greeter", "cosmic-greeter", "cosmic-greeter.service"),
        _ => return None,
    };
    Some(DisplayManager { key: dm.0, package: dm.1, service: dm.2 })
}

// ---------------------------------------------------------------------------
// Resolution + application
// ---------------------------------------------------------------------------

/// A fully-expanded desktop setup ready to install and configure.
pub struct Resolved {
    pub display_name: &'static str,
    pub packages: Vec<String>,
    pub dm: Option<DisplayManager>,
    pub session_exec: &'static str,
    pub services: Vec<String>,
    pub env: Vec<(&'static str, &'static str)>,
    pub aur: Vec<&'static str>,
}

/// Expand `manifest.desktop` (+ optional `display_manager` override) into a
/// concrete package/service/config plan. Returns `Ok(None)` when no desktop is
/// declared.
pub fn resolve(manifest: &Manifest) -> Result<Option<Resolved>> {
    let Some(key) = manifest.desktop.as_deref() else {
        return Ok(None);
    };
    let Some(r) = recipe(key) else {
        bail!(
            "unknown desktop `{key}`. Run `manifest desktops` to list supported \
             environments."
        );
    };

    // Choose the display manager: manifest override > recipe default > none.
    let dm_key = manifest.display_manager.as_deref().or(r.default_dm);
    let dm = match dm_key {
        Some(k) => Some(display_manager(k).ok_or_else(|| {
            anyhow::anyhow!("unknown display_manager `{k}` (gdm|sddm|lightdm|greetd|ly)")
        })?),
        None => None,
    };

    // Assemble the package set, de-duplicated, order preserved.
    let mut packages: Vec<String> = Vec::new();
    let push = |p: &str, packages: &mut Vec<String>| {
        if !packages.iter().any(|x| x == p) {
            packages.push(p.to_string());
        }
    };
    for &p in DESKTOP_BASE {
        push(p, &mut packages);
    }
    for &p in r.core {
        push(p, &mut packages);
    }
    push("xdg-desktop-portal-gtk", &mut packages); // sensible universal fallback
    for &p in r.portals {
        push(p, &mut packages);
    }
    if let Some(p) = r.polkit {
        push(p, &mut packages);
    }
    for &p in r.extras {
        push(p, &mut packages);
    }
    if let Some(dm) = &dm {
        push(dm.package, &mut packages);
        if dm.key == "lightdm" {
            push("lightdm-gtk-greeter", &mut packages);
        }
        if dm.key == "greetd" {
            push("greetd-tuigreet", &mut packages);
        }
    }

    let mut services: Vec<String> =
        DESKTOP_BASE_SERVICES.iter().map(|s| s.to_string()).collect();
    for &s in r.services {
        if !services.iter().any(|x| x == s) {
            services.push(s.to_string());
        }
    }

    Ok(Some(Resolved {
        display_name: r.display_name,
        packages,
        dm,
        session_exec: r.session_exec,
        services,
        env: r.env.to_vec(),
        aur: r.aur.to_vec(),
    }))
}

/// Perform the non-package setup: enable services, enable + configure the
/// display manager, and write session environment variables.
pub fn apply(d: &Resolved, ctx: &Ctx) -> Result<()> {
    // Extra system services (NetworkManager, etc.). The DM is handled below.
    for svc in &d.services {
        ctx.sudo("systemctl", &["enable", svc])?;
    }

    if let Some(dm) = &d.dm {
        println!("  · display manager: {}", dm.key);
        configure_display_manager(dm, d.session_exec, ctx)?;
        // `--force` so re-applying (or switching from another DE) overwrites
        // the existing `display-manager.service` alias instead of erroring on
        // the conflicting symlink. `switch_default` disables the old DM first
        // on sync, but --force also covers the case where that DM's package is
        // already gone.
        ctx.sudo("systemctl", &["enable", "--force", dm.service])?;
    }

    if !d.env.is_empty() {
        write_env(&d.env, ctx)?;
    }
    Ok(())
}

/// The display-manager unit currently set to start at boot — the target of the
/// `/etc/systemd/system/display-manager.service` alias symlink, e.g.
/// `"gdm.service"`. `None` if no DM is configured (fresh WM-less system) or the
/// link is missing. Read-only (`read_link`), so it also works under `--dry-run`.
pub fn active_dm_unit() -> Option<String> {
    let target = std::fs::read_link("/etc/systemd/system/display-manager.service").ok()?;
    let unit = target.file_name()?.to_string_lossy().to_string();
    (!unit.is_empty()).then_some(unit)
}

/// On a sync, if the manifest's desktop uses a different login manager than the
/// one currently active, disable the old one so the subsequent [`apply`] (which
/// force-enables the target) makes the new desktop the boot default. Returns
/// `true` when it switched. Best-effort: a failed disable never aborts the sync
/// (`apply`'s `enable --force` still repoints the alias).
pub fn switch_default(d: &Resolved, ctx: &Ctx) -> bool {
    let Some(target) = &d.dm else { return false };
    let Some(current) = active_dm_unit() else { return false };
    if current == target.service {
        return false;
    }
    println!("  · switching login manager: {current} → {}", target.service);
    let _ = ctx.sudo("systemctl", &["disable", &current]);
    true
}

/// Write whatever config a display manager needs to actually launch the chosen
/// session. SDDM and GDM auto-detect sessions and need nothing.
fn configure_display_manager(dm: &DisplayManager, session_exec: &str, ctx: &Ctx) -> Result<()> {
    match dm.key {
        // LightDM needs to be told which greeter to use.
        "lightdm" => ctx.write_root(
            "/etc/lightdm/lightdm.conf.d/50-manifest.conf",
            "[Seat:*]\ngreeter-session=lightdm-gtk-greeter\n",
        ),
        // greetd has no session picker of its own; tuigreet provides one, and
        // we can pre-select the environment via --cmd when we know it.
        "greetd" => {
            let command = if session_exec.is_empty() {
                "tuigreet --time --remember".to_string()
            } else {
                format!("tuigreet --time --remember --cmd {session_exec}")
            };
            let toml = format!(
                "[terminal]\nvt = 1\n\n[default_session]\ncommand = \"{command}\"\nuser = \"greeter\"\n"
            );
            ctx.write_root("/etc/greetd/config.toml", &toml)
        }
        // gdm, sddm, ly: nothing to write.
        _ => Ok(()),
    }
}

/// Persist session environment variables to a login-time drop-in. Overwritten
/// each run, so it stays idempotent.
fn write_env(env: &[(&str, &str)], ctx: &Ctx) -> Result<()> {
    let mut body = String::from("# Managed by Manifest OS — desktop session environment\n");
    for (k, v) in env {
        body.push_str(&format!("export {k}={v}\n"));
    }
    ctx.write_root("/etc/profile.d/manifest-desktop.sh", &body)
}

// ---------------------------------------------------------------------------
// The catalog
// ---------------------------------------------------------------------------

const CATALOG: &[Recipe] = &[
    // ===================== Full desktop environments =====================
    Recipe {
        key: "gnome",
        display_name: "GNOME",
        session: Session::Both,
        core: &["gnome-shell", "gnome-control-center", "nautilus", "gnome-terminal", "gnome-tweaks"],
        default_dm: Some("gdm"),
        portals: &["xdg-desktop-portal-gnome"],
        polkit: None, // bundled with gnome-shell
        extras: &["gnome-keyring", "xdg-user-dirs-gtk"],
        services: &[],
        env: &[],
        session_exec: "gnome-session",
        aur: &[],
        notes: "GDM is tightly integrated; Wayland session is the default, Xorg also available.",
    },
    Recipe {
        key: "plasma",
        display_name: "KDE Plasma",
        session: Session::Both,
        core: &["plasma-meta", "konsole", "dolphin", "kate"],
        default_dm: Some("sddm"),
        portals: &["xdg-desktop-portal-kde"],
        polkit: None, // polkit-kde-agent is part of plasma-meta
        extras: &["ark", "spectacle", "plasma-systemmonitor"],
        services: &[],
        env: &[("QT_QPA_PLATFORMTHEME", "kde")],
        session_exec: "startplasma-wayland",
        aur: &[],
        notes: "Plasma 6 defaults to Wayland; SDDM is KDE's display manager and auto-detects sessions.",
    },
    Recipe {
        key: "xfce",
        display_name: "Xfce",
        session: Session::X11,
        core: &["xfce4", "xfce4-goodies"],
        default_dm: Some("lightdm"),
        portals: &["xdg-desktop-portal-gtk"],
        polkit: Some("polkit-gnome"),
        extras: &["network-manager-applet", "xfce4-terminal"],
        services: &[],
        env: &[],
        session_exec: "startxfce4",
        aur: &[],
        notes: "X11 only. LightDM + GTK greeter is the conventional pairing.",
    },
    Recipe {
        key: "cinnamon",
        display_name: "Cinnamon",
        session: Session::X11,
        core: &["cinnamon", "cinnamon-translations", "gnome-terminal", "nemo"],
        default_dm: Some("lightdm"),
        portals: &["xdg-desktop-portal-gtk"],
        polkit: Some("polkit-gnome"),
        extras: &["network-manager-applet", "gnome-keyring"],
        services: &[],
        env: &[],
        session_exec: "cinnamon-session",
        aur: &[],
        notes: "X11. Wayland session is experimental and not enabled by default.",
    },
    Recipe {
        key: "mate",
        display_name: "MATE",
        session: Session::X11,
        core: &["mate", "mate-extra"],
        default_dm: Some("lightdm"),
        portals: &["xdg-desktop-portal-gtk"],
        polkit: Some("mate-polkit"),
        extras: &["network-manager-applet"],
        services: &[],
        env: &[],
        session_exec: "mate-session",
        aur: &[],
        notes: "Classic GTK desktop, X11 only.",
    },
    Recipe {
        key: "lxqt",
        display_name: "LXQt",
        session: Session::X11,
        core: &["lxqt", "openbox"],
        default_dm: Some("sddm"),
        portals: &["xdg-desktop-portal-lxqt"],
        polkit: Some("lxqt-policykit"),
        extras: &["breeze-icons", "network-manager-applet", "qterminal"],
        services: &[],
        env: &[],
        session_exec: "startlxqt",
        aur: &[],
        notes: "LXQt needs a window manager; Openbox is the recommended pairing.",
    },
    Recipe {
        key: "lxde",
        display_name: "LXDE",
        session: Session::X11,
        core: &["lxde"],
        default_dm: Some("lightdm"),
        portals: &["xdg-desktop-portal-gtk"],
        polkit: Some("polkit-gnome"),
        extras: &["network-manager-applet"],
        services: &[],
        env: &[],
        session_exec: "startlxde",
        aur: &[],
        notes: "Lightweight legacy GTK2 desktop.",
    },
    Recipe {
        key: "budgie",
        display_name: "Budgie",
        session: Session::X11,
        core: &["budgie-desktop", "budgie-desktop-view", "gnome-control-center"],
        default_dm: Some("lightdm"),
        portals: &["xdg-desktop-portal-gtk"],
        polkit: Some("polkit-gnome"),
        extras: &["gnome-terminal", "network-manager-applet", "gnome-keyring"],
        services: &[],
        env: &[],
        session_exec: "budgie-desktop",
        aur: &[],
        notes: "Solus's desktop on Arch; X11 session.",
    },
    Recipe {
        key: "deepin",
        display_name: "Deepin",
        session: Session::X11,
        core: &["deepin", "deepin-extra"],
        default_dm: Some("lightdm"),
        portals: &["xdg-desktop-portal-gtk"],
        polkit: Some("polkit-gnome"),
        extras: &["network-manager-applet"],
        services: &[],
        env: &[],
        session_exec: "startdde",
        aur: &[],
        notes: "Uses LightDM with the Deepin greeter (lightdm-deepin-greeter) on a real install.",
    },
    Recipe {
        key: "cosmic",
        display_name: "COSMIC",
        session: Session::Wayland,
        core: &["cosmic-session", "cosmic-comp", "cosmic-applets", "cosmic-panel", "cosmic-settings", "cosmic-files", "cosmic-terminal", "cosmic-launcher"],
        default_dm: Some("cosmic-greeter"), // COSMIC's native greetd-based greeter
        portals: &["xdg-desktop-portal-cosmic"],
        polkit: Some("polkit-gnome"),
        extras: &[],
        services: &[],
        env: &[],
        session_exec: "cosmic-session",
        aur: &[],
        notes: "System76's Rust desktop, now in the official Arch repos. cosmic-greeter is the native login manager.",
    },
    // ===================== Wayland window managers =====================
    Recipe {
        key: "hyprland",
        display_name: "Hyprland",
        session: Session::Wayland,
        core: &["hyprland", "xorg-xwayland"],
        default_dm: Some("sddm"),
        portals: &["xdg-desktop-portal-hyprland"],
        polkit: Some("hyprpolkitagent"),
        extras: &["kitty", "waybar", "wofi", "mako", "hyprpaper", "hyprlock", "hypridle", "network-manager-applet", "qt5-wayland", "qt6-wayland"],
        services: &[],
        env: &[("XDG_CURRENT_DESKTOP", "Hyprland"), ("QT_QPA_PLATFORM", "wayland;xcb"), ("MOZ_ENABLE_WAYLAND", "1")],
        session_exec: "Hyprland",
        aur: &[],
        notes: "Dynamic Wayland compositor. SDDM auto-detects the hyprland session; greetd works too.",
    },
    Recipe {
        key: "sway",
        display_name: "Sway",
        session: Session::Wayland,
        core: &["sway", "xorg-xwayland"],
        default_dm: Some("greetd"),
        portals: &["xdg-desktop-portal-wlr"],
        polkit: Some("polkit-gnome"),
        extras: &["swaybg", "swaylock", "swayidle", "waybar", "foot", "wmenu", "mako", "network-manager-applet"],
        services: &[],
        env: &[("XDG_CURRENT_DESKTOP", "sway"), ("MOZ_ENABLE_WAYLAND", "1")],
        session_exec: "sway",
        aur: &[],
        notes: "i3-compatible Wayland tiler. Commonly launched via greetd/tuigreet or from a TTY.",
    },
    Recipe {
        key: "niri",
        display_name: "Niri",
        session: Session::Wayland,
        core: &["niri", "xwayland-satellite"],
        default_dm: Some("gdm"),
        portals: &["xdg-desktop-portal-gnome", "xdg-desktop-portal-gtk"],
        polkit: Some("polkit-gnome"),
        extras: &["foot", "fuzzel", "waybar", "mako", "swaylock", "swaybg", "network-manager-applet"],
        services: &[],
        env: &[("XDG_CURRENT_DESKTOP", "niri")],
        session_exec: "niri-session",
        aur: &[],
        notes: "Scrollable-tiling Wayland compositor. The GNOME portal gives the best screencast support; xwayland-satellite provides X app support.",
    },
    Recipe {
        key: "river",
        display_name: "River",
        session: Session::Wayland,
        core: &["river", "xorg-xwayland"],
        default_dm: Some("greetd"),
        portals: &["xdg-desktop-portal-wlr"],
        polkit: Some("polkit-gnome"),
        extras: &["foot", "waybar", "fuzzel", "mako", "swaybg", "network-manager-applet"],
        services: &[],
        env: &[("XDG_CURRENT_DESKTOP", "river")],
        session_exec: "river",
        aur: &[],
        notes: "Dynamically-tiling Wayland compositor configured by an external script.",
    },
    Recipe {
        key: "labwc",
        display_name: "labwc",
        session: Session::Wayland,
        core: &["labwc", "xorg-xwayland"],
        default_dm: Some("greetd"),
        portals: &["xdg-desktop-portal-wlr"],
        polkit: Some("polkit-gnome"),
        extras: &["foot", "waybar", "fuzzel", "mako", "swaybg", "network-manager-applet"],
        services: &[],
        env: &[("XDG_CURRENT_DESKTOP", "labwc")],
        session_exec: "labwc",
        aur: &[],
        notes: "Openbox-like stacking Wayland compositor.",
    },
    Recipe {
        key: "wayfire",
        display_name: "Wayfire",
        session: Session::Wayland,
        core: &["wayfire", "xorg-xwayland"],
        default_dm: Some("greetd"),
        portals: &["xdg-desktop-portal-wlr"],
        polkit: Some("polkit-gnome"),
        extras: &["wf-shell", "foot", "fuzzel", "mako", "network-manager-applet"],
        services: &[],
        env: &[("XDG_CURRENT_DESKTOP", "wayfire")],
        session_exec: "wayfire",
        aur: &["wf-shell"],
        notes: "Compiz-style 3D Wayland compositor.",
    },
    // ===================== X11 window managers =====================
    Recipe {
        key: "i3",
        display_name: "i3",
        session: Session::X11,
        core: &["i3-wm", "i3status", "i3lock", "xorg-server", "xorg-xinit"],
        default_dm: Some("lightdm"),
        portals: &["xdg-desktop-portal-gtk"],
        polkit: Some("polkit-gnome"),
        extras: &["dmenu", "rofi", "picom", "feh", "dunst", "alacritty", "network-manager-applet"],
        services: &[],
        env: &[],
        session_exec: "i3",
        aur: &[],
        notes: "i3-gaps is merged into i3-wm. LightDM lists the i3 xsession automatically.",
    },
    Recipe {
        key: "bspwm",
        display_name: "bspwm",
        session: Session::X11,
        core: &["bspwm", "sxhkd", "xorg-server", "xorg-xinit"],
        default_dm: Some("lightdm"),
        portals: &["xdg-desktop-portal-gtk"],
        polkit: Some("polkit-gnome"),
        extras: &["polybar", "rofi", "picom", "feh", "dunst", "alacritty", "network-manager-applet"],
        services: &[],
        env: &[],
        session_exec: "bspwm",
        aur: &[],
        notes: "sxhkd is mandatory — bspwm has no built-in keybindings.",
    },
    Recipe {
        key: "awesome",
        display_name: "awesome",
        session: Session::X11,
        core: &["awesome", "xorg-server", "xorg-xinit"],
        default_dm: Some("lightdm"),
        portals: &["xdg-desktop-portal-gtk"],
        polkit: Some("polkit-gnome"),
        extras: &["rofi", "picom", "feh", "dunst", "alacritty", "network-manager-applet"],
        services: &[],
        env: &[],
        session_exec: "awesome",
        aur: &[],
        notes: "Lua-configured framework WM.",
    },
    Recipe {
        key: "qtile",
        display_name: "Qtile",
        session: Session::X11,
        core: &["qtile", "xorg-server", "xorg-xinit"],
        default_dm: Some("lightdm"),
        portals: &["xdg-desktop-portal-gtk"],
        polkit: Some("polkit-gnome"),
        extras: &["rofi", "picom", "feh", "dunst", "alacritty", "network-manager-applet"],
        services: &[],
        env: &[],
        session_exec: "qtile start",
        aur: &[],
        notes: "Python-configured WM with an experimental Wayland backend.",
    },
    Recipe {
        key: "openbox",
        display_name: "Openbox",
        session: Session::X11,
        core: &["openbox", "xorg-server", "xorg-xinit"],
        default_dm: Some("lightdm"),
        portals: &["xdg-desktop-portal-gtk"],
        polkit: Some("polkit-gnome"),
        extras: &["obconf-qt", "tint2", "rofi", "picom", "feh", "dunst", "lxappearance", "network-manager-applet"],
        services: &[],
        env: &[],
        session_exec: "openbox-session",
        aur: &[],
        notes: "Minimal stacking WM, common as a base for other setups.",
    },
    Recipe {
        key: "xmonad",
        display_name: "xmonad",
        session: Session::X11,
        core: &["xmonad", "xmonad-contrib", "xorg-server", "xorg-xinit"],
        default_dm: Some("lightdm"),
        portals: &["xdg-desktop-portal-gtk"],
        polkit: Some("polkit-gnome"),
        extras: &["xmobar", "dmenu", "picom", "feh", "dunst", "alacritty", "network-manager-applet"],
        services: &[],
        env: &[],
        session_exec: "xmonad",
        aur: &[],
        notes: "Haskell-configured tiler; recompiles config on the fly.",
    },
    Recipe {
        key: "herbstluftwm",
        display_name: "herbstluftwm",
        session: Session::X11,
        core: &["herbstluftwm", "xorg-server", "xorg-xinit"],
        default_dm: Some("lightdm"),
        portals: &["xdg-desktop-portal-gtk"],
        polkit: Some("polkit-gnome"),
        extras: &["dmenu", "picom", "feh", "dunst", "alacritty", "network-manager-applet"],
        services: &[],
        env: &[],
        session_exec: "herbstluftwm",
        aur: &[],
        notes: "Manual tiling WM driven entirely by an IPC tool (herbstclient).",
    },
    Recipe {
        key: "fluxbox",
        display_name: "Fluxbox",
        session: Session::X11,
        core: &["fluxbox", "xorg-server", "xorg-xinit"],
        default_dm: Some("lightdm"),
        portals: &["xdg-desktop-portal-gtk"],
        polkit: Some("polkit-gnome"),
        extras: &["feh", "picom", "network-manager-applet"],
        services: &[],
        env: &[],
        session_exec: "startfluxbox",
        aur: &[],
        notes: "Lightweight stacking WM with a classic taskbar.",
    },
    Recipe {
        key: "icewm",
        display_name: "IceWM",
        session: Session::X11,
        core: &["icewm", "xorg-server", "xorg-xinit"],
        default_dm: Some("lightdm"),
        portals: &["xdg-desktop-portal-gtk"],
        polkit: Some("polkit-gnome"),
        extras: &["feh", "network-manager-applet"],
        services: &[],
        env: &[],
        session_exec: "icewm-session",
        aur: &[],
        notes: "Very low-resource WM with a built-in taskbar and menu.",
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;

    fn manifest_with_desktop(desktop: &str) -> Manifest {
        let json = format!(r#"{{"schema_version":"1.0.0","desktop":"{desktop}"}}"#);
        Manifest::from_str(&json).unwrap()
    }

    #[test]
    fn resolve_maps_desktop_to_its_default_dm() {
        let gnome = resolve(&manifest_with_desktop("gnome")).unwrap().unwrap();
        assert_eq!(gnome.dm.as_ref().unwrap().service, "gdm.service");
        let plasma = resolve(&manifest_with_desktop("plasma")).unwrap().unwrap();
        assert_eq!(plasma.dm.as_ref().unwrap().service, "sddm.service");
        let niri = resolve(&manifest_with_desktop("niri")).unwrap().unwrap();
        assert_eq!(niri.dm.as_ref().unwrap().service, "gdm.service");
    }

    #[test]
    fn switch_default_no_ops_when_dm_matches_or_absent() {
        // Dry-run ctx never executes; switch_default reads the live symlink,
        // which won't exist on a dev box, so active_dm_unit() is None → false.
        let ctx = Ctx::new(true);
        let gnome = resolve(&manifest_with_desktop("gnome")).unwrap().unwrap();
        assert!(!switch_default(&gnome, &ctx));
    }

    #[test]
    fn display_manager_override_wins_over_recipe_default() {
        let json = r#"{"schema_version":"1.0.0","desktop":"niri","display_manager":"greetd"}"#;
        let m = Manifest::from_str(json).unwrap();
        let r = resolve(&m).unwrap().unwrap();
        assert_eq!(r.dm.as_ref().unwrap().service, "greetd.service");
    }
}
