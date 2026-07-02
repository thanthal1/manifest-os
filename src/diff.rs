//! `manifest diff` — preview what applying a manifest would change.
//!
//! Compares a new manifest against the **last-applied** one (recorded in the
//! git history by [`crate::history`]). Because the manifest is the system's
//! declared state, that comparison is exactly "what `sync` would do": which
//! packages get added, whether the desktop / login manager / kernel / theme
//! changes, and so on. With no history yet (a fresh system), every field is
//! shown as new.
//!
//! This is a *declared-state* diff, not a live-system scan — it answers "how
//! does this manifest differ from the one I last applied", which is the
//! question you ask before an edit-then-sync.

use crate::manifest::{Manifest, Theme, Wallpaper};

/// Print the diff of `new` against `current` (the last-applied manifest, or
/// `None` on a fresh system). Returns nothing — it's a report.
pub fn run(new: &Manifest, current: Option<&Manifest>) {
    match current {
        Some(_) => println!("Changes this manifest would apply (vs. the last-applied one):\n"),
        None => println!("No manifest on record yet — showing everything as new:\n"),
    }
    // An empty stand-in so "no history" naturally renders as all-additions.
    let empty = Manifest::from_str(r#"{"schema_version":"1.0.0"}"#).expect("valid empty manifest");
    let old = current.unwrap_or(&empty);

    let mut any = false;

    // Packages — the most common edit. sync never uninstalls, so removed
    // entries are informational (they just stop being declared).
    any |= list_diff("Packages", &old.packages, &new.packages);

    // Single-value fields: old → new, only when they differ.
    any |= field("Desktop", old.desktop.as_deref(), new.desktop.as_deref());
    any |= field("Login manager", old.display_manager.as_deref(), new.display_manager.as_deref());
    any |= field("Kernel", old.system.kernel.as_deref(), new.system.kernel.as_deref());
    any |= field("Hostname", old.system.hostname.as_deref(), new.system.hostname.as_deref());
    any |= field("Locale", old.system.locale.as_deref(), new.system.locale.as_deref());
    any |= field("Timezone", old.system.timezone.as_deref(), new.system.timezone.as_deref());
    any |= field("Keymap", old.system.keymap.as_deref(), new.system.keymap.as_deref());

    // Repos.
    any |= field("multilib", Some(yesno(old.repos.multilib)), Some(yesno(new.repos.multilib)));
    any |= field("cachyos repo", Some(yesno(old.repos.cachyos)), Some(yesno(new.repos.cachyos)));

    // Wallpaper (compare the source).
    any |= field(
        "Wallpaper",
        old.wallpaper.as_ref().map(wallpaper_src),
        new.wallpaper.as_ref().map(wallpaper_src),
    );

    // Theme — one line per changed sub-field.
    any |= theme_diff(old.theme.as_ref(), new.theme.as_ref());

    // Keybindings / services / users — by count and membership.
    any |= field(
        "Keybindings",
        Some(old.keybindings.len().to_string()),
        Some(new.keybindings.len().to_string()),
    );
    let old_svc: Vec<String> = old.services.system.clone();
    let new_svc: Vec<String> = new.services.system.clone();
    any |= list_diff("Services", &old_svc, &new_svc);
    let old_users: Vec<String> = old.users.iter().map(|u| u.name.clone()).collect();
    let new_users: Vec<String> = new.users.iter().map(|u| u.name.clone()).collect();
    any |= list_diff("Users", &old_users, &new_users);

    if !any {
        println!("  (no differences — the system already matches this manifest)");
    } else {
        println!("\nRun `manifest sync <file>` to apply. Packages are never uninstalled;");
        println!("removed (-) entries just stop being declared.");
    }
}

/// A "Label: old → new" line, printed only when the values differ. `None`
/// renders as `(none)`. Returns whether it printed (i.e. whether it changed).
fn field(label: &str, old: Option<impl AsRef<str>>, new: Option<impl AsRef<str>>) -> bool {
    let o = old.as_ref().map(|s| s.as_ref()).unwrap_or("(none)");
    let n = new.as_ref().map(|s| s.as_ref()).unwrap_or("(none)");
    if o == n {
        return false;
    }
    println!("  {label}:  {o} → {n}");
    true
}

/// Added (`+`) / removed (`-`) entries between two lists. Returns whether any
/// difference was printed.
fn list_diff(label: &str, old: &[String], new: &[String]) -> bool {
    let added: Vec<&String> = new.iter().filter(|p| !old.contains(p)).collect();
    let removed: Vec<&String> = old.iter().filter(|p| !new.contains(p)).collect();
    if added.is_empty() && removed.is_empty() {
        return false;
    }
    println!("  {label}:");
    for a in &added {
        println!("    + {a}");
    }
    for r in &removed {
        println!("    - {r}");
    }
    true
}

/// One line per changed theme sub-field.
fn theme_diff(old: Option<&Theme>, new: Option<&Theme>) -> bool {
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
    let mut changed = false;
    changed |= field("Theme (gtk)", o.gtk.as_deref(), n.gtk.as_deref());
    changed |= field("Theme (icons)", o.icons.as_deref(), n.icons.as_deref());
    changed |= field("Theme (cursor)", o.cursor.as_deref(), n.cursor.as_deref());
    changed |= field(
        "Theme (font)",
        o.font.as_deref(),
        n.font.as_deref(),
    );
    changed |= field(
        "Theme (dark)",
        o.dark.map(yesno),
        n.dark.map(yesno),
    );
    changed
}

fn wallpaper_src(w: &Wallpaper) -> String {
    w.source().to_string()
}

fn yesno(b: bool) -> String {
    if b { "yes".to_string() } else { "no".to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Manifest {
        Manifest::from_str(json).unwrap()
    }

    #[test]
    fn list_diff_reports_added_and_removed() {
        assert!(list_diff("P", &["a".into(), "b".into()], &["b".into(), "c".into()]));
        // identical lists → no diff
        assert!(!list_diff("P", &["a".into()], &["a".into()]));
    }

    #[test]
    fn field_prints_only_on_change() {
        assert!(field("D", Some("niri"), Some("gnome")));
        assert!(!field("D", Some("niri"), Some("niri")));
        assert!(field("D", None::<&str>, Some("gnome")));
        assert!(!field("D", None::<&str>, None::<&str>));
    }

    #[test]
    fn theme_diff_detects_subfield_change() {
        let a = parse(r#"{"schema_version":"1.0.0","theme":{"gtk":"Adwaita"}}"#);
        let b = parse(r#"{"schema_version":"1.0.0","theme":{"gtk":"Materia-dark","dark":true}}"#);
        assert!(theme_diff(a.theme.as_ref(), b.theme.as_ref()));
        assert!(!theme_diff(a.theme.as_ref(), a.theme.as_ref()));
    }

    #[test]
    fn run_against_none_does_not_panic_and_shows_new() {
        // Smoke: a fresh-system diff (no history) just prints additions.
        let new = parse(r#"{"schema_version":"1.0.0","desktop":"gnome","packages":["firefox"]}"#);
        run(&new, None);
    }

    #[test]
    fn run_against_current_exercises_the_changed_path() {
        // Smoke: a DM switch + package swap against a prior manifest.
        let old = parse(r#"{"schema_version":"1.0.0","desktop":"niri","display_manager":"greetd","packages":["vim"]}"#);
        let new = parse(r#"{"schema_version":"1.0.0","desktop":"niri","display_manager":"gdm","packages":["neovim"]}"#);
        run(&new, Some(&old));
    }

    #[test]
    fn identical_manifests_report_no_differences() {
        // Every field-level comparator must return false for identical inputs.
        let m = parse(r#"{"schema_version":"1.0.0","desktop":"niri","packages":["git","vim"]}"#);
        assert!(!list_diff("P", &m.packages, &m.packages));
        assert!(!field("D", m.desktop.as_deref(), m.desktop.as_deref()));
        assert!(!theme_diff(m.theme.as_ref(), m.theme.as_ref()));
    }
}
