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
    // Graphics with a software-rendering safety net, so a machine with a weak,
    // missing, or half-broken GPU (a VM's virtual GPU, a headless server, a
    // laptop before its driver loads) still runs apps instead of crashing them.
    // `mesa` provides hardware GL *and* the llvmpipe software GL fallback;
    // `vulkan-swrast` (lavapipe) is a software Vulkan device, which is what lets
    // Vulkan-first toolkits like GTK4/libadwaita fall back to the CPU when no
    // usable GPU Vulkan exists. On real hardware these are no-ops — the hardware
    // driver is preferred; they only kick in when the GPU can't do the job.
    "mesa",
    "vulkan-icd-loader",
    "vulkan-swrast",
];

/// System services every graphical system wants enabled.
const DESKTOP_BASE_SERVICES: &[&str] = &["NetworkManager"];

/// The catalog keys that are **bare window managers** — a compositor/WM with
/// no panel, launcher, notification daemon or keybindings of its own, so it
/// needs a user-provided config (a Segment, or Designer/`files` edits) to be
/// usable. Everything else in the catalog is a **complete desktop
/// environment**. This is the single source of truth for the DE↔WM split
/// (the wallpaper daemon logic and the Snapshots app both consult it).
pub const WINDOW_MANAGERS: &[&str] = &[
    // Wayland
    "hyprland", "sway", "niri", "river", "labwc", "wayfire",
    // X11
    "i3", "bspwm", "awesome", "qtile", "openbox", "xmonad",
    "herbstluftwm", "fluxbox", "icewm",
];

/// Whether a desktop key names a bare window manager (vs. a complete DE).
/// Case-insensitive. Unknown keys are treated as not-a-WM.
pub fn is_window_manager(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    WINDOW_MANAGERS.contains(&k.as_str())
}

impl Recipe {
    /// Whether this recipe is a bare window manager (see [`is_window_manager`]).
    pub fn is_wm(&self) -> bool {
        is_window_manager(self.key)
    }
}

/// Look up a recipe by key (case-insensitive).
pub fn recipe(key: &str) -> Option<&'static Recipe> {
    let k = key.to_ascii_lowercase();
    CATALOG.iter().find(|r| r.key == k)
}

/// Every supported environment, for `manifest desktops`.
pub fn catalog() -> &'static [Recipe] {
    CATALOG
}

/// Detect which catalog desktop is installed, given a predicate that answers
/// "is this package installed?". Matches on each recipe's signature package
/// (the first entry of its `core`, the defining one — `gnome-shell`,
/// `plasma-meta`, `hyprland`, `niri`, …). Returns the first catalog match, or
/// `None` if no known environment is present. Used by `manifest export`.
pub fn detect_installed(is_installed: impl Fn(&str) -> bool) -> Option<&'static str> {
    CATALOG
        .iter()
        .find(|r| r.core.first().is_some_and(|sig| is_installed(sig)))
        .map(|r| r.key)
}

