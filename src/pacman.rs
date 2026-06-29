//! pacman repo configuration and paru bootstrap.
//!
//! Everything here is idempotent: each step checks whether it's already done
//! before acting, so `manifest install` is safe to re-run on the same VM.

use crate::exec::Ctx;
use crate::kernel::Kernel;
use crate::manifest::Manifest;
use anyhow::Result;

const PACMAN_CONF: &str = "/etc/pacman.conf";
// Bootstrap from the *source* paru package, not paru-bin. The prebuilt -bin
// binary links against a fixed libalpm soname and breaks whenever pacman bumps
// its ABI (e.g. libalpm.so.15 -> .16). Building from source links against the
// installed libalpm, so it is always ABI-correct on a freshly upgraded system.
const PARU_AUR: &str = "https://aur.archlinux.org/paru.git";

/// Step 3 — enable multilib / CachyOS repos as declared, then refresh the
/// package databases so later steps can see them. CachyOS is also implied by
/// `kernel: "cachy"`, since linux-cachyos lives in that repo.
pub fn enable_repos(manifest: &Manifest, kernel: &Kernel, ctx: &Ctx) -> Result<()> {
    let repos = &manifest.repos;
    let needs_cachy = repos.cachyos || kernel.needs_cachyos_repo;

    if repos.multilib {
        if ctx.check("grep", &["-q", r"^\[multilib\]", PACMAN_CONF]) {
            println!("  · multilib already enabled");
        } else {
            println!("  · enabling [multilib]");
            // Uncomment the [multilib] header and its adjacent Include line.
            ctx.sudo(
                "sed",
                &["-i", r"/\[multilib\]/,/Include/ s/^#//", PACMAN_CONF],
            )?;
        }
    }

    if needs_cachy {
        if repo_present(ctx, "cachyos") {
            println!("  · cachyos repo already present");
        } else {
            println!("  · adding cachyos repo + signing key");
            add_cachyos_repo(ctx)?;
        }
        if repos.cachy_optimized_packages {
            println!("  · cachyos-v3/v4 optimized repos handled by cachyos-repo script");
        }
    }
    Ok(())
}

/// Full system upgrade. Run after enabling repos and before building any AUR
/// package. This is mandatory on Arch: a bare `pacman -Sy` (refresh without
/// upgrade) leaves the system in a partial-upgrade state, so a freshly built
/// AUR package can link against a newer libalpm than the one installed. Always
/// `-Syu`, never `-Sy`.
pub fn sync_system(ctx: &Ctx) -> Result<()> {
    ctx.sudo("pacman", &["-Syu", "--noconfirm"])
}

/// Whether a named repo is configured in pacman.conf.
fn repo_present(ctx: &Ctx, repo: &str) -> bool {
    ctx.check("grep", &["-q", &format!(r"^\[{repo}\]"), PACMAN_CONF])
}

/// Run the official CachyOS repo bootstrap, which imports the signing key and
/// appends the repos to pacman.conf.
fn add_cachyos_repo(ctx: &Ctx) -> Result<()> {
    let script = "\
        cd \"$(mktemp -d)\" && \
        curl -fsSL https://mirror.cachyos.org/cachyos-repo.tar.xz -o cachyos-repo.tar.xz && \
        tar xf cachyos-repo.tar.xz && \
        cd cachyos-repo && \
        sudo ./cachyos-repo.sh";
    // cachyos-repo.sh refuses to run unless EUID==0 (it does NOT self-escalate),
    // so it must be invoked *with* sudo. The wrapping shell stays at user level
    // because the surrounding curl/tar/mktemp do not need root.
    ctx.shell(script, false)
}

/// Step 4 — ensure paru exists. paru is the one hardcoded AUR helper.
/// Bootstrapped from the AUR: base-devel + git, clone paru-bin, makepkg -si.
///
/// makepkg refuses to run as root, so the clone/build run at user level; only
/// the dependency install uses sudo.
pub fn bootstrap_paru(ctx: &Ctx) -> Result<()> {
    if ctx.check("sh", &["-c", "command -v paru"]) {
        println!("  · paru already installed");
        return Ok(());
    }
    println!("  · installing build prerequisites (base-devel, git)");
    ctx.sudo(
        "pacman",
        &["-S", "--needed", "--noconfirm", "base-devel", "git"],
    )?;

    println!("  · building paru from the AUR (low-memory mode)");
    let build = format!(
        "cd \"$(mktemp -d)\" && \
         git clone --depth 1 {PARU_AUR} && \
         cd paru && \
         MAKEFLAGS=-j1 CARGO_BUILD_JOBS=1 makepkg -si --noconfirm"
    );
    ctx.shell(&build, false)
}

/// Step 6 — install every package (plus the kernel package) via paru. paru
/// transparently resolves both official-repo and AUR packages in one call and
/// escalates with sudo internally, so it runs at user level.
pub fn install_packages(
    manifest: &Manifest,
    kernel: &Kernel,
    extra: &[String],
    ctx: &Ctx,
) -> Result<()> {
    // Order: kernel + headers, then desktop-recipe packages, then the
    // manifest's own list. De-duplicated, first occurrence wins.
    let mut pkgs: Vec<&str> = vec![kernel.package, kernel.headers];
    for p in extra.iter().chain(manifest.packages.iter()) {
        if !pkgs.contains(&p.as_str()) {
            pkgs.push(p.as_str());
        }
    }

    if pkgs.is_empty() {
        return Ok(());
    }
    println!("  installing {} package(s)", pkgs.len());

    let mut args = vec![
        "MAKEFLAGS=-j1".to_string(),
        "CARGO_BUILD_JOBS=1".to_string(),
        "paru".to_string(),
        "-S".to_string(),
        "--needed".to_string(),
        "--noconfirm".to_string(),
    ];
    args.extend(pkgs.into_iter().map(shell_quote));
    ctx.shell(&args.join(" "), false)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
