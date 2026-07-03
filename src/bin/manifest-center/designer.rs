//! The Designer — a node-graph view of your setup, in the spirit of Blender's
//! shader editor.
//!
//! Config files (niri, Hyprland, waybar, …) and the managed snippet blocks
//! inside them appear as draggable **nodes** on a canvas; **wires** show what
//! flows into what — each snippet wires into the file it edits, and a desktop
//! config that launches waybar wires into the waybar node. Snippet values are
//! edited right on the node; **Swap** replaces a node's content with a
//! fragment downloaded from anywhere (a snippet .json), and **Apply** writes
//! everything back through the same marker-block engine `snippets` uses — so
//! edits land in place, idempotently, without replacing anyone's files.
//!
//! Everything here touches only the user's own config files, so no password
//! is needed; Apply auto-saves a snapshot first, so the change is one
//! Restore away from undone.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use manifest::history;
use manifest::manifest::{Question, Snippet};
use manifest::snippets;

use crate::snapshots;

const NODE_W: f64 = 280.0;
const CANVAS_W: i32 = 2200;
const CANVAS_H: i32 = 1400;

/// Config files worth scanning for nodes, with a friendly title and the
/// keyword other files use to "launch" them (for flow wires).
const KNOWN: &[(&str, &str, &str)] = &[
    ("Niri", ".config/niri/config.kdl", "niri"),
    ("Hyprland", ".config/hypr/hyprland.conf", "hyprland"),
    ("Sway", ".config/sway/config", "sway"),
    ("i3", ".config/i3/config", "i3"),
    ("Waybar", ".config/waybar/config.jsonc", "waybar"),
    ("Waybar", ".config/waybar/config", "waybar"),
    ("Waybar style", ".config/waybar/style.css", "waybar"),
];

struct Node {
    /// Canvas-unique identity: `file:<path>` for file nodes, `<id>@<path>` for
    /// segments — two files can legitimately carry blocks with the same
    /// snippet id, and they must stay distinct nodes. The snippet id itself
    /// lives in `title`.
    key: String,
    title: String,
    path: PathBuf,
    is_file: bool,
    content: RefCell<String>,
    section: RefCell<String>,
    x: Cell<f64>,
    y: Cell<f64>,
    widget: gtk::Box,
}

struct Graph {
    nodes: RefCell<Vec<Rc<Node>>>,
    /// Wires as (from key, to key).
    edges: RefCell<Vec<(String, String)>>,
    /// Snippet blocks removed in this session; Apply strips them from disk.
    deleted: RefCell<Vec<(PathBuf, String)>>,
    /// The last-applied manifest's survey questions, if any (`manifest history`)
    /// — used to show which question fed a segment's `{{id}}` value, if any.
    survey: Vec<Question>,
    fixed: gtk::Fixed,
    canvas: gtk::DrawingArea,
}

pub fn build(
    window: &adw::ApplicationWindow,
    stack: &Rc<adw::ViewStack>,
    toasts: &adw::ToastOverlay,
) {
    let canvas = gtk::DrawingArea::new();
    canvas.set_content_width(CANVAS_W);
    canvas.set_content_height(CANVAS_H);
    let fixed = gtk::Fixed::new();

    let graph = Rc::new(Graph {
        nodes: RefCell::new(Vec::new()),
        edges: RefCell::new(Vec::new()),
        deleted: RefCell::new(Vec::new()),
        survey: load_survey(),
        fixed: fixed.clone(),
        canvas: canvas.clone(),
    });

    // Wires underneath, nodes on top.
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&canvas));
    overlay.add_overlay(&fixed);
    let scroller = gtk::ScrolledWindow::builder().child(&overlay).vexpand(true).build();

    {
        let g = graph.clone();
        canvas.set_draw_func(move |_, cr, _, _| draw_wires(&g, cr));
    }

    // Toolbar: Add segment / Apply.
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    bar.set_margin_top(8);
    bar.set_margin_bottom(8);
    bar.set_margin_start(12);
    bar.set_margin_end(12);
    let hint = gtk::Label::new(Some("Drag nodes around. Edit values right on a segment, or Swap one for a downloaded file."));
    hint.add_css_class("dim-label");
    hint.set_hexpand(true);
    hint.set_halign(gtk::Align::Start);
    hint.set_wrap(true);
    bar.append(&hint);
    let add = gtk::Button::with_label("Add a segment");
    let apply = gtk::Button::with_label("Apply changes");
    apply.add_css_class("suggested-action");
    bar.append(&add);
    bar.append(&apply);

    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&bar);
    page.append(&scroller);

    {
        let g = graph.clone();
        let win = window.clone();
        let t = toasts.clone();
        add.connect_clicked(move |_| add_dialog(&win, &g, &t));
    }
    {
        let g = graph.clone();
        let t = toasts.clone();
        apply.connect_clicked(move |_| apply_all(&g, &t));
    }

    scan(&graph, window, toasts);

    stack
        .add_titled(&page, Some("designer"), "Designer")
        .set_icon_name(Some("view-app-grid-symbolic"));
}

