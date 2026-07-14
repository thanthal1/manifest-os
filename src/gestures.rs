//! Cross-desktop touchpad gestures — the same "native-first, daemon-fallback"
//! shape as [`crate::keybindings`].
//!
//!   * **Workspace swipe** (swipe horizontally to change workspace) uses each
//!     environment's NATIVE support where it exists — Hyprland's
//!     `workspace_swipe` (a `gestures { }` block) and niri's built-in 3-finger
//!     swipes (on by default; nothing to write). No extra packages.
//!   * **Everything else** — custom `command` gestures, or any gesture on an
//!     environment without native support (Sway, i3, bspwm, …) — goes through
//!     the `libinput-gestures` daemon: a `~/.config/libinput-gestures.conf`, the
//!     user added to the `input` group, and an autostart entry.
//!
//! The daemon package(s) are added to the manifest's `packages` (and the
//! recorded JSON) automatically by [`required_packages`] — called from
//! `main`'s `load_manifest` — so a gesture-configured manifest reproduces its
//! own dependencies, matching how survey/plugin packages are folded in.
//!
//! Config files are written through [`crate::files`]/[`crate::snippets`], so an
//! explicit `files` entry at the same path (a hand-authored config) still wins
//! — those run after this step.

use crate::exec::Ctx;
use crate::files;
use crate::manifest::{Gesture, Snippet};
use crate::snippets;
use anyhow::Result;

/// Whether `desktop` handles a horizontal workspace swipe natively.
fn native_workspace(desktop: &str) -> bool {
    matches!(desktop, "hyprland" | "niri")
}

/// X11 window managers — libinput-gestures needs `xdotool` to drive a custom
/// gesture there (no Wayland compositor IPC to dispatch to).
fn is_x11(desktop: &str) -> bool {
    matches!(
        desktop,
        "i3" | "bspwm" | "awesome" | "qtile" | "openbox" | "xmonad" | "herbstluftwm" | "fluxbox" | "icewm"
    )
}

/// Whether a gesture is satisfied natively (a plain workspace swipe on a
/// native-capable desktop) rather than needing the libinput-gestures daemon.
fn is_native(g: &Gesture, desktop: &str) -> bool {
    g.command.is_none() && g.action == "workspace" && native_workspace(desktop)
}

/// Packages the gestures need beyond what's installed: the libinput-gestures
/// daemon (+ `xdotool` on X11) when any gesture falls back to it. Empty when
/// every gesture is native, or there are none. The caller adds these to the
/// manifest's package set and the recorded JSON so the setup is reproducible.
pub fn required_packages(gestures: &[Gesture], desktop: Option<&str>) -> Vec<String> {
    if gestures.is_empty() {
        return Vec::new();
    }
    let Some(d) = desktop else {
        return Vec::new();
    };
    if gestures.iter().all(|g| is_native(g, d)) {
        return Vec::new();
    }
    let mut pkgs = vec!["libinput-gestures".to_string()];
    if is_x11(d) {
        pkgs.push("xdotool".to_string());
    }
    pkgs
}

/// Apply the manifest's gestures for `desktop`. `primary_user` is the manifest's
/// first account (see [`crate::keybindings::apply`] for why it matters).
pub fn apply(
    gestures: &[Gesture],
    desktop: Option<&str>,
    primary_user: Option<&str>,
    ctx: &Ctx,
) -> Result<()> {
    if gestures.is_empty() {
        return Ok(());
    }
    let Some(desktop) = desktop else {
        println!("  · warning: `gestures` set but no `desktop` declared — nothing to apply them to");
        return Ok(());
    };

    // 1) Native workspace swipe.
    if gestures.iter().any(|g| is_native(g, desktop)) {
        match desktop {
            "hyprland" => {
                let fingers = gestures
                    .iter()
                    .find(|g| is_native(g, desktop))
                    .map(|g| g.fingers)
                    .unwrap_or(3);
                let content = format!(
                    "gestures {{\n    workspace_swipe = true\n    workspace_swipe_fingers = {fingers}\n}}"
                );
                snippets::apply(
                    &[Snippet {
                        id: "manifest-gestures".into(),
                        path: "~/.config/hypr/hyprland.conf".into(),
                        section: None,
                        content,
                    }],
                    primary_user,
                    ctx,
                )?;
            }
            "niri" => {
                println!("  · niri handles workspace swipes natively (built-in) — nothing to configure")
            }
            _ => {}
        }
    }

    // 2) Everything else via libinput-gestures.
    let daemon: Vec<&Gesture> = gestures.iter().filter(|g| !is_native(g, desktop)).collect();
    if daemon.is_empty() {
        return Ok(());
    }

    let mut conf = String::from("# Managed by Manifest OS — touchpad gestures.\n");
    let mut any = false;
    for g in daemon {
        if let Some(cmd) = &g.command {
            let dir = if g.direction.is_empty() { "left" } else { &g.direction };
            conf.push_str(&format!("gesture swipe {dir} {} {cmd}\n", g.fingers));
            any = true;
        } else if g.action == "workspace" {
            match workspace_cmds(desktop) {
                Some((next, prev)) => {
                    conf.push_str(&format!("gesture swipe left {} {next}\n", g.fingers));
                    conf.push_str(&format!("gesture swipe right {} {prev}\n", g.fingers));
                    any = true;
                }
                None => println!(
                    "  · warning: no workspace-switch command known for `{desktop}` — give the gesture an explicit `command`. Skipping."
                ),
            }
        } else {
            println!(
                "  · warning: unknown gesture action `{}` — use `command` for a literal one. Skipping.",
                g.action
            );
        }
    }
    if !any {
        return Ok(());
    }

    files::apply(
        &[files::home_spec(primary_user, ".config/libinput-gestures.conf", conf)],
        ctx,
    )?;
    // The user must be in the `input` group to read the touchpad event device.
    if let Some(user) = primary_user {
        let _ = ctx.sudo("gpasswd", &["-a", user, "input"]);
    } else {
        println!(
            "  · note: add your user to the `input` group for libinput-gestures (`sudo gpasswd -a $USER input`)"
        );
    }
    files::apply(
        &[files::home_spec(
            primary_user,
            ".config/autostart/libinput-gestures.desktop",
            AUTOSTART.to_string(),
        )],
        ctx,
    )?;
    println!(
        "  · touchpad gestures via libinput-gestures — log out/in for the `input` group + autostart to take effect"
    );
    Ok(())
}

/// The workspace next/prev command for `desktop`'s IPC (used when a workspace
/// gesture falls back to libinput-gestures). `None` for WMs with no universal
/// IPC — those need an explicit `command` on the gesture.
fn workspace_cmds(desktop: &str) -> Option<(&'static str, &'static str)> {
    match desktop {
        "sway" => Some(("swaymsg workspace next_on_output", "swaymsg workspace prev_on_output")),
        "i3" => Some(("i3-msg workspace next", "i3-msg workspace prev")),
        "hyprland" => Some(("hyprctl dispatch workspace e+1", "hyprctl dispatch workspace e-1")),
        "niri" => Some(("niri msg action focus-workspace-down", "niri msg action focus-workspace-up")),
        _ => None,
    }
}

const AUTOSTART: &str = "[Desktop Entry]\n\
Type=Application\n\
Name=libinput-gestures\n\
Comment=Touchpad gestures (Manifest OS)\n\
Exec=libinput-gestures\n\
X-GNOME-Autostart-enabled=true\n\
NoDisplay=true\n";
