//! pacman repo configuration and paru bootstrap.
//!
//! Everything here is idempotent: each step checks whether it's already done
//! before acting, so `manifest install` is safe to re-run on the same VM.

use crate::exec::Ctx;
use crate::kernel::Kernel;
use crate::manifest::Manifest;
use anyhow::Result;
use std::collections::HashSet;

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
            // The cachyos-repo script enables the v3/v4 *optimized package* repos
            // whenever the CPU supports them — which reroutes the whole base
            // system (gcc, pipewire, …) through the single CachyOS CDN. That's an
            // install-killing single point of failure when the CDN is flaky, and
            // usually not what someone who just set `kernel: "cachy"` wanted. So
            // unless they explicitly opted into optimized packages, disable those
            // repos: linux-cachyos still installs from plain [cachyos], and every
            // other package comes from Arch's many mirrors. Done here (freshly
            // added) so a re-sync doesn't reprocess an already-edited config.
            if repos.cachy_optimized_packages {
                println!("  · keeping CachyOS v3/v4 optimized package repos (opted in)");
            } else {
                disable_optimized_cachyos_repos(ctx)?;
            }
        }
    }
    Ok(())
}

/// Comment out the CachyOS *optimized* repos (`[cachyos-v3]`, `[cachyos-v4]`,
/// `[cachyos-core-v3]`, `[cachyos-znver4]`, …) in the live pacman.conf, leaving
/// plain `[cachyos]` (which carries linux-cachyos) intact.
fn disable_optimized_cachyos_repos(ctx: &Ctx) -> Result<()> {
    let conf = std::fs::read_to_string(PACMAN_CONF).unwrap_or_default();
    if conf.is_empty() {
        return Ok(());
    }
    let edited = without_optimized_cachyos(&conf);
    if edited != conf {
        println!(
            "  · disabled CachyOS v3/v4 optimized repos — base packages come from Arch mirrors\n\
             \x20   (set repos.cachy_optimized_packages: true to keep the optimized builds)"
        );
        ctx.write_root(PACMAN_CONF, &edited)?;
    }
    Ok(())
}