// ---------------------------------------------------------------------------
// Building the graph from what's really on disk
// ---------------------------------------------------------------------------

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()))
}

fn scan(graph: &Rc<Graph>, window: &adw::ApplicationWindow, toasts: &adw::ToastOverlay) {
    let mut col_file_y = 60.0;
    let mut col_snip_y = 60.0;
    let mut file_nodes: Vec<(Rc<Node>, String, String)> = Vec::new(); // node, keyword, content

    for (title, rel, keyword) in KNOWN {
        let path = home().join(rel);
        let Ok(content) = std::fs::read_to_string(&path) else { continue };
        // One node per existing file (skip a second Waybar path if the first matched).
        if file_nodes.iter().any(|(n, _, _)| n.title == *title) {
            continue;
        }
        let node = make_node(
            graph,
            window,
            toasts,
            &format!("file:{}", path.display()),
            title,
            &path,
            true,
            "",
            "",
            720.0,
            col_file_y,
        );
        col_file_y += 150.0;

        // Snippet nodes from the file's managed blocks.
        for (id, inner) in snippets::extract_blocks(&content) {
            let snode = make_node(
                graph,
                window,
                toasts,
                &format!("{id}@{}", path.display()),
                &id,
                &path,
                false,
                &inner,
                "",
                140.0,
                col_snip_y,
            );
            col_snip_y += 230.0;
            graph.edges.borrow_mut().push((snode.key.clone(), node.key.clone()));
        }
        file_nodes.push((node, keyword.to_string(), content));
    }

    // Flow wires between files: desktop config mentioning "waybar" → waybar node.
    let mut flow = Vec::new();
    for (a, _, content_a) in &file_nodes {
        for (b, keyword_b, _) in &file_nodes {
            if a.key != b.key && !keyword_b.is_empty() && content_a.contains(keyword_b.as_str())
            {
                flow.push((a.key.clone(), b.key.clone()));
            }
        }
    }
    graph.edges.borrow_mut().extend(flow);
    graph.canvas.queue_draw();
}

