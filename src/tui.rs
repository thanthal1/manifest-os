//! The guided installer TUI — a full-screen, beginner-friendly wizard.
//!
//! It owns the steps a manifest can't: bring the network up, pick a disk, pick
//! a manifest. It collects choices into an [`InstallPlan`]; the caller then runs
//! the real install (disk → pacstrap → `manifest install`). Pure terminal, but
//! styled to feel like a graphical installer: a step breadcrumb, highlighted
//! selections, inline help, and big clear warnings before anything destructive.

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{prelude::*, widgets::*};
use std::time::Duration;

use crate::probe::{self, Disk, InstallPlan};

// Catppuccin Mocha — friendly, high-contrast.
const BASE: Color = Color::Rgb(30, 30, 46);
const SURFACE: Color = Color::Rgb(49, 50, 68);
const TEXT: Color = Color::Rgb(205, 214, 244);
const SUBT: Color = Color::Rgb(127, 132, 156);
const ACCENT: Color = Color::Rgb(203, 166, 247);
const GREEN: Color = Color::Rgb(166, 227, 161);
const RED: Color = Color::Rgb(243, 139, 168);
const YELLOW: Color = Color::Rgb(249, 226, 175);

#[derive(Clone, Copy, PartialEq)]
enum Screen {
    Welcome,
    Network,
    Disk,
    Manifest,
    Confirm,
}

const STEPS: [&str; 5] = ["Welcome", "Network", "Disk", "Manifest", "Install"];

struct App {
    screen: Screen,
    quit: bool,
    plan: Option<InstallPlan>,

    online: bool,
    wifi_dev: Option<String>,
    networks: Vec<String>,
    net_sel: usize,
    wifi_pass: String,
    pass_focus: bool,
    net_status: String,

    disks: Vec<Disk>,
    disk_sel: usize,
    disk_field: usize, // 0 disk list, 1 filesystem, 2 swap
    fs_idx: usize,     // 0 ext4, 1 btrfs
    swap_idx: usize,   // 0 zram, 1 none

    manifests: Vec<String>,
    man_sel: usize,
    man_input: String,
    man_typing: bool,
}

impl App {
    fn new() -> Self {
        App {
            screen: Screen::Welcome,
            quit: false,
            plan: None,
            online: probe::is_online(),
            wifi_dev: probe::wifi_device(),
            networks: Vec::new(),
            net_sel: 0,
            wifi_pass: String::new(),
            pass_focus: false,
            net_status: String::new(),
            disks: probe::list_disks(),
            disk_sel: 0,
            disk_field: 0,
            fs_idx: 0,
            swap_idx: 0,
            manifests: probe::bundled_manifests(),
            man_sel: 0,
            man_input: String::new(),
            man_typing: false,
        }
    }

    fn step_index(&self) -> usize {
        match self.screen {
            Screen::Welcome => 0,
            Screen::Network => 1,
            Screen::Disk => 2,
            Screen::Manifest => 3,
            Screen::Confirm => 4,
        }
    }
}

/// Run the wizard. Returns the plan to install, or None if the user quit.
pub fn run() -> Result<Option<InstallPlan>> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let mut app = App::new();
    let result = event_loop(&mut app, &mut terminal);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result?;
    Ok(app.plan)
}

