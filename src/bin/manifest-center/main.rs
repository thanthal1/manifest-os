//! System Snapshots — the friendly desktop app for Manifest OS.
//!
//! A non-technical front-end over the manifest lifecycle: it lets anyone save
//! a restore point of their setup, go back to an earlier one, and apply a
//! setup someone shared — the words "manifest", "sync", "diff" and "git" never
//! appear. Underneath it's [`manifest::export`] (capture), [`manifest::diff`]
//! (preview) and `manifest sync` (apply, via `pkexec` so the user is prompted
//! for their password only when the system actually changes).
//!
//! Saving and browsing snapshots need no privileges (see
//! [`snapshots`] — a user-owned git repo in `~/.local/share`); only restoring
//! or applying does. Built with GTK4 + libadwaita; only compiles with
//! `--features gui`.

mod designer;
mod settings;
mod snapshots;
mod updates;

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

use manifest::diff::{self, ChangeKind};
use manifest::export;
use manifest::manifest::Manifest;

const APP_ID: &str = "os.manifest.Snapshots";

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &adw::Application) {
    // Use the installed "{ }" icon for the window + taskbar (matches app-id).
    gtk::Window::set_default_icon_name(APP_ID);

    let toasts = adw::ToastOverlay::new();

    let stack = adw::ViewStack::new();
    let stack = std::rc::Rc::new(stack);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(720)
        .default_height(620)
        .title("System Snapshots")
        .build();

    // Snapshots page first, so we can hand its refresh closure to Home's button.
    let (snap_page, refresh_snaps) = build_snapshots(&window, &stack, &toasts);
    build_home(&stack, &toasts, refresh_snaps.clone());
    stack.add_titled(&snap_page, Some("snapshots"), "Snapshots")
        .set_icon_name(Some("document-open-recent-symbolic"));
    build_apply(&window, &stack, &toasts);
    build_updates(&window, &stack, &toasts);
    settings::build(&window, &stack, &toasts);
    designer::build(&window, &stack, &toasts);

    let switcher = adw::ViewSwitcher::builder()
        .stack(stack.as_ref())
        .policy(adw::ViewSwitcherPolicy::Wide)
        .build();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&switcher));

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(stack.as_ref()));

    toasts.set_child(Some(&toolbar));
    window.set_content(Some(&toasts));
    window.present();
}

// ---------------------------------------------------------------------------
// Home — current setup at a glance + one-tap save
// ---------------------------------------------------------------------------

fn build_home(
    stack: &std::rc::Rc<adw::ViewStack>,
    toasts: &adw::ToastOverlay,
    refresh_snaps: std::rc::Rc<dyn Fn()>,
) {
    let page = adw::PreferencesPage::new();

    // What your system looks like right now.
    let now = export::capture_manifest();
    let group = adw::PreferencesGroup::builder().title("Your current setup").build();

    let desktop = now.desktop.clone().unwrap_or_else(|| "—".into());
    group.add(&info_row("Desktop", &pretty(&desktop)));
    let theme = now
        .theme
        .as_ref()
        .and_then(|t| t.gtk.clone())
        .unwrap_or_else(|| "System default".into());
    group.add(&info_row("Theme", &theme));
    group.add(&info_row("Installed apps", &format!("{}", now.packages.len())));
    page.add(&group);

    // The one big action.
    let save = gtk::Button::builder()
        .label("Save a snapshot")
        .halign(gtk::Align::Start)
        .build();
    save.add_css_class("suggested-action");
    save.add_css_class("pill");
    {
        let window_toasts = toasts.clone();
        let refresh = refresh_snaps.clone();
        save.connect_clicked(move |btn| {
            let win = btn.root().and_downcast::<gtk::Window>();
            prompt_save(win.as_ref(), &window_toasts, refresh.clone());
        });
    }
    // Titled like the discarded save_group was meant to be, so the button has
    // context ("what's a snapshot?") for first-time users.
    let holder = adw::PreferencesGroup::builder()
        .title("Snapshots")
        .description("A snapshot remembers your whole setup, so you can go back to it later — before trying something new, for peace of mind.")
        .build();
    holder.add(&wrap(&save));
    page.add(&holder);

    stack.add_titled(&page, Some("home"), "Home")
        .set_icon_name(Some("go-home-symbolic"));
}