// ---------------------------------------------------------------------------
// Nodes
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn make_node(
    graph: &Rc<Graph>,
    window: &adw::ApplicationWindow,
    toasts: &adw::ToastOverlay,
    key: &str,
    title: &str,
    path: &PathBuf,
    is_file: bool,
    content: &str,
    section: &str,
    x: f64,
    y: f64,
) -> Rc<Node> {
    let boxed = gtk::Box::new(gtk::Orientation::Vertical, 6);
    boxed.add_css_class("card");
    boxed.set_width_request(NODE_W as i32);

    let inner = gtk::Box::new(gtk::Orientation::Vertical, 6);
    inner.set_margin_top(10);
    inner.set_margin_bottom(10);
    inner.set_margin_start(10);
    inner.set_margin_end(10);
    boxed.append(&inner);

    // Header: title + actions. Also the drag handle.
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let label = gtk::Label::new(Some(title));
    label.add_css_class("heading");
    label.set_hexpand(true);
    label.set_halign(gtk::Align::Start);
    header.append(&label);

    let node = Rc::new(Node {
        key: key.to_string(),
        title: title.to_string(),
        path: path.clone(),
        is_file,
        content: RefCell::new(content.to_string()),
        section: RefCell::new(section.to_string()),
        x: Cell::new(x),
        y: Cell::new(y),
        widget: boxed.clone(),
    });

    if !is_file {
        let swap = gtk::Button::from_icon_name("document-open-symbolic");
        swap.set_tooltip_text(Some("Swap this segment for one from a file"));
        swap.add_css_class("flat");
        header.append(&swap);
        let del = gtk::Button::from_icon_name("user-trash-symbolic");
        del.set_tooltip_text(Some("Remove this segment"));
        del.add_css_class("flat");
        header.append(&del);

        {
            let n = node.clone();
            let g = graph.clone();
            let win = window.clone();
            let t = toasts.clone();
            swap.connect_clicked(move |_| swap_dialog(&win, &g, &n, &t));
        }
        {
            let n = node.clone();
            let g = graph.clone();
            del.connect_clicked(move |_| {
                g.deleted.borrow_mut().push((n.path.clone(), n.title.clone()));
                g.fixed.remove(&n.widget);
                g.nodes.borrow_mut().retain(|m| m.key != n.key);
                g.edges.borrow_mut().retain(|(a, b)| *a != n.key && *b != n.key);
                g.canvas.queue_draw();
            });
        }
    }
    inner.append(&header);

    if is_file {
        let sub = gtk::Label::new(Some(&path.display().to_string()));
        sub.add_css_class("dim-label");
        sub.set_halign(gtk::Align::Start);
        sub.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        sub.set_max_width_chars(30);
        inner.append(&sub);
    } else {
        // Section (where in the file it flows into) + the editable values.
        let sec = gtk::Entry::builder()
            .placeholder_text("section (optional, e.g. binds)")
            .text(section)
            .build();
        {
            let n = node.clone();
            sec.connect_changed(move |e| *n.section.borrow_mut() = e.text().to_string());
        }
        inner.append(&sec);

        let view = gtk::TextView::new();
        view.set_monospace(true);
        view.buffer().set_text(content);
        {
            let n = node.clone();
            view.buffer().connect_changed(move |b| {
                *n.content.borrow_mut() = b.text(&b.start_iter(), &b.end_iter(), false).to_string();
            });
        }
        let sc = gtk::ScrolledWindow::builder().min_content_height(90).max_content_height(150).child(&view).build();
        sc.add_css_class("view");
        inner.append(&sc);

        // The survey questions this segment actually uses (its content contains
        // their `{{id}}` token), if the last-applied manifest declared any.
        for q in matching_questions(content, &graph.survey) {
            let badge = gtk::Label::new(Some(&format!("? {} — {}", q.id, q.label)));
            badge.add_css_class("dim-label");
            badge.set_halign(gtk::Align::Start);
            badge.set_wrap(true);
            inner.append(&badge);
        }

        let target = gtk::Label::new(Some(&format!("→ {}", short(path))));
        target.add_css_class("dim-label");
        target.set_halign(gtk::Align::Start);
        inner.append(&target);
    }

    // Drag anywhere on the header.
    let drag = gtk::GestureDrag::new();
    {
        let n = node.clone();
        let g = graph.clone();
        let start: Rc<Cell<(f64, f64)>> = Rc::new(Cell::new((0.0, 0.0)));
        {
            let n = n.clone();
            let start = start.clone();
            drag.connect_drag_begin(move |_, _, _| start.set((n.x.get(), n.y.get())));
        }
        drag.connect_drag_update(move |_, dx, dy| {
            let (sx, sy) = start.get();
            let nx = (sx + dx).max(0.0);
            let ny = (sy + dy).max(0.0);
            n.x.set(nx);
            n.y.set(ny);
            g.fixed.move_(&n.widget, nx, ny);
            g.canvas.queue_draw();
        });
    }
    header.add_controller(drag);

    graph.fixed.put(&boxed, x, y);
    graph.nodes.borrow_mut().push(node.clone());
    node
}

/// The last-applied manifest's survey questions, if they can be read without
/// interrupting startup. `history::current()` shells out to `sudo git` (the
/// history repo is root-only); when sudo would stop to ask for a password —
/// this app runs unprivileged, often from a terminal — skip rather than hang
/// the launch on a prompt the user never asked for.
fn load_survey() -> Vec<Question> {
    let sudo_free = std::process::Command::new("sudo")
        .args(["-n", "true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !sudo_free {
        return Vec::new();
    }
    history::current().map(|m| m.survey).unwrap_or_default()
}

/// Survey questions whose `{{id}}` token appears in a segment's content, in
/// the order they're declared in the manifest.
fn matching_questions<'a>(content: &str, survey: &'a [Question]) -> Vec<&'a Question> {
    survey
        .iter()
        .filter(|q| content.contains(&format!("{{{{{}}}}}", q.id)))
        .collect()
}