fn event_loop<B: Backend>(app: &mut App, terminal: &mut Terminal<B>) -> Result<()> {
    while !app.quit {
        terminal.draw(|f| draw(f, app))?;
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    handle_key(app, key.code);
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Input handling
// ---------------------------------------------------------------------------

fn handle_key(app: &mut App, key: KeyCode) {
    // Text entry modes consume keys first.
    if app.pass_focus {
        match key {
            KeyCode::Enter => {
                if let (Some(dev), Some(ssid)) =
                    (app.wifi_dev.clone(), app.networks.get(app.net_sel).cloned())
                {
                    let (online, status) = probe::connect_wifi(&dev, &ssid, &app.wifi_pass);
                    app.online = online;
                    app.net_status = status;
                }
                app.pass_focus = false;
            }
            KeyCode::Esc => app.pass_focus = false,
            KeyCode::Backspace => {
                app.wifi_pass.pop();
            }
            KeyCode::Char(c) => app.wifi_pass.push(c),
            _ => {}
        }
        return;
    }
    if app.man_typing {
        match key {
            KeyCode::Enter | KeyCode::Esc => app.man_typing = false,
            KeyCode::Backspace => {
                app.man_input.pop();
            }
            KeyCode::Char(c) => app.man_input.push(c),
            _ => {}
        }
        return;
    }

    match key {
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Esc => back(app),
        _ => match app.screen {
            Screen::Welcome => {
                if key == KeyCode::Enter {
                    app.screen = Screen::Network;
                }
            }
            Screen::Network => network_keys(app, key),
            Screen::Disk => disk_keys(app, key),
            Screen::Manifest => manifest_keys(app, key),
            Screen::Confirm => {
                if key == KeyCode::Enter {
                    app.plan = Some(build_plan(app));
                    app.quit = true;
                }
            }
        },
    }
}

fn back(app: &mut App) {
    app.screen = match app.screen {
        Screen::Welcome => {
            app.quit = true;
            Screen::Welcome
        }
        Screen::Network => Screen::Welcome,
        Screen::Disk => Screen::Network,
        Screen::Manifest => Screen::Disk,
        Screen::Confirm => Screen::Manifest,
    };
}

fn network_keys(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Up => app.net_sel = app.net_sel.saturating_sub(1),
        KeyCode::Down => {
            if !app.networks.is_empty() {
                app.net_sel = (app.net_sel + 1).min(app.networks.len() - 1);
            }
        }
        KeyCode::Char('s') => {
            if let Some(dev) = &app.wifi_dev {
                app.networks = probe::scan_wifi(dev);
                app.net_sel = 0;
            }
        }
        KeyCode::Char('p') => {
            if !app.networks.is_empty() {
                app.pass_focus = true;
                app.wifi_pass.clear();
            }
        }
        KeyCode::Enter => {
            // Online or chose to proceed -> next.
            app.online = probe::is_online();
            app.screen = Screen::Disk;
        }
        _ => {}
    }
}

fn disk_keys(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Tab => app.disk_field = (app.disk_field + 1) % 3,
        KeyCode::Up | KeyCode::Down => {
            let dir_down = key == KeyCode::Down;
            match app.disk_field {
                0 => {
                    if !app.disks.is_empty() {
                        app.disk_sel = move_sel(app.disk_sel, app.disks.len(), dir_down);
                    }
                }
                1 => app.fs_idx ^= 1,
                2 => app.swap_idx ^= 1,
                _ => {}
            }
        }
        KeyCode::Enter => {
            if !app.disks.is_empty() {
                app.screen = Screen::Manifest;
            }
        }
        _ => {}
    }
}

fn manifest_keys(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Up => app.man_sel = app.man_sel.saturating_sub(1),
        KeyCode::Down => app.man_sel = (app.man_sel + 1).min(app.manifests.len()),
        KeyCode::Char('u') => {
            app.man_typing = true;
            app.man_sel = app.manifests.len(); // the "custom" row
        }
        KeyCode::Enter => app.screen = Screen::Confirm,
        _ => {}
    }
}

fn move_sel(cur: usize, len: usize, down: bool) -> usize {
    if down {
        (cur + 1).min(len - 1)
    } else {
        cur.saturating_sub(1)
    }
}

fn build_plan(app: &App) -> InstallPlan {
    let manifest = if app.man_sel < app.manifests.len() {
        app.manifests[app.man_sel].clone()
    } else {
        app.man_input.clone()
    };
    InstallPlan {
        disk: app.disks.get(app.disk_sel).map(|d| d.name.clone()).unwrap_or_default(),
        install_mode: "erase".to_string(),
        filesystem: ["ext4", "btrfs"][app.fs_idx].to_string(),
        swap: ["zram", "none"][app.swap_idx].to_string(),
        manifest,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    f.render_widget(Block::default().style(Style::default().bg(BASE)), area);

    let rows = Layout::vertical([
        Constraint::Length(3), // header + breadcrumb
        Constraint::Min(0),    // body
        Constraint::Length(1), // footer
    ])
    .split(area);

    draw_header(f, rows[0], app);
    let body = centered(rows[1], 78, 80);
    match app.screen {
        Screen::Welcome => draw_welcome(f, body),
        Screen::Network => draw_network(f, body, app),
        Screen::Disk => draw_disk(f, body, app),
        Screen::Manifest => draw_manifest(f, body, app),
        Screen::Confirm => draw_confirm(f, body, app),
    }
    draw_footer(f, rows[2], app);
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let cur = app.step_index();
    let mut spans = vec![Span::styled(" Manifest OS ", Style::default().fg(BASE).bg(ACCENT).bold()), Span::raw("  ")];
    for (i, s) in STEPS.iter().enumerate() {
        let style = if i == cur {
            Style::default().fg(ACCENT).bold()
        } else if i < cur {
            Style::default().fg(GREEN)
        } else {
            Style::default().fg(SUBT)
        };
        let mark = if i < cur { "✓ " } else { "" };
        spans.push(Span::styled(format!("{mark}{s}"), style));
        if i < STEPS.len() - 1 {
            spans.push(Span::styled("  ›  ", Style::default().fg(SUBT)));
        }
    }
    let p = Paragraph::new(Line::from(spans)).block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(SURFACE)),
    );
    f.render_widget(p, area);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let hint = if app.pass_focus || app.man_typing {
        "type · Enter confirm · Esc cancel"
    } else {
        match app.screen {
            Screen::Welcome => "Enter begin · q quit",
            Screen::Network => "↑↓ select · s scan · p password · Enter continue · Esc back",
            Screen::Disk => "Tab field · ↑↓ change · Enter continue · Esc back",
            Screen::Manifest => "↑↓ select · u enter URL · Enter continue · Esc back",
            Screen::Confirm => "Enter INSTALL · Esc back · q quit",
        }
    };
    f.render_widget(
        Paragraph::new(Span::styled(format!("  {hint}"), Style::default().fg(SUBT))),
        area,
    );
}

