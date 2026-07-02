//! Manifest OS — the graphical installer.
//!
//! A friendly, plain-language front-end over the very same engine the CLI and
//! TUI use: it collects a [`probe::InstallPlan`] and hands it to
//! [`installer::execute`]. Built with GTK4 + libadwaita, it runs fullscreen in a
//! `cage` kiosk session on the live ISO.
//!
//! Two modes via a header toggle:
//!   * **Easy** — only the essentials: choose a setup, choose a disk, create an
//!     account. Everything else uses sensible defaults (whole-disk, ext4, zram).
//!   * **Advanced** — additionally exposes filesystem, swap, username and hostname.
//!
//! Long work (Wi-Fi connect, the install) runs on a worker thread; results are
//! delivered back to the GTK main loop by polling a shared slot, so the UI never
//! blocks. This binary only builds with `--features gui`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

use manifest::exec::Ctx;
use manifest::probe::{self, Account, ExtraUser, InstallPlan, StaticIp};
use manifest::installer;

const APP_ID: &str = "os.manifest.Installer";

/// Everything the wizard collects. Updated live as the user edits fields, so the
/// install step just reads it.
#[derive(Default)]
struct State {
    advanced: bool,
    manifest: String, // bundled name, local path, or URL
    answers: HashMap<String, String>, // answers to the manifest's survey questions
    disk: String,
    install_mode: String,       // "erase" or "alongside" (dual boot with Windows)
    alongside_gib: Option<u32>, // GiB to give Manifest OS when dual-booting
    filesystem: String,
    swap: String,
    swap_size_gib: Option<u32>,
    encrypt_mode: String, // "none" | "full" | "home"
    encrypt_passphrase: String,
    root_gib: Option<u32>,  // root size when encrypt_mode == "home"
    lvm: bool,
    raid1_disk: String, // "" = off, else the mirror disk's path
    timezone: String,
    locale: String,
    keymap: String,
    full_name: String,
    username: String,
    password: String,
    hostname: String,
    root_password: String,
    autologin: bool,
    install_nvidia: bool,
    install_printing: bool,
    extra_users_text: String, // one "name:password[:sudo]" per line
    post_install_script: String,
    static_ip: String,
    gateway: String,
    dns: String,
    proxy: String,
    vlan_id: String,
    vlan_parent: String,
    large_text: bool,
}

impl State {
    fn new() -> Self {
        State {
            install_mode: "erase".into(),
            filesystem: "ext4".into(),
            swap: "zram".into(),
            encrypt_mode: "none".into(),
            ..Default::default()
        }
    }
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run()
}

/// Run a blocking `job` on a worker thread; call `done` on the GTK main thread
/// with its result. Avoids freezing the UI during Wi-Fi connect / the install.
fn run_async<T, F, D>(job: F, done: D)
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
    D: Fn(T) + 'static,
{
    let slot: Arc<Mutex<Option<T>>> = Arc::new(Mutex::new(None));
    let worker = slot.clone();
    std::thread::spawn(move || {
        let result = job();
        *worker.lock().unwrap() = Some(result);
    });
    glib::timeout_add_local(Duration::from_millis(200), move || {
        if let Some(value) = slot.lock().unwrap().take() {
            done(value);
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

fn build_ui(app: &adw::Application) {
    let state = Rc::new(RefCell::new(State::new()));
    // Widgets that only appear in Advanced mode; the header toggle flips them.
    let advanced_widgets: Rc<RefCell<Vec<gtk::Widget>>> = Rc::new(RefCell::new(Vec::new()));
    // The current install attempt's cancellation flag — set fresh by
    // start_install each attempt, read by the Installing page's Cancel button.
    let cancel_flag: Rc<RefCell<Option<Arc<AtomicBool>>>> = Rc::new(RefCell::new(None));

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::SlideLeftRight)
        .build();
    let stack = Rc::new(stack);

    // Header bar with the Easy/Advanced toggle.
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&gtk::Label::new(Some("Install Manifest OS"))));
    let adv_toggle = gtk::ToggleButton::with_label("Advanced");
    {
        let state = state.clone();
        let adv = advanced_widgets.clone();
        adv_toggle.connect_toggled(move |b| {
            let on = b.is_active();
            state.borrow_mut().advanced = on;
            for w in adv.borrow().iter() {
                w.set_visible(on);
            }
        });
    }
    header.pack_end(&adv_toggle);

    // Pages.
    // The survey page's question area is rebuilt per chosen manifest, so its
    // container is shared between the setup page (which fills it) and the survey
    // page (which shows it).
    let survey_content = Rc::new(gtk::Box::new(gtk::Orientation::Vertical, 16));

    add_welcome(&stack, &state);
    add_network(&stack, &state, &advanced_widgets);
    add_setup(&stack, &state, &advanced_widgets, &survey_content);
    add_survey(&stack, &survey_content);
    add_disk(&stack, &state, &advanced_widgets);
    add_account(&stack, &state, &advanced_widgets);
    add_review(&stack, &state, &cancel_flag);
    add_installing(&stack, &advanced_widgets, &cancel_flag);
    add_done(&stack);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&header);
    root.append(stack.as_ref());

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(900)
        .default_height(650)
        .content(&root)
        .build();
    window.fullscreen();
    window.present();
}

// ---------------------------------------------------------------------------
// Page scaffolding
// ---------------------------------------------------------------------------

/// A centered, max-width column with a big title, a content area, and a bottom
/// button bar — the shape of every page.
fn page(title: &str, subtitle: &str) -> (gtk::Box, gtk::Box, gtk::Box) {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.set_vexpand(true);
    outer.set_hexpand(true);

    let clamp = adw::Clamp::builder().maximum_size(620).build();
    clamp.set_vexpand(true);
    let col = gtk::Box::new(gtk::Orientation::Vertical, 18);
    col.set_valign(gtk::Align::Center);
    col.set_margin_top(24);
    col.set_margin_bottom(24);
    col.set_margin_start(24);
    col.set_margin_end(24);

    let h = gtk::Label::new(None);
    h.set_markup(&format!("<span size='xx-large' weight='bold'>{}</span>", glib::markup_escape_text(title)));
    h.set_halign(gtk::Align::Start);
    col.append(&h);
    if !subtitle.is_empty() {
        let s = gtk::Label::new(Some(subtitle));
        s.set_halign(gtk::Align::Start);
        s.add_css_class("dim-label");
        s.set_wrap(true);
        col.append(&s);
    }

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_vexpand(true);
    col.append(&content);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    buttons.set_halign(gtk::Align::End);
    col.append(&buttons);

    clamp.set_child(Some(&col));
    outer.append(&clamp);
    (outer, content, buttons)
}

fn nav_button(label: &str, primary: bool) -> gtk::Button {
    let b = gtk::Button::with_label(label);
    if primary {
        b.add_css_class("suggested-action");
        b.add_css_class("pill");
    }
    b
}

