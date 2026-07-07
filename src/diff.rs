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
}
