//! Cross-desktop keyboard shortcuts.
//!
//! Every environment stores custom keybindings differently — niri/Hyprland/
//! Sway/i3 want lines in a plain-text config file, KDE and LXQt have their own
//! config-file-backed shortcut daemons (khotkeys, lxqt-globalkeyshortcuts),
//! and GNOME/Cinnamon/MATE/Xfce store them in a running session's
//! dconf/xfconf database. This module takes the manifest's single universal
//! `keybindings` list and, per the declared `desktop`, applies it through
//! that environment's own first-party mechanism:
//!
//!   * **niri** — a `binds { }` block in `~/.config/niri/config.kdl`.
//!   * **Hyprland** — `bind = ...` lines in `~/.config/hypr/hyprland.conf`.
//!   * **Sway / i3** — `bindsym ...` lines in their config file.
//!   * **KDE Plasma** — a `~/.config/khotkeysrc` ("Custom Shortcuts").
//!   * **LXQt** — `~/.config/lxqt/globalkeyshortcuts.conf` (its global
//!     shortcuts daemon).
//!   * **GNOME / Cinnamon / MATE / Xfce** — these live in a per-user
//!     dconf/xfconf database that only a running session can write to, so
//!     (like [`crate::wallpaper`]) we install a small script and run it once
//!     from XDG autostart at first login.
//!
//! Since `desktop` is fixed by the manifest itself (unlike wallpaper, which
//! must runtime-detect whatever session happens to be running), every
//! generator below is built directly from the manifest's declared
//! environment — no runtime detection needed.
//!
//! The config-file environments are written through [`crate::files::apply`],
//! so an explicit `files` entry at the same path (e.g. a fully hand-authored
//! niri config) still wins — `files` always runs after this step.
//!
//! `keys` syntax: `+`-joined modifiers (`SUPER`/`CTRL`/`ALT`/`SHIFT`, case
//! insensitive, with common aliases like `WIN`/`META`/`MOD` for `SUPER`)
//! followed by a key name — a single letter/digit, a common name
//! (`Enter`/`Left`/`F5`/`Escape`), or a raw `XF86...` media-key name. That
//! same key name (an X11/GDK/Qt keysym) is valid verbatim across all ten
//! environments; only the modifier syntax differs, which is what the
//! per-environment formatters below handle.

use crate::exec::Ctx;
use crate::files;
use crate::manifest::Keybinding;
use anyhow::Result;

/// `primary_user` is the manifest's first declared account (`users[0].name`),
/// if any. Config-file environments need it: during a real disk install,
/// `manifest install` itself runs as a throwaway bootstrap account (see
/// `installer.rs`'s `create_bootstrap_user`), so a plain `~/...` path would
/// land in *that* account's home, not the manifest's actual user — the same
/// trap `files` entries avoid by writing an absolute `/home/<user>/...` path
/// with an explicit `owner`. Without a declared user, we fall back to `~/...`
/// (correct when `manifest install` is run directly by a human on an
/// existing system, which has no such bootstrap account).
pub fn apply(bindings: &[Keybinding], desktop: Option<&str>, primary_user: Option<&str>, ctx: &Ctx) -> Result<()> {
    if bindings.is_empty() {
        return Ok(());
    }
    let Some(desktop) = desktop else {
        println!("  · warning: `keybindings` set but no `desktop` declared — nothing to apply them to");
        return Ok(());
    };

    let mut resolved: Vec<Resolved> = Vec::new();
    for kb in bindings {
        let keys = parse_keys(&kb.keys);
        if keys.key.is_empty() {
            println!("  · warning: couldn't parse keybinding `{}` — skipping", kb.keys);
            continue;
        }
        match resolve(kb, &keys, desktop) {
            Some(r) => resolved.push(r),
            None => println!(
                "  · warning: no mapping for action `{}` on `{desktop}` — add \"command\" explicitly. Skipping `{}`.",
                kb.action.as_deref().unwrap_or("?"),
                kb.keys
            ),
        }
    }
    if resolved.is_empty() {
        return Ok(());
    }

    match desktop {
        "niri" => write_config(ctx, primary_user, ".config/niri/config.kdl", niri_config(&resolved)),
        "hyprland" => write_config(ctx, primary_user, ".config/hypr/hyprland.conf", hyprland_config(&resolved)),
        "sway" => write_config(ctx, primary_user, ".config/sway/config", sway_i3_config(&resolved)),
        "i3" => write_config(ctx, primary_user, ".config/i3/config", sway_i3_config(&resolved)),
        "plasma" => write_config(ctx, primary_user, ".config/khotkeysrc", kde_khotkeysrc(&resolved)),
        "lxqt" => write_config(
            ctx,
            primary_user,
            ".config/lxqt/globalkeyshortcuts.conf",
            lxqt_config(&resolved),
        ),
        "gnome" => install_runtime_script(ctx, &gnome_script(&resolved)),
        "cinnamon" => install_runtime_script(ctx, &cinnamon_script(&resolved)),
        "mate" => install_runtime_script(ctx, &mate_script(&resolved)),
        "xfce" => install_runtime_script(ctx, &xfce_script(&resolved)),
        other => {
            println!(
                "  · warning: no keybinding mechanism mapped for `{other}` — set them up via `files`/`post_install`"
            );
            Ok(())
        }
    }
}