/// Wire a button to jump to a named page.
fn goto(stack: &Rc<gtk::Stack>, name: &'static str) -> impl Fn(&gtk::Button) {
    let stack = stack.clone();
    move |_| stack.set_visible_child_name(name)
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

fn add_welcome(stack: &Rc<gtk::Stack>, state: &Rc<RefCell<State>>) {
    let (root, content, buttons) = page(
        "Welcome to Manifest OS",
        "We'll set up your computer in a few simple steps. It only takes a few minutes.",
    );

    // Larger text — a real, always-available accessibility option (not hidden
    // behind Advanced). A true "high contrast" theme isn't bundled on the live
    // ISO, so this is scoped to what we can honestly deliver.
    let large_text = switch_row("Larger text", {
        let state = state.clone();
        move |on| {
            state.borrow_mut().large_text = on;
            apply_large_text(on);
        }
    });
    content.append(&large_text);

    // Skip the wizard entirely with a saved configuration (a preseed
    // InstallPlan as JSON — see `manifest provision --config`).
    let preseed_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let preseed_path = gtk::Entry::builder().placeholder_text("Path to a saved configuration (.json)").hexpand(true).build();
    let preseed_status = gtk::Label::new(None);
    preseed_status.add_css_class("dim-label");
    let preseed_btn = gtk::Button::with_label("Load & review");
    preseed_row.append(&preseed_path);
    preseed_row.append(&preseed_btn);
    content.append(&preseed_row);
    content.append(&preseed_status);
    {
        let state = state.clone();
        let stack = stack.clone();
        let preseed_path = preseed_path.clone();
        let preseed_status = preseed_status.clone();
        preseed_btn.connect_clicked(move |_| {
            let path = preseed_path.text().to_string();
            if path.trim().is_empty() {
                return;
            }
            match std::fs::read_to_string(path.trim())
                .map_err(|e| e.to_string())
                .and_then(|raw| serde_json::from_str::<InstallPlan>(&raw).map_err(|e| e.to_string()))
            {
                Ok(plan) => {
                    load_preseed_into_state(&state, plan);
                    stack.set_visible_child_name("review");
                }
                Err(e) => preseed_status.set_text(&format!("Couldn't load that file: {e}")),
            }
        });
    }

    let start = nav_button("Get started", true);
    start.connect_clicked(goto(stack, "network"));
    buttons.append(&start);
    stack.add_named(&root, Some("welcome"));
}

/// Bump the default font size app-wide via a CSS provider — the "larger text"
/// accessibility toggle. Re-applying with `on = false` removes it.
fn apply_large_text(on: bool) {
    use gtk::gdk;
    thread_local! {
        static PROVIDER: gtk::CssProvider = gtk::CssProvider::new();
    }
    PROVIDER.with(|p| {
        let Some(display) = gdk::Display::default() else { return };
        gtk::style_context_remove_provider_for_display(&display, p);
        if on {
            p.load_from_data("label, entry, button, textview { font-size: 1.25em; }");
            gtk::style_context_add_provider_for_display(&display, p, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
    });
}

fn add_network(stack: &Rc<gtk::Stack>, state: &Rc<RefCell<State>>, adv: &Rc<RefCell<Vec<gtk::Widget>>>) {
    let (root, content, buttons) = page(
        "Internet connection",
        "Manifest OS downloads your software while it installs, so it needs to be online.",
    );

    let status = gtk::Label::new(None);
    status.set_halign(gtk::Align::Start);
    status.set_wrap(true);
    content.append(&status);

    // Wi-Fi controls (shown only when offline and a Wi-Fi adapter exists).
    let wifi_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let net_list = gtk::DropDown::from_strings(&[]);
    let pass = gtk::PasswordEntry::builder().show_peek_icon(true).build();
    pass.set_property("placeholder-text", "Wi-Fi password");
    let scan = gtk::Button::with_label("Scan for networks");
    let connect = gtk::Button::with_label("Connect");
    connect.add_css_class("suggested-action");
    wifi_box.append(&scan);
    wifi_box.append(&net_list);
    wifi_box.append(&pass);
    wifi_box.append(&connect);
    content.append(&wifi_box);

    // Advanced: static IP, an HTTP(S) proxy, and a VLAN — for networks that
    // need more than plug-in-and-go DHCP.
    let ip = text_field_row("Static IP (CIDR)", "e.g. 192.168.1.50/24 — leave blank for DHCP", {
        let state = state.clone();
        move |v| state.borrow_mut().static_ip = v
    });
    let gw = text_field_row("Gateway", "e.g. 192.168.1.1", {
        let state = state.clone();
        move |v| state.borrow_mut().gateway = v
    });
    let dns = text_field_row("DNS servers", "comma-separated, e.g. 1.1.1.1,8.8.8.8", {
        let state = state.clone();
        move |v| state.borrow_mut().dns = v
    });
    let proxy = text_field_row("HTTP(S) proxy", "e.g. http://10.0.0.1:3128", {
        let state = state.clone();
        move |v| state.borrow_mut().proxy = v
    });
    let vlan_id = text_field_row("VLAN ID", "e.g. 100 — leave blank for none", {
        let state = state.clone();
        move |v| state.borrow_mut().vlan_id = v
    });
    let vlan_parent = text_field_row("VLAN parent interface", "e.g. eth0", {
        let state = state.clone();
        move |v| state.borrow_mut().vlan_parent = v
    });
    for w in [&ip, &gw, &dns, &proxy, &vlan_id, &vlan_parent] {
        w.set_visible(false);
        content.append(w);
        adv.borrow_mut().push(w.clone().upcast());
    }

    let back = nav_button("Back", false);
    back.connect_clicked(goto(stack, "welcome"));
    let next = nav_button("Continue", true);
    next.connect_clicked(goto(stack, "setup"));
    buttons.append(&back);
    buttons.append(&next);

    // Reflect connectivity whenever this page is shown.
    let refresh = {
        let status = status.clone();
        let wifi_box = wifi_box.clone();
        let next = next.clone();
        Rc::new(move || {
            if probe::is_online() {
                status.set_markup("<span weight='bold'>✓ You're connected.</span>");
                wifi_box.set_visible(false);
                next.set_sensitive(true);
            } else if probe::wifi_device().is_some() {
                status.set_text("Not connected. Pick a Wi-Fi network below, or plug in Ethernet.");
                wifi_box.set_visible(true);
                next.set_sensitive(false);
            } else {
                status.set_text("Not connected. Plug in an Ethernet cable — it connects automatically — then press Continue.");
                wifi_box.set_visible(false);
                next.set_sensitive(true);
            }
        })
    };
    refresh();

    // Scan (threaded).
    {
        let net_list = net_list.clone();
        let scan_btn = scan.clone();
        scan.connect_clicked(move |_| {
            let Some(dev) = probe::wifi_device() else { return };
            scan_btn.set_label("Scanning…");
            scan_btn.set_sensitive(false);
            let net_list = net_list.clone();
            let scan_btn = scan_btn.clone();
            run_async(
                move || probe::scan_wifi(&dev),
                move |nets| {
                    let refs: Vec<&str> = nets.iter().map(|s| s.as_str()).collect();
                    net_list.set_model(Some(&gtk::StringList::new(&refs)));
                    scan_btn.set_label("Scan for networks");
                    scan_btn.set_sensitive(true);
                },
            );
        });
    }

    // Connect (threaded), then refresh connectivity.
    {
        let net_list = net_list.clone();
        let pass = pass.clone();
        let connect_btn = connect.clone();
        let refresh = refresh.clone();
        let status = status.clone();
        connect.connect_clicked(move |_| {
            let Some(dev) = probe::wifi_device() else { return };
            let ssid = net_list
                .selected_item()
                .and_then(|o| o.downcast::<gtk::StringObject>().ok())
                .map(|s| s.string().to_string())
                .unwrap_or_default();
            if ssid.is_empty() {
                return;
            }
            let pw = pass.text().to_string();
            connect_btn.set_label("Connecting…");
            connect_btn.set_sensitive(false);
            let connect_btn = connect_btn.clone();
            let refresh = refresh.clone();
            let status = status.clone();
            run_async(
                move || probe::connect_wifi(&dev, &ssid, &pw),
                move |(_online, msg)| {
                    status.set_text(&msg);
                    connect_btn.set_label("Connect");
                    connect_btn.set_sensitive(true);
                    refresh();
                },
            );
        });
    }

    // Re-check connectivity each time the page becomes visible.
    {
        let refresh = refresh.clone();
        stack.connect_visible_child_name_notify(move |s| {
            if s.visible_child_name().as_deref() == Some("network") {
                refresh();
            }
        });
    }

    stack.add_named(&root, Some("network"));
}

fn add_setup(
    stack: &Rc<gtk::Stack>,
    state: &Rc<RefCell<State>>,
    adv: &Rc<RefCell<Vec<gtk::Widget>>>,
    survey_content: &Rc<gtk::Box>,
) {
    let (root, content, buttons) = page(
        "Choose your setup",
        "Pick a ready-made style. Each one is a complete, declared system.",
    );

    let sources = probe::bundled_manifests();
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::Single);

    for src in &sources {
        let title = probe::manifest_display_name(src);
        let subtitle = probe::manifest_description(src).unwrap_or_default();
        let row = adw::ActionRow::builder().title(&title).subtitle(&subtitle).build();
        list.append(&row);
    }
    content.append(&list);

    // Easy + Advanced: a free-form source (a link or a file path on a USB).
    let custom = gtk::Entry::builder()
        .placeholder_text("Or paste a link (https://…) or a file path")
        .build();
    content.append(&custom);

    // Select first by default.
    if !sources.is_empty() {
        list.select_row(list.row_at_index(0).as_ref());
        state.borrow_mut().manifest = sources[0].clone();
    }
    let _ = adv;

    {
        let state = state.clone();
        let sources = sources.clone();
        list.connect_row_selected(move |_, row| {
            if let Some(row) = row {
                let i = row.index();
                if i >= 0 {
                    if let Some(src) = sources.get(i as usize) {
                        state.borrow_mut().manifest = src.clone();
                    }
                }
            }
        });
    }
    {
        let state = state.clone();
        custom.connect_changed(move |e| {
            let t = e.text().to_string();
            if !t.trim().is_empty() {
                state.borrow_mut().manifest = t.trim().to_string();
            }
        });
    }

    let back = nav_button("Back", false);
    back.connect_clicked(goto(stack, "network"));
    let next = nav_button("Continue", true);
    {
        // Build the survey from the chosen manifest; go to it only if it asks
        // anything, otherwise skip straight to the disk step.
        let stack = stack.clone();
        let state = state.clone();
        let sc = survey_content.clone();
        next.connect_clicked(move |_| {
            let count = populate_survey(&sc, &state);
            stack.set_visible_child_name(if count > 0 { "survey" } else { "disk" });
        });
    }
    buttons.append(&back);
    buttons.append(&next);
    stack.add_named(&root, Some("setup"));
}

/// The survey page — its questions are filled in by `populate_survey` when the
/// user leaves the setup step, based on the manifest they chose.
fn add_survey(stack: &Rc<gtk::Stack>, survey_content: &Rc<gtk::Box>) {
    let (root, content, buttons) = page(
        "A few questions",
        "Your chosen setup asks for a couple of details.",
    );
    content.append(survey_content.as_ref());
    let back = nav_button("Back", false);
    back.connect_clicked(goto(stack, "setup"));
    let next = nav_button("Continue", true);
    next.connect_clicked(goto(stack, "disk"));
    buttons.append(&back);
    buttons.append(&next);
    stack.add_named(&root, Some("survey"));
}

/// Render the chosen manifest's `survey` questions into `container`, wiring each
/// answer into `state.answers`. Returns how many questions were shown (0 = skip
/// the survey page). Questions the account/disk steps already cover are dropped.
fn populate_survey(container: &gtk::Box, state: &Rc<RefCell<State>>) -> usize {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
    // Drop any answers from a previously-viewed survey: if the user opened one
    // manifest's survey then backed out and chose another, its (possibly secret)
    // answers must not leak into the new install's `--answers`.
    state.borrow_mut().answers.clear();
    let source = state.borrow().manifest.clone();
    let questions: Vec<_> = probe::manifest_survey(&source)
        .into_iter()
        .filter(|q| !matches!(q.id.as_str(), "username" | "full_name" | "password" | "hostname"))
        .collect();

    for q in &questions {
        let default = q.default.as_ref().map(json_value_to_string).unwrap_or_default();
        state.borrow_mut().answers.insert(q.id.clone(), default.clone());

        let row = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let label = gtk::Label::new(Some(&q.label));
        label.set_halign(gtk::Align::Start);
        label.add_css_class("heading");
        row.append(&label);

        match q.qtype.as_str() {
            "boolean" => {
                let sw = gtk::Switch::new();
                sw.set_halign(gtk::Align::Start);
                sw.set_active(default == "true");
                let st = state.clone();
                let id = q.id.clone();
                sw.connect_active_notify(move |s| {
                    st.borrow_mut().answers.insert(id.clone(), s.is_active().to_string());
                });
                row.append(&sw);
            }
            "select" => {
                let opts: Vec<&str> = q.options.iter().map(|s| s.as_str()).collect();
                let dd = gtk::DropDown::from_strings(&opts);
                if let Some(pos) = q.options.iter().position(|o| *o == default) {
                    dd.set_selected(pos as u32);
                }
                let st = state.clone();
                let id = q.id.clone();
                let optv = q.options.clone();
                dd.connect_selected_notify(move |d| {
                    if let Some(o) = optv.get(d.selected() as usize) {
                        st.borrow_mut().answers.insert(id.clone(), o.clone());
                    }
                });
                row.append(&dd);
            }
            "secret" => {
                let e = gtk::PasswordEntry::builder().show_peek_icon(true).build();
                let st = state.clone();
                let id = q.id.clone();
                e.connect_changed(move |e| {
                    st.borrow_mut().answers.insert(id.clone(), e.text().to_string());
                });
                row.append(&e);
            }
            // text / path / number / multiselect → free text entry
            _ => {
                let e = gtk::Entry::new();
                e.set_text(&default);
                let st = state.clone();
                let id = q.id.clone();
                e.connect_changed(move |e| {
                    st.borrow_mut().answers.insert(id.clone(), e.text().to_string());
                });
                row.append(&e);
            }
        }
        container.append(&row);
    }
    questions.len()
}

/// Render a manifest survey default (a JSON scalar) as the string to seed a field.
fn json_value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Array(a) => {
            a.iter().map(json_value_to_string).collect::<Vec<_>>().join(" ")
        }
        _ => String::new(),
    }
}

