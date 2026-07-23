//! Foreign-distro **strata** — Bedrock-style multi-distro binary access.
//!
//! A stratum is a full foreign-distro rootfs living under `/bedrock/strata/<name>`.
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

/// Where strata rootfs trees live. Borrowed from Bedrock's layout convention;
/// we are *not* Bedrock-compatible beyond this path.
const STRATA_ROOT: &str = "/bedrock/strata";
/// Generated per-stratum "enter" helpers.
const LIBEXEC_DIR: &str = "/bedrock/libexec";
/// Generated shims, added to PATH via a profile.d drop-in.
const BIN_DIR: &str = "/bedrock/bin";
/// profile.d drop-in that puts [`BIN_DIR`] on every login shell's PATH.
const PROFILE_D: &str = "/etc/profile.d/00-manifest-strata.sh";

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
    // expose the same binary name would otherwise collide at /bedrock/bin/<name>
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
    Ok(())
}

fn apply_one(s: &Stratum, bare_winners: &std::collections::HashSet<(String, String)>, ctx: &Ctx) -> Result<()> {
    let backend = match backend_for(&s.distro) {
        Some(Backend::Apk) => bail!(
            "stratum '{}': alpine (musl) is recognized but not implemented yet — the \
             debootstrap (debian/ubuntu) and dnf (fedora) backends are (see \
             docs/strata-design.md §10)",
            s.name
        ),
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
        Backend::Apk => unreachable!("apk bails above"),
    };

    bootstrap(s, backend, &root, keyring.as_deref(), ctx)?;
    install_in_stratum(s, backend, &root, ctx)?;
    write_enter_helper(s, ctx)?;
    write_shims(s, bare_winners, ctx)?;
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
    }
    if backends.contains(&Backend::Dnf) {
        // dnf5 is Arch's current dnf (the older `dnf` package is dnf4 and
        // *conflicts* with dnf5); distribution-gpg-keys carries the Fedora keys.
        pkgs.push("dnf5");
        pkgs.push("distribution-gpg-keys");
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

/// Put the shim dir on PATH for every login shell.
fn write_profile_d(ctx: &Ctx) -> Result<()> {
    ctx.write_root(PROFILE_D, &profile_d_script())
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
    for rc in ["/etc/bash.bashrc", "/etc/zsh/zshrc"] {
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
        Backend::Apk => bail!("internal: apk backend has no bootstrap command yet"),
    }
}

/// The `debootstrap` command line. `--variant=minbase` keeps the rootfs small.
/// `--keyring=<path>` is passed explicitly so signatures are actually verified:
/// debootstrap does NOT fail on a missing keyring, it warns and bootstraps
/// unverified, so [`ensure_keyring`] installs the keyring and we point at it here
/// (never `--no-check-gpg` — a manifest disabling verification is a marketplace
/// finding, see docs §9).
fn debootstrap_cmd(s: &Stratum, root: &str, keyring: &str) -> String {
    let suite = s.suite.clone().unwrap_or_else(|| default_suite(&s.distro).to_string());
    let mirror = resolve_mirror(s, Backend::Debootstrap);
    format!(
        "debootstrap --variant=minbase --keyring={} {} {} {}",
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
/// rollback), copy in resolv.conf if shared, set a standard PATH, and
/// `chroot … env <cmd>` so the exposed binary resolves against the stratum's own
/// PATH and libraries. Pure text — unit-tested.
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
         # usage: enter-{name} <cmd> [args...]\n\
         set -e\n\
         root={root_q}\n\
         [ -d \"$root\" ] || {{ echo \"strata: stratum '{name}' not installed ($root)\" >&2; exit 1; }}\n\
         exec unshare --mount --propagation private -- /bin/sh -c '\n  \
         root=$1; shift\n  \
         for m in {binds}; do\n    \
         {{ [ -d \"/$m\" ] && [ -d \"$root/$m\" ]; }} && mount --rbind \"/$m\" \"$root/$m\"\n  \
         done\n  \
         {copy_resolv}export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\n  \
         exec chroot \"$root\" /usr/bin/env \"$@\"\n\
         ' sh \"$root\" \"$@\"\n",
        name = s.name,
        root_q = shell_quote(&root),
        binds = binds,
        copy_resolv = copy_resolv,
    )
}

/// A single exposed-binary shim: a one-liner that hands off to the stratum's
/// enter helper (via sudo, since the mount/chroot setup needs root). The bare
/// binary name is resolved against the stratum's PATH inside the chroot.
fn shim_script(stratum: &str, bin: &str) -> String {
    format!(
        "#!/bin/sh\n\
         # ManifestOS strata shim → {stratum}:{bin}  (generated; do not edit)\n\
         exec sudo {helper} {bin_q} \"$@\"\n",
        stratum = stratum,
        bin = bin,
        helper = enter_helper_path(stratum),
        bin_q = shell_quote(bin),
    )
}

/// The command-not-found handler shell text. When an uninstalled package manager
/// (`apt`, `dnf`, …) is run, map it to its distro and offer to add a stratum.
/// Defines both bash (`command_not_found_handle`) and zsh
/// (`command_not_found_handler`) hooks; only distros the engine can bootstrap are
/// mapped, so the offer never leads to a "not implemented" error.
fn cnf_handler_script() -> &'static str {
    "# ManifestOS strata — offer to add a foreign-distro stratum when an\n\
     # uninstalled package manager is invoked. Generated; edits are overwritten.\n\
     __manifest_cnf() {\n  \
       cmd=$1\n  \
       case $cmd in\n    \
         apt|apt-get|apt-cache|dpkg|dpkg-query|add-apt-repository) distro=debian ;;\n    \
         dnf|dnf5|yum|rpm|rpm2cpio) distro=fedora ;;\n    \
         *) return 127 ;;\n  \
       esac\n  \
       printf '\\n%s is not installed — it comes from %s.\\n' \"$cmd\" \"$distro\" >&2\n  \
       if [ -t 0 ] && [ -t 2 ]; then\n    \
         printf 'Add a %s stratum and put %s on your PATH? [y/N] ' \"$distro\" \"$cmd\" >&2\n    \
         read -r __r\n    \
         case $__r in\n      \
           [yY]|[yY][eE][sS]) sudo manifest strata add \"$distro\" --expose \"$cmd\"; return $? ;;\n    \
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
        let cmd = debootstrap_cmd(&s, "/bedrock/strata/debian", keyring);
        assert!(cmd.contains("debootstrap --variant=minbase"), "{cmd}");
        assert!(cmd.contains("'bookworm'"), "{cmd}");
        assert!(cmd.contains("'/bedrock/strata/debian'"), "{cmd}");
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
        let cmd = dnf_bootstrap_cmd(&s, "/bedrock/strata/fedora");
        assert!(cmd.contains("dnf5 -y"), "must use dnf5 (dnf4 conflicts): {cmd}");
        assert!(cmd.contains(&format!("--releasever='{FEDORA_DEFAULT_RELEASE}'")), "{cmd}");
        assert!(cmd.contains("--installroot='/bedrock/strata/fedora'"), "{cmd}");
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
    fn shim_hands_off_to_the_enter_helper() {
        let shim = shim_script("debian", "apt");
        assert!(shim.starts_with("#!/bin/sh"), "{shim}");
        assert!(shim.contains("exec sudo /bedrock/libexec/enter-debian 'apt' \"$@\""), "{shim}");
    }

    #[test]
    fn cnf_handler_maps_pkg_managers_and_defines_both_hooks() {
        let s = cnf_handler_script();
        assert!(s.contains("apt|apt-get|apt-cache|dpkg|dpkg-query|add-apt-repository) distro=debian"), "{s}");
        assert!(s.contains("dnf|dnf5|yum|rpm|rpm2cpio) distro=fedora"), "{s}");
        // Only bootstrappable distros are offered (no alpine → no dead-end offer).
        assert!(!s.contains("alpine"), "{s}");
        // Both shells' hooks + the actionable command.
        assert!(s.contains("command_not_found_handle()"), "bash hook: {s}");
        assert!(s.contains("command_not_found_handler()"), "zsh hook: {s}");
        assert!(s.contains("sudo manifest strata add \"$distro\" --expose \"$cmd\""), "{s}");
    }

    #[test]
    fn profile_d_prepends_bin_dir_idempotently() {
        let p = profile_d_script();
        assert!(p.contains("/bedrock/bin"), "{p}");
        assert!(p.contains("case \":$PATH:\""), "{p}"); // guarded against double-add
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote("plain"), "'plain'");
    }
}