/// Ask for an optional snapshot name, then save. No privileges required.
fn prompt_save(
    parent: Option<&gtk::Window>,
    toasts: &adw::ToastOverlay,
    refresh_snaps: std::rc::Rc<dyn Fn()>,
) {
    let entry = gtk::Entry::builder()
        .placeholder_text("e.g. Before installing new apps")
        .build();
    let dialog = adw::MessageDialog::new(
        parent,
        Some("Save a snapshot"),
        Some("Give it a name so it's easy to find later (optional)."),
    );
    dialog.set_extra_child(Some(&entry));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("save", "Save");
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("save"));

    let toasts = toasts.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp != "save" {
            return;
        }
        match snapshots::save(&entry.text()) {
            Ok(()) => {
                toasts.add_toast(adw::Toast::new("Snapshot saved"));
                refresh_snaps();
            }
            Err(e) => toasts.add_toast(adw::Toast::new(&format!("Couldn't save: {e}"))),
        }
    });
    dialog.present();
}

// ---------------------------------------------------------------------------
// Snapshots — the list + restore
// ---------------------------------------------------------------------------

fn build_snapshots(
    window: &adw::ApplicationWindow,
    stack: &std::rc::Rc<adw::ViewStack>,
    toasts: &adw::ToastOverlay,
) -> (gtk::Widget, std::rc::Rc<dyn Fn()>) {
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder()
        .title("Saved snapshots")
        .description("Restore any of these to return your setup to how it was. You'll see exactly what changes first.")
        .build();
    page.add(&group);

    // A refresh closure the Home page and post-action handlers can call.
    let group_ref = group.clone();
    let window = window.clone();
    let stack = stack.clone();
    let toasts = toasts.clone();
    let rows: std::rc::Rc<std::cell::RefCell<Vec<gtk::Widget>>> =
        std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));

    let refresh: std::rc::Rc<dyn Fn()> = std::rc::Rc::new(move || {
        // Clear previous rows.
        for r in rows.borrow_mut().drain(..) {
            group_ref.remove(&r);
        }
        let snaps = snapshots::list();
        if snaps.is_empty() {
            let row = adw::ActionRow::builder()
                .title("No snapshots yet")
                .subtitle("Save one from the Home tab to create your first restore point.")
                .build();
            group_ref.add(&row);
            rows.borrow_mut().push(row.upcast());
            return;
        }
        for s in snaps {
            let row = adw::ActionRow::builder()
                .title(if s.label.is_empty() { "Snapshot".to_string() } else { s.label.clone() })
                .subtitle(&s.date)
                .build();
            let restore = gtk::Button::builder()
                .label("Restore")
                .valign(gtk::Align::Center)
                .build();
            restore.add_css_class("flat");
            let id = s.id.clone();
            let win = window.clone();
            let stack2 = stack.clone();
            let toasts2 = toasts.clone();
            restore.connect_clicked(move |_| {
                confirm_restore(&win, &stack2, &toasts2, &id);
            });
            row.add_suffix(&restore);
            row.set_activatable_widget(Some(&restore));
            group_ref.add(&row);
            rows.borrow_mut().push(row.upcast());
        }
    });
    refresh();

    (page.upcast(), refresh)
}

/// Show what restoring would change, then (on confirm) apply it with a
/// password prompt.
fn confirm_restore(
    window: &adw::ApplicationWindow,
    stack: &std::rc::Rc<adw::ViewStack>,
    toasts: &adw::ToastOverlay,
    id: &str,
) {
    let json = match snapshots::json_at(id) {
        Ok(j) => j,
        Err(e) => {
            toasts.add_toast(adw::Toast::new(&format!("Couldn't read snapshot: {e}")));
            return;
        }
    };
    let target = match Manifest::from_str(&json) {
        Ok(m) => m,
        Err(e) => {
            toasts.add_toast(adw::Toast::new(&format!("Snapshot is unreadable: {e}")));
            return;
        }
    };
    let changes = diff::compute(&target, Some(&export::capture_manifest()));

    let dialog = adw::MessageDialog::new(
        Some(window),
        Some("Restore this snapshot?"),
        Some("Your setup will change back to match this snapshot. Here's what's different right now:"),
    );
    dialog.set_extra_child(Some(&change_list(&changes)));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("restore", "Restore");
    dialog.set_response_appearance("restore", adw::ResponseAppearance::Destructive);

    // A temp file `manifest sync` can read — written only on confirm, and a
    // failed write surfaces here instead of as a confusing pkexec error.
    let path = std::env::temp_dir().join(format!("manifest-restore-{id}.json"));
    let path_str = path.to_string_lossy().to_string();

    let window = window.clone();
    let stack = stack.clone();
    let toasts = toasts.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp != "restore" {
            return;
        }
        if let Err(e) = std::fs::write(&path, &json) {
            toasts.add_toast(adw::Toast::new(&format!("Couldn't prepare the restore: {e}")));
            return;
        }
        run_privileged(
            &window,
            &stack,
            &toasts,
            "Restoring your snapshot",
            vec!["sync".into(), path_str.clone()],
        );
    });
    dialog.present();
}