fn add_disk(stack: &Rc<gtk::Stack>, state: &Rc<RefCell<State>>, adv: &Rc<RefCell<Vec<gtk::Widget>>>) {
    let (root, content, buttons) = page(
        "Where should it go?",
        "Choose the disk to install onto. Everything on it will be erased.",
    );

    let disks = probe::list_disks();
    let disk_names: Vec<String> = disks.iter().map(|d| d.name.clone()).collect();

    // If an OS (Windows, another Linux, …) is on a disk, offer to keep it (dual
    // boot) instead of erasing. A blank disk yields None → just the erase flow.
    let win = probe::detect_existing_os();

    // The disk picker (the "erase" target). For dual boot the disk is fixed to
    // the one the existing OS lives on, so we only steer state.disk here in erase mode.
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::Single);
    for d in &disks {
        let row = adw::ActionRow::builder()
            .title(&format!("{} ({})", d.model, d.size))
            .subtitle(&format!("Erase {} and install here", d.name))
            .build();
        list.append(&row);
    }
    if !disks.is_empty() {
        list.select_row(list.row_at_index(0).as_ref());
        state.borrow_mut().disk = disks[0].name.clone();
    }
    {
        let state = state.clone();
        let disk_names = disk_names.clone();
        list.connect_row_selected(move |_, row| {
            if let Some(row) = row {
                let i = row.index();
                if i >= 0 {
                    if state.borrow().install_mode == "erase" {
                        if let Some(name) = disk_names.get(i as usize) {
                            state.borrow_mut().disk = name.clone();
                        }
                    }
                }
            }
        });
    }

    if let Some(w) = &win {
        // Dual-boot chooser. Default to keeping the existing OS — the friendly choice.
        let intro = gtk::Label::new(Some(&format!(
            "Found {} on {} ({} GB). You can keep it and choose which to start, or erase everything.",
            w.label, w.disk, w.shrink_size_gib
        )));
        intro.set_wrap(true);
        intro.set_xalign(0.0);
        intro.add_css_class("dim-label");
        content.append(&intro);

        // Radio buttons (not a selectable list): a binary choice that must NOT
        // change just because focus moved through it — important for keyboard
        // users, who would otherwise flip "erase"/"alongside" by tabbing past.
        let along = gtk::CheckButton::with_label(&format!(
            "Install alongside it — keep {} and pick which to start (recommended)",
            w.label
        ));
        let erase = gtk::CheckButton::with_label(
            "Erase the whole disk — remove everything and start fresh",
        );
        erase.set_group(Some(&along));
        along.set_active(true); // default to keeping the existing OS
        content.append(&along);
        content.append(&erase);

        // Start in dual-boot mode, targeting the existing OS's disk.
        {
            let mut st = state.borrow_mut();
            st.install_mode = "alongside".into();
            st.disk = w.disk.clone();
        }

        let win_disk = w.disk.clone();
        {
            let state_m = state.clone();
            let win_disk = win_disk.clone();
            along.connect_toggled(move |b| {
                if b.is_active() {
                    let mut st = state_m.borrow_mut();
                    st.install_mode = "alongside".into();
                    st.disk = win_disk.clone();
                }
            });
        }
        {
            let state_m = state.clone();
            let list_for_modes = list.clone();
            let disk_names = disk_names.clone();
            erase.connect_toggled(move |b| {
                if b.is_active() {
                    state_m.borrow_mut().install_mode = "erase".into();
                    // Re-apply the picked erase target.
                    if let Some(sel) = list_for_modes.selected_row() {
                        let i = sel.index();
                        if i >= 0 {
                            if let Some(name) = disk_names.get(i as usize) {
                                state_m.borrow_mut().disk = name.clone();
                            }
                        }
                    }
                }
            });
        }
    }

    content.append(&list);

    // Advanced: filesystem + swap + storage (encryption/LVM/RAID) + locale.
    let fs = labeled_choice("Filesystem", &["ext4", "btrfs", "xfs"], 0, {
        let state = state.clone();
        move |v| state.borrow_mut().filesystem = v
    });
    let sw = swap_row(state);
    let enc = encrypt_mode_row(state);
    let storage = lvm_raid_row(state, &disk_names);
    let timezones = probe::list_timezones();
    let tz = searchable_dropdown_row("Timezone", &timezones, {
        let state = state.clone();
        move |v| state.borrow_mut().timezone = v
    });
    let common_locales: Vec<String> = probe::COMMON_LOCALES.iter().map(|s| s.to_string()).collect();
    let loc = searchable_dropdown_row("Locale", &common_locales, {
        let state = state.clone();
        move |v| state.borrow_mut().locale = v
    });
    let loc_manual = text_field_row("Or type a locale manually", "overrides the list above, e.g. en_NZ.UTF-8", {
        let state = state.clone();
        move |v| state.borrow_mut().locale = v
    });
    let km = text_field_row("Keymap", "console keymap, e.g. us", {
        let state = state.clone();
        move |v| state.borrow_mut().keymap = v
    });
    let printing = switch_row("Set up printing (CUPS)", {
        let state = state.clone();
        move |on| state.borrow_mut().install_printing = on
    });
    let post_script = text_field_row("Post-install script (path on this USB)", "run in the new system after everything else", {
        let state = state.clone();
        move |v| state.borrow_mut().post_install_script = v
    });
    for w in [&fs, &sw, &enc, &storage, &tz, &loc, &loc_manual, &km, &printing, &post_script] {
        w.set_visible(false);
        content.append(w);
        adv.borrow_mut().push(w.clone().upcast());
    }

    if win.is_some() {
        let size = alongside_size_row(state);
        size.set_visible(false);
        content.append(&size);
        adv.borrow_mut().push(size.upcast());
    }

    // NVIDIA proprietary driver — offered only when an NVIDIA GPU is present.
    if probe::has_nvidia_gpu() {
        let nvidia = switch_row("Install NVIDIA driver (proprietary)", {
            let state = state.clone();
            move |on| state.borrow_mut().install_nvidia = on
        });
        nvidia.set_visible(false);
        content.append(&nvidia);
        adv.borrow_mut().push(nvidia.upcast());
    }

    let back = nav_button("Back", false);
    back.connect_clicked(goto(stack, "setup"));
    let next = nav_button("Continue", true);
    next.connect_clicked(goto(stack, "account"));
    buttons.append(&back);
    buttons.append(&next);
    stack.add_named(&root, Some("disk"));
}