/// Return `conf` with every CachyOS *optimized* repo block commented out, plain
/// `[cachyos]` untouched. A block is its `[header]` plus the following lines up
/// to the next section header or a blank line. Pure — unit-tested.
fn without_optimized_cachyos(conf: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    for line in conf.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            let name = &t[1..t.len() - 1];
            // An *optimized* CachyOS repo: cachyos + a v3/v4/znver ISA marker.
            // Plain "cachyos" (the kernel repo) has none, so it's kept.
            skipping = name.starts_with("cachyos")
                && (name.contains("v3") || name.contains("v4") || name.contains("znver"));
        } else if t.is_empty() {
            skipping = false; // blank line ends the block
        }
        if skipping && !t.is_empty() && !t.starts_with('#') {
            out.push('#');
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Full system upgrade. Run after enabling repos and before building any AUR
/// package. This is mandatory on Arch: a bare `pacman -Sy` (refresh without
/// upgrade) leaves the system in a partial-upgrade state, so a freshly built
/// AUR package can link against a newer libalpm than the one installed. Always
/// `-Syu`, never `-Sy`.
pub fn sync_system(ctx: &Ctx) -> Result<()> {
    // --disable-download-timeout: don't abort a slow-but-progressing download
    // (a busy mirror trickling bytes) — pacman's default kills it at <1 B/s.
    // Retried: a mirror blip mid-`-Syu` shouldn't sink the install.
    ctx.shell(
        &with_retries("pacman -Syu --noconfirm --disable-download-timeout", 3),
        true,
    )
}

/// Wrap a package command in a bounded retry loop. A single flaky mirror — the
/// CachyOS CDN especially, which pacman drops for a whole transaction after a
/// few errors ("too many errors from …, skipping") — must not sink an install on
/// a transient blip. pacman/paru resume cleanly (`--needed`), so re-running is
/// safe. `cmd` runs under `sh -c`; keep it a single pipeline.
fn with_retries(cmd: &str, tries: u32) -> String {
    format!(
        "n=0; until {cmd}; do \
           n=$((n+1)); \
           if [ \"$n\" -ge {tries} ]; then echo '  · still failing after {tries} attempts — giving up' >&2; exit 1; fi; \
           echo \"  · package step failed (mirror hiccup?) — retry $n/{tries} in 12s\"; sleep 12; \
         done"
    )
}

/// Whether a named repo is configured in pacman.conf.
fn repo_present(ctx: &Ctx, repo: &str) -> bool {
    ctx.check("grep", &["-q", &format!(r"^\[{repo}\]"), PACMAN_CONF])
}

/// Run the official CachyOS repo bootstrap, which imports the signing key and
/// appends the repos to pacman.conf.
fn add_cachyos_repo(ctx: &Ctx) -> Result<()> {
    // cachyos-repo.sh fetches the signing key from a public keyserver, which
    // fails intermittently ("keyserver receive failed: Server indicated a
    // failure") — a flaky-keyserver problem, not ours. Retry the whole bootstrap
    // a few times before giving up. cachyos-repo.sh refuses to run unless
    // EUID==0 (no self-escalation), so it's invoked *with* sudo; the surrounding
    // curl/tar/mktemp stay at user level.
    // cachyos-repo.sh imports its signing key with `pacman-key --recv-keys` over
    // hkp, which fails intermittently ("Server indicated a failure") — and since
    // the script runs `set -e`, that aborts the whole install. Import the key
    // ourselves over HTTPS (the keyserver's web lookup, port 443 — no flaky hkp),
    // trying a few mirrors, then neuter the script's keyserver fetch so a
    // keyserver hiccup can't break a CachyOS install. cachyos-repo.sh refuses to
    // run unless EUID==0, so it's invoked with sudo; the wrapping shell stays at
    // user level for curl/tar/mktemp.
    let script = "\
        KEY=F3B607488DB35A47; imported=0; \
        for ks in keyserver.ubuntu.com keys.openpgp.org pgp.mit.edu; do \
          if curl -fsSL \"https://$ks/pks/lookup?op=get&search=0x$KEY\" 2>/dev/null | sudo pacman-key --add - >/dev/null 2>&1; then \
            sudo pacman-key --lsign-key $KEY >/dev/null 2>&1 && { imported=1; echo \"  · imported CachyOS key over HTTPS ($ks)\"; break; }; \
          fi; \
        done; \
        for attempt in 1 2 3; do \
          d=$(mktemp -d) && cd \"$d\" && \
          curl -fsSL https://mirror.cachyos.org/cachyos-repo.tar.xz -o c.tar.xz && \
          tar xf c.tar.xz && cd cachyos-repo && \
          { [ \"$imported\" = 1 ] && sed -i '/pacman-key --recv-keys/s/.*/true/; /pacman-key --lsign-key/s/.*/true/' cachyos-repo.sh; true; } && \
          yes | sudo ./cachyos-repo.sh && exit 0; \
          echo \"  · CachyOS repo attempt $attempt failed; retrying in 6s\"; sleep 6; \
        done; \
        echo 'CachyOS repo setup failed' >&2; exit 1";
    ctx.shell(script, false)
}

/// Step 4 — ensure paru exists. paru is the one hardcoded AUR helper.
/// Bootstrapped from the AUR: base-devel + git + a real `rust` toolchain, clone
/// the source `paru` package, `makepkg -si`.
///
/// makepkg refuses to run as root, so the clone/build run at user level; only
/// the dependency install uses sudo.
pub fn bootstrap_paru(ctx: &Ctx) -> Result<()> {
    if ctx.check("sh", &["-c", "command -v paru"]) {
        println!("  · paru already installed");
        return Ok(());
    }

    // Fast path: a prebuilt `paru` package in the pacman cache — baked into the
    // ISO, or saved by an earlier build on this machine (see the end of this
    // fn). Building paru from source costs 20-30 min on modest hardware, so
    // reusing one is a big win for repeat installs and the VM test rig. This is
    // NOT the paru-bin trap: we *verify the binary actually runs* after
    // installing it, so a cache built against an older libalpm (which won't run)
    // cleanly falls back to a source build instead of shipping a broken paru.
    if !ctx.dry_run {
        if let Some(pkg) = cached_paru_pkg() {
            println!("  · found a cached paru ({pkg}) — trying it before a source build");
            let installed = ctx
                .sudo("pacman", &["-U", "--noconfirm", &pkg])
                .is_ok();
            if installed && ctx.check("sh", &["-c", "paru --version >/dev/null 2>&1"]) {
                println!("  · installed cached paru — skipped the ~20-30 min source build");
                return Ok(());
            }
            println!("  · cached paru unusable (libalpm bump?) — building from source instead");
        }
    }
    // paru's PKGBUILD needs a Rust toolchain (`cargo` makedepend). That dep is
    // provided by BOTH `rust` and `rustup`; with `--noconfirm`, makepkg can pull
    // `rustup`, whose `cargo` is a shim that dies with "rustup could not choose a
    // version of cargo to run" until a default toolchain is set — sinking the
    // whole install. So pin the provider to the real `rust` package up front (an
    // exact name → no ambiguity). If `rustup` is somehow already the provider,
    // don't fight it (installing `rust` would conflict); the build step below
    // sets a default toolchain instead.
    let rustup_present = ctx.check("sh", &["-c", "pacman -Qq rustup >/dev/null 2>&1"]);
    println!(
        "  · installing build prerequisites (base-devel, git{})",
        if rustup_present { "" } else { ", rust" }
    );
    let mut prereqs = vec![
        "-S", "--needed", "--noconfirm", "--disable-download-timeout", "base-devel", "git",
    ];
    if !rustup_present {
        prereqs.push("rust");
    }
    ctx.sudo("pacman", &prereqs)?;

    println!("  · building paru from the AUR");
    // Parallelism sized to the machine, but CONSERVATIVELY: rustc can spike to
    // ~2 GB+ per job at link time, and this runs mid-install with the live env /
    // makepkg / chroot all using memory. Being too eager OOM-kills the build
    // AND takes down the VM's guest daemon. So reserve 2 GB for the system, then
    // one job per 2.5 GB of what's left, capped at nproc. A 6 GB VM stays at the
    // safe -j1; a 16 GB+ machine gets real parallelism.
    let build = format!(
        "cd \"$(mktemp -d)\" && \
         git clone --depth 1 {PARU_AUR} && \
         cd paru && \
         if command -v rustup >/dev/null 2>&1 && ! rustup default >/dev/null 2>&1; then \
             echo '  · configuring default rust toolchain for rustup'; \
             rustup default stable; \
         fi; \
         mem_kb=$(awk '/MemAvailable/ {{print $2}}' /proc/meminfo); \
         jobs=$(( (${{mem_kb:-0}} - 2097152) / 2621440 )); \
         [ \"$jobs\" -lt 1 ] && jobs=1; \
         [ \"$jobs\" -gt \"$(nproc)\" ] && jobs=$(nproc); \
         echo \"  · building with $jobs parallel job(s)\"; \
         MAKEFLAGS=-j$jobs CARGO_BUILD_JOBS=$jobs makepkg -si --noconfirm && \
         {{ sudo cp -f paru-[0-9]*.pkg.tar.* /var/cache/pacman/pkg/ 2>/dev/null || true; }}"
    );
    ctx.shell(&build, false)
}

/// The newest prebuilt `paru` package in the pacman cache, if any. Skips the
/// `-debug` byproduct and `paru-bin` (the ABI-fragile prebuilt we never want) by
/// requiring a digit right after `paru-` (i.e. a version). Returns a full path
/// ready for `pacman -U`.
fn cached_paru_pkg() -> Option<String> {
    const CACHE: &str = "/var/cache/pacman/pkg";
    let mut hits: Vec<String> = std::fs::read_dir(CACHE)
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|f| is_cached_paru(f))
        .collect();
    hits.sort();
    hits.pop().map(|f| format!("{CACHE}/{f}"))
}

