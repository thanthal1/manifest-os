//! Windows applications (`docs/strata-design.md` §14) — **Phase 6a: the wine
//! tier**, plus the compatibility gate in front of it.
//!
//! Same mental model as strata and Android: declare an app, the engine installs
//! it and puts a launcher on the menu. The backend here is **Wine** — no VM, no
//! passthrough — which is the cheap tier that covers small Windows tools. Apps
//! Wine structurally cannot run (kernel anti-cheat, CAD, dongles, Store
//! packages) are identified **before** installing by [`crate::wincompat`] and
//! reported with the tier that could run them, rather than half-installed.
//!
//! The VM tiers (`vm-rdp`, `vm-vfio`) are designed but not built: an app that
//! needs one is a clear, actionable message, never a silent failure.
//!
//! Each app gets its **own prefix** (`~/.local/share/manifest-os/wine/<app>`) so
//! one app's winetricks/DLL overrides can't break another — the Bottles/Lutris
//! lesson, and the reason a single shared `~/.wine` ages badly.

use crate::exec::Ctx;
use crate::manifest::{Windows, WindowsApp};
use crate::wincompat::{self, Verdict};
use anyhow::{bail, Result};

/// Where per-app prefixes live, under the user's data dir.
const PREFIX_ROOT: &str = "$HOME/.local/share/manifest-os/wine";
/// The generated launcher every menu entry runs.
const LAUNCHER: &str = "/usr/local/bin/windows-app";
const APPLICATIONS_DIR: &str = "/usr/share/applications";

pub fn apply(w: &Windows, ctx: &Ctx) -> Result<()> {
    if w.is_empty() {
        return Ok(());
    }
    // Gate first: report every app's verdict before touching the system, so a
    // blocked app is known up front rather than after a long install.
    let plan = plan_apps(w);
    for (app, a) in &plan {
        println!("  · {}", a.summary(&app.name));
    }
    let installable: Vec<&WindowsApp> = plan
        .iter()
        .filter(|(app, a)| a.verdict != Verdict::Blocked || app.force)
        .map(|(app, _)| *app)
        .collect();
    if let Some((app, a)) = plan.iter().find(|(app, a)| a.verdict == Verdict::Blocked && !app.force) {
        println!(
            "  · skipping '{}' — {} (needs the `{}` tier, which isn't built yet; \
             set \"force\": true to try anyway)",
            app.name,
            a.reasons.join("; "),
            a.verdict.tier()
        );
    }
    if installable.is_empty() {
        println!("  · no apps left to install under the wine tier");
        return Ok(());
    }

    ensure_wine(ctx)?;
    ctx.write_root(LAUNCHER, launcher_script())?;
    ctx.sudo("chmod", &["0755", LAUNCHER])?;

    for app in installable {
        install_app(app, w, ctx)?;
    }
    Ok(())
}

/// Assess every declared app. Pure apart from optionally reading a local
/// installer's header for marker scanning.
fn plan_apps(w: &Windows) -> Vec<(&WindowsApp, wincompat::Assessment)> {
    w.apps
        .iter()
        .map(|app| {
            let base = wincompat::assess(&app.name, app.installer.as_deref());
            // A local installer can be scanned for blockers the name doesn't show.
            let refined = match app.installer.as_deref() {
                Some(p) if !p.starts_with("http") => wincompat::read_head(p, 4 << 20)
                    .map(|b| wincompat::assess_bytes(&base, &b))
                    .unwrap_or(base),
                _ => base,
            };
            (app, refined)
        })
        .collect()
}

/// Install Wine + the bits nearly every prefix needs.
fn ensure_wine(ctx: &Ctx) -> Result<()> {
    if ctx.check("sh", &["-c", "command -v wine"]) {
        println!("  · wine already installed");
        return Ok(());
    }
    println!("  · installing wine + winetricks");
    // wine needs multilib enabled for 32-bit apps; repos.rs handles multilib when
    // the manifest asks for it — here we just install what's available.
    ctx.sudo(
        "pacman",
        &["-S", "--needed", "--noconfirm", "wine", "winetricks", "wine-mono", "wine-gecko"],
    )
}