fn add_account(stack: &Rc<gtk::Stack>, state: &Rc<RefCell<State>>, adv: &Rc<RefCell<Vec<gtk::Widget>>>) {
    let (root, content, buttons) = page("Create your account", "This is how you'll sign in.");

    let name = gtk::Entry::builder().placeholder_text("Your name").build();
    let pass = gtk::PasswordEntry::builder().show_peek_icon(true).build();
    pass.set_property("placeholder-text", "Choose a password");
    content.append(&name);
    content.append(&pass);

    // A live strength meter, always visible — not an Advanced feature.
    let (strength_row, update_strength) = password_strength_meter();
    content.append(&strength_row);
    pass.connect_changed(move |e| update_strength(&e.text()));

    // Advanced: explicit username + hostname.
    let user = gtk::Entry::builder().placeholder_text("Username").build();
    let host = gtk::Entry::builder().placeholder_text("Computer name (hostname)").build();
    user.set_visible(false);
    host.set_visible(false);
    content.append(&user);
    content.append(&host);
    adv.borrow_mut().push(user.clone().upcast());
    adv.borrow_mut().push(host.clone().upcast());

    // Advanced: an optional root password (root stays locked — login via this
    // account's wheel/sudo — unless explicitly set), auto-login, and any
    // additional accounts beyond this primary one.
    let root_pw = password_field_row("Root password (optional)", "Leave blank to keep root locked", {
        let state = state.clone();
        move |v| state.borrow_mut().root_password = v
    });
    let autologin = switch_row("Log in automatically", {
        let state = state.clone();
        move |on| state.borrow_mut().autologin = on
    });
    let extra_users = extra_users_row(state);
    for w in [&root_pw, &autologin] {
        w.set_visible(false);
        content.append(w);
        adv.borrow_mut().push(w.clone().upcast());
    }
    extra_users.set_visible(false);
    content.append(&extra_users);
    adv.borrow_mut().push(extra_users.upcast());

    {
        let state = state.clone();
        let user = user.clone();
        name.connect_changed(move |e| {
            let full = e.text().to_string();
            let mut st = state.borrow_mut();
            st.full_name = full.clone();
            // Auto-derive username from the first word unless the user set one.
            if !st.advanced || user.text().trim().is_empty() {
                let u: String = full
                    .trim()
                    .to_ascii_lowercase()
                    .chars()
                    .take_while(|c| !c.is_whitespace())
                    .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                    .collect();
                st.username = u;
            }
        });
    }
    {
        let state = state.clone();
        pass.connect_changed(move |e| state.borrow_mut().password = e.text().to_string());
    }
    {
        let state = state.clone();
        user.connect_changed(move |e| state.borrow_mut().username = e.text().to_string());
    }
    {
        let state = state.clone();
        host.connect_changed(move |e| state.borrow_mut().hostname = e.text().to_string());
    }

    let back = nav_button("Back", false);
    back.connect_clicked(goto(stack, "disk"));
    let next = nav_button("Continue", true);
    next.connect_clicked(goto(stack, "review"));
    buttons.append(&back);
    buttons.append(&next);
    stack.add_named(&root, Some("account"));
}