// ---------------------------------------------------------------------------
// Updates — package-version history + the "hold versions" switch
// ---------------------------------------------------------------------------

fn build_updates(
    window: &adw::ApplicationWindow,
    stack: &std::rc::Rc<adw::ViewStack>,
    toasts: &adw::ToastOverlay,
) {
    let page = adw::PreferencesPage::new();

    // The pin switch: newest-by-default is the secure default, so this is off
    // unless the user chose stability. Flipping it edits pacman.conf (root).
    let pin_group = adw::PreferencesGroup::builder()
        .title("Updates")
        .description("Normally your apps update to the newest version (best for security). Turn this on to hold everything at its current version instead.")
        .build();
    let pin = adw::SwitchRow::builder()
        .title("Hold current versions")
        .subtitle("Skip updates until you turn this back off")
        .active(updates::pinned())
        .build();
    {
        let window = window.clone();
        let stack = stack.clone();
        let toasts = toasts.clone();
        pin.connect_active_notify(move |row| {
            let state = if row.is_active() { "on" } else { "off" };
            run_privileged(
                &window,
                &stack,
                &toasts,
                "Changing update setting",
                vec!["pin-versions".into(), state.into()],
            );
        });
    }
    pin_group.add(&pin);
    page.add(&pin_group);

    // The version-snapshot history — restore any past set of versions.
    let group = adw::PreferencesGroup::builder()
        .title("Version history")
        .description("Each time your packages change, the exact versions are saved here. If an update broke something, restore the set from before it.")
        .build();

    let snaps = updates::list();
    if snaps.is_empty() {
        group.add(
            &adw::ActionRow::builder()
                .title("No version history yet")
                .subtitle("It starts building the next time your packages change.")
                .build(),
        );
    } else {
        for s in snaps {
            let row = adw::ActionRow::builder()
                .title(if s.label.is_empty() { "Package change".to_string() } else { s.label.clone() })
                .subtitle(&s.date)
                .build();
            let restore = gtk::Button::builder().label("Restore").valign(gtk::Align::Center).build();
            restore.add_css_class("flat");
            let id = s.id.clone();
            let label = s.label.clone();
            let win = window.clone();
            let stack2 = stack.clone();
            let toasts2 = toasts.clone();
            restore.connect_clicked(move |_| {
                confirm_restore_versions(&win, &stack2, &toasts2, &id, &label);
            });
            row.add_suffix(&restore);
            row.set_activatable_widget(Some(&restore));
            group.add(&row);
        }
    }
    page.add(&group);

    stack.add_titled(&page, Some("updates"), "Updates")
        .set_icon_name(Some("software-update-available-symbolic"));
}

/// Confirm, then downgrade to a recorded version snapshot (via `pkexec`).
fn confirm_restore_versions(
    window: &adw::ApplicationWindow,
    stack: &std::rc::Rc<adw::ViewStack>,
    toasts: &adw::ToastOverlay,
    id: &str,
    label: &str,
) {
    let body = if label.is_empty() {
        "Your packages will be moved back to the exact versions from this point. Anything that can't be found in the cache stays as-is.".to_string()
    } else {
        format!("Your packages will be moved back to the versions from “{label}”. Anything that can't be found in the cache stays as-is.")
    };
    let dialog = adw::MessageDialog::new(
        Some(window),
        Some("Restore these versions?"),
        Some(&body),
    );
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("restore", "Restore");
    dialog.set_response_appearance("restore", adw::ResponseAppearance::Destructive);

    let window = window.clone();
    let stack = stack.clone();
    let toasts = toasts.clone();
    let id = id.to_string();
    dialog.connect_response(None, move |_, resp| {
        if resp != "restore" {
            return;
        }
        run_privileged(
            &window,
            &stack,
            &toasts,
            "Restoring package versions",
            vec!["restore-versions".into(), id.clone()],
        );
    });
    dialog.present();
}

// ---------------------------------------------------------------------------
// Apply — a setup someone shared
// ---------------------------------------------------------------------------

