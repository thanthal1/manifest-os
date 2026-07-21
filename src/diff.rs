//! Diffing two manifests — the engine behind `manifest diff` and the desktop
//! app's change previews.
//!
//! [`compute`] returns a structured list of [`Change`]s between a `new`
//! manifest and the `current` one; the CLI [`run`] prints them, and the GUI
//! renders them as coloured rows. Because a manifest is the system's declared
//! state, this is exactly "what `sync` would change": packages added, whether
//! the desktop / login manager / theme changes, and so on.

use crate::manifest::{Manifest, Theme, Wallpaper};

/// Whether a change adds something, removes it, or alters a value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChangeKind {
    Added,
    Removed,
    Changed,
}

/// One difference between two manifests, in plain language.
#[derive(Clone, Debug)]
pub struct Change {
    pub kind: ChangeKind,
    /// e.g. "Desktop", "Apps", "Theme".
    pub category: String,
    /// e.g. "niri → GNOME", "firefox", "Dark mode: off → on".
    pub detail: String,
}

/// The structured difference of `new` against `current` (or `None` for a fresh
/// system, where everything reads as added). An empty result means the system
/// already matches `new`.
pub fn compute(new: &Manifest, current: Option<&Manifest>) -> Vec<Change> {
    let empty = Manifest::from_str(r#"{"schema_version":"1.0.0"}"#).expect("valid empty manifest");
    let old = current.unwrap_or(&empty);
    let mut out = Vec::new();

    list_changes(&mut out, "Apps", &old.packages, &new.packages);
    value_change(&mut out, "Desktop", old.desktop.as_deref(), new.desktop.as_deref());
    value_change(&mut out, "Login screen", old.display_manager.as_deref(), new.display_manager.as_deref());
    value_change(&mut out, "Kernel", old.system.kernel.as_deref(), new.system.kernel.as_deref());
    value_change(&mut out, "Computer name", old.system.hostname.as_deref(), new.system.hostname.as_deref());
    value_change(&mut out, "Language", old.system.locale.as_deref(), new.system.locale.as_deref());
    value_change(&mut out, "Time zone", old.system.timezone.as_deref(), new.system.timezone.as_deref());
    value_change(&mut out, "Keyboard", old.system.keymap.as_deref(), new.system.keymap.as_deref());

    let (wall_old, wall_new) = wall_pair(old, new);
    value_change(&mut out, "Wallpaper", wall_old.as_deref(), wall_new.as_deref());

    theme_changes(&mut out, old.theme.as_ref(), new.theme.as_ref());
    flatpak_changes(&mut out, old, new);
    defaults_changes(&mut out, old, new);

    if old.keybindings.len() != new.keybindings.len() {
        out.push(changed(
            "Shortcuts",
            &format!("{} → {} custom shortcut(s)", old.keybindings.len(), new.keybindings.len()),
        ));
    }

    list_changes(&mut out, "Services", &old.services.system, &new.services.system);
    let old_users: Vec<String> = old.users.iter().map(|u| u.name.clone()).collect();
    let new_users: Vec<String> = new.users.iter().map(|u| u.name.clone()).collect();
    list_changes(&mut out, "Users", &old_users, &new_users);

    out
}

/// Whether applying `new` over `current` needs the **full** pipeline (package
/// installs, desktop/user/bootloader/service work) rather than just regenerating
/// config. A settings/variables-only edit touches none of these, so it can take
/// the fast [`crate::install::reconfigure`] path; anything here forces a sync.
pub fn requires_full_apply(new: &Manifest, current: Option<&Manifest>) -> bool {
    let empty = Manifest::from_str(r#"{"schema_version":"1.0.0"}"#).expect("valid empty manifest");
    let old = current.unwrap_or(&empty);

    let set = |v: &[String]| v.iter().cloned().collect::<std::collections::BTreeSet<_>>();
    let flatpak_apps =
        |m: &Manifest| m.flatpak.as_ref().map(|f| set(&f.apps)).unwrap_or_default();
    let users = |m: &Manifest| m.users.iter().map(|u| u.name.clone()).collect::<std::collections::BTreeSet<_>>();

    set(&old.packages) != set(&new.packages)
        || old.desktop != new.desktop
        || old.display_manager != new.display_manager
        || old.system.kernel != new.system.kernel
        || old.boot.is_some() != new.boot.is_some()
        || set(&old.services.system) != set(&new.services.system)
        || set(&old.services.user) != set(&new.services.user)
        || flatpak_apps(old) != flatpak_apps(new)
        || users(old) != users(new)
}

/// Print the diff (CLI `manifest diff`).
pub fn run(new: &Manifest, current: Option<&Manifest>) {
    match current {
        Some(_) => println!("Changes this manifest would apply (vs. the last-applied one):\n"),
        None => println!("No manifest on record yet — showing everything as new:\n"),
    }
    let changes = compute(new, current);
    if changes.is_empty() {
        println!("  (no differences — the system already matches this manifest)");
        return;
    }
    for c in &changes {
        let sign = match c.kind {
            ChangeKind::Added => "+",
            ChangeKind::Removed => "-",
            ChangeKind::Changed => "~",
        };
        println!("  {sign} {}: {}", c.category, c.detail);
    }
    println!("\nRun `manifest sync <file>` to apply. Apps are never uninstalled;");
    println!("removed (-) entries just stop being declared.");
}

// ---------------------------------------------------------------------------

fn changed(category: &str, detail: &str) -> Change {
    Change { kind: ChangeKind::Changed, category: category.into(), detail: detail.into() }
}

/// A single-value field: emit a `Changed` when old and new differ (`(none)`
/// stands in for an unset value).
fn value_change(out: &mut Vec<Change>, category: &str, old: Option<&str>, new: Option<&str>) {
    let o = old.unwrap_or("(none)");
    let n = new.unwrap_or("(none)");
    if o != n {
        out.push(changed(category, &format!("{o} → {n}")));
    }
}

/// Added/removed entries between two lists.
fn list_changes(out: &mut Vec<Change>, category: &str, old: &[String], new: &[String]) {
    for a in new.iter().filter(|p| !old.contains(p)) {
        out.push(Change { kind: ChangeKind::Added, category: category.into(), detail: a.clone() });
    }
    for r in old.iter().filter(|p| !new.contains(p)) {
        out.push(Change { kind: ChangeKind::Removed, category: category.into(), detail: r.clone() });
    }
}

fn theme_changes(out: &mut Vec<Change>, old: Option<&Theme>, new: Option<&Theme>) {
    let none = Theme {
        global: None,
        global_source: None,
        global_install: None,
        gtk: None,
        icons: None,
        cursor: None,
        cursor_size: None,
        font: None,
        monospace_font: None,
        dark: None,
    };
    let o = old.unwrap_or(&none);
    let n = new.unwrap_or(&none);
    value_change(out, "Global theme", o.global.as_deref(), n.global.as_deref());
    value_change(out, "App theme", o.gtk.as_deref(), n.gtk.as_deref());
    value_change(out, "Icons", o.icons.as_deref(), n.icons.as_deref());
    value_change(out, "Cursor", o.cursor.as_deref(), n.cursor.as_deref());
    let os = o.cursor_size.map(|s| s.to_string());
    let ns = n.cursor_size.map(|s| s.to_string());
    value_change(out, "Cursor size", os.as_deref(), ns.as_deref());
    value_change(out, "Font", o.font.as_deref(), n.font.as_deref());
    value_change(out, "Monospace font", o.monospace_font.as_deref(), n.monospace_font.as_deref());
    let od = o.dark.map(yesno);
    let nd = n.dark.map(yesno);
    value_change(out, "Dark mode", od, nd);
}

fn flatpak_changes(out: &mut Vec<Change>, old: &Manifest, new: &Manifest) {
    let old_apps = old.flatpak.as_ref().map(|f| f.apps.clone()).unwrap_or_default();
    let new_apps = new.flatpak.as_ref().map(|f| f.apps.clone()).unwrap_or_default();
    list_changes(out, "Flatpak apps", &old_apps, &new_apps);

    let old_remotes: Vec<String> = old
        .flatpak
        .as_ref()
        .map(|f| f.remotes.iter().map(|r| r.name.clone()).collect())
        .unwrap_or_default();
    let new_remotes: Vec<String> = new
        .flatpak
        .as_ref()
        .map(|f| f.remotes.iter().map(|r| r.name.clone()).collect())
        .unwrap_or_default();
    list_changes(out, "Flatpak remotes", &old_remotes, &new_remotes);
}

fn defaults_changes(out: &mut Vec<Change>, old: &Manifest, new: &Manifest) {
    value_change(
        out,
        "Default browser",
        old.defaults.as_ref().and_then(|d| d.browser.as_deref()),
        new.defaults.as_ref().and_then(|d| d.browser.as_deref()),
    );
    let old_mime = old.defaults.as_ref().map(|d| d.mime.len()).unwrap_or(0);
    let new_mime = new.defaults.as_ref().map(|d| d.mime.len()).unwrap_or(0);
    if old_mime != new_mime {
        out.push(changed(
            "Default apps",
            &format!("{old_mime} -> {new_mime} MIME association(s)"),
        ));
    }
}

fn wall_pair(old: &Manifest, new: &Manifest) -> (Option<String>, Option<String>) {
    (old.wallpaper.as_ref().map(wallpaper_src), new.wallpaper.as_ref().map(wallpaper_src))
}

fn wallpaper_src(w: &Wallpaper) -> String {
    w.source().to_string()
}

fn yesno(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Manifest {
        Manifest::from_str(json).unwrap()
    }

    #[test]
    fn packages_added_and_removed() {
        let old = parse(r#"{"schema_version":"1.0.0","packages":["vim","git"]}"#);
        let new = parse(r#"{"schema_version":"1.0.0","packages":["git","firefox"]}"#);
        let changes = compute(&new, Some(&old));
        assert!(changes.iter().any(|c| c.kind == ChangeKind::Added && c.detail == "firefox"));
        assert!(changes.iter().any(|c| c.kind == ChangeKind::Removed && c.detail == "vim"));
        assert!(!changes.iter().any(|c| c.detail == "git")); // unchanged
    }

    #[test]
    fn desktop_and_theme_changes() {
        let old = parse(r#"{"schema_version":"1.0.0","desktop":"niri","theme":{"dark":false}}"#);
        let new = parse(r#"{"schema_version":"1.0.0","desktop":"gnome","theme":{"dark":true}}"#);
        let changes = compute(&new, Some(&old));
        assert!(changes.iter().any(|c| c.category == "Desktop" && c.detail == "niri → gnome"));
        assert!(changes.iter().any(|c| c.category == "Dark mode" && c.detail == "off → on"));
    }

    #[test]
    fn identical_manifests_yield_no_changes() {
        let m = parse(r#"{"schema_version":"1.0.0","desktop":"niri","packages":["git"]}"#);
        assert!(compute(&m, Some(&m)).is_empty());
    }

    #[test]
    fn no_current_shows_everything_as_added() {
        let new = parse(r#"{"schema_version":"1.0.0","desktop":"gnome","packages":["firefox"]}"#);
        let changes = compute(&new, None);
        assert!(changes.iter().any(|c| c.category == "Desktop" && c.detail == "(none) → gnome"));
        assert!(changes.iter().any(|c| c.kind == ChangeKind::Added && c.detail == "firefox"));
    }

    #[test]
    fn run_smoke_both_paths() {
        let new = parse(r#"{"schema_version":"1.0.0","desktop":"gnome","packages":["firefox"]}"#);
        run(&new, None);
        let old = parse(r#"{"schema_version":"1.0.0","desktop":"niri"}"#);
        run(&new, Some(&old));
    }

    #[test]
    fn config_only_edit_does_not_require_full_apply() {
        // A variables/theme/wallpaper edit (what the Settings app produces) must
        // take the fast reconfigure path.
        let old = parse(r#"{"schema_version":"1.0.0","packages":["git"],"desktop":"niri","theme":{"dark":false}}"#);
        let new = parse(r#"{"schema_version":"1.0.0","packages":["git"],"desktop":"niri","theme":{"dark":true},"variables":{"scale":"1.5"}}"#);
        assert!(!requires_full_apply(&new, Some(&old)));
    }

    #[test]
    fn package_or_desktop_edit_requires_full_apply() {
        let old = parse(r#"{"schema_version":"1.0.0","packages":["git"],"desktop":"niri"}"#);
        // added a package
        let more_pkgs = parse(r#"{"schema_version":"1.0.0","packages":["git","firefox"],"desktop":"niri"}"#);
        assert!(requires_full_apply(&more_pkgs, Some(&old)));
        // changed the desktop
        let other_de = parse(r#"{"schema_version":"1.0.0","packages":["git"],"desktop":"gnome"}"#);
        assert!(requires_full_apply(&other_de, Some(&old)));
        // added a service
        let svc = parse(r#"{"schema_version":"1.0.0","packages":["git"],"desktop":"niri","services":{"system":["sshd"]}}"#);
        assert!(requires_full_apply(&svc, Some(&old)));
    }

    #[test]
    fn flatpak_and_defaults_changes() {
        let old = parse(r#"{"schema_version":"1.0.0"}"#);
        let new = parse(
            r#"{
                "schema_version":"1.0.0",
                "flatpak":{
                    "remotes":[{"name":"flathub","url":"https://flathub.org/repo/flathub.flatpakrepo"}],
                    "apps":["com.visualstudio.code"]
                },
                "defaults":{
                    "browser":"firefox.desktop",
                    "mime":{"application/pdf":"org.gnome.Evince.desktop"}
                }
            }"#,
        );
        let changes = compute(&new, Some(&old));
        assert!(changes.iter().any(|c| c.category == "Flatpak apps" && c.detail == "com.visualstudio.code"));
        assert!(changes.iter().any(|c| c.category == "Flatpak remotes" && c.detail == "flathub"));
        assert!(changes.iter().any(|c| c.category == "Default browser" && c.detail.contains("firefox.desktop")));
        assert!(changes.iter().any(|c| c.category == "Default apps" && c.detail == "0 -> 1 MIME association(s)"));
    }
}