fn add_review(stack: &Rc<gtk::Stack>, state: &Rc<RefCell<State>>, cancel_flag: &Rc<RefCell<Option<Arc<AtomicBool>>>>) {
    let (root, content, buttons) = page("Ready to install", "Please review — this will erase the selected disk.");

    let summary = gtk::Label::new(None);
    summary.set_halign(gtk::Align::Start);
    summary.set_wrap(true);
    content.append(&summary);

    // The equivalent headless command (power-user transparency) — selectable so
    // it can be copied. Secrets (passphrase, password) are never shown.
    let cli = gtk::Label::new(None);
    cli.set_halign(gtk::Align::Start);
    cli.set_wrap(true);
    cli.set_selectable(true);
    cli.add_css_class("dim-label");
    cli.set_widget_name("cli-equivalent");
    content.append(&cli);

    // Refresh the summary each time we land here.
    {
        let summary = summary.clone();
        let cli = cli.clone();
        let state = state.clone();
        stack.connect_visible_child_name_notify(move |s| {
            if s.visible_child_name().as_deref() != Some("review") {
                return;
            }
            let st = state.borrow();
            let setup = probe::manifest_display_name(&st.manifest);
            let swap_str = match (st.swap.as_str(), st.swap_size_gib.unwrap_or(2)) {
                ("swapfile", g) => format!("file ({g} GiB)"),
                ("partition", g) => format!("partition ({g} GiB)"),
                (s, _) => s.to_string(),
            };
            let disk_str = if st.install_mode == "alongside" {
                format!("{} — alongside the existing OS ({} GiB for Manifest OS)", st.disk, st.alongside_gib.unwrap_or(40))
            } else {
                format!("{} (will be erased)", st.disk)
            };
            let mut fs_line = st.filesystem.clone();
            match st.encrypt_mode.as_str() {
                "full" => fs_line.push_str(" + full-disk LUKS"),
                "home" => fs_line.push_str(" + encrypted /home"),
                _ => {}
            }
            if st.lvm {
                fs_line.push_str(" + LVM");
            }
            if !st.raid1_disk.trim().is_empty() {
                fs_line.push_str(&format!(" + RAID1 with {}", st.raid1_disk.trim()));
            }
            let extra_n = parse_extra_users(&st.extra_users_text).len();
            let account_extra = if extra_n > 0 { format!(" (+{extra_n} more)") } else { String::new() };
            summary.set_markup(&format!(
                "<b>Setup:</b> {}\n<b>Disk:</b> {}\n<b>Account:</b> {} ({}){}\n<b>Filesystem:</b> {}   <b>Swap:</b> {}",
                glib::markup_escape_text(&setup),
                glib::markup_escape_text(&disk_str),
                glib::markup_escape_text(&st.full_name),
                glib::markup_escape_text(&st.username),
                account_extra,
                glib::markup_escape_text(&fs_line),
                glib::markup_escape_text(&swap_str),
            ));
            cli.set_markup(&format!(
                "<small>Equivalent command:\n<tt>{}</tt></small>",
                glib::markup_escape_text(&cli_command(&st))
            ));
        });
    }

    let back = nav_button("Back", false);
    back.connect_clicked(goto(stack, "account"));
    let install = nav_button("Install now", true);
    {
        let stack = stack.clone();
        let state = state.clone();
        let cancel_flag = cancel_flag.clone();
        install.connect_clicked(move |_| start_install(&stack, &state, &cancel_flag));
    }
    buttons.append(&back);
    buttons.append(&install);
    stack.add_named(&root, Some("review"));
}

