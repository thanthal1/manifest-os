//! The **Settings** page — a manifest that declared a `settings` block becomes
//! a friendly control panel here.
//!
//! Each `settings` entry names a [`variables`](manifest::manifest) key; we show
//! that variable's current value with the right control (a slider for a number,
//! a switch for a boolean, a dropdown for a select, a field otherwise). Saving
//! writes the new values back into the manifest's `variables` and re-applies it
//! through the same privileged `manifest sync` the Designer/Apply pages use —
//! so `{{id}}` updates everywhere (scale, wallpaper, accent, …) and the system
//! matches. This is what lets a well-authored manifest double as a settings app.

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;
use manifest::manifest::Question;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

/// Build and register the Settings page. Only shown with content when the
/// applied manifest actually exposes a `settings` block.
pub fn build(
    window: &adw::ApplicationWindow,
    stack: &Rc<adw::ViewStack>,
    toasts: &adw::ToastOverlay,
) {
    let page = adw::PreferencesPage::new();

    let Some((raw_path, doc)) = load_applied() else {
        page.add(&empty_group(
            "No settings to show",
            "This system wasn't set up from a manifest that exposes quick settings.",
        ));
        register(stack, &page);
        return;
    };

    // Read just the `settings` descriptors from the JSON — not a full
    // Manifest parse, which would choke on the still-unresolved `{{tokens}}`
    // in a staged manifest (e.g. `"scale": "{{scale}}"`).
    let settings: Vec<Question> = doc
        .get("settings")
        .cloned()
        .and_then(|s| serde_json::from_value(s).ok())
        .unwrap_or_default();

    if settings.is_empty() {
        page.add(&empty_group(
            "No settings to show",
            "This setup didn't expose any quick settings. Its author can add a \
             `settings` block to the manifest to turn it into a control panel.",
        ));
        register(stack, &page);
        return;
    }

    let doc = Rc::new(RefCell::new(doc));

    let group = adw::PreferencesGroup::builder()
        .title("Settings")
        .description("Tweak what this setup exposed, then Save to apply it live.")
        .build();
    for q in &settings {
        group.add(&row_for(q, &doc));
    }
    page.add(&group);

    // Save & apply.
    let actions = adw::PreferencesGroup::new();
    let save = gtk::Button::builder()
        .label("Save & apply")
        .halign(gtk::Align::Center)
        .build();
    save.add_css_class("suggested-action");
    save.add_css_class("pill");
    {
        let window = window.clone();
        let stack = stack.clone();
        let toasts = toasts.clone();
        let doc = doc.clone();
        save.connect_clicked(move |_| {
            match stage(&doc, &raw_path) {
                Ok(path) => crate::run_privileged(
                    &window,
                    &stack,
                    &toasts,
                    "Applying settings",
                    vec!["sync".into(), path],
                ),
                Err(e) => toasts.add_toast(adw::Toast::new(&format!("Couldn't save settings: {e}"))),
            }
        });
    }
    actions.add(&save);
    page.add(&actions);

    register(stack, &page);
}

fn register(stack: &Rc<adw::ViewStack>, page: &adw::PreferencesPage) {
    stack
        .add_titled(page, Some("settings"), "Settings")
        .set_icon_name(Some("emblem-system-symbolic"));
}

fn empty_group(title: &str, body: &str) -> adw::PreferencesGroup {
    adw::PreferencesGroup::builder().title(title).description(body).build()
}

/// The current value of a setting: the variable's value if set, else the
/// question's default, else empty.
fn current_value(q: &Question, doc: &serde_json::Value) -> String {
    if let Some(v) = doc.get("variables").and_then(|m| m.get(&q.id)) {
        return json_scalar(v);
    }
    q.default.as_ref().map(json_scalar).unwrap_or_default()
}