fn card<'a>(title: &'a str) -> Block<'a> {
    Block::default()
        .title(Span::styled(format!(" {title} "), Style::default().fg(ACCENT).bold()))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(SURFACE))
        .padding(Padding::new(2, 2, 1, 1))
}

fn draw_welcome(f: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled("Welcome to Manifest OS", Style::default().fg(TEXT).bold())),
        Line::from(Span::styled("Declare it. Share it. Deploy it.", Style::default().fg(SUBT))),
        Line::from(""),
        Line::from(Span::styled("This installer will guide you through four steps:", Style::default().fg(TEXT))),
        Line::from(Span::styled("  1. Connect to the internet", Style::default().fg(SUBT))),
        Line::from(Span::styled("  2. Choose a disk to install onto", Style::default().fg(SUBT))),
        Line::from(Span::styled("  3. Pick a manifest (your system, declared)", Style::default().fg(SUBT))),
        Line::from(Span::styled("  4. Sit back while it installs", Style::default().fg(SUBT))),
        Line::from(""),
        Line::from(Span::styled("Press Enter to begin →", Style::default().fg(GREEN).bold())),
    ];
    f.render_widget(Paragraph::new(lines).block(card("Welcome")), area);
}

fn draw_network(f: &mut Frame, area: Rect, app: &App) {
    let mut lines = Vec::new();
    if app.online {
        let msg = if app.net_status.is_empty() { "✓ You're online.".to_string() } else { app.net_status.clone() };
        lines.push(Line::from(Span::styled(msg, Style::default().fg(GREEN).bold())));
        lines.push(Line::from(Span::styled("Press Enter to continue.", Style::default().fg(SUBT))));
    } else if app.wifi_dev.is_some() {
        // Surface the last connection result (e.g. wrong password) prominently.
        if !app.net_status.is_empty() {
            lines.push(Line::from(Span::styled(app.net_status.clone(), Style::default().fg(RED).bold())));
            lines.push(Line::from(""));
        }
        if app.pass_focus {
            let masked: String = "•".repeat(app.wifi_pass.len());
            let target = app.networks.get(app.net_sel).cloned().unwrap_or_default();
            lines.push(Line::from(Span::styled(format!("Connecting to: {target}"), Style::default().fg(TEXT))));
            lines.push(Line::from(vec![
                Span::styled("Password: ", Style::default().fg(TEXT)),
                Span::styled(masked, Style::default().fg(ACCENT)),
                Span::styled("▏", Style::default().fg(ACCENT)),
            ]));
        } else {
            let header = if app.networks.is_empty() {
                "Not connected. Press 's' to scan for WiFi.".to_string()
            } else {
                format!("Available networks ({}):", app.networks.len())
            };
            lines.push(Line::from(Span::styled(header, Style::default().fg(YELLOW))));
            lines.push(Line::from(""));
            for (i, n) in app.networks.iter().enumerate() {
                let sel = i == app.net_sel;
                let style = if sel { Style::default().fg(BASE).bg(ACCENT) } else { Style::default().fg(TEXT) };
                lines.push(Line::from(Span::styled(format!("  {n}  "), style)));
            }
            if !app.networks.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("↑↓ pick · 'p' enter password · 's' rescan", Style::default().fg(SUBT))));
            }
        }
    } else {
        lines.push(Line::from(Span::styled("No WiFi adapter found.", Style::default().fg(YELLOW))));
        lines.push(Line::from(Span::styled("Plug in ethernet — it connects automatically.", Style::default().fg(SUBT))));
        lines.push(Line::from(Span::styled("Press Enter to re-check and continue.", Style::default().fg(SUBT))));
    }
    f.render_widget(Paragraph::new(lines).block(card("Step 1 — Network")), area);
}