/// `rel_path` is relative to a home directory, e.g. `.config/niri/config.kdl`.
/// See [`files::home_spec`] for why a declared user gets an absolute,
/// explicitly-owned path instead of `~/...`.
fn write_config(ctx: &Ctx, primary_user: Option<&str>, rel_path: &str, content: String) -> Result<()> {
    files::apply(&[files::home_spec(primary_user, rel_path, content)], ctx)
}

// ---------------------------------------------------------------------------
// Key syntax
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Keys {
    super_: bool,
    ctrl: bool,
    alt: bool,
    shift: bool,
    /// A single X11/GDK/Qt keysym name (e.g. `Return`, `B`, `F5`,
    /// `XF86AudioRaiseVolume`) — this exact spelling is valid, unmodified,
    /// across every target environment.
    key: String,
}

/// Parse `"SUPER+Shift+Left"` into modifier flags + a normalized key name.
/// Every token but the last is treated as a modifier (unrecognized ones are
/// ignored); the last token is the key.
fn parse_keys(spec: &str) -> Keys {
    let parts: Vec<&str> = spec.split('+').map(str::trim).filter(|s| !s.is_empty()).collect();
    let mut k = Keys::default();
    let Some((key, mods)) = parts.split_last() else { return k };
    for m in mods {
        match m.to_ascii_uppercase().as_str() {
            "SUPER" | "WIN" | "WINDOWS" | "META" | "MOD" | "CMD" | "LOGO" => k.super_ = true,
            "CTRL" | "CONTROL" => k.ctrl = true,
            "ALT" | "OPTION" | "MOD1" => k.alt = true,
            "SHIFT" => k.shift = true,
            _ => {}
        }
    }
    k.key = normalize_key(key);
    k
}

