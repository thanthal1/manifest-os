//! The Designer — a **tree** of your setup you drop shareable *segments* onto.
//!
//! The goal: someone downloads, say, a fancy waybar clock segment and just
//! **drags it onto their bar** — no path, no section, no JSON, no shell. The
//! tree is generated from what's actually on disk (window manager, bar,
//! terminal, notifications…), each config a **drop target**. A segment carries
//! what it fits (`applies_to`), so dropping a waybar segment onto a niri config
//! is simply refused. A dropped segment lands **pending** (amber), is scanned
//! for anything risky, and previewed — it only touches disk on **Apply**, which
//! saves a snapshot first so it's one Restore away from undone.
//!
//! Everything here touches only the user's own config files (no password).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::gdk;
use gtk4 as gtk;
use libadwaita as adw;

use manifest::segment::{self, Segment};
use manifest::snippets;

use crate::snapshots;

/// A config file discovered on disk that segments can be dropped into.
struct Target {
    title: String,
    path: PathBuf,
    kind: &'static str,
    /// Existing managed segments in the file: (id, inner content).
    existing: Vec<(String, String)>,
}

/// A segment staged onto a target, not yet written.
struct Pending {
    target: PathBuf,
    seg: Segment,
    warnings: Vec<String>,
}

struct Designer {
    /// The manifest being edited, as JSON — the tree is generated from this
    /// (`export::capture_json`), and every field edit writes back into it.
    doc: RefCell<serde_json::Value>,
    /// The manifest as first captured, to tell whether anything field-level
    /// changed (so Apply only re-syncs when it needs to).
    baseline: String,
    /// Loaded shareable segments the user can drag (drag-key → segment).
    tray: RefCell<HashMap<String, Segment>>,
    tray_seq: RefCell<u32>,
    /// Segments dropped onto a target, awaiting Apply.
    pending: RefCell<Vec<Pending>>,
    /// Existing segments the user removed, stripped on Apply: (path, id).
    deleted: RefCell<Vec<(PathBuf, String)>>,
    /// Where the tray cards + the tree get (re)built.
    tray_box: gtk::Box,
    tree_box: gtk::Box,
    window: adw::ApplicationWindow,
    stack: Rc<adw::ViewStack>,
    toasts: adw::ToastOverlay,
}

pub fn build(
    window: &adw::ApplicationWindow,
    stack: &Rc<adw::ViewStack>,
    toasts: &adw::ToastOverlay,
) {
    // Generate the tree from the system's manifest (as JSON — the source of truth).
    let captured = manifest::export::capture_json();
    let doc: serde_json::Value =
        serde_json::from_str(&captured).unwrap_or_else(|_| serde_json::json!({}));

    let d = Rc::new(Designer {
        baseline: serde_json::to_string(&doc).unwrap_or_default(),
        doc: RefCell::new(doc),
        tray: RefCell::new(HashMap::new()),
        tray_seq: RefCell::new(0),
        pending: RefCell::new(Vec::new()),
        deleted: RefCell::new(Vec::new()),
        tray_box: gtk::Box::new(gtk::Orientation::Horizontal, 8),
        tree_box: gtk::Box::new(gtk::Orientation::Vertical, 12),
        window: window.clone(),
        stack: stack.clone(),
        toasts: toasts.clone(),
    });

    // Toolbar: open a segment / apply.
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    bar.set_margin_top(10);
    bar.set_margin_bottom(4);
    bar.set_margin_start(12);
    bar.set_margin_end(12);
    let hint = gtk::Label::new(Some(
        "Open a segment you downloaded, then drag it onto a matching part of your setup below.",
    ));
    hint.add_css_class("dim-label");
    hint.set_hexpand(true);
    hint.set_halign(gtk::Align::Start);
    hint.set_wrap(true);
    bar.append(&hint);
    let open = gtk::Button::with_label("Open a segment…");
    let apply = gtk::Button::with_label("Apply changes");
    apply.add_css_class("suggested-action");
    bar.append(&open);
    bar.append(&apply);

    // The segment tray (loaded segments, draggable) + a drop zone for .json files.
    let tray_frame = gtk::Box::new(gtk::Orientation::Vertical, 6);
    tray_frame.set_margin_start(12);
    tray_frame.set_margin_end(12);
    let tray_label = gtk::Label::new(Some("Your segments — drag one onto a match below"));
    tray_label.add_css_class("dim-label");
    tray_label.set_halign(gtk::Align::Start);
    tray_frame.append(&tray_label);
    d.tray_box.add_css_class("card");
    d.tray_box.set_margin_bottom(4);
    let tray_scroll = gtk::ScrolledWindow::builder()
        .child(&d.tray_box)
        .min_content_height(96)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .build();
    tray_frame.append(&tray_scroll);
    // Drop a .json file straight onto the tray to load it.
    {
        let dd = d.clone();
        let file_drop = gtk::DropTarget::new(gtk::gio::File::static_type(), gdk::DragAction::COPY);
        file_drop.connect_drop(move |_, value, _, _| {
            if let Ok(f) = value.get::<gtk::gio::File>() {
                if let Some(p) = f.path() {
                    dd.load_segment_file(&p);
                }
                return true;
            }
            false
        });
        d.tray_box.add_controller(file_drop);
    }

    let scroller = gtk::ScrolledWindow::builder().child(&d.tree_box).vexpand(true).build();
    d.tree_box.set_margin_top(6);
    d.tree_box.set_margin_bottom(12);
    d.tree_box.set_margin_start(12);
    d.tree_box.set_margin_end(12);

    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&bar);
    page.append(&tray_frame);
    page.append(&scroller);

    {
        let dd = d.clone();
        open.connect_clicked(move |_| dd.open_segment_dialog());
    }
    {
        let dd = d.clone();
        apply.connect_clicked(move |_| dd.apply());
    }

    d.rebuild_tray();
    d.rebuild_tree();

    stack
        .add_titled(&page, Some("designer"), "Designer")
        .set_icon_name(Some("view-list-symbolic"));
}