fn add_installing(stack: &Rc<gtk::Stack>, adv: &Rc<RefCell<Vec<gtk::Widget>>>, cancel_flag: &Rc<RefCell<Option<Arc<AtomicBool>>>>) {
    let (root, content, buttons) = page("Installing Manifest OS", "Sit back — this takes a few minutes. Don't turn off your computer.");
    let spinner = gtk::Spinner::new();
    spinner.set_size_request(48, 48);
    spinner.start();
    spinner.set_halign(gtk::Align::Center);
    content.append(&spinner);

    // A running "Step N — <name>" counter, so a long silent stretch (a big
    // package download, building paru) still reads as progress, not a hang.
    // No fabricated "of M" total — which steps run varies with the plan, so a
    // fake denominator would be dishonest.
    let step_label = gtk::Label::new(None);
    step_label.set_halign(gtk::Align::Center);
    step_label.add_css_class("dim-label");
    step_label.set_widget_name("step-counter");
    content.append(&step_label);

    // A live log, so a long step (building paru, big package sets) doesn't look
    // frozen. It tails the same output the installer writes to
    // /tmp/manifest-install.log (see start_log_tail). Advanced-only — Easy mode
    // stays calm and uncluttered.
    let view = gtk::TextView::new();
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_monospace(true);
    view.set_widget_name("install-log");
    view.add_css_class("card");
    let scroller = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .min_content_height(280)
        .child(&view)
        .build();
    scroller.set_visible(false);
    content.append(&scroller);
    adv.borrow_mut().push(scroller.upcast());

    // Cancel, with a "click again to confirm" pattern (never a silent
    // one-click cancel) — the install stops after the current step finishes,
    // never mid-command, so a partial disk write can't happen.
    let cancel_btn = gtk::Button::with_label("Cancel");
    cancel_btn.add_css_class("destructive-action");
    let confirming = Rc::new(std::cell::Cell::new(false));
    {
        let cancel_flag = cancel_flag.clone();
        let confirming = confirming.clone();
        let btn = cancel_btn.clone();
        cancel_btn.connect_clicked(move |_| {
            if confirming.get() {
                if let Some(flag) = cancel_flag.borrow().as_ref() {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                btn.set_label("Cancelling…");
                btn.set_sensitive(false);
            } else {
                confirming.set(true);
                btn.set_label("Click again to cancel");
                let confirming2 = confirming.clone();
                let btn2 = btn.clone();
                glib::timeout_add_local_once(Duration::from_secs(4), move || {
                    if confirming2.get() {
                        confirming2.set(false);
                        btn2.set_label("Cancel");
                    }
                });
            }
        });
    }
    buttons.append(&cancel_btn);
    // Reset the button each time a fresh install attempt shows this page.
    {
        let stack = stack.clone();
        let confirming = confirming.clone();
        let btn = cancel_btn.clone();
        stack.connect_visible_child_name_notify(move |s| {
            if s.visible_child_name().as_deref() == Some("installing") {
                confirming.set(false);
                btn.set_label("Cancel");
                btn.set_sensitive(true);
            }
        });
    }

    stack.add_named(&root, Some("installing"));
}

fn add_done(stack: &Rc<gtk::Stack>) {
    let (root, content, buttons) = page("All done!", "");
    let msg = gtk::Label::new(None);
    msg.set_halign(gtk::Align::Start);
    msg.set_wrap(true);
    msg.set_widget_name("done-message");
    content.append(&msg);

    let restart = nav_button("Restart now", true);
    restart.connect_clicked(|_| installer::reboot());
    buttons.append(&restart);
    stack.add_named(&root, Some("done"));
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

/// Trim a field; empty becomes None (so the manifest's value is kept).
fn opt(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() { None } else { Some(t.to_string()) }
}

/// Populate `state` from a loaded preseed [`InstallPlan`], mapping its shape
/// back into the GUI's flat per-widget fields — the same fields the wizard
/// pages would have set, so Review/start_install work identically either way.
fn load_preseed_into_state(state: &Rc<RefCell<State>>, plan: InstallPlan) {
    let mut st = state.borrow_mut();
    st.manifest = plan.manifest;
    st.disk = plan.disk;
    st.install_mode = plan.install_mode;
    st.alongside_gib = plan.alongside_gib;
    st.filesystem = plan.filesystem;
    st.swap = plan.swap;
    st.swap_size_gib = plan.swap_size_gib;
    st.answers = plan.answers.into_iter().collect();
    if let Some(acct) = plan.account {
        st.full_name = acct.full_name;
        st.username = acct.username;
        st.password = acct.password;
    }
    st.hostname = plan.hostname.unwrap_or_default();
    st.encrypt_mode = plan.encrypt_mode;
    st.encrypt_passphrase = plan.encrypt_passphrase;
    st.root_gib = plan.root_gib;
    st.lvm = plan.lvm;
    st.raid1_disk = plan.raid1_disk.unwrap_or_default();
    st.timezone = plan.timezone.unwrap_or_default();
    st.locale = plan.locale.unwrap_or_default();
    st.keymap = plan.keymap.unwrap_or_default();
    st.root_password = plan.root_password.unwrap_or_default();
    st.autologin = plan.autologin;
    st.install_nvidia = plan.install_nvidia;
    st.install_printing = plan.install_printing;
    st.extra_users_text = plan
        .extra_users
        .iter()
        .map(|u| {
            if u.sudo {
                format!("{}:{}:sudo", u.username, u.password)
            } else {
                format!("{}:{}", u.username, u.password)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    st.post_install_script = plan.post_install_script.unwrap_or_default();
    if let Some(ip) = plan.static_ip {
        st.static_ip = ip.address;
        st.gateway = ip.gateway;
        st.dns = ip.dns;
    }
    st.proxy = plan.proxy.unwrap_or_default();
    st.vlan_id = plan.vlan_id.map(|v| v.to_string()).unwrap_or_default();
    st.vlan_parent = plan.vlan_parent.unwrap_or_default();
}

/// The `manifest provision …` command equivalent to the current selections, for
/// the review page. Secrets are shown as `******`, never the real values.
fn cli_command(st: &State) -> String {
    let manifest = if st.manifest.is_empty() { "<manifest>" } else { &st.manifest };
    let disk = if st.disk.is_empty() { "<disk>" } else { &st.disk };
    let mut c = format!("manifest provision {manifest} --disk {disk}");
    if st.install_mode == "alongside" {
        c.push_str(" --mode alongside");
        if let Some(g) = st.alongside_gib {
            c.push_str(&format!(" --alongside-gib {g}"));
        }
    }
    if st.filesystem != "ext4" {
        c.push_str(&format!(" --filesystem {}", st.filesystem));
    }
    if st.swap != "zram" {
        c.push_str(&format!(" --swap {}", st.swap));
    }
    if let Some(g) = st.swap_size_gib {
        c.push_str(&format!(" --swap-size-gib {g}"));
    }
    if st.encrypt_mode != "none" {
        c.push_str(&format!(" --encrypt-mode {} --passphrase ******", st.encrypt_mode));
        if let Some(g) = st.root_gib {
            c.push_str(&format!(" --root-gib {g}"));
        }
    }
    if st.lvm {
        c.push_str(" --lvm");
    }
    if !st.raid1_disk.trim().is_empty() {
        c.push_str(&format!(" --raid1-disk {}", st.raid1_disk.trim()));
    }
    for (flag, val) in [
        ("--timezone", &st.timezone),
        ("--locale", &st.locale),
        ("--keymap", &st.keymap),
        ("--hostname", &st.hostname),
    ] {
        if !val.trim().is_empty() {
            c.push_str(&format!(" {flag} {}", val.trim()));
        }
    }
    if !st.username.trim().is_empty() {
        c.push_str(&format!(" --user {} --password ******", st.username.trim()));
    }
    for u in parse_extra_users(&st.extra_users_text) {
        c.push_str(&format!(" --extra-user {}:******{}", u.username, if u.sudo { ":sudo" } else { "" }));
    }
    if !st.root_password.trim().is_empty() {
        c.push_str(" --root-password ******");
    }
    if st.autologin {
        c.push_str(" --autologin");
    }
    if st.install_nvidia {
        c.push_str(" --install-nvidia");
    }
    if st.install_printing {
        c.push_str(" --install-printing");
    }
    if !st.post_install_script.trim().is_empty() {
        c.push_str(&format!(" --post-script {}", st.post_install_script.trim()));
    }
    if !st.static_ip.trim().is_empty() {
        c.push_str(&format!(" --static-ip {} --gateway {}", st.static_ip.trim(), st.gateway.trim()));
        if !st.dns.trim().is_empty() {
            c.push_str(&format!(" --dns {}", st.dns.trim()));
        }
    }
    if !st.proxy.trim().is_empty() {
        c.push_str(&format!(" --proxy {}", st.proxy.trim()));
    }
    if !st.vlan_id.trim().is_empty() {
        c.push_str(&format!(" --vlan-id {} --vlan-parent {}", st.vlan_id.trim(), st.vlan_parent.trim()));
    }
    c
}

/// Parse the extra-users textarea: one "username:password[:sudo]" per line,
/// same format (and same parsing) as `provision --extra-user`.
fn parse_extra_users(text: &str) -> Vec<ExtraUser> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let mut parts = line.splitn(3, ':');
            let username = parts.next()?.to_string();
            let password = parts.next()?.to_string();
            let sudo = parts.next() == Some("sudo");
            Some(ExtraUser { username, password, sudo })
        })
        .collect()
}

fn start_install(
    stack: &Rc<gtk::Stack>,
    state: &Rc<RefCell<State>>,
    cancel_flag: &Rc<RefCell<Option<Arc<AtomicBool>>>>,
) {
    // Build the plan from collected state.
    let plan = {
        let st = state.borrow();
        let static_ip = if st.static_ip.trim().is_empty() {
            None
        } else {
            Some(StaticIp {
                address: st.static_ip.trim().to_string(),
                gateway: st.gateway.trim().to_string(),
                dns: st.dns.trim().to_string(),
            })
        };
        InstallPlan {
            disk: st.disk.clone(),
            install_mode: st.install_mode.clone(),
            alongside_gib: st.alongside_gib,
            filesystem: st.filesystem.clone(),
            swap: st.swap.clone(),
            swap_size_gib: st.swap_size_gib,
            manifest: st.manifest.clone(),
            answers: st.answers.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            account: if st.username.trim().is_empty() || st.password.is_empty() {
                None
            } else {
                Some(Account {
                    full_name: st.full_name.clone(),
                    username: st.username.clone(),
                    password: st.password.clone(),
                })
            },
            extra_users: parse_extra_users(&st.extra_users_text),
            hostname: opt(&st.hostname),
            encrypt_mode: st.encrypt_mode.clone(),
            encrypt_passphrase: st.encrypt_passphrase.clone(),
            root_gib: st.root_gib,
            lvm: st.lvm,
            raid1_disk: opt(&st.raid1_disk),
            timezone: opt(&st.timezone),
            locale: opt(&st.locale),
            keymap: opt(&st.keymap),
            root_password: opt(&st.root_password),
            autologin: st.autologin,
            install_nvidia: st.install_nvidia,
            install_printing: st.install_printing,
            post_install_script: opt(&st.post_install_script),
            static_ip,
            proxy: opt(&st.proxy),
            vlan_id: st.vlan_id.trim().parse().ok(),
            vlan_parent: opt(&st.vlan_parent),
        }
    };

    let flag = Arc::new(AtomicBool::new(false));
    *cancel_flag.borrow_mut() = Some(flag.clone());

    stack.set_visible_child_name("installing");
    start_log_tail(stack);

    let stack2 = stack.clone();
    run_async(
        move || installer::execute(&plan, &Ctx::with_cancel_flag(false, flag)).map_err(|e| format!("{e:#}")),
        move |result| match result {
            Ok(()) => {
                set_done_message(&stack2);
                stack2.set_visible_child_name("done");
            }
            Err(e) => {
                // Preserve the install log (target + writable USB) for debugging.
                installer::save_install_log(&Ctx::new(false));
                show_error(&stack2, &e);
            }
        },
    );
}

/// Tail the install log into the Installing page's text view while the install
/// runs. The installer (and the pacman/paru/etc. it spawns) writes to the GUI's
/// stdout, which `.zlogin` redirects to /tmp/manifest-install.log. We poll the
/// tail of that file and stop once we leave the installing page.
fn start_log_tail(stack: &Rc<gtk::Stack>) {
    const LOG: &str = "/tmp/manifest-install.log";
    let Some(page) = stack.child_by_name("installing") else { return };
    let Some(w) = find_named(&page, "install-log") else { return };
    let Ok(view) = w.downcast::<gtk::TextView>() else { return };
    let step_label = find_named(&page, "step-counter").and_then(|w| w.downcast::<gtk::Label>().ok());
    let buffer = view.buffer();
    // Skip whatever predates the install (GUI/cage startup chatter).
    let start = std::fs::metadata(LOG).map(|m| m.len()).unwrap_or(0);
    let stack = stack.clone();
    glib::timeout_add_local(Duration::from_millis(600), move || {
        if stack.visible_child_name().as_deref() != Some("installing") {
            return glib::ControlFlow::Break;
        }
        let text = read_log_tail(LOG, start, 250);
        let current = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
        if current != text {
            buffer.set_text(&text);
            let mut end = buffer.end_iter();
            view.scroll_to_iter(&mut end, 0.0, true, 0.0, 1.0);
        }
        if let Some(label) = &step_label {
            let steps = scan_steps(LOG, start);
            if let Some(current) = steps.last() {
                label.set_text(&format!("Step {} — {}", steps.len(), current));
            }
        }
        glib::ControlFlow::Continue
    });
}

/// Distinct `[Step Name]` markers seen so far in the install log, from byte
/// `start` onward — used for the "Step N — <name>" counter. Reads the whole
/// remaining file each call (typically well under 1 MB), not just the
/// display-truncated tail `read_log_tail` shows.
fn scan_steps(path: &str, start: u64) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return Vec::new() };
    let _ = f.seek(SeekFrom::Start(start));
    let mut buf = String::new();
    let _ = f.read_to_string(&mut buf);
    buf.lines()
        .filter_map(|l| {
            let l = l.trim();
            (l.len() > 2 && l.starts_with('[') && l.ends_with(']')).then(|| l[1..l.len() - 1].to_string())
        })
        .collect()
}