fn draw_disk(f: &mut Frame, area: Rect, app: &App) {
    let mut lines = vec![Line::from(Span::styled("Choose a disk:", Style::default().fg(TEXT).bold()))];
    if app.disks.is_empty() {
        lines.push(Line::from(Span::styled("  (no disks detected)", Style::default().fg(RED))));
    }
    for (i, d) in app.disks.iter().enumerate() {
        let sel = app.disk_field == 0 && i == app.disk_sel;
        let style = if sel { Style::default().fg(BASE).bg(ACCENT) } else { Style::default().fg(TEXT) };
        lines.push(Line::from(Span::styled(
            format!("  {:<10} {:>8}  {}", d.name, d.size, d.model),
            style,
        )));
    }
    lines.push(Line::from(""));
    lines.push(toggle_line("Filesystem", &["ext4", "btrfs"], app.fs_idx, app.disk_field == 1));
    lines.push(toggle_line("Swap      ", &["zram", "none"], app.swap_idx, app.disk_field == 2));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "⚠  The selected disk will be ERASED.",
        Style::default().fg(RED).bold(),
    )));
    f.render_widget(Paragraph::new(lines).block(card("Step 2 — Disk")), area);
}

fn toggle_line<'a>(label: &'a str, opts: &[&'a str], idx: usize, focused: bool) -> Line<'a> {
    let mut spans = vec![Span::styled(format!("  {label}  "), Style::default().fg(if focused { ACCENT } else { SUBT }))];
    for (i, o) in opts.iter().enumerate() {
        let style = if i == idx {
            Style::default().fg(BASE).bg(if focused { ACCENT } else { SUBT })
        } else {
            Style::default().fg(SUBT)
        };
        spans.push(Span::styled(format!(" {o} "), style));
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

fn draw_manifest(f: &mut Frame, area: Rect, app: &App) {
    let mut lines = vec![Line::from(Span::styled("Pick a manifest to install:", Style::default().fg(TEXT).bold())), Line::from("")];
    for (i, m) in app.manifests.iter().enumerate() {
        let sel = i == app.man_sel;
        let style = if sel { Style::default().fg(BASE).bg(ACCENT) } else { Style::default().fg(TEXT) };
        lines.push(Line::from(Span::styled(format!("  {m}  "), style)));
    }
    // Custom URL row.
    let custom_sel = app.man_sel == app.manifests.len();
    let style = if custom_sel { Style::default().fg(BASE).bg(ACCENT) } else { Style::default().fg(TEXT) };
    let shown = if app.man_input.is_empty() { "enter URL or path…".to_string() } else { app.man_input.clone() };
    lines.push(Line::from(Span::styled(format!("  ⌨  {shown}  "), style)));
    if app.man_typing {
        lines.push(Line::from(Span::styled("  (typing — Enter to accept)", Style::default().fg(SUBT))));
    }
    f.render_widget(Paragraph::new(lines).block(card("Step 3 — Manifest")), area);
}

fn draw_confirm(f: &mut Frame, area: Rect, app: &App) {
    let plan = build_plan(app);
    let lines = vec![
        Line::from(Span::styled("Ready to install. Please review:", Style::default().fg(TEXT).bold())),
        Line::from(""),
        kv("Network", if app.online { "connected" } else { "see step 1" }),
        kv("Disk", &format!("{}  ({} will be erased)", plan.disk, plan.disk)),
        kv("Filesystem", &plan.filesystem),
        kv("Swap", &plan.swap),
        kv("Manifest", &plan.manifest),
        Line::from(""),
        Line::from(Span::styled("⚠  Pressing Enter ERASES the disk and installs.", Style::default().fg(RED).bold())),
        Line::from(Span::styled("Press Esc to go back and change anything.", Style::default().fg(SUBT))),
    ];
    f.render_widget(Paragraph::new(lines).block(card("Step 4 — Confirm & Install")), area);
}

fn kv(k: &str, v: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {k:<12}"), Style::default().fg(SUBT)),
        Span::styled(v.to_string(), Style::default().fg(TEXT)),
    ])
}

fn centered(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let v = Layout::vertical([
        Constraint::Percentage((100 - pct_y) / 2),
        Constraint::Percentage(pct_y),
        Constraint::Percentage((100 - pct_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - pct_x) / 2),
        Constraint::Percentage(pct_x),
        Constraint::Percentage((100 - pct_x) / 2),
    ])
    .split(v[1])[1]
}