/// The full package set a `desktop` recipe pulls in (base + core + portals +
/// polkit + extras + its display manager) — everything `resolve` would install.
/// Used by export to subtract desktop-implied packages from the user's list.
pub fn implied_packages(key: &str) -> Vec<String> {
    let json = format!(r#"{{"schema_version":"1.0.0","desktop":"{key}"}}"#);
    Manifest::from_str(&json)
        .ok()
        .and_then(|m| resolve(&m).ok().flatten())
        .map(|r| r.packages)
        .unwrap_or_default()
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
        // ly ships a per-tty template unit (`ly@.service`), enabled on tty2 by
        // Arch convention — there is no plain `ly.service`, and it does NOT
        // provide the `display-manager.service` alias the graphical DMs do
        // (it's a TUI greeter bound to a VT). So `switch_default` can't detect
        // a running ly by the alias symlink: switching *to* ly works (the old
        // alias DM is disabled, ly@tty2 enabled), but switching *away from* ly
        // to a graphical DM won't auto-disable ly — a documented edge case.
        "ly" => ("ly", "ly", "ly@tty2.service"),
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
    /// The manifest's optional login-screen appearance settings.
    pub login: Option<crate::manifest::Login>,
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
            anyhow::anyhow!("unknown display_manager `{k}` (gdm|sddm|lightdm|greetd|ly|cosmic-greeter)")
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
    // Command-line clipboard tools, matched to the session type. Without these,
    // copy/paste in GUI apps still works (the toolkit + compositor handle it),
    // but any keybind or script that pipes to/from the clipboard
    // (`wl-copy`/`xclip`) — and many WM rice configs do — would silently fail.
    // A "Both" DE ships both sessions, so give it both helpers.
    match r.session {
        Session::Wayland => push("wl-clipboard", &mut packages),
        Session::X11 => push("xclip", &mut packages),
        Session::Both => {
            push("wl-clipboard", &mut packages);
            push("xclip", &mut packages);
        }
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
        login: manifest.login.clone(),
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
        configure_display_manager(dm, d.session_exec, d.login.as_ref(), ctx)?;
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

    // Software-rendering fallback for virtual / 3D-less GPUs. DESKTOP_BASE
    // makes llvmpipe/lavapipe *available*, but availability isn't selection: a
    // VM's virtual GPU still advertises a real-looking GL driver that tops out
    // at GL 2.1 (VirtualBox VMSVGA) or has no 3D at all (QXL/bochs), so
    // GL-3.3+ clients — GTK4/libadwaita apps like System Snapshots, kitty —
    // try the hardware path and fail to start instead of falling back, while
    // CPU-rendered apps (foot, Firefox's own fallback) run fine. The drop-in
    // detects those GPUs at login and forces the software path; real GPUs are
    // untouched.
    ctx.write_root(GPU_FALLBACK_PATH, GPU_FALLBACK)?;

    // Let wheel members actually use a power/logout button. logind gates
    // poweroff/reboot/suspend behind polkit, and — this is the part that bites
    // ricers — only the *-multiple-sessions actions require authentication when
    // more than one session is registered (a second TTY, a lingering greeter
    // session, some resume states). With a single session it's silent and
    // works; with two it silently needs a prompt that may never appear (no
    // agent, or the agent doesn't grab focus) — so the exact same power button
    // "randomly" does nothing depending on session count. This is the standard
    // Arch Wiki fix (Polkit § Allow users in wheel group to run power actions).
    ctx.write_root(POLKIT_POWER_PATH, POLKIT_POWER_RULE)?;
    Ok(())
}

const GPU_FALLBACK_PATH: &str = "/etc/profile.d/manifest-gpu-fallback.sh";

const POLKIT_POWER_PATH: &str = "/etc/polkit-1/rules.d/46-manifest-power.rules";

const POLKIT_POWER_RULE: &str = r#"// Managed by Manifest OS — let wheel members power off/reboot/suspend without
// a polkit prompt, even when more than one session is registered (the case
// that otherwise makes a desktop's power/logout button silently do nothing).
polkit.addRule(function(action, subject) {
    var powerActions = [
        "org.freedesktop.login1.power-off",
        "org.freedesktop.login1.power-off-multiple-sessions",
        "org.freedesktop.login1.reboot",
        "org.freedesktop.login1.reboot-multiple-sessions",
        "org.freedesktop.login1.suspend",
        "org.freedesktop.login1.suspend-multiple-sessions",
        "org.freedesktop.login1.hibernate",
        "org.freedesktop.login1.hibernate-multiple-sessions"
    ];
    if (powerActions.indexOf(action.id) !== -1 && subject.isInGroup("wheel")) {
        return polkit.Result.YES;
    }
});
"#;

const GPU_FALLBACK: &str = r#"# Managed by Manifest OS — software rendering on virtual / 3D-less GPUs.
# VirtualBox (vboxvideo/vmwgfx), VMware (vmwgfx) and QEMU (qxl/bochs/cirrus)
# expose GL drivers too old (or absent) for GTK4 apps and GL-3.3 terminals,
# which then fail to start instead of falling back. Force Mesa's llvmpipe
# (full OpenGL in software) there. Real GPUs — and virtio-gpu with virgl —
# are left alone.
manifest_gpu_needs_software() {
    _found=""
    for _drv in /sys/class/drm/card*/device/driver; do
        [ -e "$_drv" ] || continue
        _found=1
        case "$(basename "$(readlink -f "$_drv")")" in
            vmwgfx|vboxvideo|qxl|bochs|bochs-drm|cirrus) return 0 ;;
        esac
    done
    # A DRM card but no render node can't do hardware 3D either.
    if [ -n "$_found" ] && ls /dev/dri/renderD* >/dev/null 2>&1; then
        return 1
    fi
    return 0
}
if manifest_gpu_needs_software; then
    export LIBGL_ALWAYS_SOFTWARE=1   # Mesa llvmpipe: full GL, on the CPU
    export GSK_RENDERER=cairo        # GTK4: skip the GL/Vulkan probe entirely
    export WLR_NO_HARDWARE_CURSORS=1 # visible cursor on wlroots compositors
    export AQ_NO_HARDWARE_CURSORS=1  # ... and on Hyprland's aquamarine backend
fi
unset -f manifest_gpu_needs_software
"#;

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

/// tuigreet's `--theme` spec (component=named-ANSI-color pairs, semicolon
/// separated — see the tuigreet README; hex colors aren't supported, only
/// ratatui's named ANSI set). Plain black-and-white is the default with no
/// `--theme` at all, which is what made the greetd login screen look bare.
pub const TUIGREET_THEME: &str =
    "border=magenta;text=white;prompt=green;time=cyan;action=magenta;button=white;container=black;input=white";

const SDDM_THEME_QML: &str = include_str!("../assets/sddm-theme/Main.qml");
const SDDM_THEME_METADATA: &str = include_str!("../assets/sddm-theme/metadata.desktop");

/// Write whatever config a display manager needs to actually launch the chosen
/// session, and apply the manifest's [`Login`](crate::manifest::Login) theming.
/// SDDM and GDM auto-detect sessions; SDDM additionally gets a theme.
fn configure_display_manager(
    dm: &DisplayManager,
    session_exec: &str,
    login: Option<&crate::manifest::Login>,
    ctx: &Ctx,
) -> Result<()> {
    match dm.key {
        // LightDM needs to be told which greeter to use.
        "lightdm" => ctx.write_root(
            "/etc/lightdm/lightdm.conf.d/50-manifest.conf",
            "[Seat:*]\ngreeter-session=lightdm-gtk-greeter\n",
        ),
        // SDDM's own default theme is plain black-and-white. Pick a theme by the
        // manifest's `login.theme`: unset / "manifest" ships the bundled theme
        // (styled from `login.accent`/`panel`/`background`/`font`); any other
        // name just *selects* that already-installed theme and skips ours
        // entirely — so it's a default, never a lock-in.
        "sddm" => configure_sddm(login, ctx),
        // greetd has no session picker of its own; tuigreet provides one, and
        // we can pre-select the environment via --cmd when we know it.
        "greetd" => {
            let theme = login
                .and_then(|l| l.tuigreet_theme.as_deref())
                .unwrap_or(TUIGREET_THEME);
            let base = format!("tuigreet --time --remember --theme '{theme}'");
            let command = if session_exec.is_empty() {
                base
            } else {
                format!("{base} --cmd {session_exec}")
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

/// Apply the SDDM login theme per the manifest's [`Login`](crate::manifest::Login).
/// A `login.theme` other than `"manifest"` just selects that installed theme and
/// leaves ours off the disk; otherwise ship the bundled theme and build its
/// `theme.conf` from the `login` colour fields (with sensible defaults).
fn configure_sddm(login: Option<&crate::manifest::Login>, ctx: &Ctx) -> Result<()> {
    let chosen = login.and_then(|l| l.theme.as_deref()).unwrap_or("manifest");
    if chosen != "manifest" {
        // Use a theme the manifest installed itself — don't ship or clobber it.
        return ctx.write_root(
            "/etc/sddm.conf.d/10-manifest-theme.conf",
            &format!("[Theme]\nCurrent={chosen}\n"),
        );
    }
    let get = |f: fn(&crate::manifest::Login) -> Option<&str>, default: &'static str| {
        login.and_then(f).unwrap_or(default).to_string()
    };
    let theme_conf = format!(
        "[General]\nAccentColor={}\nPanelColor={}\nFontFamily={}\nBackground={}\n",
        get(|l| l.accent.as_deref(), "#7aa2f7"),
        get(|l| l.panel.as_deref(), "#141414"),
        get(|l| l.font.as_deref(), "Noto Sans"),
        get(|l| l.background.as_deref(), ""),
    );
    ctx.write_root("/usr/share/sddm/themes/manifest/Main.qml", SDDM_THEME_QML)?;
    ctx.write_root("/usr/share/sddm/themes/manifest/theme.conf", &theme_conf)?;
    ctx.write_root("/usr/share/sddm/themes/manifest/metadata.desktop", SDDM_THEME_METADATA)?;
    ctx.write_root("/etc/sddm.conf.d/10-manifest-theme.conf", "[Theme]\nCurrent=manifest\n")
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
        extras: &["gnome-keyring", "xdg-user-dirs-gtk", "gnome-screenshot"],
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
        extras: &["network-manager-applet", "gnome-keyring", "gnome-screenshot"],
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
        extras: &["kitty", "waybar", "wofi", "mako", "hyprpaper", "hyprlock", "hypridle", "network-manager-applet", "qt5-wayland", "qt6-wayland", "grim", "slurp"],
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
        extras: &["swaybg", "swaylock", "swayidle", "waybar", "foot", "wmenu", "mako", "network-manager-applet", "grim", "slurp"],
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
        extras: &["dmenu", "rofi", "picom", "feh", "dunst", "alacritty", "network-manager-applet", "scrot"],
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

    #[test]
    fn gpu_fallback_dropin_covers_the_virtual_gpu_traps() {
        // The login-time fallback must handle every no-3D virtual driver and
        // force both the GL and GTK software paths (see the VM boot-test trap:
        // GTK4 apps + kitty fail on Hyprland while foot/Firefox run fine).
        for needle in [
            "vmwgfx",
            "vboxvideo",
            "qxl",
            "LIBGL_ALWAYS_SOFTWARE=1",
            "GSK_RENDERER=cairo",
            "WLR_NO_HARDWARE_CURSORS=1",
            "AQ_NO_HARDWARE_CURSORS=1",
        ] {
            assert!(GPU_FALLBACK.contains(needle), "gpu fallback missing: {needle}");
        }
        // virtio-gpu can do real 3D via virgl — it must NOT be in the driver
        // match list (the prose comment may mention it; the case arm may not).
        assert!(GPU_FALLBACK.contains("vmwgfx|vboxvideo|qxl|bochs|bochs-drm|cirrus)"));
        assert!(!GPU_FALLBACK.contains("virtio)") && !GPU_FALLBACK.contains("|virtio"));
    }

    #[test]
    fn every_catalog_entry_is_classified_de_or_wm() {
        // The Snapshots app splits the picker into complete DEs vs. bare WMs;
        // a new catalog entry that no one classified would land in the wrong
        // bucket. Sanity-check the split covers the whole catalog.
        let wms = CATALOG.iter().filter(|r| r.is_wm()).count();
        let des = CATALOG.iter().filter(|r| !r.is_wm()).count();
        assert_eq!(wms, WINDOW_MANAGERS.len(), "a WINDOW_MANAGERS key isn't in the catalog (or vice-versa)");
        assert!(des >= 8, "expected the full DEs (gnome, plasma, xfce, …) to outnumber this");
        assert_eq!(wms + des, CATALOG.len());
        // Spot-check the split lands where humans expect.
        assert!(is_window_manager("hyprland") && is_window_manager("niri"));
        assert!(!is_window_manager("gnome") && !is_window_manager("plasma"));
        assert!(!is_window_manager("cosmic")); // a full DE, not a WM
        assert!(!is_window_manager("unknown-thing"));
    }

    #[test]
    fn ly_maps_to_its_per_tty_template_unit() {
        // Modern ly ships `ly@.service` (enabled on tty2), not `ly.service` —
        // the wrong name would fail at `systemctl enable`.
        let dm = display_manager("ly").unwrap();
        assert_eq!(dm.service, "ly@tty2.service");
    }
}