impl Designer {
    fn home() -> PathBuf {
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()))
    }

    /// Every known config file that exists on disk, with its current segments.
    fn scan_targets(&self) -> Vec<Target> {
        let mut out: Vec<Target> = Vec::new();
        for (title, rel, kind) in segment::KNOWN_TARGETS {
            let path = Self::home().join(rel);
            let Ok(content) = std::fs::read_to_string(&path) else { continue };
            if out.iter().any(|t| t.path == path) {
                continue;
            }
            out.push(Target {
                title: title.to_string(),
                path,
                kind,
                existing: snippets::extract_blocks(&content),
            });
        }
        out
    }

    // ---- segment tray -----------------------------------------------------

    fn open_segment_dialog(self: &Rc<Self>) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Segment files (*.json)"));
        filter.add_pattern("*.json");
        let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder()
            .title("Open a segment")
            .filters(&filters)
            .build();
        let this = self.clone();
        dialog.open(Some(&self.window), gtk::gio::Cancellable::NONE, move |res| {
            if let Ok(file) = res {
                if let Some(p) = file.path() {
                    this.load_segment_file(&p);
                }
            }
        });
    }

    fn load_segment_file(self: &Rc<Self>, path: &std::path::Path) {
        let raw = match std::fs::read_to_string(path) {
            Ok(r) => r,
            Err(_) => {
                self.toast("Couldn't read that file.");
                return;
            }
        };
        match Segment::from_json(&raw) {
            Ok(seg) => {
                let mut seq = self.tray_seq.borrow_mut();
                *seq += 1;
                let key = format!("seg-{}", *seq);
                self.tray.borrow_mut().insert(key, seg);
                drop(seq);
                self.rebuild_tray();
                self.toast("Segment loaded — drag it onto a matching part of your setup.");
            }
            Err(e) => self.toast(&format!("Not a usable segment: {e}")),
        }
    }

    fn rebuild_tray(self: &Rc<Self>) {
        while let Some(c) = self.tray_box.first_child() {
            self.tray_box.remove(&c);
        }
        let tray = self.tray.borrow();
        if tray.is_empty() {
            let empty = gtk::Label::new(Some(
                "No segments yet. Click “Open a segment…”, or drop a .json here.",
            ));
            empty.add_css_class("dim-label");
            empty.set_margin_top(16);
            empty.set_margin_bottom(16);
            empty.set_margin_start(16);
            empty.set_hexpand(true);
            empty.set_halign(gtk::Align::Center);
            self.tray_box.append(&empty);
            return;
        }
        // stable order
        let mut items: Vec<(&String, &Segment)> = tray.iter().collect();
        items.sort_by(|a, b| a.0.cmp(b.0));
        for (key, seg) in items {
            self.tray_box.append(&self.segment_card(key, seg));
        }
    }

    /// A draggable card for a loaded segment.
    fn segment_card(self: &Rc<Self>, key: &str, seg: &Segment) -> gtk::Widget {
        let card = gtk::Box::new(gtk::Orientation::Vertical, 2);
        card.add_css_class("card");
        card.set_margin_top(8);
        card.set_margin_bottom(8);
        card.set_margin_start(8);
        card.set_width_request(220);
        let inner = gtk::Box::new(gtk::Orientation::Vertical, 2);
        inner.set_margin_top(8);
        inner.set_margin_bottom(8);
        inner.set_margin_start(10);
        inner.set_margin_end(10);
        card.append(&inner);

        let title = gtk::Label::new(Some(&seg.label));
        title.add_css_class("heading");
        title.set_halign(gtk::Align::Start);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        inner.append(&title);

        let fits = if seg.applies_to.trim().is_empty() {
            "fits: anything (untagged — review)".to_string()
        } else {
            format!("fits: {}", seg.applies_to)
        };
        let sub = gtk::Label::new(Some(&fits));
        sub.add_css_class("dim-label");
        sub.set_halign(gtk::Align::Start);
        inner.append(&sub);

        if !seg.description.trim().is_empty() {
            let desc = gtk::Label::new(Some(&seg.description));
            desc.add_css_class("dim-label");
            desc.set_halign(gtk::Align::Start);
            desc.set_wrap(true);
            desc.set_max_width_chars(28);
            inner.append(&desc);
        }
        let drag_hint = gtk::Label::new(Some("↧ drag onto your setup"));
        drag_hint.add_css_class("dim-label");
        drag_hint.set_halign(gtk::Align::Start);
        inner.append(&drag_hint);

        // Drag source: carry the tray key.
        let src = gtk::DragSource::new();
        src.set_actions(gdk::DragAction::COPY);
        let k = key.to_string();
        src.connect_prepare(move |_, _, _| Some(gdk::ContentProvider::for_value(&k.to_value())));
        card.add_controller(src);
        card.upcast()
    }

    // ---- the tree ---------------------------------------------------------

    fn rebuild_tree(self: &Rc<Self>) {
        while let Some(c) = self.tree_box.first_child() {
            self.tree_box.remove(&c);
        }
        // The manifest itself, generated from the captured JSON — editable.
        self.tree_box.append(&self.system_group());
        self.tree_box.append(&self.desktop_group());
        self.tree_box.append(&self.appearance_group());

        // The config files on disk + the segments you drop onto them.
        let targets = self.scan_targets();
        let group = adw::PreferencesGroup::builder()
            .title("Config files")
            .description("Drag a segment from above onto the matching part.")
            .build();
        if targets.is_empty() {
            let row = adw::ActionRow::builder()
                .title("No desktop config files yet")
                .subtitle("They appear here once your window manager / bar is set up.")
                .build();
            group.add(&row);
        } else {
            for t in &targets {
                group.add(&self.target_row(t));
            }
        }
        self.tree_box.append(&group);
    }

    // ---- manifest field groups (generated from the JSON) ------------------

    fn system_group(self: &Rc<Self>) -> adw::PreferencesGroup {
        let g = adw::PreferencesGroup::builder()
            .title("System")
            .description("The basics — pick from a list, no editing files.")
            .build();
        let doc = self.doc.borrow();
        let kernels: Vec<(String, String)> = manifest::kernel::catalog()
            .iter()
            .map(|k| (k.key.to_string(), format!("{} ({})", k.display, k.key)))
            .collect();
        let cur_kernel = {
            let v = jstr(&doc, &["system", "kernel"]);
            if v.is_empty() { "linux".into() } else { v }
        };
        g.add(&self.combo_row("Kernel", &kernels, &cur_kernel, vec!["system".into(), "kernel".into()]));
        g.add(&self.entry_row("Hostname", &jstr(&doc, &["system", "hostname"]), vec!["system".into(), "hostname".into()]));
        g.add(&self.entry_row("Timezone", &jstr(&doc, &["system", "timezone"]), vec!["system".into(), "timezone".into()]));
        g.add(&self.entry_row("Locale", &jstr(&doc, &["system", "locale"]), vec!["system".into(), "locale".into()]));
        g
    }

    fn desktop_group(self: &Rc<Self>) -> adw::PreferencesGroup {
        let g = adw::PreferencesGroup::builder().title("Desktop").build();
        let doc = self.doc.borrow();
        let mut desktops: Vec<(String, String)> = vec![(String::new(), "(none)".into())];
        desktops.extend(
            manifest::desktop::catalog()
                .iter()
                .map(|r| (r.key.to_string(), r.display_name.to_string())),
        );
        let cur = jstr(&doc, &["desktop"]);
        g.add(&self.combo_row("Desktop / window manager", &desktops, &cur, vec!["desktop".into()]));
        g.add(&self.entry_row(
            "Display manager (optional)",
            &jstr(&doc, &["display_manager"]),
            vec!["display_manager".into()],
        ));
        g
    }

    fn appearance_group(self: &Rc<Self>) -> adw::PreferencesGroup {
        let g = adw::PreferencesGroup::builder().title("Appearance").build();
        let doc = self.doc.borrow();
        g.add(&self.entry_row("App theme (GTK)", &jstr(&doc, &["theme", "gtk"]), vec!["theme".into(), "gtk".into()]));
        g.add(&self.entry_row("Icons", &jstr(&doc, &["theme", "icons"]), vec!["theme".into(), "icons".into()]));
        g.add(&self.switch_row("Dark mode", jbool(&doc, &["theme", "dark"]), vec!["theme".into(), "dark".into()]));
        g.add(&self.entry_row("Wallpaper (path or URL)", &wallpaper_src(&doc), vec!["wallpaper".into()]));
        g
    }

    fn combo_row(
        self: &Rc<Self>,
        title: &str,
        options: &[(String, String)],
        current_key: &str,
        path: Vec<String>,
    ) -> adw::ComboRow {
        let names: Vec<&str> = options.iter().map(|(_, d)| d.as_str()).collect();
        let model = gtk::StringList::new(&names);
        let row = adw::ComboRow::builder().title(title).model(&model).build();
        let keys: Vec<String> = options.iter().map(|(k, _)| k.clone()).collect();
        let sel = keys.iter().position(|k| k == current_key).unwrap_or(0);
        row.set_selected(sel as u32); // set before connecting so it doesn't fire
        let this = self.clone();
        row.connect_selected_notify(move |r| {
            if let Some(k) = keys.get(r.selected() as usize) {
                let p: Vec<&str> = path.iter().map(String::as_str).collect();
                jset_str(&mut this.doc.borrow_mut(), &p, k);
            }
        });
        row
    }

    fn entry_row(self: &Rc<Self>, title: &str, current: &str, path: Vec<String>) -> adw::EntryRow {
        let row = adw::EntryRow::builder().title(title).text(current).build();
        let this = self.clone();
        row.connect_changed(move |r| {
            let p: Vec<&str> = path.iter().map(String::as_str).collect();
            jset_str(&mut this.doc.borrow_mut(), &p, &r.text());
        });
        row
    }

    fn switch_row(self: &Rc<Self>, title: &str, current: bool, path: Vec<String>) -> adw::SwitchRow {
        let row = adw::SwitchRow::builder().title(title).active(current).build();
        let this = self.clone();
        row.connect_active_notify(move |r| {
            let p: Vec<&str> = path.iter().map(String::as_str).collect();
            jset_bool(&mut this.doc.borrow_mut(), &p, r.is_active());
        });
        row
    }

    /// One expandable config-file row: a drop target, with its existing +
    /// pending segments nested inside.
    fn target_row(self: &Rc<Self>, t: &Target) -> adw::ExpanderRow {
        let n_pending = self.pending.borrow().iter().filter(|x| x.target == t.path).count();
        let subtitle = format!(
            "{}  ·  {} segment(s){}",
            t.path.display(),
            t.existing.len(),
            if n_pending > 0 { format!(", {n_pending} pending") } else { String::new() },
        );
        let row = adw::ExpanderRow::builder()
            .title(&format!("{}  ({})", t.title, t.kind))
            .subtitle(&subtitle)
            .build();
        if n_pending > 0 {
            row.set_expanded(true);
        }

        // Drop target: accept a tray key, check compatibility, stage it.
        let this = self.clone();
        let kind = t.kind;
        let path = t.path.clone();
        let drop = gtk::DropTarget::new(String::static_type(), gdk::DragAction::COPY);
        drop.connect_drop(move |_, value, _, _| {
            if let Ok(key) = value.get::<String>() {
                this.handle_drop(&key, &path, kind);
                return true;
            }
            false
        });
        row.add_controller(drop);

        // Existing segments.
        for (id, inner) in &t.existing {
            row.add_row(&self.existing_seg_row(&t.path, id, inner));
        }
        // Pending segments dropped onto this target.
        let pending = self.pending.borrow();
        for (idx, p) in pending.iter().enumerate() {
            if p.target == t.path {
                row.add_row(&self.pending_seg_row(idx, p));
            }
        }
        row
    }

    fn existing_seg_row(self: &Rc<Self>, path: &PathBuf, id: &str, inner: &str) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .title(id)
            .subtitle(&one_line(inner, 60))
            .build();
        let del = gtk::Button::from_icon_name("user-trash-symbolic");
        del.set_tooltip_text(Some("Remove this segment"));
        del.add_css_class("flat");
        del.set_valign(gtk::Align::Center);
        let this = self.clone();
        let p = path.clone();
        let i = id.to_string();
        del.connect_clicked(move |_| {
            this.deleted.borrow_mut().push((p.clone(), i.clone()));
            this.rebuild_tree();
            this.toast("Marked for removal on Apply.");
        });
        row.add_suffix(&del);
        row
    }

    fn pending_seg_row(self: &Rc<Self>, idx: usize, p: &Pending) -> adw::ActionRow {
        let title = if p.warnings.is_empty() {
            format!("＋ {}  (pending)", p.seg.label)
        } else {
            format!("⚠ {}  (pending — {} warning(s))", p.seg.label, p.warnings.len())
        };
        let sub = if p.warnings.is_empty() {
            one_line(&p.seg.content, 60)
        } else {
            p.warnings.join("  •  ")
        };
        let row = adw::ActionRow::builder().title(&title).subtitle(&sub).build();
        row.add_css_class(if p.warnings.is_empty() { "success" } else { "warning" });

        let remove = gtk::Button::from_icon_name("edit-undo-symbolic");
        remove.set_tooltip_text(Some("Don't add this segment"));
        remove.add_css_class("flat");
        remove.set_valign(gtk::Align::Center);
        let this = self.clone();
        remove.connect_clicked(move |_| {
            if idx < this.pending.borrow().len() {
                this.pending.borrow_mut().remove(idx);
            }
            this.rebuild_tree();
        });
        row.add_suffix(&remove);
        row
    }

    /// A segment was dropped onto a target: check it fits, scan it, stage it.
    fn handle_drop(self: &Rc<Self>, key: &str, target: &PathBuf, kind: &str) {
        let seg = match self.tray.borrow().get(key).cloned() {
            Some(s) => s,
            None => return,
        };
        if !segment::segment_fits(&seg.applies_to, kind) {
            let want = if seg.applies_to.trim().is_empty() { "anything" } else { &seg.applies_to };
            self.toast(&format!(
                "“{}” is a {want} segment — it doesn't fit {kind}. Drop it on a matching part.",
                seg.label
            ));
            return;
        }
        let warnings = scan_segment(&seg.content, &seg);
        self.pending.borrow_mut().push(Pending { target: target.clone(), seg, warnings });
        self.rebuild_tree();
        self.toast("Segment staged — review it, then Apply.");
    }

    // ---- apply ------------------------------------------------------------

    fn apply(self: &Rc<Self>) {
        let _ = snapshots::save("Before Designer changes");
        let mut touched = 0usize;

        // 1) Config-file segments — user-owned files, written directly (no root).
        for (path, id) in self.deleted.borrow_mut().drain(..) {
            if let Ok(current) = std::fs::read_to_string(&path) {
                let out = snippets::remove_block(&current, &id);
                if out != current && std::fs::write(&path, out).is_ok() {
                    touched += 1;
                }
            }
        }
        for p in self.pending.borrow_mut().drain(..) {
            let current = std::fs::read_to_string(&p.target).unwrap_or_default();
            let sn = p.seg.to_snippet(&p.target.display().to_string());
            let out = snippets::upsert(&current, &sn);
            if out != current {
                if let Some(dir) = p.target.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                if std::fs::write(&p.target, out).is_ok() {
                    touched += 1;
                }
            }
        }

        // 2) Manifest field changes (kernel, desktop, theme, …) — these need the
        //    system re-synced to match, which is privileged. Hand the edited
        //    manifest to the same `manifest sync` flow the "Apply a setup" page
        //    uses (pkexec + a live progress view).
        let now = serde_json::to_string(&*self.doc.borrow()).unwrap_or_default();
        if now != self.baseline {
            let pretty = serde_json::to_string_pretty(&*self.doc.borrow()).unwrap_or_default();
            let path = std::env::temp_dir().join("manifest-designer.json");
            if std::fs::write(&path, pretty).is_ok() {
                crate::run_privileged(
                    &self.window,
                    &self.stack,
                    &self.toasts,
                    "Applying your changes",
                    vec!["sync".into(), path.to_string_lossy().to_string()],
                );
                return; // run_privileged takes over the view + streams progress
            }
            self.toast("Couldn't stage the manifest changes.");
            return;
        }

        self.rebuild_tree();
        let msg = if touched == 0 {
            "Nothing to change — everything already matches.".to_string()
        } else {
            format!("Applied — {touched} file update(s). A snapshot was saved first, so you can restore.")
        };
        self.toast(&msg);
    }

    fn toast(&self, msg: &str) {
        self.toasts.add_toast(adw::Toast::new(msg));
    }
}

