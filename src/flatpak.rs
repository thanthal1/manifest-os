//! Flatpak remotes and apps.
//!
//! This replaces simple Flatpak setup hooks with a declarative block:
//! add remotes idempotently, then install or update app ids system-wide.

use crate::exec::Ctx;
use crate::manifest::Flatpak;
use anyhow::Result;

const FLATHUB_URL: &str = "https://flathub.org/repo/flathub.flatpakrepo";

pub fn apply(flatpak: &Flatpak, ctx: &Ctx) -> Result<()> {
    if flatpak.is_empty() {
        return Ok(());
    }

    ensure_flatpak(ctx)?;

    let app_remote = flatpak
        .remotes
        .first()
        .map(|r| r.name.as_str())
        .unwrap_or("flathub");

    if flatpak.remotes.is_empty() && !flatpak.apps.is_empty() {
        add_remote("flathub", FLATHUB_URL, ctx)?;
    }

    for remote in &flatpak.remotes {
        add_remote(&remote.name, &remote.url, ctx)?;
    }

    for app in &flatpak.apps {
        install_app(app_remote, app, ctx)?;
    }

    Ok(())
}

fn ensure_flatpak(ctx: &Ctx) -> Result<()> {
    if ctx.check("flatpak", &["--version"]) {
        println!("  * flatpak already installed");
        return Ok(());
    }
    println!("  * installing flatpak");
    ctx.sudo("pacman", &["-S", "--needed", "--noconfirm", "flatpak"])
}

fn add_remote(name: &str, url: &str, ctx: &Ctx) -> Result<()> {
    println!("  * flatpak remote: {name}");
    ctx.sudo(
        "flatpak",
        &["remote-add", "--system", "--if-not-exists", name, url],
    )
}

fn install_app(remote: &str, app: &str, ctx: &Ctx) -> Result<()> {
    println!("  * flatpak app: {app}");
    ctx.sudo(
        "flatpak",
        &[
            "install",
            "--system",
            "-y",
            "--noninteractive",
            "--or-update",
            remote,
            app,
        ],
    )
}