/// A cache filename that's a real source `paru` package: `paru-<digit>…` ending
/// in `.pkg.tar.zst`/`.xz`, excluding `.sig`, `paru-bin-` and `paru-debug-`.
fn is_cached_paru(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("paru-") else {
        return false;
    };
    rest.starts_with(|c: char| c.is_ascii_digit())
        && (name.ends_with(".pkg.tar.zst") || name.ends_with(".pkg.tar.xz"))
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
    let mut pkgs: Vec<String> = vec![kernel.package.to_string(), kernel.headers.to_string()];
    for p in extra.iter().chain(manifest.packages.iter()) {
        if !pkgs.iter().any(|x| x == p) {
            pkgs.push(p.clone());
        }
    }
    println!("  {} package(s) total", pkgs.len());

    // Route official-repo packages through plain `pacman` and only the rest
    // through the AUR. paru — a 20-30 min source build — is bootstrapped ONLY
    // when a package actually comes from the AUR, so an all-official manifest
    // (most of them) never pays for it. Membership comes from the enabled sync
    // databases; if they can't be read, everything falls back to paru, which
    // resolves both. (Groups/virtual names aren't literal package names, so they
    // route to paru too — correct, just not as fast.)
    let (official, aur) = partition_packages(&pkgs, &official_packages(ctx));

    if !official.is_empty() {
        println!("  · {} from official repos → pacman", official.len());
        let list = official.iter().map(|p| shell_quote(p)).collect::<Vec<_>>().join(" ");
        let cmd = format!("pacman -S --needed --noconfirm --disable-download-timeout {list}");
        ctx.shell(&with_retries(&cmd, 3), true)?;
    }

    if aur.is_empty() {
        println!("  · no AUR packages — skipping the paru bootstrap");
        return Ok(());
    }

    println!("  · {} from the AUR → bootstrapping paru first", aur.len());
    bootstrap_paru(ctx)?;
    let list = aur.iter().map(|s| shell_quote(s)).collect::<Vec<_>>().join(" ");
    // don't let one slow mirror's trickle abort the whole desktop install
    let cmd = format!(
        "MAKEFLAGS=-j1 CARGO_BUILD_JOBS=1 paru -S --needed --noconfirm --disable-download-timeout {list}"
    );
    ctx.shell(&with_retries(&cmd, 3), false)
}