/// Read `path` from byte `start` to EOF and return its last `max_lines` lines.
fn read_log_tail(path: &str, start: u64, max_lines: usize) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return String::new() };
    let _ = f.seek(SeekFrom::Start(start));
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    let s = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = s.lines().collect();
    let tail = if lines.len() > max_lines {
        &lines[lines.len() - max_lines..]
    } else {
        &lines[..]
    };
    tail.join("\n")
}

/// Fill the Done page with firmware-appropriate guidance.
fn set_done_message(stack: &Rc<gtk::Stack>) {
    if let Some(page) = stack.child_by_name("done") {
        if let Some(msg) = find_named(&page, "done-message") {
            if let Ok(label) = msg.downcast::<gtk::Label>() {
                let text = if installer::is_uefi() {
                    "Manifest OS is installed. Press Restart — you can leave the USB plugged in; it will boot into your new system."
                } else {
                    "Manifest OS is installed. Remove the install USB (or eject the disc), then press Restart."
                };
                label.set_text(text);
            }
        }
    }
}

/// Swap the Installing page's spinner view for an error message + a Back button.
fn show_error(stack: &Rc<gtk::Stack>, err: &str) {
    let (root, content, buttons) = page("Something went wrong", "The install didn't finish. You can go back and try again.");
    let label = gtk::Label::new(Some(err));
    label.set_halign(gtk::Align::Start);
    label.set_wrap(true);
    label.set_selectable(true);
    content.append(&label);
    let back = nav_button("Back to start", false);
    back.connect_clicked(goto(stack, "review"));
    buttons.append(&back);
    // Replace any previous error page, then show it.
    if let Some(old) = stack.child_by_name("error") {
        stack.remove(&old);
    }
    stack.add_named(&root, Some("error"));
    stack.set_visible_child_name("error");
}

// ---------------------------------------------------------------------------
// Small widgets / helpers
// ---------------------------------------------------------------------------

/// Swap chooser: zram / none / file / partition, plus a size (GiB) field that
/// appears only for file and partition. Reports into the shared state.
fn swap_row(state: &Rc<RefCell<State>>) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let label = gtk::Label::new(Some("Swap"));
    label.set_halign(gtk::Align::Start);
    label.set_hexpand(true);
    row.append(&label);

    let size = gtk::Entry::builder()
        .placeholder_text("Size (GiB)")
        .max_width_chars(9)
        .build();
    size.set_visible(false);

    // (button label, value stored in state)
    let opts = [("zram", "zram"), ("none", "none"), ("file", "swapfile"), ("partition", "partition")];
    let toggles: Rc<RefCell<Vec<gtk::ToggleButton>>> = Rc::new(RefCell::new(Vec::new()));
    for (i, (text, value)) in opts.iter().enumerate() {
        let b = gtk::ToggleButton::with_label(text);
        if i == 0 {
            b.set_active(true);
        }
        let value = *value;
        let state = state.clone();
        let toggles_c = toggles.clone();
        let size = size.clone();
        b.connect_clicked(move |btn| {
            if btn.is_active() {
                state.borrow_mut().swap = value.to_string();
                for o in toggles_c.borrow().iter() {
                    if o != btn {
                        o.set_active(false);
                    }
                }
                size.set_visible(value == "swapfile" || value == "partition");
            } else if !toggles_c.borrow().iter().any(|t| t.is_active()) {
                btn.set_active(true);
            }
        });
        toggles.borrow_mut().push(b.clone());
        row.append(&b);
    }
    {
        let state = state.clone();
        size.connect_changed(move |e| {
            state.borrow_mut().swap_size_gib = e.text().trim().parse::<u32>().ok();
        });
    }
    row.append(&size);
    row
}

/// "Space for Manifest OS" — how many GiB to carve from Windows when dual-booting.
/// Shown only in Advanced; Easy mode uses the engine's sensible default.
fn alongside_size_row(state: &Rc<RefCell<State>>) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let label = gtk::Label::new(Some("Space for Manifest OS (GiB)"));
    label.set_halign(gtk::Align::Start);
    label.set_hexpand(true);
    row.append(&label);

    let size = gtk::Entry::builder()
        .placeholder_text("40")
        .max_width_chars(9)
        .build();
    {
        let state = state.clone();
        size.connect_changed(move |e| {
            state.borrow_mut().alongside_gib = e.text().trim().parse::<u32>().ok();
        });
    }
    row.append(&size);
    row
}

/// A "Label: [searchable dropdown]" row — a long list (e.g. every timezone)
/// with GTK4's built-in incremental search, so typing narrows it instantly.
fn searchable_dropdown_row(
    label: &str,
    options: &[String],
    on_change: impl Fn(String) + 'static,
) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let l = gtk::Label::new(Some(label));
    l.set_halign(gtk::Align::Start);
    l.set_hexpand(true);
    row.append(&l);
    let refs: Vec<&str> = options.iter().map(|s| s.as_str()).collect();
    let dd = gtk::DropDown::from_strings(&refs);
    dd.set_enable_search(true);
    let opts = options.to_vec();
    dd.connect_selected_notify(move |d| {
        let i = d.selected() as usize;
        if let Some(v) = opts.get(i) {
            on_change(v.clone());
        }
    });
    row.append(&dd);
    row
}

/// A "Label: [entry]" row that reports the entry text on every change.
fn text_field_row(
    label: &str,
    placeholder: &str,
    on_change: impl Fn(String) + 'static,
) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let l = gtk::Label::new(Some(label));
    l.set_halign(gtk::Align::Start);
    l.set_hexpand(true);
    row.append(&l);
    let e = gtk::Entry::builder().placeholder_text(placeholder).build();
    e.connect_changed(move |e| on_change(e.text().to_string()));
    row.append(&e);
    row
}

/// A "Label: [password entry]" row for an optional secret, reporting changes.
fn password_field_row(
    label: &str,
    placeholder: &str,
    on_change: impl Fn(String) + 'static,
) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let l = gtk::Label::new(Some(label));
    l.set_halign(gtk::Align::Start);
    l.set_hexpand(true);
    row.append(&l);
    let e = gtk::PasswordEntry::builder().show_peek_icon(true).build();
    e.set_property("placeholder-text", placeholder);
    e.connect_changed(move |e| on_change(e.text().to_string()));
    row.append(&e);
    row
}

/// A "Label: [switch]" row for a plain boolean toggle.
fn switch_row(label: &str, on_change: impl Fn(bool) + 'static) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let l = gtk::Label::new(Some(label));
    l.set_halign(gtk::Align::Start);
    l.set_hexpand(true);
    row.append(&l);
    let sw = gtk::Switch::new();
    sw.set_valign(gtk::Align::Center);
    sw.connect_active_notify(move |s| on_change(s.is_active()));
    row.append(&sw);
    row
}

