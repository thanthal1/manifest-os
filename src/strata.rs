//! Foreign-distro **strata** — Bedrock-style multi-distro binary access.
//!
//! A stratum is a full foreign-distro rootfs living under `/strata/<name>`.
//! It is **never booted**: Arch's systemd stays PID 1, and we `chroot` into a
//! stratum only to install and run its packages. Each exposed binary gets a
//! generated **shim** on the host PATH that enters the stratum (in a private
//! mount namespace, so binds auto-unmount when the process exits) and execs the
//! real binary. That chroot is the correctness boundary: a foreign binary
//! resolves *its own* stratum's `ld.so` and libs, so glibc-version skew between
//! host and stratum can't break it. See `docs/strata-design.md` for the full
//! rationale (and why shims come before crossfs).
//!
//! Phase 1 scope: glibc distros only (Debian/Ubuntu via `debootstrap`), binary
//! access only (no `/etc` merge, no foreign services, no crossfs, no Alpine).
//!
//! Everything user-facing here is idempotent, and every side effect goes through
//! [`Ctx`] so `--dry-run` prints the whole plan without touching anything. The
//! logic that decides *what* to run (shim text, mount set, mirror URL, bootstrap
//! command) is factored into pure functions, unit-tested on any host.

use crate::exec::Ctx;
use crate::manifest::Stratum;
use anyhow::{bail, Result};

/// Where strata rootfs trees live — one per stratum, `/strata/<name>`. (The
/// concept and this layout are inspired by Bedrock Linux, but there's no Bedrock
/// code here; the path is our own, not `/bedrock`.)
const STRATA_ROOT: &str = "/strata";
/// Generated per-stratum "enter" helpers. A dot-dir so it isn't mistaken for a
/// stratum (and export's os-release scan skips it).
const LIBEXEC_DIR: &str = "/strata/.libexec";
/// Generated shims, added to PATH. A dot-dir for the same reason as libexec.
const BIN_DIR: &str = "/strata/.bin";
/// profile.d drop-in that puts [`BIN_DIR`] on every login shell's PATH.
const PROFILE_D: &str = "/etc/profile.d/00-manifest-strata.sh";
/// Scoped sudoers drop-in for passwordless user-app launch (see [`write_sudoers`]).
const SUDOERS: &str = "/etc/sudoers.d/manifest-strata";
/// Where `.desktop` menu launchers for exposed GUI apps are written.
const APPLICATIONS_DIR: &str = "/usr/share/applications";

/// Bind-shares set up when a stratum lists none explicitly. `x11`/`wayland` ride
/// on `/tmp` and `/run` (already shared), so they need no extra bind here — they
/// stay in the list for intent/documentation and forward-compat.
pub const DEFAULT_SHARES: &[&str] = &["home", "resolv", "tmp", "x11", "wayland"];

/// Mount points always bound into every stratum (handled like `arch-chroot`
/// does), regardless of the `share` list.
const ALWAYS_BOUND: &[&str] = &["proc", "sys", "dev", "run"];

/// Which bootstrap backend a `distro` string selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Backend {
    /// Debian/Ubuntu family — `debootstrap`, glibc, `apt`.
    Debootstrap,
    /// Fedora family — `dnf --installroot`, glibc, `dnf`/`rpm`.
    Dnf,
    /// Alpine — static `apk`, musl. Parsed but not implemented (Phase 3+).
    Apk,
}

/// Default Fedora release used when a fedora stratum doesn't pin one via `suite`.
/// Bump on new stable releases (same maintenance as the ubuntu `noble` default);
/// `distribution-gpg-keys` ships keys well ahead, so this only needs to track
/// what's actually released.
const FEDORA_DEFAULT_RELEASE: &str = "42";

/// Map a manifest `distro` string to a backend. Unknown distros are an error the
/// caller surfaces; known-but-unimplemented ones map to a backend so the caller
/// can give a precise "not yet" message rather than "unknown distro".
fn backend_for(distro: &str) -> Option<Backend> {
    match distro.trim().to_ascii_lowercase().as_str() {
        "debian" | "ubuntu" => Some(Backend::Debootstrap),
        "fedora" => Some(Backend::Dnf),
        "alpine" => Some(Backend::Apk),
        _ => None,
    }
}

/// The Arch package + installed keyring path that lets debootstrap *verify* a
/// distro's package signatures. Both are in Arch's official repos. Returns
/// `(package, keyring_path)`.
///
/// This exists because debootstrap does NOT fail when its keyring is absent — it
/// prints `W: Cannot check Release signature; keyring file not available` and
/// bootstraps the rootfs **unverified**. That's a silent supply-chain hole, so
/// we install the keyring and pass `--keyring` explicitly (see [`ensure_keyring`]
/// / [`bootstrap_cmd`]).
fn keyring_for(distro: &str) -> Option<(&'static str, &'static str)> {
    match distro.trim().to_ascii_lowercase().as_str() {
        "debian" => Some((
            "debian-archive-keyring",
            "/usr/share/keyrings/debian-archive-keyring.gpg",
        )),
        "ubuntu" => Some((
            "ubuntu-keyring",
            "/usr/share/keyrings/ubuntu-archive-keyring.gpg",
        )),
        _ => None,
    }
}

/// Apply every stratum in order. The engine step (`install.rs::apply`).
pub fn apply(strata: &[Stratum], ctx: &Ctx) -> Result<()> {
    if strata.is_empty() {
        return Ok(());
    }
    // Host-side tools the feature needs, as the union over the backends actually
    // used: arch-install-scripts (arch-chroot + the enter helper's chroot) always,
    // debootstrap for debian/ubuntu, dnf + distribution-gpg-keys for fedora.
    // Installed once, idempotently, like flatpak.rs / gestures.rs auto-add deps.
    ensure_host_tools(strata, ctx)?;

    // Resolve bare-name shim ownership once, across all strata: two strata that
    // expose the same binary name would otherwise collide at /strata/.bin/<name>
    // (last applied silently wins). First in manifest order gets the bare name;
    // every exposed binary also gets an unambiguous <stratum>-<bin> alias.
    let bare_winners: std::collections::HashSet<(String, String)> =
        bare_shim_winners(strata).into_iter().collect();

    for s in strata {
        if s.is_empty() {
            continue;
        }
        apply_one(s, &bare_winners, ctx)?;
    }

    // One profile.d drop-in puts all shims on PATH for every login shell.
    write_profile_d(ctx)?;
    // Passwordless launch for foreign user apps (so they run from the app menu,
    // not just a terminal). Safe/scoped — see write_sudoers.
    write_sudoers(ctx)?;
    Ok(())
}

fn apply_one(s: &Stratum, bare_winners: &std::collections::HashSet<(String, String)>, ctx: &Ctx) -> Result<()> {
    let backend = match backend_for(&s.distro) {
        Some(b) => b,
        None => bail!(
            "stratum '{}': unknown distro '{}' (expected debian/ubuntu/fedora/alpine)",
            s.name,
            s.distro
        ),
    };

    let root = stratum_root(&s.name);
    println!("  · stratum '{}' ({}) → {root}", s.name, s.distro);

    // Verification is enforced per backend before any bytes land: debootstrap
    // gets an explicit --keyring; dnf verifies against distribution-gpg-keys.
    // Never bootstrap a root-privileged foreign rootfs unverified.
    let keyring = match backend {
        Backend::Debootstrap => {
            if s.snapshot.is_none() {
                println!(
                    "  · warning: stratum '{}' has no `snapshot` pin — it will bootstrap \
                     \"latest at install time\" and is NOT reproducible (docs §6)",
                    s.name
                );
            }
            Some(ensure_keyring(s, ctx)?)
        }
        Backend::Dnf => {
            ensure_fedora_key(s, ctx)?;
            if s.snapshot.is_some() {
                println!(
                    "  · note: `snapshot` pins aren't supported for fedora — ignoring \
                     (fedora has no debian-style snapshot archive)"
                );
            }
            None
        }
        Backend::Apk => {
            // Alpine's signing keys aren't packaged on Arch; the bootstrap
            // downloads them (+ `apk.static`) over HTTPS from the official CDN and
            // verifies against them (never `--allow-untrusted`). See apk_bootstrap_cmd.
            None
        }
    };

    bootstrap(s, backend, &root, keyring.as_deref(), ctx)?;
    install_in_stratum(s, backend, &root, ctx)?;
    write_enter_helper(s, ctx)?;
    write_shims(s, bare_winners, ctx)?;
    write_desktop_entries(s, ctx)?;
    Ok(())
}