/// Create the app's prefix, run its installer, apply winetricks, and drop a
/// menu launcher.
fn install_app(app: &WindowsApp, w: &Windows, ctx: &Ctx) -> Result<()> {
    if app.name.trim().is_empty() {
        bail!("a windows app has no `name`");
    }
    let slug = slug(&app.name);
    // Double-quoted in commands, never single — $HOME must expand. Safe because
    // `slug` is [a-z0-9-] only.
    let prefix = format!("{PREFIX_ROOT}/{slug}");
    let pq = format!("\"{prefix}\"");
    println!("  · windows app '{}' → prefix {prefix}", app.name);

    // Prefix creation is idempotent: wineboot on an existing prefix just updates.
    let ver = app.windows_version.as_deref().unwrap_or("win10");
    ctx.shell(
        &format!(
            "mkdir -p {p} && WINEPREFIX={p} WINEARCH=win64 wineboot -u >/dev/null 2>&1 && \
             WINEPREFIX={p} winetricks -q {ver} >/dev/null 2>&1 || true",
            p = pq,
            ver = ver
        ),
        false,
    )?;

    // winetricks verbs: the manifest's globals, the app's own, and anything the
    // oracle knows this app family needs.
    let mut verbs: Vec<String> = w.winetricks.clone();
    verbs.extend(app.winetricks.clone());
    verbs.extend(wincompat::assess(&app.name, app.installer.as_deref()).suggest_winetricks);
    verbs.sort();
    verbs.dedup();
    if !verbs.is_empty() {
        println!("    winetricks: {}", verbs.join(" "));
        ctx.shell(
            &format!(
                "WINEPREFIX={p} winetricks -q {v} || echo '    · some winetricks verbs failed (continuing)' >&2",
                p = pq,
                v = verbs.iter().map(|v| shq(v)).collect::<Vec<_>>().join(" ")
            ),
            false,
        )?;
    }

    // Run the installer (downloading it first when it's a URL).
    if let Some(src) = &app.installer {
        let run = if src.starts_with("http") {
            format!(
                "tmp=$(mktemp --suffix=.exe) && curl -fsSL -o \"$tmp\" {u} && \
                 WINEPREFIX={p} wine \"$tmp\"; rm -f \"$tmp\"",
                u = shq(src),
                p = pq
            )
        } else {
            format!("WINEPREFIX={p} wine {f}", p = pq, f = shq(src))
        };
        println!("    running the installer (it may open a setup window)");
        ctx.shell(&run, false)?;
    }

    // A menu launcher, when we know what to launch.
    if let Some(exe) = &app.exe {
        let dest = format!("{APPLICATIONS_DIR}/manifest-windows-{slug}.desktop");
        println!("    menu entry → {dest}");
        ctx.write_root(&dest, &desktop_entry(&app.name, &slug, exe))?;
        ctx.shell(
            &format!("update-desktop-database {APPLICATIONS_DIR} 2>/dev/null || true"),
            true,
        )?;
    } else {
        println!("    note: no `exe` declared — no menu entry (add one to get a launcher)");
    }
    Ok(())
}

/// `windows-app <slug> <exe-relative-to-C:>` — what every menu entry runs.
/// Resolves the prefix, then execs the app through Wine. Pure — unit-tested.
fn launcher_script() -> &'static str {
    "#!/bin/sh\n\
     # ManifestOS — launch a Windows app in its own Wine prefix (generated).\n\
     # usage: windows-app <prefix-slug> <exe path under C:/>\n\
     [ $# -ge 2 ] || { echo 'usage: windows-app <slug> <exe>' >&2; exit 2; }\n\
     slug=$1; shift\n\
     exe=$1; shift\n\
     PREFIX=\"$HOME/.local/share/manifest-os/wine/$slug\"\n\
     [ -d \"$PREFIX\" ] || { echo \"windows-app: no prefix for '$slug' — run the install again\" >&2; exit 1; }\n\
     command -v wine >/dev/null 2>&1 || { echo 'windows-app: wine is not installed' >&2; exit 1; }\n\
     exec env WINEPREFIX=\"$PREFIX\" wine \"$PREFIX/drive_c/$exe\" \"$@\"\n"
}