fn build_apply(
    window: &adw::ApplicationWindow,
    stack: &std::rc::Rc<adw::ViewStack>,
    toasts: &adw::ToastOverlay,
) {
    let status = adw::StatusPage::builder()
        .icon_name("document-send-symbolic")
        .title("Apply a shared setup")
        .description("Got a setup file from a friend or the web? Open it here to preview and apply it — desktop, theme, apps and all.")
        .build();
    let open = gtk::Button::builder().label("Open a setup file…").halign(gtk::Align::Center).build();
    open.add_css_class("suggested-action");
    open.add_css_class("pill");
    {
        let window = window.clone();
        let stack = stack.clone();
        let toasts = toasts.clone();
        open.connect_clicked(move |_| pick_and_preview(&window, &stack, &toasts));
    }
    status.set_child(Some(&open));

    stack.add_titled(&status, Some("apply"), "Apply a setup")
        .set_icon_name(Some("document-send-symbolic"));
}

fn pick_and_preview(
    window: &adw::ApplicationWindow,
    stack: &std::rc::Rc<adw::ViewStack>,
    toasts: &adw::ToastOverlay,
) {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Setup files (*.json)"));
    filter.add_pattern("*.json");
    let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);

    let dialog = gtk::FileDialog::builder().title("Choose a setup file").filters(&filters).build();
    let parent = window.clone();
    let window = window.clone();
    let stack = stack.clone();
    let toasts = toasts.clone();
    dialog.open(Some(&parent), gtk::gio::Cancellable::NONE, move |res| {
        let Ok(file) = res else { return };
        let Some(path) = file.path() else { return };
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => {
                toasts.add_toast(adw::Toast::new(&format!("Couldn't open file: {e}")));
                return;
            }
        };
        let target = match Manifest::from_str(&raw) {
            Ok(m) => m,
            Err(_) => {
                toasts.add_toast(adw::Toast::new("That doesn't look like a valid setup file."));
                return;
            }
        };
        let changes = diff::compute(&target, Some(&export::capture_manifest()));
        let dlg = adw::MessageDialog::new(
            Some(&window),
            Some("Apply this setup?"),
            Some("Here's what would change on your computer:"),
        );
        dlg.set_extra_child(Some(&change_list(&changes)));
        dlg.add_response("cancel", "Cancel");
        dlg.add_response("apply", "Apply");
        dlg.set_response_appearance("apply", adw::ResponseAppearance::Suggested);
        let path_str = path.to_string_lossy().to_string();
        let window2 = window.clone();
        let stack2 = stack.clone();
        let toasts2 = toasts.clone();
        dlg.connect_response(None, move |_, resp| {
            if resp != "apply" {
                return;
            }
            run_privileged(
                &window2,
                &stack2,
                &toasts2,
                "Applying the setup",
                vec!["sync".into(), path_str.clone()],
            );
        });
        dlg.present();
    });
}

// ---------------------------------------------------------------------------
// Running a privileged change (pkexec) with live progress
// ---------------------------------------------------------------------------