/// Ensure the distro's archive keyring is installed so debootstrap actually
/// verifies package signatures, and return its path. debootstrap only *warns*
/// and proceeds unverified when the keyring is absent, so we install it from
/// Arch's official repos and hard-fail if it's still missing — refusing to
/// bootstrap a root-privileged foreign rootfs from unverified packages.
fn ensure_keyring(s: &Stratum, ctx: &Ctx) -> Result<String> {
    let (pkg, path) = keyring_for(&s.distro).ok_or_else(|| {
        anyhow::anyhow!(
            "stratum '{}': no known archive keyring for distro '{}' — cannot verify \
             signatures, refusing to bootstrap",
            s.name,
            s.distro
        )
    })?;
    if !ctx.check("test", &["-f", path]) {
        println!("  · installing {pkg} so the bootstrap can verify package signatures");
        ctx.sudo("pacman", &["-S", "--needed", "--noconfirm", pkg])?;
    }
    if !ctx.dry_run && !ctx.check("test", &["-f", path]) {
        bail!(
            "stratum '{}': archive keyring {path} still missing after installing {pkg} — \
             refusing to bootstrap unverified (a supply-chain risk)",
            s.name
        );
    }
    Ok(path.to_string())
}

/// Ensure the host tools every used backend needs are installed (idempotent
/// `pacman -S --needed`). arch-chroot is always required (in-stratum install +
/// the enter helper); debootstrap and dnf/distribution-gpg-keys are added only
/// when a stratum actually uses that backend.
fn ensure_host_tools(strata: &[Stratum], ctx: &Ctx) -> Result<()> {
    let backends = used_backends(strata);
    let mut pkgs = vec!["arch-install-scripts"];
    if backends.contains(&Backend::Debootstrap) {
        pkgs.push("debootstrap");
        // dpkg gives debootstrap a reliable host arch: `dpkg --print-architecture`
        // → `amd64`. Without it, debootstrap falls back to `pacman-conf
        // Architecture`, which on CachyOS/hwcaps installs is multi-line
        // (x86_64 x86_64_v2 x86_64_v3) and dies with "Unknown architecture".
        pkgs.push("dpkg");
    }
    if backends.contains(&Backend::Dnf) {
        // dnf5 is Arch's current dnf (the older `dnf` package is dnf4 and
        // *conflicts* with dnf5); distribution-gpg-keys carries the Fedora keys.
        pkgs.push("dnf5");
        pkgs.push("distribution-gpg-keys");
    }
    if backends.contains(&Backend::Apk) {
        // Alpine bootstrap downloads apk.static + keys itself; it just needs curl.
        pkgs.push("curl");
    }
    println!("  · ensuring strata host tools: {}", pkgs.join(", "));
    let mut args = vec!["-S", "--needed", "--noconfirm"];
    args.extend(pkgs);
    ctx.sudo("pacman", &args)
}

/// The set of backends actually referenced by the (non-empty) strata.
fn used_backends(strata: &[Stratum]) -> std::collections::HashSet<Backend> {
    strata
        .iter()
        .filter(|s| !s.is_empty())
        .filter_map(|s| backend_for(&s.distro))
        .collect()
}

/// Verify the pinned Fedora release's signing key is present (from
/// `distribution-gpg-keys`, installed by [`ensure_host_tools`]) before letting
/// dnf bootstrap. A missing key almost always means an unknown/typo release — we
/// refuse rather than let the bootstrap fall back to unverified.
fn ensure_fedora_key(s: &Stratum, ctx: &Ctx) -> Result<()> {
    let rel = s.suite.clone().unwrap_or_else(|| FEDORA_DEFAULT_RELEASE.to_string());
    let key = fedora_key_path(&rel);
    if !ctx.dry_run && !ctx.check("test", &["-f", &key]) {
        bail!(
            "stratum '{}': Fedora signing key not found at {key} — unknown release '{rel}'? \
             (distribution-gpg-keys ships current ones) — refusing to bootstrap unverified",
            s.name
        );
    }
    Ok(())
}

/// Bootstrap the rootfs if it isn't already there. Idempotent: an existing
/// os-release (or alpine-release) means "already bootstrapped, skip".
fn bootstrap(s: &Stratum, backend: Backend, root: &str, keyring: Option<&str>, ctx: &Ctx) -> Result<()> {
    if ctx.check("test", &["-f", &format!("{root}/etc/os-release")]) {
        println!("  · rootfs already bootstrapped — skipping");
        return Ok(());
    }
    println!("  · bootstrapping rootfs (this pulls a base system — minutes)");
    let cmd = bootstrap_cmd(s, backend, root, keyring)?;
    ctx.shell(&cmd, true)
}

/// Install the stratum's own `packages` using its own package manager, inside
/// the stratum via arch-chroot. No-op when the list is empty.
fn install_in_stratum(s: &Stratum, backend: Backend, root: &str, ctx: &Ctx) -> Result<()> {
    if s.packages.is_empty() {
        return Ok(());
    }
    println!("  · installing {} package(s) inside the stratum", s.packages.len());
    let inner = in_stratum_install_cmd(backend, &s.packages);
    // Plant a real resolv.conf first: arch-chroot only bind-mounts one if the
    // target's already exists as a real file, and Fedora ships /etc/resolv.conf
    // as a *dangling* symlink (→ systemd-resolved's stub), so the package manager
    // inside can't resolve mirrors. rm the symlink, then copy the host's.
    let cmd = format!(
        "rm -f {root}/etc/resolv.conf; cp -L /etc/resolv.conf {root}/etc/resolv.conf 2>/dev/null || true; \
         arch-chroot {root_q} /bin/sh -c {inner_q}",
        root = root,
        root_q = shell_quote(root),
        inner_q = shell_quote(&inner),
    );
    ctx.shell(&cmd, true)
}

/// Write the per-stratum "enter" helper into libexec and mark it executable.
fn write_enter_helper(s: &Stratum, ctx: &Ctx) -> Result<()> {
    let path = enter_helper_path(&s.name);
    ctx.write_root(&path, &enter_helper_script(s))?;
    ctx.sudo("chmod", &["0755", &path])
}

/// Write shims for a stratum's exposed binaries. Each binary always gets an
/// unambiguous `<stratum>-<bin>` shim; the bare `<bin>` name goes to whichever
/// stratum won it in manifest order (`bare_winners`), and a collision on a later
/// stratum warns instead of silently overwriting.
fn write_shims(
    s: &Stratum,
    bare_winners: &std::collections::HashSet<(String, String)>,
    ctx: &Ctx,
) -> Result<()> {
    if s.expose.is_empty() {
        println!("  · no `expose` list — stratum installed but nothing on host PATH");
        return Ok(());
    }
    for bin in &s.expose {
        let script = shim_script(&s.name, bin);

        // Always: a stratum-prefixed alias, reachable even when the bare name is
        // claimed by another stratum.
        let alias = shim_path(&prefixed_name(&s.name, bin));
        println!("  · expose {} → {alias}", prefixed_name(&s.name, bin));
        ctx.write_root(&alias, &script)?;
        ctx.sudo("chmod", &["0755", &alias])?;

        // The bare name: only the winning stratum writes it; others warn.
        if bare_winners.contains(&(s.name.clone(), bin.clone())) {
            let bare = shim_path(bin);
            ctx.write_root(&bare, &script)?;
            ctx.sudo("chmod", &["0755", &bare])?;
            println!("    also on PATH as `{bin}`");
        } else {
            println!(
                "  · note: `{bin}` is already exposed by an earlier stratum — this one \
                 is reachable as `{}` (bare `{bin}` unchanged)",
                prefixed_name(&s.name, bin)
            );
        }
    }
    Ok(())
}