/// Lightweight safety scan of a dropped segment's content — enough to warn a
/// non-technical user before they Apply. The full scanner is `marketplace/`.
fn scan_segment(content: &str, seg: &Segment) -> Vec<String> {
    let mut w = Vec::new();
    let c = content.to_ascii_lowercase();
    let piped_shell = (c.contains("curl") || c.contains("wget"))
        && (c.contains("| sh") || c.contains("|sh") || c.contains("| bash") || c.contains("|bash"));
    if piped_shell {
        w.push("runs a downloaded script (curl | sh) — high risk".into());
    }
    if c.contains("base64 -d") || c.contains("base64 --decode") {
        w.push("decodes and runs base64 — hidden payload?".into());
    }
    if c.contains("rm -rf /") {
        w.push("contains a destructive delete (rm -rf /)".into());
    }
    if content.contains("http://") {
        w.push("uses an insecure http:// URL".into());
    }
    for host in ["github.com", "gist.", "pastebin", "raw.githubusercontent"] {
        if c.contains(host) {
            w.push(format!("links to {host} — review the source"));
            break;
        }
    }
    if seg.applies_to.trim().is_empty() {
        w.push("untagged segment (fits anything) — make sure it belongs here".into());
    }
    w
}

// ---- tiny serde_json path get/set helpers ---------------------------------