/// Map common aliases to their canonical X11/GDK/Qt keysym spelling; passes
/// single letters/digits, `F<n>` keys and `XF86...` media keys through as-is.
fn normalize_key(k: &str) -> String {
    if k.starts_with("XF86") {
        return k.to_string();
    }
    let up = k.to_ascii_uppercase();
    let named = match up.as_str() {
        "ENTER" | "RETURN" => "Return",
        "ESC" | "ESCAPE" => "Escape",
        "SPACE" | "SPACEBAR" => "space",
        "TAB" => "Tab",
        "BACKSPACE" => "BackSpace",
        "DELETE" | "DEL" => "Delete",
        "LEFT" => "Left",
        "RIGHT" => "Right",
        "UP" => "Up",
        "DOWN" => "Down",
        "HOME" => "Home",
        "END" => "End",
        "PAGEUP" | "PGUP" => "Prior",
        "PAGEDOWN" | "PGDN" => "Next",
        "PRINT" | "PRINTSCREEN" | "PRTSC" | "PRTSCN" => "Print",
        "MINUS" => "minus",
        "EQUAL" | "PLUS" => "equal",
        "COMMA" => "comma",
        "PERIOD" => "period",
        "SLASH" => "slash",
        _ => "",
    };
    if !named.is_empty() {
        return named.to_string();
    }
    if k.chars().count() == 1 {
        return k.to_ascii_uppercase();
    }
    if up.starts_with('F') && up[1..].chars().all(|c| c.is_ascii_digit()) && up.len() > 1 {
        return up;
    }
    // Unrecognized multi-char name — Title-case it and hope it's a valid
    // keysym (covers names we didn't think to alias, e.g. "Insert").
    let mut c = k.chars();
    match c.next() {
        Some(f) => f.to_ascii_uppercase().to_string() + &c.as_str().to_ascii_lowercase(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Action → command resolution
// ---------------------------------------------------------------------------

/// What a resolved binding does: run a shell command, or (WMs only) invoke a
/// native compositor action that has no shell-command equivalent.
enum Effect {
    Spawn(String),
    /// A family-specific native keyword — only ever produced for
    /// `close_window`/`fullscreen` and only consumed by the WM family that
    /// produced it.
    Native(&'static str),
}

struct Resolved {
    keys: Keys,
    effect: Effect,
}

/// Resolve one manifest keybinding into a concrete effect for `desktop`, or
/// `None` if it can't be mapped (an unknown action with no explicit
/// `command`, or `close_window`/`fullscreen` on an environment that has no
/// sensible native equivalent — those already have a default keybinding on
/// full desktop environments, so we leave it alone rather than guess).
fn resolve(kb: &Keybinding, keys: &Keys, desktop: &str) -> Option<Resolved> {
    if let Some(cmd) = &kb.command {
        return Some(Resolved { keys: clone_keys(keys), effect: Effect::Spawn(cmd.clone()) });
    }
    let action = kb.action.as_deref()?;
    if matches!(action, "close_window" | "fullscreen" | "screenshot" if desktop == "niri") {
        // niri has native dispatchers for all three; every other WM either
        // has its own native close/fullscreen (handled below) or needs an
        // external screenshot tool (handled by `action_command`).
        let native = match action {
            "close_window" => "close-window",
            "fullscreen" => "fullscreen-window",
            "screenshot" => "screenshot",
            _ => unreachable!(),
        };
        return Some(Resolved { keys: clone_keys(keys), effect: Effect::Native(native) });
    }
    if matches!(action, "close_window" | "fullscreen") {
        let native = match (action, desktop) {
            ("close_window", "hyprland") => "killactive",
            ("close_window", "sway" | "i3") => "kill",
            ("fullscreen", "hyprland") => "fullscreen, 0",
            ("fullscreen", "sway" | "i3") => "fullscreen toggle",
            _ => return None, // full DEs already bind this by default
        };
        return Some(Resolved { keys: clone_keys(keys), effect: Effect::Native(native) });
    }
    action_command(action, desktop).map(|cmd| Resolved { keys: clone_keys(keys), effect: Effect::Spawn(cmd) })
}

fn clone_keys(k: &Keys) -> Keys {
    Keys { super_: k.super_, ctrl: k.ctrl, alt: k.alt, shift: k.shift, key: k.key.clone() }
}

/// The command a built-in action spawns on a given environment, using
/// whichever terminal/launcher/locker that environment's own recipe already
/// installs (see `desktop.rs`'s `CATALOG`). `volume_*` and `brightness_*` are
/// universal: `wpctl` (pipewire) is in every desktop's base packages, and
/// `brightnessctl` follows the same convention the bundled niri-rice example
/// already uses (add it to `packages` if a manifest wants brightness keys).
fn action_command(action: &str, desktop: &str) -> Option<String> {
    let s = |c: &str| Some(c.to_string());
    match action {
        "volume_up" => s("wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%+"),
        "volume_down" => s("wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%-"),
        "volume_mute" => s("wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle"),
        "brightness_up" => s("brightnessctl set +5%"),
        "brightness_down" => s("brightnessctl set 5%-"),
        "browser" => s(
            "command -v firefox >/dev/null 2>&1 && firefox || \
             command -v chromium >/dev/null 2>&1 && chromium || \
             xdg-open about:blank",
        ),
        "terminal" => s(match desktop {
            "gnome" => "gnome-terminal",
            "plasma" => "konsole",
            "xfce" => "xfce4-terminal",
            "cinnamon" => "gnome-terminal",
            "mate" => "mate-terminal",
            "lxqt" => "qterminal",
            "niri" | "sway" => "foot",
            "hyprland" => "kitty",
            "i3" => "alacritty",
            _ => return None,
        }),
        "launcher" => s(match desktop {
            "plasma" => "krunner",
            "xfce" => "xfce4-appfinder",
            "mate" => "mate-panel --run-dialog",
            "lxqt" => "lxqt-runner",
            "niri" => "fuzzel",
            "hyprland" => "wofi --show drun",
            "sway" => "wmenu-run",
            "i3" => "rofi -show drun",
            // GNOME/Cinnamon have no standalone launcher binary in their
            // recipe — the built-in Activities/menu already covers this.
            _ => return None,
        }),
        "lock" => s(match desktop {
            "gnome" | "plasma" | "lxqt" => "loginctl lock-session",
            "xfce" => "xflock4",
            "cinnamon" => "cinnamon-screensaver-command -l",
            "mate" => "mate-screensaver-command -l",
            "niri" | "sway" => "swaylock",
            "hyprland" => "hyprlock",
            "i3" => "i3lock",
            _ => return None,
        }),
        "screenshot" => s(match desktop {
            "gnome" | "cinnamon" => "gnome-screenshot -i",
            "plasma" => "spectacle",
            "xfce" => "xfce4-screenshooter",
            "mate" => "mate-screenshot",
            "hyprland" | "sway" => "grim -g \"$(slurp)\" \"$HOME/Pictures/screenshot-$(date +%s).png\"",
            "i3" => "scrot \"$HOME/Pictures/screenshot-%Y%m%d-%H%M%S.png\"",
            // niri uses its own native screenshot dispatcher — see `resolve`.
            _ => return None,
        }),
        _ => None,
    }
}

/// Single-quote `s` for safe embedding in a POSIX shell command line.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// niri (KDL)
// ---------------------------------------------------------------------------

fn niri_combo(k: &Keys) -> String {
    let mut s = String::new();
    if k.super_ {
        s.push_str("Mod+");
    }
    if k.ctrl {
        s.push_str("Ctrl+");
    }
    if k.alt {
        s.push_str("Alt+");
    }
    if k.shift {
        s.push_str("Shift+");
    }
    s.push_str(&k.key);
    s
}

fn niri_config(bindings: &[Resolved]) -> String {
    let mut out = String::from("// Managed by Manifest OS — keybindings\nbinds {\n");
    for r in bindings {
        let combo = niri_combo(&r.keys);
        match &r.effect {
            Effect::Spawn(cmd) => {
                out.push_str(&format!("    {combo} {{ spawn \"sh\" \"-c\" \"{}\"; }}\n", kdl_escape(cmd)))
            }
            Effect::Native(action) => out.push_str(&format!("    {combo} {{ {action}; }}\n")),
        }
    }
    out.push_str("}\n");
    out
}

fn kdl_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ---------------------------------------------------------------------------
// Hyprland
// ---------------------------------------------------------------------------

fn hyprland_mods(k: &Keys) -> String {
    let mut v = Vec::new();
    if k.super_ {
        v.push("SUPER");
    }
    if k.ctrl {
        v.push("CTRL");
    }
    if k.alt {
        v.push("ALT");
    }
    if k.shift {
        v.push("SHIFT");
    }
    v.join(" ")
}

fn hyprland_config(bindings: &[Resolved]) -> String {
    let mut out = String::from("# Managed by Manifest OS — keybindings\n");
    for r in bindings {
        let mods = hyprland_mods(&r.keys);
        match &r.effect {
            Effect::Spawn(cmd) => out.push_str(&format!("bind = {mods}, {}, exec, {cmd}\n", r.keys.key)),
            Effect::Native(action) => out.push_str(&format!("bind = {mods}, {}, {action}\n", r.keys.key)),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Sway / i3 (i3-compatible `bindsym`)
// ---------------------------------------------------------------------------

fn i3_mods(k: &Keys) -> String {
    let mut v = Vec::new();
    if k.super_ {
        v.push("Mod4");
    }
    if k.ctrl {
        v.push("Control");
    }
    if k.alt {
        v.push("Mod1");
    }
    if k.shift {
        v.push("Shift");
    }
    let mut s = v.join("+");
    if !s.is_empty() {
        s.push('+');
    }
    s
}

/// Sway and i3 share the same `bindsym` syntax and modifier names.
fn sway_i3_config(bindings: &[Resolved]) -> String {
    let mut out = String::from("# Managed by Manifest OS — keybindings\n");
    for r in bindings {
        let combo = format!("{}{}", i3_mods(&r.keys), r.keys.key);
        match &r.effect {
            Effect::Spawn(cmd) => out.push_str(&format!("bindsym {combo} exec {cmd}\n")),
            Effect::Native(action) => out.push_str(&format!("bindsym {combo} {action}\n")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// KDE Plasma (khotkeysrc — "Custom Shortcuts")
// ---------------------------------------------------------------------------

fn kde_combo(k: &Keys) -> String {
    let mut s = String::new();
    if k.super_ {
        s.push_str("Meta+");
    }
    if k.ctrl {
        s.push_str("Ctrl+");
    }
    if k.alt {
        s.push_str("Alt+");
    }
    if k.shift {
        s.push_str("Shift+");
    }
    s.push_str(&k.key);
    s
}

fn kde_khotkeysrc(bindings: &[Resolved]) -> String {
    // Only Spawn effects apply here — close_window/fullscreen already
    // resolve to Native on WM families and were filtered out for `plasma`
    // by `resolve` (KDE has default bindings for both).
    let mut out = format!("[Data]\nDataCount={}\n\n", bindings.len());
    for (i, r) in bindings.iter().enumerate() {
        let n = i + 1;
        let cmd = match &r.effect {
            Effect::Spawn(cmd) => format!("sh -c {}", sh_quote(cmd)),
            Effect::Native(_) => continue,
        };
        let combo = kde_combo(&r.keys);
        out.push_str(&format!(
            "[Data_{n}]\n\
             Comment=Managed by Manifest OS\n\
             Enabled=true\n\
             Name=manifest-binding-{n}\n\
             Type=SIMPLE_ACTION_DATA\n\n\
             [Data_{n}Actions]\n\
             ActionsCount=1\n\n\
             [Data_{n}Actions0]\n\
             CommandURL={cmd}\n\
             Type=COMMAND_URL\n\n\
             [Data_{n}Conditions]\n\
             Comment=\n\
             ConditionsCount=0\n\n\
             [Data_{n}Triggers]\n\
             Comment=Simple_action\n\
             TriggersCount=1\n\n\
             [Data_{n}Triggers0]\n\
             Key={combo}\n\
             Type=SHORTCUT\n\n"
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// LXQt (lxqt-globalkeyshortcuts daemon)
// ---------------------------------------------------------------------------

fn lxqt_combo(k: &Keys) -> String {
    let mut s = String::new();
    if k.super_ {
        s.push_str("Super+");
    }
    if k.ctrl {
        s.push_str("Ctrl+");
    }
    if k.alt {
        s.push_str("Alt+");
    }
    if k.shift {
        s.push_str("Shift+");
    }
    s.push_str(&k.key);
    s
}

fn lxqt_config(bindings: &[Resolved]) -> String {
    let mut out = String::from("[Shortcuts]\n");
    for r in bindings {
        let Effect::Spawn(cmd) = &r.effect else { continue };
        out.push_str(&format!("{}=sh -c {}\n", lxqt_combo(&r.keys), sh_quote(cmd)));
    }
    out
}

// ---------------------------------------------------------------------------
// GNOME / Cinnamon / MATE / Xfce — dconf/xfconf, applied once at first login
// ---------------------------------------------------------------------------

const RUNTIME_SCRIPT_PATH: &str = "/usr/local/bin/manifest-keybindings";
const RUNTIME_AUTOSTART_PATH: &str = "/etc/xdg/autostart/manifest-keybindings.desktop";

fn install_runtime_script(ctx: &Ctx, script: &str) -> Result<()> {
    ctx.write_root(RUNTIME_SCRIPT_PATH, script)?;
    ctx.sudo("chmod", &["755", RUNTIME_SCRIPT_PATH])?;
    ctx.write_root(RUNTIME_AUTOSTART_PATH, RUNTIME_AUTOSTART)?;
    println!("  · keybindings set for first login");
    Ok(())
}

const RUNTIME_AUTOSTART: &str = "[Desktop Entry]\n\
Type=Application\n\
Name=Manifest OS keybindings\n\
Exec=/usr/local/bin/manifest-keybindings\n\
NoDisplay=true\n\
X-GNOME-Autostart-enabled=true\n\
OnlyShowIn=GNOME;Cinnamon;X-Cinnamon;MATE;XFCE;\n";

/// `<Super>`/`<Primary>`/`<Alt>`/`<Shift>` GTK accelerator syntax, used by
/// GNOME, Cinnamon, MATE and Xfce alike (all built on the same GTK parser).
fn gtk_accel(k: &Keys) -> String {
    let mut s = String::new();
    if k.ctrl {
        s.push_str("<Primary>");
    }
    if k.alt {
        s.push_str("<Alt>");
    }
    if k.shift {
        s.push_str("<Shift>");
    }
    if k.super_ {
        s.push_str("<Super>");
    }
    s.push_str(&k.key);
    s
}

/// Common header for the once-only runtime scripts below.
fn once_header(comment: &str) -> String {
    format!(
        "#!/bin/sh\n# Managed by Manifest OS — {comment}\n\
         marker=\"${{XDG_CONFIG_HOME:-$HOME/.config}}/manifest-keybindings.set\"\n\
         [ -e \"$marker\" ] && exit 0\n\
         mkdir -p \"$(dirname \"$marker\")\"\n\n"
    )
}

fn spawn_bindings(bindings: &[Resolved]) -> impl Iterator<Item = (String, &String)> {
    bindings.iter().filter_map(|r| match &r.effect {
        Effect::Spawn(cmd) => Some((gtk_accel(&r.keys), cmd)),
        Effect::Native(_) => None, // full DEs have no equivalent slot for these
    })
}

fn gnome_script(bindings: &[Resolved]) -> String {
    let mut out = once_header("custom keyboard shortcuts (GNOME)");
    let mut paths = Vec::new();
    for (i, (accel, cmd)) in spawn_bindings(bindings).enumerate() {
        let path = format!("/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom{i}/");
        let schema = "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding";
        out.push_str(&format!(
            "gsettings set {schema}:{path} name {}\n\
             gsettings set {schema}:{path} command {}\n\
             gsettings set {schema}:{path} binding {}\n\n",
            sh_quote(&format!("manifest-{i}")),
            sh_quote(&format!("sh -c {}", sh_quote(cmd))),
            sh_quote(&accel),
        ));
        paths.push(format!("'{path}'"));
    }
    out.push_str(&format!(
        "gsettings set org.gnome.settings-daemon.plugins.media-keys custom-keybindings \"[{}]\"\n",
        paths.join(", ")
    ));
    out.push_str(": > \"$marker\"\n");
    out
}

fn cinnamon_script(bindings: &[Resolved]) -> String {
    let mut out = once_header("custom keyboard shortcuts (Cinnamon)");
    let mut names = Vec::new();
    for (i, (accel, cmd)) in spawn_bindings(bindings).enumerate() {
        let id = format!("custom{i}");
        let path = format!("/org/cinnamon/desktop/keybindings/custom-keybindings/{id}/");
        let schema = "org.cinnamon.desktop.keybindings.custom-keybinding";
        out.push_str(&format!(
            "gsettings set {schema}:{path} name {}\n\
             gsettings set {schema}:{path} command {}\n\
             gsettings set {schema}:{path} binding \"['{accel}']\"\n\n",
            sh_quote(&format!("manifest-{i}")),
            sh_quote(&format!("sh -c {}", sh_quote(cmd))),
        ));
        names.push(format!("'{id}'"));
    }
    out.push_str(&format!(
        "gsettings set org.cinnamon.desktop.keybindings custom-list \"[{}]\"\n",
        names.join(", ")
    ));
    out.push_str(": > \"$marker\"\n");
    out
}

fn mate_script(bindings: &[Resolved]) -> String {
    let mut out = once_header("custom keyboard shortcuts (MATE)");
    let mut paths = Vec::new();
    for (i, (accel, cmd)) in spawn_bindings(bindings).enumerate() {
        let path = format!("/org/mate/desktop/keybindings/custom{i}/");
        let schema = "org.mate.control-center.keybindings.custom-keybinding";
        // MATE inherited its key naming ("action" for the command) from its
        // GConf-era ancestor.
        out.push_str(&format!(
            "gsettings set {schema}:{path} name {}\n\
             gsettings set {schema}:{path} action {}\n\
             gsettings set {schema}:{path} binding {}\n\n",
            sh_quote(&format!("manifest-{i}")),
            sh_quote(&format!("sh -c {}", sh_quote(cmd))),
            sh_quote(&accel),
        ));
        paths.push(format!("'{path}'"));
    }
    out.push_str(&format!(
        "gsettings set org.mate.control-center.keybindings custom-keybindings \"[{}]\"\n",
        paths.join(", ")
    ));
    out.push_str(": > \"$marker\"\n");
    out
}

fn xfce_script(bindings: &[Resolved]) -> String {
    let mut out = once_header("custom keyboard shortcuts (Xfce)");
    for (accel, cmd) in spawn_bindings(bindings) {
        out.push_str(&format!(
            "xfconf-query -c xfce4-keyboard-shortcuts -p {} -n -t string -s {}\n",
            sh_quote(&format!("/commands/custom/{accel}")),
            sh_quote(&format!("sh -c {}", sh_quote(cmd))),
        ));
    }
    out.push_str(": > \"$marker\"\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Keybinding;

    fn kb(keys: &str, action: Option<&str>, command: Option<&str>) -> Keybinding {
        Keybinding {
            keys: keys.to_string(),
            action: action.map(str::to_string),
            command: command.map(str::to_string),
        }
    }

    #[test]
    fn parses_modifiers_and_key() {
        let k = parse_keys("SUPER+Shift+Left");
        assert!(k.super_ && k.shift && !k.ctrl && !k.alt);
        assert_eq!(k.key, "Left");
    }

    #[test]
    fn aliases_common_modifier_spellings() {
        let k = parse_keys("win+ctrl+b");
        assert!(k.super_ && k.ctrl);
        assert_eq!(k.key, "B");
    }

    #[test]
    fn passes_media_keys_through_verbatim() {
        assert_eq!(parse_keys("XF86AudioRaiseVolume").key, "XF86AudioRaiseVolume");
    }

    #[test]
    fn resolves_literal_command_everywhere() {
        let keys = parse_keys("SUPER+B");
        let r = resolve(&kb("SUPER+B", None, Some("firefox")), &keys, "gnome").unwrap();
        assert!(matches!(r.effect, Effect::Spawn(ref c) if c == "firefox"));
    }

    #[test]
    fn resolves_terminal_per_desktop() {
        assert_eq!(action_command("terminal", "niri").as_deref(), Some("foot"));
        assert_eq!(action_command("terminal", "plasma").as_deref(), Some("konsole"));
        assert_eq!(action_command("terminal", "gnome").as_deref(), Some("gnome-terminal"));
    }

    #[test]
    fn close_window_is_native_on_wms_and_unmapped_on_full_des() {
        let keys = parse_keys("SUPER+Q");
        assert!(resolve(&kb("SUPER+Q", Some("close_window"), None), &keys, "niri").is_some());
        assert!(resolve(&kb("SUPER+Q", Some("close_window"), None), &keys, "hyprland").is_some());
        assert!(resolve(&kb("SUPER+Q", Some("close_window"), None), &keys, "gnome").is_none());
    }

    #[test]
    fn niri_screenshot_is_native() {
        let keys = parse_keys("Print");
        let r = resolve(&kb("Print", Some("screenshot"), None), &keys, "niri").unwrap();
        assert!(matches!(r.effect, Effect::Native("screenshot")));
    }

    #[test]
    fn niri_config_contains_expected_binds() {
        let bindings = vec![
            Resolved { keys: parse_keys("SUPER+Enter"), effect: Effect::Spawn("foot".into()) },
            Resolved { keys: parse_keys("SUPER+Q"), effect: Effect::Native("close-window") },
        ];
        let out = niri_config(&bindings);
        assert!(out.contains("Mod+Return { spawn \"sh\" \"-c\" \"foot\"; }"));
        assert!(out.contains("Mod+Q { close-window; }"));
    }

    #[test]
    fn hyprland_config_formats_multi_modifier_combos() {
        let bindings =
            vec![Resolved { keys: parse_keys("SUPER+Shift+Q"), effect: Effect::Native("killactive") }];
        let out = hyprland_config(&bindings);
        assert!(out.contains("bind = SUPER SHIFT, Q, killactive"));
    }

    #[test]
    fn sway_i3_config_uses_mod4_for_super() {
        let bindings =
            vec![Resolved { keys: parse_keys("SUPER+Return"), effect: Effect::Spawn("foot".into()) }];
        let out = sway_i3_config(&bindings);
        assert!(out.contains("bindsym Mod4+Return exec foot"));
    }

    #[test]
    fn gtk_accel_matches_gnome_syntax() {
        let k = parse_keys("SUPER+Shift+Return");
        assert_eq!(gtk_accel(&k), "<Shift><Super>Return");
    }

    #[test]
    fn gnome_script_wraps_command_and_sets_the_list() {
        let bindings = vec![Resolved { keys: parse_keys("SUPER+B"), effect: Effect::Spawn("firefox".into()) }];
        let out = gnome_script(&bindings);
        // The command is nested inside two layers of shell quoting (the
        // gsettings arg is itself a shell-quoted "sh -c '<cmd>'" string), so
        // its single quotes end up escaped rather than appearing verbatim —
        // just confirm both layers are present.
        assert!(out.contains("sh -c"));
        assert!(out.contains("firefox"));
        assert!(out.contains("custom-keybindings \"['/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom0/']\""));
    }

    #[test]
    fn sh_quote_escapes_embedded_single_quotes() {
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn unknown_desktop_does_not_panic() {
        let bindings = [kb("SUPER+B", None, Some("firefox"))];
        apply(&bindings, Some("budgie"), Some("alice"), &Ctx::new(true)).unwrap();
    }

    #[test]
    fn declared_user_gets_an_absolute_owned_path_not_bootstrap_home() {
        let spec = files::home_spec(Some("alice"), ".config/niri/config.kdl", String::new());
        assert_eq!(spec.path, "/home/alice/.config/niri/config.kdl");
        assert_eq!(spec.owner.as_deref(), Some("alice:alice"));
    }

    #[test]
    fn no_declared_user_falls_back_to_tilde() {
        let spec = files::home_spec(None, ".config/niri/config.kdl", String::new());
        assert_eq!(spec.path, "~/.config/niri/config.kdl");
        assert!(spec.owner.is_none());
    }

    #[test]
    fn kde_khotkeysrc_has_matching_group_counts() {
        let bindings = vec![
            Resolved { keys: parse_keys("SUPER+B"), effect: Effect::Spawn("firefox".into()) },
            Resolved { keys: parse_keys("SUPER+Return"), effect: Effect::Spawn("konsole".into()) },
        ];
        let out = kde_khotkeysrc(&bindings);
        assert!(out.contains("DataCount=2"));
        assert!(out.contains("[Data_1]"));
        assert!(out.contains("[Data_2]"));
        assert!(out.contains("[Data_1Triggers0]\nKey=Meta+B"));
        assert!(out.contains("Type=COMMAND_URL"));
        assert!(out.contains("Type=SHORTCUT"));
    }

    #[test]
    fn lxqt_config_uses_ini_shortcuts_section() {
        let bindings = vec![Resolved { keys: parse_keys("SUPER+Return"), effect: Effect::Spawn("foot".into()) }];
        let out = lxqt_config(&bindings);
        assert!(out.starts_with("[Shortcuts]\n"));
        assert!(out.contains("Super+Return=sh -c 'foot'"));
    }

    #[test]
    fn xfce_script_writes_xfconf_query_per_binding() {
        let bindings = vec![Resolved { keys: parse_keys("SUPER+B"), effect: Effect::Spawn("firefox".into()) }];
        let out = xfce_script(&bindings);
        assert!(out.contains("xfconf-query -c xfce4-keyboard-shortcuts"));
        assert!(out.contains("/commands/custom/<Super>B"));
    }

    #[test]
    fn mate_uses_action_key_not_command() {
        let bindings = vec![Resolved { keys: parse_keys("SUPER+B"), effect: Effect::Spawn("firefox".into()) }];
        let out = mate_script(&bindings);
        assert!(out.contains(" action "));
        assert!(!out.contains(" command "));
    }

    #[test]
    fn cinnamon_binding_is_a_string_array() {
        let bindings = vec![Resolved { keys: parse_keys("SUPER+B"), effect: Effect::Spawn("firefox".into()) }];
        let out = cinnamon_script(&bindings);
        assert!(out.contains("binding \"['<Super>B']\""));
    }
}