fn json_scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Write `variables[id] = value` (as a string — variables substitute the same
/// either way) into the in-memory doc.
fn set_var(doc: &Rc<RefCell<serde_json::Value>>, id: &str, value: String) {
    let mut d = doc.borrow_mut();
    let obj = d.as_object_mut().expect("manifest is an object");
    let vars = obj
        .entry("variables")
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    if let Some(map) = vars.as_object_mut() {
        map.insert(id.to_string(), serde_json::Value::String(value));
    }
}

/// Build the right control for a setting, wired to update the doc on change.
fn row_for(q: &Question, doc: &Rc<RefCell<serde_json::Value>>) -> gtk::Widget {
    let cur = current_value(q, &doc.borrow());
    let tooltip = q.description.clone().unwrap_or_default();

    // A scale-type setting (or one literally named "scale") gets a clean
    // preset dropdown — a free spinner is fiddly (rejects 1.75, jumps by odd
    // steps) and there are only a handful of scales anyone wants.
    if q.qtype == "scale" || q.id == "scale" {
        return scale_dropdown(q, doc, &cur, &tooltip);
    }

    match q.qtype.as_str() {
        "boolean" => {
            let row = adw::SwitchRow::builder()
                .title(&q.label)
                .active(matches!(cur.as_str(), "true" | "1" | "yes" | "on"))
                .build();
            row.set_tooltip_text(Some(&tooltip));
            let doc = doc.clone();
            let id = q.id.clone();
            row.connect_active_notify(move |r| set_var(&doc, &id, r.is_active().to_string()));
            row.upcast()
        }
        "number" => {
            let min = q.min.unwrap_or(0.0);
            let max = q.max.unwrap_or(min.max(100.0));
            let val = cur.parse::<f64>().unwrap_or(min).clamp(min, max);
            // 0.25 steps + 2 digits handle scale-like settings; integers still
            // land on whole numbers.
            let adj = gtk::Adjustment::new(val, min, max, 0.25, 1.0, 0.0);
            let row = adw::SpinRow::builder().title(&q.label).adjustment(&adj).digits(2).build();
            row.set_tooltip_text(Some(&tooltip));
            let doc = doc.clone();
            let id = q.id.clone();
            row.connect_value_notify(move |r| set_var(&doc, &id, trim_num(r.value())));
            row.upcast()
        }
        "select" => {
            let opts: Vec<&str> = q.options.iter().map(String::as_str).collect();
            let model = gtk::StringList::new(&opts);
            let row = adw::ComboRow::builder().title(&q.label).model(&model).build();
            row.set_tooltip_text(Some(&tooltip));
            let sel = q.options.iter().position(|o| *o == cur).unwrap_or(0);
            row.set_selected(sel as u32);
            let doc = doc.clone();
            let id = q.id.clone();
            let opts_owned = q.options.clone();
            row.connect_selected_notify(move |r| {
                if let Some(v) = opts_owned.get(r.selected() as usize) {
                    set_var(&doc, &id, v.clone());
                }
            });
            row.upcast()
        }
        _ => {
            // text / path / color / secret — a plain field.
            let row = adw::EntryRow::builder().title(&q.label).text(&cur).build();
            row.set_tooltip_text(Some(&tooltip));
            let doc = doc.clone();
            let id = q.id.clone();
            row.connect_changed(move |r| set_var(&doc, &id, r.text().to_string()));
            row.upcast()
        }
    }
}

/// Format a spin value compactly (`2`, `1.5`) for storing in a variable.
fn trim_num(n: f64) -> String {
    let s = format!("{n:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The scale presets everyone actually uses, `(label, value)`.
const SCALE_PRESETS: &[(&str, &str)] = &[
    ("100%", "1"),
    ("125%", "1.25"),
    ("150%", "1.5"),
    ("175%", "1.75"),
    ("200%", "2"),
    ("250%", "2.5"),
    ("300%", "3"),
];

fn scale_dropdown(
    q: &Question,
    doc: &Rc<RefCell<serde_json::Value>>,
    cur: &str,
    tooltip: &str,
) -> gtk::Widget {
    let labels: Vec<&str> = SCALE_PRESETS.iter().map(|(l, _)| *l).collect();
    let model = gtk::StringList::new(&labels);
    let row = adw::ComboRow::builder().title(&q.label).model(&model).build();
    row.set_tooltip_text(Some(tooltip));
    // Select the preset whose value equals the current one (numeric compare so
    // "1.0" matches "1"); default to 100% when it's off-grid.
    let cur_f = cur.parse::<f64>().unwrap_or(1.0);
    let sel = SCALE_PRESETS
        .iter()
        .position(|(_, v)| v.parse::<f64>().map(|f| (f - cur_f).abs() < 1e-6).unwrap_or(false))
        .unwrap_or(0);
    row.set_selected(sel as u32);
    let doc = doc.clone();
    let id = q.id.clone();
    row.connect_selected_notify(move |r| {
        if let Some((_, v)) = SCALE_PRESETS.get(r.selected() as usize) {
            set_var(&doc, &id, v.to_string());
        }
    });
    row.upcast()
}

// ---------------------------------------------------------------------------
// load / persist the applied manifest
// ---------------------------------------------------------------------------

/// Candidate locations for the applied manifest, most-preferred first: a
/// per-user copy this page saves, then the installer-staged system copy.
fn applied_paths() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        v.push(PathBuf::from(home).join(".local/share/manifest/applied.json"));
    }
    v.push(PathBuf::from("/etc/manifest-install.json"));
    v
}

/// Load the applied manifest as a JSON value (for editing) and note which path
/// it came from. `None` when nothing readable/parseable is found.
fn load_applied() -> Option<(PathBuf, serde_json::Value)> {
    for p in applied_paths() {
        if let Ok(raw) = std::fs::read_to_string(&p) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                if v.is_object() {
                    return Some((p, v));
                }
            }
        }
    }
    None
}

/// Persist the edited manifest to the per-user copy (so the page reopens with
/// the user's values) and write a temp file for `manifest sync` to read.
/// Returns the temp path.
fn stage(doc: &Rc<RefCell<serde_json::Value>>, _from: &std::path::Path) -> anyhow::Result<String> {
    let pretty = serde_json::to_string_pretty(&*doc.borrow())?;
    // Per-user persistence (best-effort — a failure here doesn't block applying).
    if let Some(home) = std::env::var_os("HOME") {
        let dir = PathBuf::from(home).join(".local/share/manifest");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("applied.json"), &pretty);
    }
    let tmp = std::env::temp_dir().join("manifest-settings.json");
    std::fs::write(&tmp, &pretty)?;
    Ok(tmp.to_string_lossy().to_string())
}