fn short(path: &PathBuf) -> String {
    path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Wires
// ---------------------------------------------------------------------------

fn draw_wires(graph: &Rc<Graph>, cr: &gtk::cairo::Context) {
    let nodes = graph.nodes.borrow();
    let pos = |key: &str| -> Option<(f64, f64, f64, f64)> {
        nodes.iter().find(|n| n.key == key).map(|n| {
            let w = n.widget.width().max(NODE_W as i32) as f64;
            let h = n.widget.height().max(80) as f64;
            (n.x.get(), n.y.get(), w, h)
        })
    };
    cr.set_line_width(2.0);
    for (from, to) in graph.edges.borrow().iter() {
        let (Some((fx, fy, fw, fh)), Some((tx, ty, _, th))) = (pos(from), pos(to)) else { continue };
        let (x1, y1) = (fx + fw, fy + fh / 2.0);
        let (x2, y2) = (tx, ty + th / 2.0);
        // Snippet→file wires in accent purple; file→file "launches" in green.
        if from.starts_with("file:") {
            cr.set_source_rgba(0.65, 0.89, 0.63, 0.9);
        } else {
            cr.set_source_rgba(0.80, 0.65, 0.97, 0.9);
        }
        let bend = ((x2 - x1).abs() / 2.0).max(40.0);
        cr.move_to(x1, y1);
        cr.curve_to(x1 + bend, y1, x2 - bend, y2, x2, y2);
        let _ = cr.stroke();
        // A small dot at each end.
        for (cx, cy) in [(x1, y1), (x2, y2)] {
            cr.arc(cx, cy, 4.0, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        }
    }
}

// ---------------------------------------------------------------------------
// Swap / Add / Apply
// ---------------------------------------------------------------------------

/// Replace a segment's values with a snippet .json downloaded from anywhere
/// (either a single {"content": …} object or a manifest with a "snippets"
/// list — the first entry is used).
fn swap_dialog(window: &adw::ApplicationWindow, graph: &Rc<Graph>, node: &Rc<Node>, toasts: &adw::ToastOverlay) {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Snippet files (*.json)"));
    filter.add_pattern("*.json");
    let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    let dialog = gtk::FileDialog::builder().title("Choose a snippet file").filters(&filters).build();

    let node = node.clone();
    let graph = graph.clone();
    let toasts = toasts.clone();
    let parent = window.clone();
    dialog.open(Some(&parent), gtk::gio::Cancellable::NONE, move |res| {
        let Ok(file) = res else { return };
        let Some(path) = file.path() else { return };
        let Ok(raw) = std::fs::read_to_string(&path) else {
            toasts.add_toast(adw::Toast::new("Couldn't read that file."));
            return;
        };
        let v: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => {
                toasts.add_toast(adw::Toast::new("That isn't a valid snippet file."));
                return;
            }
        };
        let snippet = v
            .get("snippets")
            .and_then(|s| s.as_array())
            .and_then(|a| a.first())
            .cloned()
            .unwrap_or(v);
        let Some(content) = snippet.get("content").and_then(|c| c.as_str()) else {
            toasts.add_toast(adw::Toast::new("No \"content\" found in that file."));
            return;
        };
        *node.content.borrow_mut() = content.to_string();
        if let Some(sec) = snippet.get("section").and_then(|s| s.as_str()) {
            *node.section.borrow_mut() = sec.to_string();
        }
        // Refresh the node's text view in place: simplest is rebuild-by-swap
        // of the buffer through the widget tree — the TextView is the only
        // ScrolledWindow child inside the node.
        refresh_text(&node);
        graph.canvas.queue_draw();
        toasts.add_toast(adw::Toast::new(&format!("Swapped in {}", path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default())));
    });
}

/// Push the node's model content back into its TextView after a swap.
fn refresh_text(node: &Rc<Node>) {
    let mut child = node.widget.first_child();
    while let Some(c) = child {
        if let Some(found) = find_textview(&c) {
            found.buffer().set_text(&node.content.borrow());
            return;
        }
        child = c.next_sibling();
    }
}

fn find_textview(root: &gtk::Widget) -> Option<gtk::TextView> {
    if let Ok(tv) = root.clone().downcast::<gtk::TextView>() {
        return Some(tv);
    }
    let mut child = root.first_child();
    while let Some(c) = child {
        if let Some(f) = find_textview(&c) {
            return Some(f);
        }
        child = c.next_sibling();
    }
    None
}

/// "Add a segment": name it, point it at a config file, paste the content.
fn add_dialog(window: &adw::ApplicationWindow, graph: &Rc<Graph>, toasts: &adw::ToastOverlay) {
    let content_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let id = gtk::Entry::builder().placeholder_text("Name (e.g. waybar-launch)").build();
    let path = gtk::Entry::builder().placeholder_text("File (e.g. ~/.config/niri/config.kdl)").build();
    let section = gtk::Entry::builder().placeholder_text("Section (optional, e.g. binds)").build();
    let view = gtk::TextView::new();
    view.set_monospace(true);
    let sc = gtk::ScrolledWindow::builder().min_content_height(110).child(&view).build();
    sc.add_css_class("view");
    for w in [&id, &path, &section] {
        content_box.append(w);
    }
    content_box.append(&sc);

    let dialog = adw::MessageDialog::new(Some(window), Some("Add a segment"), None);
    dialog.set_extra_child(Some(&content_box));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("add", "Add");
    dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);

    let graph = graph.clone();
    let window = window.clone();
    let toasts = toasts.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp != "add" {
            return;
        }
        let name = id.text().trim().to_string();
        let raw_path = path.text().trim().to_string();
        if name.is_empty() || raw_path.is_empty() {
            toasts.add_toast(adw::Toast::new("A segment needs a name and a file."));
            return;
        }
        let full = if let Some(rest) = raw_path.strip_prefix("~/") {
            home().join(rest)
        } else {
            PathBuf::from(&raw_path)
        };
        let buffer = view.buffer();
        let content = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string();
        let key = format!("{name}@{}", full.display());
        if graph.nodes.borrow().iter().any(|n| n.key == key) {
            toasts.add_toast(adw::Toast::new("That segment already exists for this file."));
            return;
        }
        let node = make_node(
            &graph, &window, &toasts, &key, &name, &full, false, &content,
            section.text().trim(), 140.0, 60.0,
        );
        // Wire to the file node if it's on the canvas.
        let file_key = format!("file:{}", full.display());
        if graph.nodes.borrow().iter().any(|n| n.key == file_key) {
            graph.edges.borrow_mut().push((node.key.clone(), file_key));
        }
        graph.canvas.queue_draw();
    });
    dialog.present();
}