/// Mirror each exposed **GUI** app's `.desktop` onto the host so it shows up in
/// the application menu (launching through the shim). Only binaries that actually
/// ship a `.desktop` in the stratum get an entry — CLI tools (htop, git, apt)
/// don't, so the menu isn't cluttered. Matches on `<bin>.desktop` (the common
/// case: chromium → chromium.desktop).
fn write_desktop_entries(s: &Stratum, ctx: &Ctx) -> Result<()> {
    let root = stratum_root(&s.name);
    for bin in &s.expose {
        if runs_as_root(bin) {
            continue; // package managers aren't GUI apps
        }
        let src = format!("{root}/usr/share/applications/{bin}.desktop");
        let Ok(content) = std::fs::read_to_string(&src) else {
            continue; // no .desktop → not a GUI app (or a name we don't match yet)
        };
        let entry = rewrite_desktop(&content, &shim_path(bin));
        let dest = format!("{APPLICATIONS_DIR}/strata-{}-{bin}.desktop", s.name);
        println!("  · menu entry for {bin} → {dest}");
        ctx.write_root(&dest, &entry)?;
    }
    Ok(())
}

/// Rewrite a stratum's `.desktop` so it launches through our shim instead of the
/// (host-invisible) foreign binary: point every `Exec=`/`TryExec=` at the shim
/// (keeping the field codes like `%U`), disable D-Bus activation (which can't
/// work through the shim), and tag it generated. Pure — unit-tested.
fn rewrite_desktop(content: &str, shim: &str) -> String {
    let mut out = String::from("# Generated by ManifestOS strata — launches via the shim; do not edit.\n");
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("Exec=") {
            let args = rest.split_once(char::is_whitespace).map_or("", |(_, a)| a);
            if args.is_empty() {
                out.push_str(&format!("Exec={shim}\n"));
            } else {
                out.push_str(&format!("Exec={shim} {args}\n"));
            }
        } else if line.starts_with("TryExec=") {
            out.push_str(&format!("TryExec={shim}\n"));
        } else if line.starts_with("DBusActivatable=") {
            out.push_str("DBusActivatable=false\n");
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Put the shim dir on PATH for every login shell.
fn write_profile_d(ctx: &Ctx) -> Result<()> {
    ctx.write_root(PROFILE_D, &profile_d_script())
}

/// A scoped sudoers drop-in so foreign **user** apps launch without a password
/// prompt every time (the difference between "chromium works if I type it in a
/// terminal and enter my password" and "it's in my app menu, one click").
///
/// Safe because it's narrow on two axes: only the **user**-mode enter helper is
/// passwordless (package managers run root-mode and still prompt), and the helper
/// itself refuses unless the requested uid equals `$SUDO_UID` — so a caller can
/// only ever run foreign apps **as themselves**, never as root. Validated with
/// `visudo -c` before it goes live, since a malformed sudoers file breaks sudo.
fn write_sudoers(ctx: &Ctx) -> Result<()> {
    let body = format!(
        "# ManifestOS strata — passwordless launch of foreign USER apps (generated).\n\
         # Only user-mode is passwordless, and the enter helper enforces\n\
         # uid == $SUDO_UID, so this lets a user run foreign apps as *themselves*\n\
         # only — never as root. Package managers (root mode) still prompt.\n\
         ALL ALL=(root) NOPASSWD: {LIBEXEC_DIR}/enter-* user *\n",
    );
    // Stage under a dot-name (sudo ignores dotfiles in sudoers.d), validate, then
    // move into place — a syntax error must never reach an active sudoers file.
    let staged = format!("{SUDOERS}.staged");
    ctx.write_root(&staged, &body)?;
    ctx.sudo("chmod", &["0440", &staged])?;
    if ctx.dry_run || ctx.check("visudo", &["-cf", &staged]) {
        ctx.sudo("mv", &["-f", &staged, SUDOERS])
    } else {
        let _ = ctx.sudo("rm", &["-f", &staged]);
        bail!("generated strata sudoers file failed `visudo -c` validation — not installing");
    }
}

/// The handler function lives here; interactive shells source it from this path.
const CNF_LIB: &str = "/etc/manifest-os/strata-cnf.sh";
/// Marker so the source line is added at most once per rc file.
const CNF_MARKER: &str = "manifest-os-strata-cnf";

/// Install the "command not found → offer a stratum" shell handler. Written on
/// every install (not just when strata are declared) so a fresh system can offer
/// to add Debian/Fedora the first time someone types `apt`/`dnf`.
///
/// It must load in **interactive** shells, so a single `/etc/profile.d` drop-in
/// is not enough: zsh never sources `/etc/profile.d`, and it only covers login
/// shells anyway. We keep the handler in one lib and source it from the files
/// interactive shells actually read — `/etc/bash.bashrc` (bash), `/etc/zsh/zshrc`
/// (zsh) — plus a profile.d shim for login bash. Idempotent.
pub fn write_cnf_handler(ctx: &Ctx) -> Result<()> {
    ctx.write_root(CNF_LIB, cnf_handler_script())?;
    let src = format!("[ -r {CNF_LIB} ] && . {CNF_LIB}  # {CNF_MARKER}");
    // profile.d shim (login bash): a plain file that just sources the lib.
    ctx.write_root("/etc/profile.d/09-manifest-strata-cnf.sh", &format!("{src}\n"))?;
    // Interactive bash + zsh: append the source line once, guarded by the marker.
    // Both /etc/zsh/zshrc (vanilla zsh) and /etc/zsh/zshrc.local (sourced by
    // grml-zsh-config, which owns /etc/zsh/zshrc on our images) so it loads
    // whichever zsh setup the system ended up with.
    for rc in ["/etc/bash.bashrc", "/etc/zsh/zshrc", "/etc/zsh/zshrc.local"] {
        let dir = rc.rsplit_once('/').map(|(d, _)| d).unwrap_or("/etc");
        ctx.shell(
            &format!(
                "mkdir -p {dir}; touch {rc}; grep -q {marker} {rc} 2>/dev/null || printf '%s\\n' {src} >> {rc}",
                dir = shell_quote(dir),
                rc = shell_quote(rc),
                marker = shell_quote(CNF_MARKER),
                src = shell_quote(&src),
            ),
            true,
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pure logic (unit-tested; no side effects)
// ---------------------------------------------------------------------------

fn stratum_root(name: &str) -> String {
    format!("{STRATA_ROOT}/{name}")
}

fn enter_helper_path(name: &str) -> String {
    format!("{LIBEXEC_DIR}/enter-{name}")
}

fn shim_path(bin: &str) -> String {
    format!("{BIN_DIR}/{bin}")
}

/// The unambiguous per-stratum alias name for an exposed binary, e.g.
/// `debian-apt`. Always generated so every exposed binary is reachable even when
/// two strata expose the same bare name.
fn prefixed_name(stratum: &str, bin: &str) -> String {
    format!("{stratum}-{bin}")
}

/// Decide which stratum owns each bare binary name across all strata: the first
/// stratum in manifest order to expose a name wins it; later strata reach their
/// version only via the prefixed alias. Returns the winning `(stratum, bin)`
/// pairs. Pure — unit-tested.
fn bare_shim_winners(strata: &[Stratum]) -> Vec<(String, String)> {
    let mut claimed = std::collections::HashSet::new();
    let mut winners = Vec::new();
    for s in strata {
        if s.is_empty() {
            continue;
        }
        for bin in &s.expose {
            if claimed.insert(bin.clone()) {
                winners.push((s.name.clone(), bin.clone()));
            }
        }
    }
    winners
}

/// Whether an exposed binary should run as **root** inside the stratum. Package
/// managers modify the stratum and need root; everything else runs as the
/// invoking user (design §2.1) so it can open the user's files and Wayland/X
/// socket — the difference between a GUI app that displays and one that can't.
fn runs_as_root(bin: &str) -> bool {
    matches!(
        bin.trim(),
        "apt" | "apt-get" | "apt-cache" | "aptitude" | "dpkg" | "dpkg-query"
            | "dpkg-reconfigure" | "add-apt-repository" | "dnf" | "dnf5" | "microdnf"
            | "yum" | "rpm" | "pacman" | "apk" | "zypper"
    )
}

/// The bind list for a stratum: the always-bound set plus any shared mount that
/// corresponds to a real bind (`home`, `tmp`). `resolv` is copied, not bound;
/// `x11`/`wayland` ride on already-bound `/tmp` and `/run`.
fn bind_mounts(s: &Stratum) -> Vec<String> {
    let shares = effective_shares(s);
    let mut binds: Vec<String> = ALWAYS_BOUND.iter().map(|m| m.to_string()).collect();
    for m in ["home", "tmp"] {
        if shares.iter().any(|x| x == m) {
            binds.push(m.to_string());
        }
    }
    binds
}

/// The effective share set: the stratum's own list, or [`DEFAULT_SHARES`] when
/// it declared none.
fn effective_shares(s: &Stratum) -> Vec<String> {
    if s.share.is_empty() {
        DEFAULT_SHARES.iter().map(|m| m.to_string()).collect()
    } else {
        s.share.clone()
    }
}

/// Resolve the mirror URL for a stratum, honoring a `snapshot` pin. A snapshot
/// routes through the distro's time-stamped archive so the bootstrap is
/// reproducible; otherwise the explicit `mirror`, else the distro default.
fn resolve_mirror(s: &Stratum, backend: Backend) -> String {
    if let Some(stamp) = &s.snapshot {
        return match s.distro.trim().to_ascii_lowercase().as_str() {
            "debian" => format!("https://snapshot.debian.org/archive/debian/{stamp}/"),
            "ubuntu" => format!("https://snapshot.ubuntu.com/ubuntu/{stamp}/"),
            _ => s.mirror.clone().unwrap_or_else(|| default_mirror(backend, &s.distro)),
        };
    }
    s.mirror.clone().unwrap_or_else(|| default_mirror(backend, &s.distro))
}

fn default_mirror(backend: Backend, distro: &str) -> String {
    match (backend, distro.trim().to_ascii_lowercase().as_str()) {
        (Backend::Debootstrap, "ubuntu") => "http://archive.ubuntu.com/ubuntu".to_string(),
        (Backend::Debootstrap, _) => "https://deb.debian.org/debian".to_string(),
        (Backend::Dnf, _) => String::new(),
        (Backend::Apk, _) => "https://dl-cdn.alpinelinux.org/alpine".to_string(),
    }
}

fn default_suite(distro: &str) -> &'static str {
    match distro.trim().to_ascii_lowercase().as_str() {
        "ubuntu" => "noble",
        "fedora" => FEDORA_DEFAULT_RELEASE,
        "alpine" => "latest-stable",
        _ => "stable",
    }
}

/// Build the bootstrap command line for a backend. debootstrap needs the caller
/// to have resolved a keyring path; dnf bakes its verification into the command.
fn bootstrap_cmd(s: &Stratum, backend: Backend, root: &str, keyring: Option<&str>) -> Result<String> {
    match backend {
        Backend::Debootstrap => {
            let keyring = keyring.ok_or_else(|| {
                anyhow::anyhow!("internal: debootstrap bootstrap without a resolved keyring")
            })?;
            Ok(debootstrap_cmd(s, root, keyring))
        }
        Backend::Dnf => Ok(dnf_bootstrap_cmd(s, root)),
        Backend::Apk => Ok(apk_bootstrap_cmd(s, root)),
    }
}

/// The `debootstrap` command line. `--variant=minbase` keeps the rootfs small.
/// `--arch=amd64` is passed explicitly: without `dpkg` on the host, debootstrap's
/// arch auto-detect falls back to `uname -m` → `x86_64`, which Debian doesn't
/// recognize (`Unknown architecture: x86_64`) — it wants `amd64`. Our ISO is
/// x86_64-only, so amd64 is correct for both Debian and Ubuntu. `--keyring=<path>`
/// is passed so signatures are actually verified: debootstrap does NOT fail on a
/// missing keyring, it warns and bootstraps unverified, so [`ensure_keyring`]
/// installs the keyring and we point at it here (never `--no-check-gpg` — a
/// manifest disabling verification is a marketplace finding, see docs §9).
fn debootstrap_cmd(s: &Stratum, root: &str, keyring: &str) -> String {
    let suite = s.suite.clone().unwrap_or_else(|| default_suite(&s.distro).to_string());
    let mirror = resolve_mirror(s, Backend::Debootstrap);
    format!(
        "debootstrap --variant=minbase --arch=amd64 --keyring={} {} {} {}",
        shell_quote(keyring),
        shell_quote(&suite),
        shell_quote(root),
        shell_quote(&mirror),
    )
}

/// The path to a Fedora release's primary signing key, shipped by Arch's
/// `distribution-gpg-keys` package.
fn fedora_key_path(releasever: &str) -> String {
    format!("/usr/share/distribution-gpg-keys/fedora/RPM-GPG-KEY-fedora-{releasever}-primary")
}

/// A throwaway dnf `.repo` file for bootstrapping Fedora `$releasever` off a
/// non-Fedora host. Defaults to the **metalink** (the full mirror list, so dnf
/// fails over when a mirror is down — a single baseurl does not, and one dead
/// mirror killed the whole bootstrap in testing). A custom `mirror` switches to a
/// `baseurl` (one host, the user's choice). `$releasever`/`$basearch` are dnf
/// variables it expands itself; `gpgcheck=1` + the distribution-gpg-keys key
/// enforce verification.
fn fedora_repo_file(mirror: Option<&str>) -> String {
    let key = "file:///usr/share/distribution-gpg-keys/fedora/RPM-GPG-KEY-fedora-$releasever-primary";
    let (fedora_src, updates_src) = match mirror {
        Some(m) => (
            format!("baseurl={m}/releases/$releasever/Everything/$basearch/os/"),
            format!("baseurl={m}/updates/$releasever/Everything/$basearch/"),
        ),
        None => (
            "metalink=https://mirrors.fedoraproject.org/metalink?repo=fedora-$releasever&arch=$basearch".to_string(),
            "metalink=https://mirrors.fedoraproject.org/metalink?repo=updates-released-f$releasever&arch=$basearch".to_string(),
        ),
    };
    format!(
        "[fedora]\nname=Fedora $releasever\n{fedora_src}\nenabled=1\ngpgcheck=1\ngpgkey={key}\n\
         [updates]\nname=Fedora $releasever updates\n{updates_src}\nenabled=1\ngpgcheck=1\ngpgkey={key}\n"
    )
}

/// The `dnf5 --installroot` bootstrap command. Runs from the Arch host, which has
/// no Fedora repos, so it writes a temp `.repo` (see [`fedora_repo_file`]) and
/// points dnf at it via `reposdir`. Uses **`dnf5`** — Arch's current dnf; the
/// legacy `dnf` command (dnf4) isn't installed and its package conflicts with
/// dnf5. `--releasever` is required (dnf can't detect it off an Arch host);
/// `install_weak_deps=False` keeps the tree minimal. The temp repo dir is cleaned
/// via a trap regardless of outcome.
fn dnf_bootstrap_cmd(s: &Stratum, root: &str) -> String {
    let rel = s.suite.clone().unwrap_or_else(|| FEDORA_DEFAULT_RELEASE.to_string());
    let repo = fedora_repo_file(s.mirror.as_deref());
    format!(
        "d=\"$(mktemp -d)\" && trap 'rm -rf \"$d\"' EXIT && \
         cat > \"$d/manifest-fedora.repo\" <<'REPO'\n\
         {repo}REPO\n\
         dnf5 -y --installroot={root_q} --releasever={rel_q} \
         --setopt=install_weak_deps=False --setopt=reposdir=\"$d\" \
         install fedora-release dnf coreutils bash",
        repo = repo,
        root_q = shell_quote(root),
        rel_q = shell_quote(&rel),
    )
}

/// The Alpine bootstrap command. Alpine's tooling and signing keys aren't
/// packaged on Arch, so — the standard cross-distro path — it downloads
/// `apk.static` and `alpine-keys` over HTTPS from the official CDN, then runs
/// `apk.static --keys-dir …` so the index/packages are **signature-verified**
/// (never `--allow-untrusted`). Versions are resolved from the branch's APKINDEX
/// (they change). The rootfs's `/etc/apk/repositories` is written so a later
/// in-stratum `apk add` works; `alpine-base` pulls `alpine-keys` into the rootfs,
/// so that stays verified too. musl throughout — Alpine binaries run only through
/// their own shims (which chroot into this rootfs, resolving Alpine's `ld-musl`).
fn apk_bootstrap_cmd(s: &Stratum, root: &str) -> String {
    let branch = s.suite.clone().unwrap_or_else(|| default_suite(&s.distro).to_string());
    let mirror = s.mirror.clone().unwrap_or_else(|| default_mirror(Backend::Apk, &s.distro));
    format!(
        "set -e\n\
         d=\"$(mktemp -d)\"; trap 'rm -rf \"$d\"' EXIT\n\
         M={mirror_q}; BR={branch_q}; ROOT={root_q}; A=\"$M/$BR/main/x86_64\"\n\
         mkdir -p \"$ROOT\"\n\
         curl -fsS \"$A/APKINDEX.tar.gz\" | tar -xziO APKINDEX > \"$d/idx\"\n\
         av=$(awk '/^P:apk-tools-static$/{{f=1}} f&&/^V:/{{print substr($0,3); exit}}' \"$d/idx\")\n\
         kv=$(awk '/^P:alpine-keys$/{{f=1}} f&&/^V:/{{print substr($0,3); exit}}' \"$d/idx\")\n\
         [ -n \"$av\" ] && [ -n \"$kv\" ] || {{ echo 'strata: could not resolve apk-tools-static/alpine-keys from the Alpine APKINDEX' >&2; exit 1; }}\n\
         curl -fsS \"$A/apk-tools-static-$av.apk\" | tar -xzi -C \"$d\" 2>/dev/null\n\
         curl -fsS \"$A/alpine-keys-$kv.apk\" | tar -xzi -C \"$d\" 2>/dev/null\n\
         kd=\"$d/usr/share/apk/keys/x86_64\"; [ -d \"$kd\" ] || kd=\"$d/usr/share/apk/keys\"\n\
         \"$d/sbin/apk.static\" --keys-dir \"$kd\" --arch x86_64 -X \"$M/$BR/main\" -X \"$M/$BR/community\" --root \"$ROOT\" --initdb --no-cache add alpine-base\n\
         mkdir -p \"$ROOT/etc/apk\"; printf '%s\\n%s\\n' \"$M/$BR/main\" \"$M/$BR/community\" > \"$ROOT/etc/apk/repositories\"",
        mirror_q = shell_quote(&mirror),
        branch_q = shell_quote(&branch),
        root_q = shell_quote(root),
    )
}

/// The command run *inside* the stratum to install its `packages`.
fn in_stratum_install_cmd(backend: Backend, packages: &[String]) -> String {
    let list = packages.iter().map(|p| shell_quote(p)).collect::<Vec<_>>().join(" ");
    match backend {
        Backend::Debootstrap => {
            // Update indices then install; noninteractive so apt never prompts.
            format!("export DEBIAN_FRONTEND=noninteractive; apt-get update && apt-get install -y {list}")
        }
        Backend::Dnf => format!("dnf install -y {list}"),
        Backend::Apk => format!("apk add {list}"),
    }
}

/// The per-stratum "enter" helper: create a private mount namespace, bind the
/// stratum's mounts (auto-unmounted when the process exits — nothing to leak on
/// rollback), copy in resolv.conf if shared, set a standard PATH, then chroot and
/// exec. Two modes (the shim picks; see [`shim_script`]):
///   `root` — run the command as root (package managers).
///   `user` — drop to the invoking user (uid/gid/groups passed as args, since
///            sudo resets the environment) and export their display env, so GUI
///            foreign apps can reach the shared Wayland/X socket. Pure text.
fn enter_helper_script(s: &Stratum) -> String {
    let root = stratum_root(&s.name);
    let binds = bind_mounts(s).join(" ");
    let copy_resolv = if effective_shares(s).iter().any(|x| x == "resolv") {
        // rm first: the stratum's resolv.conf may be a dangling symlink (Fedora),
        // which `cp` onto would try to follow. Remove, then copy the host's file.
        "rm -f \"$root/etc/resolv.conf\"; cp -L /etc/resolv.conf \"$root/etc/resolv.conf\" 2>/dev/null || true\n  "
    } else {
        ""
    };
    format!(
        "#!/bin/sh\n\
         # ManifestOS strata: enter '{name}' in a private mount namespace and exec.\n\
         # Generated by `manifest` — do not edit; re-run install to regenerate.\n\
         # usage: enter-{name} root <cmd> [args...]\n\
         #        enter-{name} user <uid> <gid> <groups> <home> <disp> <wl> <xrd> <xauth> <cmd> [args...]\n\
         set -e\n\
         root={root_q}\n\
         [ -d \"$root\" ] || {{ echo \"strata: stratum '{name}' not installed ($root)\" >&2; exit 1; }}\n\
         exec unshare --mount --propagation private -- /bin/sh -c '\n  \
         root=$1; mode=$2; shift 2\n  \
         for m in {binds}; do\n    \
         {{ [ -d \"/$m\" ] && [ -d \"$root/$m\" ]; }} && mount --rbind \"/$m\" \"$root/$m\"\n  \
         done\n  \
         for share in terminfo fonts icons; do mkdir -p \"$root/usr/share/$share\" 2>/dev/null; {{ [ -d \"/usr/share/$share\" ]; }} && mount --bind \"/usr/share/$share\" \"$root/usr/share/$share\" 2>/dev/null; done; true\n  \
         {copy_resolv}export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin TERMINFO_DIRS=/usr/share/terminfo\n  \
         if [ \"$mode\" = user ]; then\n    \
         uid=$1; gid=$2; groups=$3; home=$4; disp=$5; wl=$6; xrd=$7; xauth=$8; shift 8\n    \
         [ \"${{SUDO_UID:-$uid}}\" = \"$uid\" ] || {{ echo \"strata: refusing to run as uid $uid (caller is $SUDO_UID)\" >&2; exit 1; }}\n    \
         export HOME=\"$home\" DISPLAY=\"$disp\" WAYLAND_DISPLAY=\"$wl\" XDG_RUNTIME_DIR=\"$xrd\" XAUTHORITY=\"$xauth\"\n    \
         exec chroot --userspec=\"$uid:$gid\" --groups=\"$groups\" \"$root\" /usr/bin/env \"$@\"\n  \
         fi\n  \
         exec chroot \"$root\" /usr/bin/env \"$@\"\n\
         ' sh \"$root\" \"$@\"\n",
        name = s.name,
        root_q = shell_quote(&root),
        binds = binds,
        copy_resolv = copy_resolv,
    )
}

/// A single exposed-binary shim handing off to the stratum's enter helper (via
/// sudo — the mount/chroot setup needs root). Two shapes:
///
/// **Package managers** (`runs_as_root`) run as root and then **auto-expose**:
/// they diff the stratum's bin dirs around the run and, for each newly-installed
/// binary the host doesn't already have (so host tools are never shadowed), call
/// `manifest strata add --expose` to shim it. So `apt install htop` makes `htop`
/// usable with no separate expose step.
///
/// **Everything else** runs as the invoking user with their display env forwarded
/// (`id`/`$HOME`/`$WAYLAND_DISPLAY`/… captured here, before sudo strips them) so
/// GUI/TUI apps reach the shared Wayland/X socket.
fn shim_script(stratum: &str, bin: &str) -> String {
    let helper = enter_helper_path(stratum);
    let bin_q = shell_quote(bin);
    if runs_as_root(bin) {
        let root_q = shell_quote(&stratum_root(stratum));
        let stratum_q = shell_quote(stratum);
        format!(
            "#!/bin/sh\n\
             # ManifestOS strata shim → {stratum}:{bin} (root + auto-expose; generated, do not edit)\n\
             __root={root_q}\n\
             __b=$(mktemp 2>/dev/null || echo \"/tmp/.mos-shim-$$\")\n\
             ls \"$__root/usr/bin\" \"$__root/bin\" \"$__root/usr/local/bin\" 2>/dev/null | sort -u > \"$__b\"\n\
             sudo {helper} root {bin_q} \"$@\"\n\
             __rc=$?\n\
             __add=\n\
             for __x in $(ls \"$__root/usr/bin\" \"$__root/bin\" \"$__root/usr/local/bin\" 2>/dev/null | sort -u | grep -Fxvf \"$__b\"); do\n  \
             command -v \"$__x\" >/dev/null 2>&1 || __add=\"$__add $__x\"\n\
             done\n\
             rm -f \"$__b\"\n\
             [ -n \"$__add\" ] && sudo manifest strata add {stratum_q} --expose $__add >/dev/null 2>&1\n\
             exit $__rc\n",
        )
    } else {
        format!(
            "#!/bin/sh\n\
             # ManifestOS strata shim → {stratum}:{bin} (user; generated, do not edit)\n\
             exec sudo {helper} user \"$(id -u)\" \"$(id -g)\" \"$(id -G | tr ' ' ,)\" \
             \"${{HOME:-/root}}\" \"${{DISPLAY-}}\" \"${{WAYLAND_DISPLAY-}}\" \
             \"${{XDG_RUNTIME_DIR-}}\" \"${{XAUTHORITY-}}\" {bin_q} \"$@\"\n",
        )
    }
}

/// The strata shell-integration text, sourced by *interactive* bash and zsh
/// (profile.d is not enough — zsh never reads it, and it only covers login
/// shells). It puts the exposed-binary dir (`/strata/.bin`) on PATH so shimmed
/// foreign binaries are findable in a normal terminal, and installs a
/// command-not-found handler that maps an uninstalled package manager to its
/// distro, offers to add a stratum, then makes the new shim usable in the
/// *current* shell and retries the command. Only bootstrappable distros are
/// mapped, so the offer never dead-ends; both bash (`command_not_found_handle`)
/// and zsh (`command_not_found_handler`) hooks are defined.
fn cnf_handler_script() -> &'static str {
    "# ManifestOS strata — shell integration (PATH + command-not-found).\n\
     # Generated; edits are overwritten. Sourced by interactive bash/zsh.\n\
     case \":$PATH:\" in\n  \
       *:/strata/.bin:*) ;;\n  \
       *) PATH=\"/strata/.bin:$PATH\"; export PATH ;;\n\
     esac\n\
     __manifest_cnf() {\n  \
       cmd=$1\n  \
       case $cmd in\n    \
         apt|apt-get|apt-cache|dpkg|dpkg-query|add-apt-repository) distro=debian ;;\n    \
         dnf|dnf5|yum|rpm|rpm2cpio) distro=fedora ;;\n    \
         apk) distro=alpine ;;\n    \
         *) return 127 ;;\n  \
       esac\n  \
       printf '\\n%s is not installed — it comes from %s.\\n' \"$cmd\" \"$distro\" >&2\n  \
       if [ -t 0 ] && [ -t 2 ]; then\n    \
         printf 'Add a %s stratum and put %s on your PATH? [y/N] ' \"$distro\" \"$cmd\" >&2\n    \
         read -r __r\n    \
         case $__r in\n      \
           [yY]|[yY][eE][sS])\n        \
             sudo manifest strata add \"$distro\" --expose \"$cmd\" || return $?\n        \
             case \":$PATH:\" in *:/strata/.bin:*) ;; *) PATH=\"/strata/.bin:$PATH\"; export PATH ;; esac\n        \
             hash -r 2>/dev/null\n        \
             \"$@\"\n        \
             return $?\n        \
             ;;\n    \
         esac\n  \
       fi\n  \
       printf 'Add it with:  sudo manifest strata add %s --expose %s\\n' \"$distro\" \"$cmd\" >&2\n  \
       return 127\n\
     }\n\
     command_not_found_handle() { __manifest_cnf \"$@\"; }\n\
     command_not_found_handler() { __manifest_cnf \"$@\"; }\n"
}

/// profile.d drop-in adding the shim dir to PATH for login shells.
fn profile_d_script() -> String {
    format!(
        "# ManifestOS strata — expose foreign-distro binaries on PATH (generated)\n\
         case \":$PATH:\" in\n  \
         *:{bin}:*) ;;\n  \
         *) PATH=\"{bin}:$PATH\" ;;\n\
         esac\n\
         export PATH\n",
        bin = BIN_DIR,
    )
}

/// Single-quote a value for safe use in a `/bin/sh` command line.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stratum(name: &str, distro: &str) -> Stratum {
        Stratum {
            name: name.to_string(),
            distro: distro.to_string(),
            suite: None,
            mirror: None,
            snapshot: None,
            packages: vec![],
            expose: vec![],
            share: vec![],
        }
    }

    #[test]
    fn backend_selection_maps_known_distros() {
        assert_eq!(backend_for("debian"), Some(Backend::Debootstrap));
        assert_eq!(backend_for("Ubuntu"), Some(Backend::Debootstrap));
        assert_eq!(backend_for("fedora"), Some(Backend::Dnf));
        assert_eq!(backend_for("alpine"), Some(Backend::Apk));
        assert_eq!(backend_for("plan9"), None);
    }

    #[test]
    fn default_shares_used_when_none_declared() {
        let s = stratum("debian", "debian");
        assert_eq!(effective_shares(&s), DEFAULT_SHARES);
        // An explicit list wins verbatim.
        let mut s2 = stratum("d", "debian");
        s2.share = vec!["home".into(), "resolv".into()];
        assert_eq!(effective_shares(&s2), vec!["home".to_string(), "resolv".to_string()]);
    }

    #[test]
    fn bind_mounts_always_include_base_and_add_home_tmp_when_shared() {
        let s = stratum("debian", "debian"); // default shares include home + tmp
        let binds = bind_mounts(&s);
        for base in ALWAYS_BOUND {
            assert!(binds.contains(&base.to_string()), "missing base bind {base}");
        }
        assert!(binds.contains(&"home".to_string()));
        assert!(binds.contains(&"tmp".to_string()));

        // A stratum sharing neither home nor tmp binds only the base set.
        let mut s2 = stratum("d", "debian");
        s2.share = vec!["resolv".into()];
        let binds2 = bind_mounts(&s2);
        assert!(!binds2.contains(&"home".to_string()));
        assert!(!binds2.contains(&"tmp".to_string()));
        assert_eq!(binds2.len(), ALWAYS_BOUND.len());
    }

    #[test]
    fn snapshot_pins_route_through_the_snapshot_archive() {
        let mut s = stratum("debian", "debian");
        s.snapshot = Some("20260701T000000Z".into());
        assert_eq!(
            resolve_mirror(&s, Backend::Debootstrap),
            "https://snapshot.debian.org/archive/debian/20260701T000000Z/"
        );
        let mut u = stratum("ubuntu", "ubuntu");
        u.snapshot = Some("20260701T000000Z".into());
        assert_eq!(
            resolve_mirror(&u, Backend::Debootstrap),
            "https://snapshot.ubuntu.com/ubuntu/20260701T000000Z/"
        );
    }

    #[test]
    fn mirror_defaults_and_explicit_override() {
        let s = stratum("debian", "debian");
        assert_eq!(resolve_mirror(&s, Backend::Debootstrap), "https://deb.debian.org/debian");
        let u = stratum("ubuntu", "ubuntu");
        assert_eq!(resolve_mirror(&u, Backend::Debootstrap), "http://archive.ubuntu.com/ubuntu");
        // An explicit mirror wins when there's no snapshot pin.
        let mut e = stratum("debian", "debian");
        e.mirror = Some("https://my.mirror/debian".into());
        assert_eq!(resolve_mirror(&e, Backend::Debootstrap), "https://my.mirror/debian");
    }

    #[test]
    fn bootstrap_cmd_is_minbase_and_verifies_signatures() {
        let mut s = stratum("debian", "debian");
        s.suite = Some("bookworm".into());
        let (_, keyring) = keyring_for("debian").unwrap();
        let cmd = debootstrap_cmd(&s, "/strata/debian", keyring);
        assert!(cmd.contains("debootstrap --variant=minbase"), "{cmd}");
        // Explicit arch: without dpkg, debootstrap can't map x86_64 → amd64 itself.
        assert!(cmd.contains("--arch=amd64"), "{cmd}");
        assert!(cmd.contains("'bookworm'"), "{cmd}");
        assert!(cmd.contains("'/strata/debian'"), "{cmd}");
        assert!(cmd.contains("'https://deb.debian.org/debian'"), "{cmd}");
        // Signature verification must be enforced: an explicit --keyring, never
        // --no-check-gpg (debootstrap silently skips verification without one).
        assert!(cmd.contains("--keyring='/usr/share/keyrings/debian-archive-keyring.gpg'"), "{cmd}");
        assert!(!cmd.contains("--no-check-gpg"), "GPG verification must stay on: {cmd}");
    }

    #[test]
    fn keyring_maps_debian_and_ubuntu_to_official_packages() {
        assert_eq!(
            keyring_for("debian"),
            Some(("debian-archive-keyring", "/usr/share/keyrings/debian-archive-keyring.gpg"))
        );
        assert_eq!(
            keyring_for("Ubuntu"),
            Some(("ubuntu-keyring", "/usr/share/keyrings/ubuntu-archive-keyring.gpg"))
        );
        assert_eq!(keyring_for("fedora"), None);
    }

    #[test]
    fn default_suite_per_distro() {
        let s = stratum("ubuntu", "ubuntu");
        let (_, uk) = keyring_for("ubuntu").unwrap();
        let cmd = debootstrap_cmd(&s, "/r", uk);
        assert!(cmd.contains("'noble'"), "{cmd}"); // ubuntu default
        let d = stratum("debian", "debian");
        let (_, dk) = keyring_for("debian").unwrap();
        let cmd = debootstrap_cmd(&d, "/r", dk);
        assert!(cmd.contains("'stable'"), "{cmd}"); // debian default
    }

    #[test]
    fn backend_selection_maps_fedora_and_used_backends() {
        assert_eq!(backend_for("fedora"), Some(Backend::Dnf));
        let d = stratum("debian", "debian");
        let f = stratum("fedora", "fedora");
        let used = used_backends(&[d, f]);
        assert!(used.contains(&Backend::Debootstrap));
        assert!(used.contains(&Backend::Dnf));
        assert!(!used.contains(&Backend::Apk));
    }

    #[test]
    fn dnf_bootstrap_verifies_and_pins_releasever() {
        // Default release when suite is unset.
        let s = stratum("fedora", "fedora");
        let cmd = dnf_bootstrap_cmd(&s, "/strata/fedora");
        assert!(cmd.contains("dnf5 -y"), "must use dnf5 (dnf4 conflicts): {cmd}");
        assert!(cmd.contains(&format!("--releasever='{FEDORA_DEFAULT_RELEASE}'")), "{cmd}");
        assert!(cmd.contains("--installroot='/strata/fedora'"), "{cmd}");
        assert!(cmd.contains("--setopt=install_weak_deps=False"), "{cmd}");
        assert!(cmd.contains("--setopt=reposdir="), "{cmd}");
        // Default source is the metalink (mirror failover), for both repos.
        assert!(cmd.contains("metalink=https://mirrors.fedoraproject.org/metalink?repo=fedora-$releasever"), "{cmd}");
        assert!(cmd.contains("repo=updates-released-f$releasever"), "{cmd}");
        // Verification enforced: gpgcheck on + the distribution-gpg-keys key, never off.
        assert!(cmd.contains("gpgcheck=1"), "{cmd}");
        assert!(cmd.contains("gpgkey=file:///usr/share/distribution-gpg-keys/fedora/RPM-GPG-KEY-fedora-$releasever-primary"), "{cmd}");
        assert!(!cmd.contains("nogpgcheck") && !cmd.contains("gpgcheck=0"), "{cmd}");
        // Temp repo dir is cleaned up.
        assert!(cmd.contains("trap 'rm -rf \"$d\"' EXIT"), "{cmd}");
    }

    #[test]
    fn dnf_bootstrap_custom_mirror_uses_baseurl() {
        let mut s = stratum("fedora", "fedora");
        s.suite = Some("41".into());
        s.mirror = Some("https://my.mirror/fedora".into());
        let cmd = dnf_bootstrap_cmd(&s, "/r");
        assert!(cmd.contains("--releasever='41'"), "{cmd}");
        // A custom mirror switches metalink → baseurl (their single host).
        assert!(cmd.contains("baseurl=https://my.mirror/fedora/releases/$releasever/Everything/$basearch/os/"), "{cmd}");
        assert!(!cmd.contains("metalink="), "custom mirror must not also use metalink: {cmd}");
    }

    #[test]
    fn apk_bootstrap_verifies_and_resolves_versions() {
        let s = stratum("alpine", "alpine"); // default branch latest-stable
        let cmd = apk_bootstrap_cmd(&s, "/strata/alpine");
        assert!(cmd.contains("BR='latest-stable'"), "default branch: {cmd}");
        assert!(cmd.contains("'https://dl-cdn.alpinelinux.org/alpine'"), "{cmd}");
        // Resolves versions from the APKINDEX, downloads apk.static + keys.
        assert!(cmd.contains("apk-tools-static-$av.apk"), "{cmd}");
        assert!(cmd.contains("alpine-keys-$kv.apk"), "{cmd}");
        // Verification enforced: --keys-dir, never --allow-untrusted.
        assert!(cmd.contains("--keys-dir"), "{cmd}");
        assert!(!cmd.contains("--allow-untrusted"), "must verify: {cmd}");
        assert!(cmd.contains("add alpine-base"), "{cmd}");
        // Rootfs repos written so in-stratum `apk add` works.
        assert!(cmd.contains("/etc/apk/repositories"), "{cmd}");
    }

    #[test]
    fn in_stratum_install_dnf_and_apk_shapes() {
        assert_eq!(
            in_stratum_install_cmd(Backend::Dnf, &["git".into()]),
            "dnf install -y 'git'"
        );
        assert_eq!(
            in_stratum_install_cmd(Backend::Apk, &["git".into()]),
            "apk add 'git'"
        );
    }

    #[test]
    fn in_stratum_install_is_noninteractive() {
        let cmd = in_stratum_install_cmd(Backend::Debootstrap, &["gcc".into(), "make".into()]);
        assert!(cmd.contains("DEBIAN_FRONTEND=noninteractive"), "{cmd}");
        assert!(cmd.contains("apt-get update"), "{cmd}");
        assert!(cmd.contains("apt-get install -y 'gcc' 'make'"), "{cmd}");
    }

    #[test]
    fn enter_helper_uses_private_mount_ns_and_chroots() {
        let mut s = stratum("debian", "debian");
        s.share = vec!["home".into(), "resolv".into(), "tmp".into()];
        let script = enter_helper_script(&s);
        assert!(script.starts_with("#!/bin/sh"), "{script}");
        assert!(script.contains("unshare --mount --propagation private"), "{script}");
        assert!(script.contains("mount --rbind"), "{script}");
        assert!(script.contains("cp -L /etc/resolv.conf"), "{script}"); // resolv shared
        assert!(script.contains("exec chroot \"$root\" /usr/bin/env \"$@\""), "{script}");
        // Base binds + the shared home/tmp all appear in the mount loop.
        assert!(script.contains("for m in proc sys dev run home tmp;"), "{script}");
        // Host terminfo + fonts/icons are bound in (target mkdir'd first, so it
        // lands on Alpine which ships no /usr/share/terminfo) and TERMINFO_DIRS
        // pins the search path so ncurses finds them regardless of distro default.
        assert!(script.contains("for share in terminfo fonts icons"), "{script}");
        assert!(script.contains("TERMINFO_DIRS=/usr/share/terminfo"), "{script}");
    }

    #[test]
    fn enter_helper_omits_resolv_copy_when_not_shared() {
        let mut s = stratum("debian", "debian");
        s.share = vec!["home".into()]; // no resolv
        let script = enter_helper_script(&s);
        assert!(!script.contains("cp -L /etc/resolv.conf"), "{script}");
    }

    #[test]
    fn bare_shim_winner_is_first_stratum_in_order() {
        let mut d = stratum("debian", "debian");
        d.expose = vec!["apt".into(), "tree".into()];
        let mut u = stratum("ubuntu", "ubuntu");
        u.expose = vec!["apt".into()]; // collides with debian's apt
        let winners = bare_shim_winners(&[d, u]);
        // debian wins bare `apt` and `tree`; ubuntu's apt gets no bare shim.
        assert!(winners.contains(&("debian".into(), "apt".into())));
        assert!(winners.contains(&("debian".into(), "tree".into())));
        assert!(!winners.contains(&("ubuntu".into(), "apt".into())));
        assert_eq!(winners.len(), 2);
    }

    #[test]
    fn prefixed_name_is_stratum_dash_bin() {
        assert_eq!(prefixed_name("ubuntu", "apt"), "ubuntu-apt");
    }

    #[test]
    fn runs_as_root_only_for_package_managers() {
        for p in ["apt", "apt-get", "dpkg", "dnf", "dnf5", "rpm", "pacman", "apk"] {
            assert!(runs_as_root(p), "{p} should run as root");
        }
        for u in ["chromium", "gcc", "gimp", "code", "vim", "firefox"] {
            assert!(!runs_as_root(u), "{u} should run as the user");
        }
    }

    #[test]
    fn shim_root_for_package_managers_auto_exposes() {
        let shim = shim_script("debian", "apt");
        assert!(shim.starts_with("#!/bin/sh"), "{shim}");
        assert!(shim.contains("sudo /strata/.libexec/enter-debian root 'apt' \"$@\""), "{shim}");
        // Diffs the stratum's bins and exposes new ones the host lacks.
        assert!(shim.contains("__root='/strata/debian'"), "{shim}");
        assert!(shim.contains("grep -Fxvf"), "diff before/after: {shim}");
        assert!(shim.contains("command -v \"$__x\" >/dev/null 2>&1 || __add"), "skip host tools: {shim}");
        assert!(shim.contains("sudo manifest strata add 'debian' --expose $__add"), "{shim}");
    }

    #[test]
    fn shim_user_for_apps_forwards_identity_and_display() {
        let shim = shim_script("debian", "chromium");
        // user mode, invoking user's identity + display env captured before sudo.
        assert!(shim.contains("enter-debian user \"$(id -u)\" \"$(id -g)\""), "{shim}");
        assert!(shim.contains("$(id -G | tr ' ' ,)"), "supplementary groups (GPU access): {shim}");
        assert!(shim.contains("${WAYLAND_DISPLAY-}"), "{shim}");
        assert!(shim.contains("${XDG_RUNTIME_DIR-}"), "{shim}");
        assert!(shim.contains("'chromium' \"$@\""), "{shim}");
    }

    #[test]
    fn enter_helper_has_root_and_user_modes() {
        let s = stratum("debian", "debian");
        let script = enter_helper_script(&s);
        assert!(script.contains("if [ \"$mode\" = user ]; then"), "{script}");
        // Drop via GNU chroot's own --userspec/--groups (host coreutils) — not the
        // stratum's setpriv, which on Alpine is BusyBox's and lacks --reuid.
        assert!(script.contains("chroot --userspec=\"$uid:$gid\" --groups=\"$groups\" \"$root\""), "{script}");
        assert!(script.contains("export HOME=\"$home\" DISPLAY=\"$disp\" WAYLAND_DISPLAY=\"$wl\""), "{script}");
        // Passwordless safety: a caller can only run as themselves ($SUDO_UID),
        // never as an arbitrary uid — otherwise the sudoers rule would be root.
        assert!(script.contains("[ \"${SUDO_UID:-$uid}\" = \"$uid\" ]"), "{script}");
    }

    #[test]
    fn rewrite_desktop_points_exec_at_the_shim() {
        let src = "[Desktop Entry]\nName=Chromium\nExec=/usr/bin/chromium %U\n\
                   TryExec=/usr/bin/chromium\nDBusActivatable=true\nIcon=chromium\n\
                   [Desktop Action new-window]\nExec=/usr/bin/chromium --new-window\n";
        let out = rewrite_desktop(src, "/strata/.bin/chromium");
        assert!(out.contains("Exec=/strata/.bin/chromium %U"), "{out}");
        assert!(out.contains("Exec=/strata/.bin/chromium --new-window"), "action rewritten: {out}");
        assert!(out.contains("TryExec=/strata/.bin/chromium"), "{out}");
        assert!(out.contains("DBusActivatable=false"), "{out}");
        // Untouched fields survive.
        assert!(out.contains("Name=Chromium"), "{out}");
        assert!(out.contains("Icon=chromium"), "{out}");
        assert!(out.starts_with("# Generated by ManifestOS strata"), "{out}");
    }

    #[test]
    fn cnf_handler_maps_pkg_managers_and_defines_both_hooks() {
        let s = cnf_handler_script();
        // Puts the shim dir on PATH (the whole point — profile.d doesn't reach zsh).
        assert!(s.contains("PATH=\"/strata/.bin:$PATH\""), "{s}");
        assert!(s.contains("apt|apt-get|apt-cache|dpkg|dpkg-query|add-apt-repository) distro=debian"), "{s}");
        assert!(s.contains("dnf|dnf5|yum|rpm|rpm2cpio) distro=fedora"), "{s}");
        assert!(s.contains("apk) distro=alpine"), "{s}");
        // All four backends are bootstrappable now, so all are offered.
        // Both shells' hooks + the actionable command.
        assert!(s.contains("command_not_found_handle()"), "bash hook: {s}");
        assert!(s.contains("command_not_found_handler()"), "zsh hook: {s}");
        assert!(s.contains("sudo manifest strata add \"$distro\" --expose \"$cmd\""), "{s}");
    }

    #[test]
    fn profile_d_prepends_bin_dir_idempotently() {
        let p = profile_d_script();
        assert!(p.contains("/strata/.bin"), "{p}");
        assert!(p.contains("case \":$PATH:\""), "{p}"); // guarded against double-add
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote("plain"), "'plain'");
    }
}