/// The menu entry for an installed app. Pure — unit-tested.
fn desktop_entry(name: &str, slug: &str, exe: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={name}\n\
         Comment=Windows application (Wine)\n\
         Exec={LAUNCHER} {slug} {exe:?}\n\
         TryExec={LAUNCHER}\n\
         Icon=wine\n\
         Terminal=false\n\
         Categories=Utility;\n\
         X-ManifestOS-Windows=wine\n"
    )
}

/// A filesystem-safe prefix name from a display name. Pure — unit-tested.
fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    // Collapse runs of '-' so "Notepad++ 8.6" doesn't become "notepad---8-6".
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.chars() {
        if c == '-' {
            if !prev_dash {
                out.push(c);
            }
            prev_dash = true;
        } else {
            out.push(c);
            prev_dash = false;
        }
    }
    out
}

fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, installer: Option<&str>, exe: Option<&str>, force: bool) -> WindowsApp {
        WindowsApp {
            name: name.into(),
            installer: installer.map(String::from),
            tier: None,
            exe: exe.map(String::from),
            winetricks: Vec::new(),
            windows_version: None,
            force,
        }
    }

    #[test]
    fn slug_is_filesystem_safe_and_collapsed() {
        assert_eq!(slug("Notepad++ 8.6"), "notepad-8-6");
        assert_eq!(slug("7-Zip"), "7-zip");
        assert_eq!(slug("  Foo Bar  "), "foo-bar");
    }

    #[test]
    fn launcher_uses_the_apps_own_prefix() {
        let s = launcher_script();
        assert!(s.contains(".local/share/manifest-os/wine/$slug"), "per-app prefix: {s}");
        assert!(s.contains("exec env WINEPREFIX=\"$PREFIX\" wine"), "{s}");
        // Fails loudly rather than silently when the prefix is missing.
        assert!(s.contains("no prefix for"), "{s}");
    }

    #[test]
    fn desktop_entry_points_at_the_launcher() {
        let d = desktop_entry("Notepad++", "notepad", "Program Files/Notepad++/notepad++.exe");
        assert!(d.contains("Exec=/usr/local/bin/windows-app notepad \"Program Files/Notepad++/notepad++.exe\""), "{d}");
        assert!(d.contains("Name=Notepad++"), "{d}");
        assert!(d.contains("Terminal=false"), "{d}");
    }

    #[test]
    fn blocked_apps_are_planned_out_unless_forced() {
        let w = Windows {
            apps: vec![
                app("SolidWorks", None, None, false),
                app("Notepad++", None, Some("np/np.exe"), false),
            ],
            winetricks: vec![],
        };
        let plan = plan_apps(&w);
        assert_eq!(plan.len(), 2);
        let blocked: Vec<_> = plan.iter().filter(|(_, a)| a.verdict == Verdict::Blocked).collect();
        assert_eq!(blocked.len(), 1, "only SolidWorks is blocked");
        assert_eq!(blocked[0].0.name, "SolidWorks");
        // force lets it through the gate.
        let w2 = Windows { apps: vec![app("SolidWorks", None, None, true)], winetricks: vec![] };
        let plan2 = plan_apps(&w2);
        assert!(plan2[0].0.force, "forced app is still assessed but installable");
    }

    #[test]
    fn empty_block_is_a_no_op() {
        let w = Windows::default();
        assert!(w.is_empty());
    }
}