fn run_privileged(
    window: &adw::ApplicationWindow,
    stack: &std::rc::Rc<adw::ViewStack>,
    toasts: &adw::ToastOverlay,
    title: &str,
    args: Vec<String>,
) {
    let dialog = adw::Window::builder()
        .transient_for(window)
        .modal(true)
        .default_width(620)
        .default_height(460)
        .title(title)
        .build();

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::builder().show_end_title_buttons(false).build();
    outer.append(&header);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    body.set_margin_top(18);
    body.set_margin_bottom(18);
    body.set_margin_start(18);
    body.set_margin_end(18);

    let status = gtk::Label::new(Some("Working… you may be asked for your password."));
    status.set_halign(gtk::Align::Start);
    status.add_css_class("title-4");
    body.append(&status);

    let spinner = gtk::Spinner::new();
    spinner.start();
    spinner.set_halign(gtk::Align::Start);
    body.append(&spinner);

    let view = gtk::TextView::new();
    view.set_editable(false);
    view.set_monospace(true);
    view.add_css_class("card");
    let scroller = gtk::ScrolledWindow::builder().vexpand(true).min_content_height(240).child(&view).build();
    body.append(&scroller);

    let close = gtk::Button::with_label("Close");
    close.set_halign(gtk::Align::End);
    close.set_sensitive(false);
    {
        let dialog = dialog.clone();
        close.connect_clicked(move |_| dialog.close());
    }
    body.append(&close);
    outer.append(&body);
    dialog.set_content(Some(&outer));
    dialog.present();

    // (lines, Some(success)) shared with the worker thread.
    let shared: Arc<Mutex<(Vec<String>, Option<bool>)>> = Arc::new(Mutex::new((Vec::new(), None)));
    let worker = shared.clone();
    let bin = manifest_bin();
    std::thread::spawn(move || {
        let child = Command::new("pkexec")
            .arg(&bin)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                let mut g = worker.lock().unwrap();
                g.0.push(format!("Couldn't start: {e}"));
                g.1 = Some(false);
                return;
            }
        };
        // Read stdout and stderr concurrently into the shared buffer.
        let mut handles = Vec::new();
        for pipe in [child.stdout.take().map(Pipe::Out), child.stderr.take().map(Pipe::Err)] {
            if let Some(p) = pipe {
                let w = worker.clone();
                handles.push(std::thread::spawn(move || {
                    let reader: Box<dyn BufRead + Send> = match p {
                        Pipe::Out(o) => Box::new(BufReader::new(o)),
                        Pipe::Err(e) => Box::new(BufReader::new(e)),
                    };
                    for line in reader.lines().map_while(Result::ok) {
                        w.lock().unwrap().0.push(line);
                    }
                }));
            }
        }
        for h in handles {
            let _ = h.join();
        }
        let ok = child.wait().map(|s| s.success()).unwrap_or(false);
        worker.lock().unwrap().1 = Some(ok);
    });

    // Drain output into the view on the main loop.
    let buffer = view.buffer();
    let toasts = toasts.clone();
    let stack = stack.clone();
    let mut shown = 0usize;
    glib::timeout_add_local(Duration::from_millis(200), move || {
        // Copy only the lines that arrived since the last tick — cloning the
        // whole buffer every 200ms goes quadratic over a long install log.
        let (fresh, done) = {
            let g = shared.lock().unwrap();
            (g.0[shown..].to_vec(), g.1)
        };
        if !fresh.is_empty() {
            for l in &fresh {
                buffer.insert(&mut buffer.end_iter(), l);
                buffer.insert(&mut buffer.end_iter(), "\n");
            }
            shown += fresh.len();
            let mut end = buffer.end_iter();
            view.scroll_to_iter(&mut end, 0.0, true, 0.0, 1.0);
        }
        if let Some(ok) = done {
            spinner.stop();
            spinner.set_visible(false);
            close.set_sensitive(true);
            if ok {
                status.set_text("Done! Your setup has been updated.");
                toasts.add_toast(adw::Toast::new("Setup updated"));
                stack.set_visible_child_name("snapshots");
            } else {
                status.set_text("That didn't finish. Nothing was left half-applied that a retry can't fix.");
            }
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
}

enum Pipe {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// The `manifest` CLI next to this binary (installer stages both in
/// /usr/local/bin), falling back to the standard path.
fn manifest_bin() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("manifest")))
        .filter(|p| p.exists())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "/usr/local/bin/manifest".into())
}

fn info_row(title: &str, value: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    let label = gtk::Label::new(Some(value));
    label.add_css_class("dim-label");
    row.add_suffix(&label);
    row
}

/// A vertical list widget of changes, for the confirm dialogs.
fn change_list(changes: &[diff::Change]) -> gtk::Widget {
    if changes.is_empty() {
        let l = gtk::Label::new(Some("No changes — your system already matches this."));
        l.set_wrap(true);
        l.add_css_class("dim-label");
        return l.upcast();
    }
    let list = gtk::Box::new(gtk::Orientation::Vertical, 4);
    for c in changes {
        let (sign, css) = match c.kind {
            ChangeKind::Added => ("＋", "success"),
            ChangeKind::Removed => ("－", "error"),
            ChangeKind::Changed => ("→", "accent"),
        };
        let row = gtk::Label::new(Some(&format!("{sign}  {}: {}", c.category, c.detail)));
        row.set_halign(gtk::Align::Start);
        row.set_wrap(true);
        row.add_css_class(css);
        list.append(&row);
    }
    let scroller = gtk::ScrolledWindow::builder()
        .min_content_height(140)
        .max_content_height(320)
        .propagate_natural_height(true)
        .child(&list)
        .build();
    scroller.upcast()
}

fn wrap(w: &impl IsA<gtk::Widget>) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    b.append(w);
    b
}

/// Title-case a desktop key for display ("gnome" → "Gnome", "niri" → "Niri").
fn pretty(key: &str) -> String {
    let mut c = key.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => key.to_string(),
    }
}