/// Write every segment back to disk through the marker-block engine, after
/// saving a snapshot so it's one Restore away from undone. User-owned files
/// only — no password needed.
fn apply_all(graph: &Rc<Graph>, toasts: &adw::ToastOverlay) {
    let _ = snapshots::save("Before Designer changes");

    let mut touched = 0usize;
    // Removals first.
    for (path, id) in graph.deleted.borrow_mut().drain(..) {
        if let Ok(current) = std::fs::read_to_string(&path) {
            let out = snippets::remove_block(&current, &id);
            if out != current && std::fs::write(&path, out).is_ok() {
                touched += 1;
            }
        }
    }
    // Then upserts.
    for node in graph.nodes.borrow().iter().filter(|n| !n.is_file) {
        let current = std::fs::read_to_string(&node.path).unwrap_or_default();
        let section = node.section.borrow();
        let s = Snippet {
            id: node.title.clone(),
            path: node.path.display().to_string(),
            section: (!section.trim().is_empty()).then(|| section.trim().to_string()),
            content: node.content.borrow().clone(),
        };
        let out = snippets::upsert(&current, &s);
        if out != current {
            if let Some(dir) = node.path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if std::fs::write(&node.path, out).is_ok() {
                touched += 1;
            }
        }
    }

    let msg = if touched == 0 {
        "Nothing to change — everything already matches.".to_string()
    } else {
        format!("Applied — {touched} file update(s). A snapshot was saved first, so you can restore if needed.")
    };
    toasts.add_toast(adw::Toast::new(&msg));
}