/// Encryption mode: none / full-disk LUKS / home-only LUKS, plus the
/// passphrase (shown for full or home) and a root-size field (shown for home
/// only — the rest of the disk becomes /home). Returns a Box containing all of
/// it, so callers add/hide it as one Advanced-only unit.
fn encrypt_mode_row(state: &Rc<RefCell<State>>) -> gtk::Box {
    let col = gtk::Box::new(gtk::Orientation::Vertical, 8);

    let pass = gtk::PasswordEntry::builder().show_peek_icon(true).build();
    pass.set_property("placeholder-text", "Encryption passphrase");
    pass.set_visible(false);
    let root_gib = gtk::Entry::builder().placeholder_text("40").max_width_chars(9).build();
    let root_gib_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let root_gib_label = gtk::Label::new(Some("Root size (GiB) — the rest becomes /home"));
    root_gib_label.set_halign(gtk::Align::Start);
    root_gib_label.set_hexpand(true);
    root_gib_row.append(&root_gib_label);
    root_gib_row.append(&root_gib);
    root_gib_row.set_visible(false);

    let choice = labeled_choice("Encryption", &["Off", "Full disk", "Home only"], 0, {
        let state = state.clone();
        let pass = pass.clone();
        let root_gib_row = root_gib_row.clone();
        move |v| {
            let mode = match v.as_str() {
                "Full disk" => "full",
                "Home only" => "home",
                _ => "none",
            };
            state.borrow_mut().encrypt_mode = mode.to_string();
            pass.set_visible(mode != "none");
            root_gib_row.set_visible(mode == "home");
        }
    });
    {
        let state = state.clone();
        pass.connect_changed(move |e| state.borrow_mut().encrypt_passphrase = e.text().to_string());
    }
    {
        let state = state.clone();
        root_gib.connect_changed(move |e| {
            state.borrow_mut().root_gib = e.text().trim().parse::<u32>().ok();
        });
    }

    col.append(&choice);
    col.append(&pass);
    col.append(&root_gib_row);
    col
}

/// LVM (root on a single logical volume) + RAID1 (mirror root across a second
/// disk). Composable with each other and with encryption — see
/// `installer::build_storage`.
fn lvm_raid_row(state: &Rc<RefCell<State>>, other_disks: &[String]) -> gtk::Box {
    let col = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let lvm = switch_row("Use LVM (root on a resizable logical volume)", {
        let state = state.clone();
        move |on| state.borrow_mut().lvm = on
    });
    col.append(&lvm);

    if !other_disks.is_empty() {
        let raid_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let label = gtk::Label::new(Some("Mirror root across a second disk (RAID1)"));
        label.set_halign(gtk::Align::Start);
        label.set_hexpand(true);
        raid_row.append(&label);
        let sw = gtk::Switch::new();
        sw.set_valign(gtk::Align::Center);
        raid_row.append(&sw);

        let refs: Vec<&str> = other_disks.iter().map(|s| s.as_str()).collect();
        let picker = gtk::DropDown::from_strings(&refs);
        picker.set_visible(false);
        raid_row.append(&picker);

        {
            let state = state.clone();
            let picker = picker.clone();
            let disks = other_disks.to_vec();
            let set_from_picker = move |st: &mut State, picker: &gtk::DropDown| {
                let i = picker.selected() as usize;
                st.raid1_disk = disks.get(i).cloned().unwrap_or_default();
            };
            sw.connect_active_notify(move |s| {
                let on = s.is_active();
                picker.set_visible(on);
                let mut st = state.borrow_mut();
                if on {
                    set_from_picker(&mut st, &picker);
                } else {
                    st.raid1_disk = String::new();
                }
            });
        }
        {
            let state = state.clone();
            let disks = other_disks.to_vec();
            picker.connect_selected_notify(move |p| {
                let i = p.selected() as usize;
                state.borrow_mut().raid1_disk = disks.get(i).cloned().unwrap_or_default();
            });
        }
        col.append(&raid_row);
    }
    col
}

/// Additional accounts beyond the primary one: one "username:password[:sudo]"
/// per line, parsed the same way `provision --extra-user` is. A plain
/// multi-line text field, rather than dynamic add/remove rows — this is
/// Advanced-only, and the format is simple enough to type directly.
fn extra_users_row(state: &Rc<RefCell<State>>) -> gtk::Box {
    let col = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let label = gtk::Label::new(Some("Additional accounts (one per line: username:password or username:password:sudo)"));
    label.set_halign(gtk::Align::Start);
    label.set_wrap(true);
    col.append(&label);
    let view = gtk::TextView::new();
    view.set_monospace(true);
    view.add_css_class("card");
    let scroller = gtk::ScrolledWindow::builder().min_content_height(80).child(&view).build();
    col.append(&scroller);
    {
        let state = state.clone();
        view.buffer().connect_changed(move |b| {
            let text = b.text(&b.start_iter(), &b.end_iter(), false).to_string();
            state.borrow_mut().extra_users_text = text;
        });
    }
    col
}

/// A rough live password-strength meter (length + character variety — no
/// external dependency). Purely advisory; doesn't block a weak password.
fn password_strength(pw: &str) -> (f64, &'static str) {
    if pw.is_empty() {
        return (0.0, "");
    }
    let len_score = (pw.len() as f64 / 12.0).min(1.0);
    let kinds = [
        pw.chars().any(|c| c.is_ascii_lowercase()),
        pw.chars().any(|c| c.is_ascii_uppercase()),
        pw.chars().any(|c| c.is_ascii_digit()),
        pw.chars().any(|c| !c.is_ascii_alphanumeric()),
    ];
    let variety = kinds.iter().filter(|&&k| k).count() as f64 / 4.0;
    let score = (len_score * 0.6 + variety * 0.4).min(1.0);
    let label = if score < 0.34 { "Weak" } else if score < 0.67 { "Okay" } else { "Strong" };
    (score, label)
}

/// A GtkLevelBar + word ("Weak"/"Okay"/"Strong") under a password entry.
/// Returns the row; call `update(&entry.text())` from the entry's
/// `connect_changed` to keep it live.
fn password_strength_meter() -> (gtk::Box, impl Fn(&str)) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let bar = gtk::LevelBar::new();
    bar.set_min_value(0.0);
    bar.set_max_value(1.0);
    bar.set_hexpand(true);
    let word = gtk::Label::new(None);
    word.add_css_class("dim-label");
    word.set_width_chars(6);
    row.append(&bar);
    row.append(&word);
    let update = {
        let bar = bar.clone();
        let word = word.clone();
        move |pw: &str| {
            let (score, label) = password_strength(pw);
            bar.set_value(score);
            word.set_text(label);
        }
    };
    (row, update)
}

/// A "Label: [A] [B]" segmented choice row that reports the chosen string.
fn labeled_choice(
    label: &str,
    options: &[&'static str],
    default_idx: usize,
    on_change: impl Fn(String) + 'static,
) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let l = gtk::Label::new(Some(label));
    l.set_halign(gtk::Align::Start);
    l.set_hexpand(true);
    row.append(&l);

    let on_change = Rc::new(on_change);
    let toggles: Rc<RefCell<Vec<gtk::ToggleButton>>> = Rc::new(RefCell::new(Vec::new()));
    for (i, opt) in options.iter().enumerate() {
        let b = gtk::ToggleButton::with_label(opt);
        if i == default_idx {
            b.set_active(true);
        }
        let opt = *opt;
        let on_change = on_change.clone();
        let toggles_c = toggles.clone();
        b.connect_clicked(move |btn| {
            if btn.is_active() {
                on_change(opt.to_string());
                // Radio behavior: deactivate the others.
                for other in toggles_c.borrow().iter() {
                    if other != btn {
                        other.set_active(false);
                    }
                }
            } else if !toggles_c.borrow().iter().any(|t| t.is_active()) {
                // Don't allow zero selected.
                btn.set_active(true);
            }
        });
        toggles.borrow_mut().push(b.clone());
        row.append(&b);
    }
    row
}

/// Depth-first search for a descendant widget by its `widget_name`.
fn find_named(root: &gtk::Widget, name: &str) -> Option<gtk::Widget> {
    if root.widget_name() == name {
        return Some(root.clone());
    }
    let mut child = root.first_child();
    while let Some(c) = child {
        if let Some(found) = find_named(&c, name) {
            return Some(found);
        }
        child = c.next_sibling();
    }
    None
}