/// Every package name available in the enabled sync repos (`pacman -Slq`), as a
/// set. Empty if the databases can't be read — the caller then treats every
/// package as AUR, the safe fallback that always resolves via paru. Read-only,
/// so no root needed.
fn official_packages(ctx: &Ctx) -> HashSet<String> {
    ctx.output(false, "pacman", &["-Slq"])
        .map(|s| s.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())
        .unwrap_or_default()
}

/// Split packages into `(official, aur)` by sync-repo membership. An empty
/// `official` set (databases unreadable) routes everything to AUR so paru still
/// resolves the lot — never a false "official" that would make `pacman -S` fail.
fn partition_packages(pkgs: &[String], official: &HashSet<String>) -> (Vec<String>, Vec<String>) {
    if official.is_empty() {
        return (Vec::new(), pkgs.to_vec());
    }
    pkgs.iter().cloned().partition(|p| official.contains(p))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn partitions_packages_by_sync_membership() {
        let official = set(&["firefox", "linux", "mesa"]);
        let pkgs: Vec<String> =
            ["firefox", "paru", "wf-shell", "linux"].iter().map(|s| s.to_string()).collect();
        let (off, aur) = partition_packages(&pkgs, &official);
        assert_eq!(off, vec!["firefox".to_string(), "linux".to_string()]);
        assert_eq!(aur, vec!["paru".to_string(), "wf-shell".to_string()]);
    }

    #[test]
    fn empty_official_set_routes_everything_to_aur() {
        // db unreadable → nothing classified official → paru resolves all.
        let (off, aur) = partition_packages(&["a".to_string(), "b".to_string()], &HashSet::new());
        assert!(off.is_empty());
        assert_eq!(aur, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn without_optimized_cachyos_comments_v3_v4_keeps_plain() {
        let conf = "\
[options]
HoldPkg = pacman

[cachyos-v3]
Include = /etc/pacman.d/cachyos-v3-mirrorlist

[cachyos-core-v3]
Include = /etc/pacman.d/cachyos-v3-mirrorlist

[cachyos]
Include = /etc/pacman.d/cachyos-mirrorlist

[core]
Include = /etc/pacman.d/mirrorlist
";
        let out = without_optimized_cachyos(conf);
        // Optimized repos + their Include lines are commented.
        assert!(out.contains("#[cachyos-v3]"), "{out}");
        assert!(out.contains("#[cachyos-core-v3]"), "{out}");
        assert!(out.contains("#Include = /etc/pacman.d/cachyos-v3-mirrorlist"), "{out}");
        // Plain cachyos (the kernel repo), core and options are untouched.
        assert!(!out.contains("#[cachyos]"), "{out}");
        assert!(!out.contains("#[core]"), "{out}");
        assert!(!out.contains("#[options]"), "{out}");
        assert!(out.contains("\nInclude = /etc/pacman.d/cachyos-mirrorlist"), "{out}");
    }

    #[test]
    fn without_optimized_cachyos_is_a_noop_without_cachy_repos() {
        let conf = "[options]\nHoldPkg = pacman\n\n[core]\nInclude = /x\n";
        assert_eq!(without_optimized_cachyos(conf), conf);
    }

    #[test]
    fn with_retries_loops_and_bounds() {
        let s = with_retries("pacman -S foo", 3);
        assert!(s.contains("until pacman -S foo; do"));
        assert!(s.contains("-ge 3"));
        assert!(s.contains("exit 1"));
    }

    #[test]
    fn is_cached_paru_matches_source_pkgs_only() {
        assert!(is_cached_paru("paru-2.0.4-1-x86_64.pkg.tar.zst"));
        assert!(is_cached_paru("paru-1.11.2-1-x86_64.pkg.tar.xz"));
        // Not the ABI-fragile prebuilt, the debug byproduct, or a signature.
        assert!(!is_cached_paru("paru-bin-2.0.4-1-x86_64.pkg.tar.zst"));
        assert!(!is_cached_paru("paru-debug-2.0.4-1-x86_64.pkg.tar.zst"));
        assert!(!is_cached_paru("paru-2.0.4-1-x86_64.pkg.tar.zst.sig"));
        // Unrelated packages.
        assert!(!is_cached_paru("parui-0.1-1-x86_64.pkg.tar.zst"));
        assert!(!is_cached_paru("firefox-1-1-x86_64.pkg.tar.zst"));
    }
}