/// Read a string at `path` (e.g. `["system","kernel"]`); "" if absent.
fn jstr(v: &serde_json::Value, path: &[&str]) -> String {
    let mut cur = v;
    for k in path {
        match cur.get(k) {
            Some(x) => cur = x,
            None => return String::new(),
        }
    }
    cur.as_str().unwrap_or("").to_string()
}

fn jbool(v: &serde_json::Value, path: &[&str]) -> bool {
    let mut cur = v;
    for k in path {
        match cur.get(k) {
            Some(x) => cur = x,
            None => return false,
        }
    }
    cur.as_bool().unwrap_or(false)
}

/// Set (or, when `val` is empty, remove) a string at `path`, creating any
/// intermediate objects.
fn jset_str(v: &mut serde_json::Value, path: &[&str], val: &str) {
    let (last, parents) = path.split_last().expect("non-empty path");
    let mut cur = v;
    for k in parents {
        if !cur.get(k).map(|x| x.is_object()).unwrap_or(false) {
            cur[*k] = serde_json::json!({});
        }
        cur = cur.get_mut(k).unwrap();
    }
    if val.trim().is_empty() {
        if let Some(o) = cur.as_object_mut() {
            o.remove(*last);
        }
    } else {
        cur[*last] = serde_json::Value::String(val.to_string());
    }
}

fn jset_bool(v: &mut serde_json::Value, path: &[&str], val: bool) {
    let (last, parents) = path.split_last().expect("non-empty path");
    let mut cur = v;
    for k in parents {
        if !cur.get(k).map(|x| x.is_object()).unwrap_or(false) {
            cur[*k] = serde_json::json!({});
        }
        cur = cur.get_mut(k).unwrap();
    }
    cur[*last] = serde_json::Value::Bool(val);
}

/// Wallpaper source, whether stored as a bare string or `{ "source": … }`.
fn wallpaper_src(v: &serde_json::Value) -> String {
    match v.get("wallpaper") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(o) => o.get("source").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        None => String::new(),
    }
}

fn one_line(s: &str, n: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= n {
        flat
    } else {
        format!("{}…", flat.chars().take(n).collect::<String>())
    }
}
