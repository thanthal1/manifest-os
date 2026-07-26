# Strata — multi-distro package access (design)

> Status: **Phases 1–3 complete + real-hardware-validated; shipped as `manifest-os`
> 0.1.0-18** (2026-07-25). **All four distros — Debian, Ubuntu, Fedora, Alpine —**
> bootstrap verified (incl. the CachyOS/hwcaps arch fix — install `dpkg` for a
> clean host arch; Alpine downloads apk.static + keys from the CDN, verified).
> Privilege-drop for GUI/user apps uses the host's GNU `chroot --userspec`
> (does chroot + drop in one step, needs no tool inside the stratum) — fixes
> Alpine, whose BusyBox `setpriv` lacks `--reuid`. Host terminfo/fonts/icons are
> bound in via a **mkdir-first** loop (so the bind lands on Alpine, which ships no
> `/usr/share/terminfo`) with `TERMINFO_DIRS` pinned — so TUIs (htop) initialise
> under the host's `$TERM`. `--expose` takes multiple binaries after one flag
> (`num_args=1..`), which is what the auto-expose shim emits — so multi-binary
> installs are exposed (was silently a no-op). **GUI proven end-to-end in-VM**:
> xeyes + xclock from an Alpine stratum render on a real X server through the full
> shim→sudo→helper→chroot→drop path, **including X authentication** with a
> user-owned cookie. X11 apps still need an X server (XWayland) up on the host
> session — strata forwards `DISPLAY`/`WAYLAND_DISPLAY`/`XAUTHORITY` but does not
> provide one (e.g. niri needs `xwayland-satellite`).
> CLI + **TUI** (host terminfo bound) + **GUI** foreign apps run from one PATH
> alongside `pacman`; `apt`/`dnf`/`apk` install auto-exposes the binaries and
> mirrors their `.desktop` into the app menu; GUI apps **launch from the menu, one
> click, no password** (passwordless enter-helper guarded by `$SUDO_UID` so a
> caller can only run as themselves). GUI/user binaries run as the invoking user
> with host **fonts + icons** bound in (a minimal rootfs ships none) and the
> Wayland/X display env forwarded; package managers stay root. Command-not-found
> offers the right distro for `apt`/`dnf`/`apk`; typing `paru` offers to install
> the AUR helper itself (`manifest paru`). **Open-to-install** file handlers:
> opening a `.deb`/`.rpm` in a file manager runs `strata-install`, which installs
> it into the matching stratum (offering to add one if none) — the file-type
> analogue of the command-not-found flow; `.apkm`/`.apks`/`.xapk` open into
> Waydroid (`android-install`). `manifest update` refreshes every source (host +
> AUR, each stratum, Flatpak, the Waydroid image). Paths are **`/strata/<name>`**. The
> **System Snapshots app is strata-aware** (an "Other apps" tab + a Home row, from
> `export::capture_manifest().strata`) — the last Phase-2 ergonomics item, done.
> **Remaining:** crossfs (Phase 4 — only if demand; shims cover the cases), then
> the new Phase 5 (Android/Waydroid, §13) and Phase 6 (Windows, §14). Draft 1.
> Owner: (you).
> Cross-refs: [`src/flatpak.rs`](../src/flatpak.rs) (the module this copies its
> shape from), [`src/install.rs`](../src/install.rs) (`apply()` step order),
> [`src/exec.rs`](../src/exec.rs) (`Ctx`), [`src/plugins.rs`](../src/plugins.rs)
> (could ship as a plugin instead of core), [`marketplace/scan.py`](../marketplace/scan.py)
> (new attack surface), [HANDOFF.md](../HANDOFF.md).

## 0. One-line mental model

A **stratum** is a full foreign-distro rootfs living in a subdirectory of the
Arch host. You never *boot* it — you `arch-chroot` into it to install and run its
packages, and the engine drops **PATH shims** on the host so an `apt`-installed
binary and a `pacman`-installed binary run from one shell. This is
[Bedrock Linux](https://bedrocklinux.org)'s idea, deliberately descoped: **binary
access, not a merged OS.**

The whole thing stays true to the repo thesis — *manifest.json is the source of
truth; the engine is a thin orchestrator of standard tools* — because a stratum
is declared, bootstrapped with the distro's **own** standard tool (`debootstrap`),
entered with our **existing** standard tool (`arch-chroot`, already used in
[`installer.rs`](../src/installer.rs)), and exposed with **generated shell shims**.
No bespoke daemon, no FUSE, no PID-1 takeover in v1.

```
/strata/arch      ← the host itself (the implicit "init" stratum)
/strata/debian    ← debootstrap'd Debian rootfs      (glibc)
/strata/alpine    ← Alpine rootfs   (musl — Phase 3, hardest)

/strata/.bin/apt   ─┐
/strata/.bin/dpkg   ├─ generated exec-shims, on the host PATH, each chroots
/strata/.bin/gcc   ─┘   into its stratum and execs the real binary
```

---

## 1. Why this shape (and why *not* crossfs first)

The original sketch made [crossfs](https://github.com/bedrocklinux/bedrocklinux-userland)
(Bedrock's FUSE union filesystem) the load-bearing piece. For the stated goal —
*run `apt`-installed and `pacman`-installed binaries from one PATH* — generated
shims get ~80% of the value with **none** of crossfs's cost. crossfs is deferred
to Phase 4 (polish), not the foundation. Three reasons:

### 1.1 Shims dodge the `ld.so` collision crossfs exists to solve

A Debian `amd64` binary and an Arch binary carry the **same** interpreter path in
their ELF header:

```
$ readelf -l /usr/bin/apt | grep interpreter
      [Requesting program interpreter: /lib64/ld-linux-x86-64.so.2]
```

If you run the Debian binary **in the host's mount namespace** (crossfs's
in-place model), the kernel loads whichever `/lib64/ld-linux-x86-64.so.2` is
visible — **Arch's** — which then has to satisfy the binary's `NEEDED` libs from
Arch's `/usr/lib`. The instant glibc versions skew (Debian bookworm's 2.36 vs a
rolling Arch 2.4x), symbol-version resolution fails:

```
./apt: /usr/lib/libc.so.6: version `GLIBC_2.38' not found (required by ...)
```

A shim that **chroots into the stratum first** makes the binary see *only its
own* `/lib`, `/usr/lib`, and `ld.so`. Per-stratum lib isolation is correct **by
construction** — it's the reason chroot is more robust than crossfs-in-place for
the cross-distro case, and it's free. crossfs has to *engineer* per-stratum
resolution with path rewriting; the chroot just has it.

> **Do not "simplify" the chroot away later.** A naive shared PATH that execs
> foreign binaries against host libs is the number-one way this feature breaks,
> and it breaks *silently* until a glibc bump. The chroot is the correctness
> boundary, not an optimization.

### 1.2 Shims are pure orchestration; crossfs is bespoke magic

A generated shim is a 3-line shell script — greppable, diffable, reproducible,
trivially rolled back, and reviewable by [`marketplace/scan.py`](../marketplace/scan.py).
crossfs is a vendored C FUSE daemon in the hot path of **every** `exec()` and
library load. That is the precise antithesis of "thin orchestrator, no bespoke
magic." Leading with shims keeps the subsystem inside the repo's design contract;
crossfs is a conscious, opt-in, later exception — not the default.

### 1.3 crossfs has a license + coupling trap

- **License.** ManifestOS is **MIT** (see [README](../README.md)). Bedrock's
  userland — crossfs included — ships under the **GPL** (`crossfs.c` carries
  GPLv2 headers; *verify current upstream before relying on this*). You cannot
  vendor GPL source into an MIT tree and keep it MIT. If crossfs ever lands it
  must be a **separately-installed component** (a package the manifest pulls in,
  with its own LICENSE and a hard module boundary), never source copied into
  `src/`.
- **"Standalone-ish" is optimistic.** crossfs is coupled to Bedrock's `/bedrock`
  layout, `libbedrock`, its stratum discovery, and `bedrock.conf`. Vendoring
  cleanly is *a fork that reads our config*, not a drop-in. Budget a fork.

**Net:** shims are the v1 and v2 mechanism. crossfs is a Phase-4 upgrade we may
never need. What crossfs buys over shims — transparent `/usr/lib`, `/etc`,
`/usr/share` (man pages, icons, `.desktop` files) and per-*file* (not per-binary)
resolution — only matters for GUI-app polish, and we'll know if it's worth the
GPL/FUSE cost only *after* Phase 1 proves the glibc↔glibc case is useful at all.

---

## 2. What is shared, what stays isolated

Merging everything is where Bedrock's multi-year complexity lives. Be ruthless.

| Path | v1 policy | Rationale |
|---|---|---|
| `/usr/bin`, `/usr/local/bin` (foreign) | **exposed via shims**, opt-in per binary | the entire payoff; explicit `expose` list, never a blanket union |
| `/etc` | **NOT merged** | two configs fighting over `hostname`/`passwd` is Bedrock's sharpest edge; each stratum keeps its own |
| `/etc/resolv.conf` | **bind-shared** (host → stratum) | foreign package managers need DNS |
| `/etc/localtime` | bind-shared | logs/timestamps sane |
| `$HOME` (the login user's) | **bind-shared** (host → stratum) | **without this the feature is inert** — a strata'd editor can't open your files |
| `/tmp` | bind-shared | IPC, editor temp files |
| `/run/user/$UID`, `/tmp/.X11-unix` | bind-shared | Wayland/X socket → GUI foreign apps can display |
| `/proc`, `/sys`, `/dev` (rbind) | bind-shared | **handled by `arch-chroot`** — do not hand-roll |
| `/etc/passwd`, `/etc/group` | **NOT merged** | see §2.1 |

### 2.1 User identity — the deliberately-lazy call

Bedrock has real subsystems to unify UIDs across strata so a user "exists"
identically everywhere. **We don't.** Policy: *each stratum manages its own users;
the host owns the real login.* This is safe **only because**:

1. We bind-share `$HOME`, so files are visible across the boundary, and
2. We **never** `useradd` inside a stratum — the only accounts a fresh
   `debootstrap` has are `root` (uid 0) and system users, which match the host.

Consequently a foreign binary run through a shim runs **as the invoking user's
uid**, sees `$HOME` at the same path, and writes files owned by that uid. UID
coherence falls out for free. If a future need appears (a foreign daemon wanting
its own service account), revisit — but not in v1.

---

## 3. Init stays singular

Arch's systemd is PID 1 and **stays** PID 1. A stratum's own init is simply never
invoked — we `chroot` to run a binary, we don't boot the stratum. This sidesteps
the entire `brl-init`/PID-1-takeover half of Bedrock, which exists only because
Bedrock treats strata as bootable targets. We don't want that; we want binary
access.

**Foreign services are explicitly out of scope for v1**, and here's the honest
reason it isn't a quick add: a chroot-exec systemd unit —

```ini
# host unit that would "proxy" a foreign daemon
[Service]
ExecStart=/usr/bin/arch-chroot /strata/debian /usr/bin/foo --foreground
```

— only works for a *simple, foregroundable* daemon. Anything using `Type=notify`
/ `sd_notify()` / socket activation expects **its own** systemd running and will
hang or fail under a bare chroot. Managing those needs `systemd-nspawn --boot`,
which boots the stratum's systemd **in a container** — reintroducing exactly the
namespace isolation that would break PATH/`$HOME` sharing. So: *binary access in
v1; service proxying is a separate, later, opt-in design with different tradeoffs.*

---

## 4. glibc first, musl (Alpine) last

| Pair | Difficulty | Notes |
|---|---|---|
| Arch ↔ Debian/Ubuntu | tractable | both glibc; the chroot-shim makes lib resolution per-stratum-correct (see §1.1). **Phase 1 target.** |
| Arch ↔ Fedora | tractable-ish | glibc, but bootstrap tool differs (`dnf --installroot`, not `debootstrap`). Phase 3. |
| Arch ↔ Alpine | **hardest** | musl vs glibc. Alpine binaries won't run outside their own chroot without static linking or a compat shim; this is where Bedrock's interpreter-rewriting earns its keep. **Phase 3+, and only if the glibc case proved worth it.** |

Alpine "seeming to work" in a naive test is a trap: a shim that chroots into the
Alpine rootfs runs the musl binary against musl `ld` **fine** — the pain only
starts if you ever try crossfs-in-place or want to feed a glibc binary Alpine
libs. Since our model always chroots, an Alpine stratum used *only through its own
shims* is actually not that bad; the hard part is any cross-*use* between an
Alpine and a glibc stratum. Scope: Alpine strata are self-contained; no
cross-Alpine-to-glibc guarantees.

---

## 5. The manifest schema (the ManifestOS-native half)

This is the part that makes it *ManifestOS* and not a shell script. A stratum is a
declarative block the engine orchestrates. Proposed shape:

```json
"strata": [
  {
    "name": "debian",
    "distro": "debian",
    "suite": "bookworm",
    "mirror": "https://deb.debian.org/debian",
    "snapshot": "20260701T000000Z",
    "packages": ["build-essential", "apt-file"],
    "expose":   ["apt", "dpkg", "gcc", "make"],
    "share":    ["home", "resolv", "x11", "wayland"]
  }
]
```

| Field | Meaning |
|---|---|
| `name` | stratum id → dir name (`/strata/<name>`) and shim namespace |
| `distro` | selects the bootstrap backend (`debian`/`ubuntu` → debootstrap; `fedora` → dnf; `alpine` → apk static). The **only** place distro branching lives. |
| `suite` | release (`bookworm`, `noble`, `40`, `edge`) |
| `mirror` | package mirror; defaults per-distro |
| `snapshot` | **reproducibility pin** — see §6. Optional but recommended. |
| `packages` | installed **inside** the stratum with **its own** package manager |
| `expose` | binaries to shim onto the host PATH (explicit allowlist, never blanket). Each also gets an unambiguous `<stratum>-<bin>` alias; if two strata expose the same bare name the **first in manifest order** wins it and the later one warns (VM finding — a naive last-writer-wins silently shadowed Debian's `apt` with Ubuntu's). |
| `share` | which host↔stratum bind-mounts to set up (`home`/`resolv`/`x11`/`wayland`/`tmp`); sensible default set if omitted |

### 5.1 Core block vs plugin

Two viable homes, and the repo convention ("keep the core schema small; new
capabilities grow at the edges as plugins") **favours a plugin**:

- **As a plugin** ([`plugins.rs`](../src/plugins.rs)): `strata` expands *before
  parse* into `packages` (the host-side tools: `debootstrap`, `arch-install-scripts`
  for `arch-chroot`), `files` (the generated shims + a profile.d PATH entry), and
  `post_install` hooks (the bootstrap + in-stratum install). **Problem:** plugin
  expansion is *pure data* — it can't run `debootstrap` at expansion time, only
  emit hooks that do. That pushes all the real logic into a shell blob inside a
  hook, which is exactly the anti-pattern [CLAUDE.md](../CLAUDE.md) warns against
  ("anything that would be a `post_install` line should become a first-class
  block the engine executes"). A stratum is too stateful (bootstrap, idempotency,
  rollback, shim regeneration) to live as a data-expansion.

- **As a core block** (`src/strata.rs`, `Manifest.strata: Vec<Stratum>`): a real
  module with an `apply()` the engine runs, exactly like [`flatpak.rs`](../src/flatpak.rs).
  Idempotent, testable, diff-able. **This is the recommendation** despite the
  "keep core small" convention, because the convention's own escape clause is
  "declarative over hooks" and strata *cannot* be honestly expressed as pure data.

**Decision: core block, `src/strata.rs`, modeled on `flatpak.rs`.** Revisit only
if it proves it can be pure-data.

### 5.2 Schema wiring checklist (per [CLAUDE.md](../CLAUDE.md) "Adding a field")

- `src/manifest.rs`: add `pub strata: Vec<Stratum>` to `Manifest` (+ `#[serde(default, skip_serializing_if = "Vec::is_empty")]`), define `struct Stratum`, add its `is_empty()`.
- `src/manifest.rs`: add `strata` to `Manifest::is_empty()` if such a gate exists.
- `src/strata.rs`: new module, `pub fn apply(strata: &[Stratum], ctx: &Ctx) -> Result<()>`.
- `src/install.rs`: call `strata::apply()` in `apply()` — **order matters**, see §7.
- `src/diff.rs`: surface stratum add/remove/expose-change in `diff`/`reconfigure`; decide `requires_full_apply()` (adding a stratum ⇒ full).
- `src/lib.rs`: `mod strata;`.
- `src/conditions.rs`: if strata should be `when`-gatable, add to the `Conditional` overlay (like `flatpak` already is).
- `marketplace/scan.py`: new rules (§9).
- `examples/reference/strata-demo.json`: a demo (Phase 1).

---

## 6. Reproducibility — the identity problem

ManifestOS's whole pitch is *reproducible* systems. `debootstrap bookworm` is a
**moving target**: you get whatever the mirror has *today*. Two manifests, same
JSON, months apart → different rootfs. That violates the pitch unless we address
it head-on. Three options, pick per-stratum:

1. **Snapshot pin (recommended default when set):** point `mirror` at a
   time-stamped archive — `https://snapshot.debian.org/archive/debian/<snapshot>/`
   — so the bootstrap is byte-reproducible. `snapshot` field carries the stamp.
   Fedora has `dnf` `--setopt` against Koji/Bodhi snapshots; Alpine pins by
   `edge`-vs-versioned branch. Not all distros have equally good snapshot infra —
   document the per-distro story.
2. **Manifest-recorded package set:** after first bootstrap, `export` records the
   exact installed version list (like a lockfile) into the manifest/history, and
   re-installs pin those versions. More work, distro-specific version syntax.
3. **Accept mutability (explicit):** treat a stratum like AUR HEAD — "latest at
   install time," documented as *not* reproducible. Fine for a dev box, not for
   the reproducibility guarantee.

**Decision needed before Phase 1 ships.** Recommend: support `snapshot` (option
1), default to option 3 with a **loud warning** when `snapshot` is absent, so the
non-reproducible case is a choice, not a surprise. Track in [HANDOFF.md](../HANDOFF.md).

---

## 7. Install order (`install.rs::apply()`)

Strata must slot in **after** the host is a working Arch box and **before**
anything that might want a foreign binary on PATH. Concretely:

```
... repos → paru → pre_install → packages → dotfiles → services ...
                                     │
                                     ├── (host tools: debootstrap, arch-install-scripts
                                     │    land here, as normal packages)
                                     ▼
                              [ strata::apply ]      ← new step, after packages
                                     │  1. bootstrap each rootfs (idempotent)
                                     │  2. in-stratum package install
                                     │  3. write bind-mount units / setup
                                     │  4. generate + place PATH shims
                                     ▼
                     ... flatpak → theme → keybindings → post_install ...
```

- **After `packages`** because `debootstrap`/`arch-install-scripts` are ordinary
  host packages installed in that step; add them to the effective package list
  automatically (the "auto-add the fallback's package" pattern from
  [`gestures.rs`](../src/gestures.rs)).
- **Before `post_install`** so an author's hook can lean on a shim.
- Each sub-step **idempotent** (the `pacman.rs`/`flatpak.rs` house rule): skip
  bootstrap if `/strata/<name>/etc/os-release` exists; `--needed`-style
  skip for in-stratum installs; regenerate shims wholesale (cheap, declarative).

### 7.1 Persistence of bind mounts

Shims that `arch-chroot` set up the binds *per invocation* (arch-chroot mounts,
runs, unmounts) — simplest, no boot-time state, slight per-exec cost. Alternative:
persistent binds via generated `systemd.mount` units activated at boot. **v1:
per-invocation via arch-chroot** (stateless, nothing to leak or leave mounted on
rollback). Measure the overhead before optimizing to persistent mounts.

---

## 8. The dev loop can't test most of this

A hard constraint that shapes *how* to build it:

- `cargo build`/`test`/`clippy` on the Windows host prove only that it
  **compiles** and that pure logic (shim text generation, schema parse,
  `is_empty()`, path mapping) is correct. Write those as **unit tests** — they're
  the only fast feedback.
- Everything real — `debootstrap`, `arch-chroot`, bind mounts, running a foreign
  binary — needs the **`manifest-build` VM** (or Docker with `--privileged` +
  loop/mount caps; debootstrap needs `CAP_SYS_ADMIN`, `arch-chroot` needs mount).
  Plain `docker/Dockerfile` may **not** be enough — chroot-in-container +
  bind-mounting needs privileged mode. Confirm early.
- `--dry-run` must print every debootstrap/chroot/shim step without touching the
  system (the `exec.rs` `Ctx` plumbing already gives this for free if all side
  effects go through `ctx.run/sudo/shell/write_*`). **Keep every side effect on
  `Ctx`** so `--dry-run` stays honest and so the whole feature is inspectable on
  Windows.

Build in **thin, VM-tested slices** (§10), not one big drop — most of the surface
is invisible to the inner loop.

---

## 9. Security / marketplace impact

A shared manifest that bootstraps a foreign distro and runs its package manager
**as root** is a large new attack surface. [`marketplace/scan.py`](../marketplace/scan.py)
must learn about `strata`:

- **Foreign mirror URL** — flag non-official mirrors (anything but the distro's
  canonical hosts / snapshot archives); a hostile `mirror` is a supply-chain hole
  as bad as an untrusted `repos` entry.
- **Foreign signing keys** — **VM finding (Phase 2):** debootstrap does *not*
  fail when its archive keyring is absent — it prints `W: Cannot check Release
  signature; keyring file not available` and bootstraps the rootfs **unverified**.
  A bare Arch box has neither the Debian nor Ubuntu keyring, so Phase 1's "GPG on
  by default" was false comfort. The engine now installs the distro's keyring
  (`debian-archive-keyring` / `ubuntu-keyring`, both in Arch's official repos) and
  passes `--keyring=<path>` explicitly, hard-failing if the keyring is still
  missing (`strata::ensure_keyring`). We never pass `--no-check-gpg`; a manifest
  that disables verification is a HIGH finding.
- **`expose` blast radius** — exposing `sudo`, `su`, a shell, or a setuid binary
  from a foreign stratum onto the host PATH is worth a finding (it's a privilege
  path the host's own tooling doesn't audit).
- **Shim content** — the boot-test's filesystem-diff (stage 2) should confirm the
  only new host-PATH entries are the declared `expose` shims and nothing else.
- **In-stratum `packages`** — same "does this name resolve / pull something nasty"
  question as host packages, now times N distros. The boot-test VM is the only
  real answer.

---

## 10. Phasing

**Phase 1 — glibc MVP (the proof).**
Arch host + one Debian stratum. `debootstrap` (snapshot-pinnable) → `arch-chroot`
→ in-stratum `apt install` → generated exec-shims for an explicit `expose` list →
`$HOME`/`resolv.conf`/sockets bind-shared. **No crossfs, no `/etc` merge, no
services, no Alpine.** Deliverable: `apt`-installed `hello` and `pacman`-installed
`hello` both run from one shell in the VM. This alone answers *is cross-distro
binary access even worth it here?* before any GPL/FUSE spend.

- Schema: `Stratum` struct + `strata::apply` + install-order wiring + `--dry-run`.
- Unit-tested: shim generation, path mapping, snapshot-URL construction, `is_empty`.
- VM-tested: full bootstrap→install→run, idempotent re-run, rollback leaves no
  mounts.

**Phase 2 — ergonomics + a second glibc distro.** *(largely done)*
Ubuntu stratum ✅ (backend generalizes), snapshot-pinned reproducibility ✅,
in-stratum `apt install` ✅, verified bootstrap + shim-collision handling ✅,
`diff`/`reconfigure` support ✅ (`strata_sig` forces a full sync on any stratum
change), `export` captures existing strata ✅ (`export::capture_strata` — name/
distro/suite/mirror/expose recovered from the rootfs + shims; in-stratum
`packages` and `snapshot` pins are *not* recovered). GUI foreign app via shared
Wayland/X socket ✅ (proven on real hardware — runs as the invoking user, display
env forwarded). Still open: *launch ergonomics* (passwordless + `.desktop` menu
entries — today GUI apps need a terminal + sudo prompt), System Snapshots UI
awareness.

**Ergonomics — command-not-found → add a stratum.** Strata are *never* installed
by default (the engine's strata step no-ops on an empty list; the ISO bakes no
stratum). But a shell handler (`strata::cnf_handler_script`, written to
`/etc/profile.d` on every install) maps an uninstalled package manager to its
distro — `apt`/`dpkg`→debian, `dnf`/`rpm`→fedora — and offers to add it:
`sudo manifest strata add <distro> --expose <cmd>`. That subcommand captures the
system as a manifest, upserts the stratum (`export::add_stratum`), applies just
the strata step, and records the result to the rollback history. Only
bootstrappable distros are offered, so the prompt never dead-ends.

**Phase 3 — Fedora (dnf backend) ✅ + Alpine (musl, self-contained).**
Fedora backend built + VM-validated: `dnf5 --installroot` bootstraps a verified
rootfs (gpgcheck + `distribution-gpg-keys`), in-stratum `dnf install` and
`tree`/`rpm`/`dnf` shims run against Fedora's own libs alongside `pacman`. Four
VM findings fixed — single-baseurl gave no mirror failover (now a metalink
`.repo`); Arch's `dnf` pkg is dnf4 and conflicts with dnf5 (use `dnf5`);
arch-chroot skips a resolv.conf when the target's is a dangling symlink, which
Fedora ships (plant one first). **Alpine ✅** (musl) — since `apk-tools-static` +
`alpine-keys` aren't in Arch's repos, the bootstrap downloads `apk.static` + the
keys over HTTPS from the official CDN (versions resolved from the branch APKINDEX)
and runs `apk.static --keys-dir …` so packages are **signature-verified** (never
`--allow-untrusted`). Gotcha fixed: `.apk`/`APKINDEX.tar.gz` are multi-stream
(sig+ctl+data), so `tar` needs `-i`. musl binaries run only through their own
shims (which chroot in, resolving Alpine's `ld-musl`). VM-validated: verified
bootstrap, in-stratum `apk add`, musl `tree` runs via the shim. **All four named
distros now implemented.**

**Phase 4 — crossfs (only if Phases 1–2 proved demand).**
Transparent `/usr/lib`/`/etc`/`/usr/share` + per-file resolution for GUI polish.
Separately-installed **GPL** component behind a hard boundary — never vendored
into `src/`. Reassess whether shims already covered the real use cases.

**Phase 5 — Android apps (a "waydroid stratum": container, not chroot or VM).**
Run Android apps (mobile-only messengers, banking apps, games) as native-feeling
windows on the ManifestOS desktop. Nearer-term and lighter than Windows — no VM,
no passthrough — so it lands first. See §13 for the full design; the short version:

- **Same mental model, different backend.** An `android` stratum is declared and
  its apps are exposed onto the menu exactly like a Linux stratum — but Android is
  bionic/ART/binder, not a chroot of Linux binaries, so the backend is
  **[Waydroid]**: a single LXC container running an Android image on the *host*
  kernel, painting each app as a Wayland window (multi-window mode). The engine
  starts the session on first launch, like the enter-helper wakes a stratum.
- **Thin orchestration of standard tools.** `waydroid init` (pinned image) →
  `waydroid session start` → `waydroid app install <apk>` / `waydroid app launch`.
  No bespoke daemon; Waydroid already emits per-app `.desktop` files we mirror with
  the existing strata mechanism.
- **Kernel + GPU are the gate.** Needs `binderfs` (mainline) and a working
  gralloc/GBM path — real GPU on hardware; the VM's GL 2.1 can't (same wall as the
  GUI work). Reproducibility = pin the system/vendor image version, like a stratum
  `snapshot`.

**Phase 6 — Windows apps (a "windows stratum": VM + seamless remoting).**
The end-goal stretch: run Windows applications (the anchor use case is
**SolidWorks** and other CAD) as if they were native, on the ManifestOS desktop.
See §14 for the full design; the short version:

- **Same mental model, different backend.** A `windows` stratum is declared and
  its apps are exposed onto the menu/PATH exactly like a Linux stratum — but you
  can't `chroot` an NT kernel, so the backend is **a Windows VM (libvirt/QEMU-KVM)
  + FreeRDP RemoteApp**: each exposed app opens as a borderless Linux window, no
  desktop-in-a-box. The engine wakes the VM on first launch, like the enter-helper
  wakes a stratum. Prior art: **WinApps** (study/reuse, MIT-compatible? verify).
- **Tiered, mirroring the debootstrap/dnf/apk split.** (1) Wine/CrossOver for apps
  that support it (cheap, no VM — *not* SolidWorks, which is Wine-hostile); (2)
  Windows VM + RDP RemoteApp for the general case; (3) VM + GPU passthrough +
  Looking Glass for GPU-heavy CAD.
- **GPU passthrough is the real work, and the actual novel feature.** VFIO
  passthrough is legendarily painful to set up by hand — precisely the fiddly,
  hardware-specific ritual ManifestOS exists to make declarative. The engine
  detects GPU topology + IOMMU groups and auto-generates the whole stack.
- **VM source: managed image (default) or attach the dual-boot partition
  (advanced).** ManifestOS already detects Windows at install (the dual-boot
  carve), so booting the *existing* install in KVM is a natural — if fragile —
  option; a dedicated managed VM is the robust default.

Deliverable ladder: 6a — managed VM + FreeRDP RemoteApp, one app end-to-end;
6b — manifest `windows` block + auto-expose + `.desktop` menu entries; 6c —
GPU passthrough (6c-desktop: iGPU+dGPU; 6c-laptop: muxless + Looking Glass); 6d
(stretch) — attach the physical dual-boot Windows partition.

**Honest gate:** this is a far heavier subsystem than the Linux strata (which is
just chroot). Its usefulness for CAD is **hardware-gated** on IOMMU + a spare
GPU. It stays design-only until the seam (6a) is proven.

---

## 11. Open questions (decide before Phase 1 code)

1. **Reproducibility default** (§6): ship `snapshot` support + loud warning when
   absent, or hard-require a pin? → *lean: support + warn.*
2. **Bind-mount lifetime** (§7.1): per-invocation vs persistent units? → *lean:
   per-invocation, measure.*
3. **Naming.** "strata" borrows Bedrock's term (good — accurate, discoverable).
   The user-facing feature name for docs/marketing? ("Run any distro's software.")
4. **ISO footprint.** Do any flagship examples ship a pre-bootstrapped stratum in
   the ISO (300 MB–1 GB each), or is `strata` install-time-only? → *lean:
   install-time only; never bake a stratum into the ISO.*
5. **Rollback semantics.** Does `manifest rollback` that removes a stratum
   `rm -rf` the rootfs, or leave it (data-loss caution)? → mirror how dotfiles/
   packages are handled; probably leave + warn.
6. **Docker testability** (§8): does the engine test container need `--privileged`
   for debootstrap+chroot, and is that acceptable in CI? Confirm before relying on
   it.

---

## 12. What this is *not*

- Not a fork of Bedrock and not Bedrock-compatible (no `brl`, no `/bedrock/cross`,
  no PID-1 takeover). We borrow the *strata* idea and the `/strata` layout
  convention, nothing more.
- Not a way to *boot* another distro — strata are never init targets.
- Not a general containerization story — no isolation is the *point*; foreign
  binaries share the host's namespaces and `$HOME`. If you want isolation, that's
  `systemd-nspawn`/Docker/Distrobox, not this.
- Not (in v1) a foreign-*service* manager — see §3.

---

## 13. Android apps — the "waydroid stratum" (Phase 5a implemented)

> Status: **Phase 5a implemented (orchestration) — rendering unverified
> (hardware-gated).** [`src/android.rs`](../src/android.rs) sets Waydroid up as a
> first cut: an `android` manifest block ([`Android`](../src/manifest.rs)), an
> `android` CLI subcommand, and a command-not-found offer when you type
> `waydroid`/`android-install` (→ `sudo manifest android`), all mirroring the
> strata/paru shape. `apply()` installs Waydroid (AUR/paru), best-effort ensures
> `binderfs`, `waydroid init -s <SYSTEM>`, enables the container service, drops
> the **`android-install`** command (a plain **`.apk`**; a **split bundle**
> `.apkm`/`.apks`/`.xapk` — unpacked, splits selected base+ABI+density+langs and
> installed as one `pm` split-session, base-only fallback; *or* an **F-Droid id**,
> version resolved via F-Droid's API), registers `.apkm`/`.apks`/`.xapk` as file
> types so opening one installs it, and installs a **first-login hook** (`/etc/xdg/autostart` →
> `waydroid-firstrun`, guarded once-per-user) that — since Waydroid app
> management needs the user's live Wayland session, absent at root install time —
> starts the session, sets multi-window mode, installs an in-Android **F-Droid**
> store, installs the declared `apps`, and writes a **launcher per exposed app so
> it shows in fuzzel/rofi**. **Lazy lifecycle** (Android is *not* kept in the
> background): the container is left **disabled** at boot; every launcher points
> at **`waydroid-launch`**, which brings Android up on demand (container start is
> passwordless via a scoped `sudoers` rule) and stamps an activity marker; a
> per-user **`waydroid-idle.timer`** stops the session **and** container after
> `idle_minutes` (default 45, `0` = stay resident) with no Waydroid window open
> (best-effort per-compositor window check: hyprctl/swaymsg/niri). Unit-tested
> (init, installer, launcher, idle, first-run, sudoers, quoting — 8 tests) +
> dry-run-verified ordering. **Documented assumptions / next:** kernel `binderfs`
> assumed present (warn, don't hard-fail); GPU/gralloc rendering is
> real-hardware-only (VM GL 2.1 can't); reproducibility image-pin (system+vendor
> version) and `export`/`diff`/Snapshots capture are follow-ups; APK trust (prefer
> F-Droid ids) needs a scan.py stance. Example:
> [`examples/reference/android-waydroid.json`](../examples/reference/android-waydroid.json).
> Original design below. Anchor use case: **mobile-only apps** — messengers,
> banking apps that refuse the web, and Android games — as native-feeling windows.
> Lands **before** Windows (§14): lighter (no VM, no passthrough) and higher
> everyday demand.

### 13.1 Why it fits the strata model (and where it diverges)

A stratum is *"foreign software, declared in the manifest, exposed onto your menu,
launched through a generated shim."* Android fits that shape — declare an
`android` stratum, list the apps to expose, get menu launchers. What changes is
the **backend**: Android is **not Linux** (bionic libc, the ART runtime, binder
IPC, its own HAL/init), so you can't `chroot` it against host libs the way you can
Debian. Instead the engine runs **[Waydroid]** — Android in a single privileged
LXC container on the *host* kernel, its surfaces composited into your Wayland
session. The per-app "shim" becomes `waydroid app launch <pkg>` against the
running session, started on first launch the way the enter-helper wakes a stratum.

Divergences from a Linux stratum, all consequences of "it's a container running a
different OS, not a chroot":

- **One container, many apps.** Unlike per-distro strata there's a single Android
  instance; "exposing an app" is installing an APK + mirroring its launcher, not
  bootstrapping a rootfs.
- **No shared `$HOME`/namespace.** Android has its own filesystem and permission
  model; files cross via Waydroid's shared folder (`~/Android`), not a bind of the
  host `$HOME`.
- **Kernel-coupled.** Needs host-kernel `binderfs` (and a memfd/ashmem path) — a
  hard dependency the chroot strata don't have.
- **Wayland-first.** Multi-window mode paints each app as its own toplevel (what
  makes it feel native, vs. the single full-screen "Android tablet" mode).

### 13.2 Backend — why Waydroid (not the alternatives)

| Option | Mechanism | Verdict |
|---|---|---|
| **Waydroid** | LXC container, host kernel, Wayland surfaces | **the pick** — mainlined, active, near-native perf, real GPU |
| Anbox | older snap-based container | superseded/dead — Waydroid is its successor |
| AVD / QEMU emulator | full CPU + device emulation | too slow, no desktop integration — a dev tool, not a daily driver |
| Genymotion / cloud | proprietary / remote | off-thesis (not local, not FOSS) |

Waydroid is the only option matching the repo thesis — thin orchestration of a
standard tool (`waydroid`), apps composited straight into the user's session, no
emulation tax.

### 13.3 The hard parts — kernel, GPU, images

- **Kernel.** Needs `binderfs` (`CONFIG_ANDROID_BINDERFS`, mainline since 5.0) and
  a memfd/ashmem path. ManifestOS owns its kernel choice, so this is a
  manifest-declarable kernel feature — but a **hard gate**: no binder, no Android.
- **GPU / gralloc.** Waydroid renders gralloc→GBM→DRM. Fine on real hardware with a
  working GL/Vulkan driver; **in the build VM it hits the exact GL 2.1 wall** that
  blocks the GTK4/Hyprland work (see the GPU-fallback notes) — so, like the GUI
  features, it's **verify-on-hardware-only**: the VM can prove the
  container/orchestration but not rendering.
- **Images + reproducibility.** `waydroid init` fetches a system + vendor image
  (LineageOS-based). Pin the image **channel + version** in the manifest, mirroring
  a stratum `snapshot`. GApps vs. vanilla is a declared choice — many banking apps
  need Play services (and pass SafetyNet only with GApps); call that out honestly,
  it isn't always achievable and we can't redistribute Play.
- **Networking.** Waydroid NATs through a `waydroid0` bridge — the engine owns that
  setup like any other declarative network bit.

### 13.4 Manifest shape (sketch)

```json
"android": {
  "image": { "system": "lineage-20", "vendor": "lineage-20", "gapps": false },
  "mode": "multi-window",
  "apps": [
    { "id": "org.telegram.messenger", "source": "fdroid" },
    { "apk": "files/some-app.apk" }
  ],
  "expose": ["org.telegram.messenger"]
}
```

Same `expose` → menu-launcher pattern as strata; `apps` installs (an F-Droid id or
a sideloaded APK path, verified before install); `image` is the reproducibility
pin. The engine mirrors Waydroid's generated `.desktop` files with the **existing**
strata desktop-mirror mechanism, rewriting `Exec` through a `waydroid app launch`
shim.

### 13.5 Open questions (before any Phase 5 code)

1. **GApps stance** — vanilla-only, or bundle a helper for the user to add
   microG/GApps themselves (we can't redistribute Play)? Governs how many real apps
   work.
2. **Session lifecycle** — lazy-start on first app launch (like the enter-helper)
   vs. a user service; clean stop/idle.
3. **Permissions** — Android's per-app runtime prompts vs. a declarative manifest:
   how much is pre-grantable.
4. **APK trust** — sideloaded APKs are unsigned-by-us binaries from anywhere;
   `scan.py`/marketplace needs an Android-APK stance (prefer F-Droid ids).
5. **Kernel gate** — confirm the shipped kernel always has binderfs; fail the
   `android` block **loudly**, never silently, when it doesn't.

---

## 14. Windows apps — the "windows stratum" (Phase 6a: wine tier implemented)

> Status: **Phase 6a implemented — the `wine` tier + the compatibility oracle.**
> [`src/wincompat.rs`](../src/wincompat.rs) answers "will this run under Wine?"
> from two independent signals: a curated knowledge base of app families (kernel
> anti-cheat, CAD, dongles, Store packaging, known-good tools) and **marker
> scanning inside a local installer** (`EasyAntiCheat`, `vgk.sys`, `HASP`, `.sys`)
> — so an innocent-looking name can't hide a blocker. Verdicts are
> `works`/`likely`/`risky`/`blocked`, each with reasons and the tier that could
> run it; `manifest windows-check <name|installer.exe>` prints them without
> installing anything. [`src/windows.rs`](../src/windows.rs) implements the
> **wine tier**: install wine+winetricks, a **per-app prefix** (so one app's
> overrides can't break another), winetricks verbs (manifest + per-app + the
> oracle's hints), run the installer (URL or local), and a `.desktop` launcher
> through the generated `windows-app` command. The gate runs **before** any
> install, so a blocked app is reported with its reason rather than
> half-installed (`"force": true` overrides). 15 unit tests. Example:
> [`examples/reference/windows-wine.json`](../examples/reference/windows-wine.json).
> **Not built:** the `vm-rdp`/`vm-vfio` tiers below — an app that needs one says
> so clearly. Original design follows.
>
> Status (original): **idea / design.** The Linux strata (Phases 1–3) are the foundation
> this reuses conceptually. This section is a plan, not a commitment; nothing here
> is built. Anchor use case: **SolidWorks** (and CAD generally) running as a
> native-feeling window on the ManifestOS desktop.

### 14.1 Why it fits the strata model (and where it diverges)

A stratum is *"foreign software, declared in the manifest, exposed onto your
PATH/menu, launched through a generated shim."* Windows fits that shape exactly —
you declare a `windows` stratum, list the apps to expose, and get menu launchers.
What changes is the **backend**: an NT kernel can't be `chroot`ed, so instead of
`debootstrap` + `chroot` the engine runs **a Windows guest under libvirt/QEMU-KVM
and paints individual app windows onto the Linux desktop via FreeRDP RemoteApp**
(RDP's "seamless" mode — one app, no visible Windows desktop). The per-app "shim"
becomes a launcher that runs `xfreerdp /app:"<app>" …` against the running guest,
starting/waking the VM on first launch the way the enter-helper wakes a stratum.

Prior art is **[WinApps]** (VM + FreeRDP RemoteApp + `.desktop` generation). We
should study it and reuse where the license permits (**verify its license against
our MIT tree before vendoring** — same rule as crossfs §1.3).

Divergences from a Linux stratum, all consequences of "it's a VM, not a chroot":
no shared `$HOME`/namespace (files cross via an RDP drive redirect or a 9p/virtiofs
share); no glibc story (it's a whole OS); heavyweight (GBs of RAM, a booting
guest) vs. a chroot's near-zero cost; and a hard hardware dependency for GPU apps.

### 14.2 Tiered backends (mirrors debootstrap / dnf / apk)

| Tier | Mechanism | Good for | Not for |
|---|---|---|---|
| **wine** | Wine/CrossOver, no VM | small, well-supported Windows tools | SolidWorks (Wine-hostile) |
| **vm-rdp** | Windows VM + FreeRDP RemoteApp, virtio GPU | most apps; office/eng tools | GPU-heavy 3D |
| **vm-vfio** | VM + **GPU passthrough** + Looking Glass | SolidWorks, CAD, GPU compute | machines without IOMMU + a spare GPU |

The engine picks (or the manifest pins) a tier. `wine` is the cheap default where
it works; `vm-vfio` is the CAD path.

### 14.3 GPU passthrough — the hard part, and the actual novel feature

CAD needs real 3D acceleration → **VFIO passthrough**: hand a physical GPU to the
guest. This is legendarily fiddly to set up by hand, which is exactly why
automating it is worth doing — it's the fiddly, hardware-specific Arch ritual
ManifestOS exists to make declarative.

**Topology matters — "2 GPUs" is a fork, not one case:**

- **Desktop, CPU iGPU + discrete GPU** — the clean case. Linux stays on the iGPU,
  the dGPU is dedicated to the guest. Passthrough is well-behaved here. *Ideal
  target.*
- **Single discrete GPU (no iGPU)** — single-GPU passthrough, which yanks the host
  display while the guest runs. Advanced/ugly; support later or not at all.
- **Laptop, muxless hybrid** — the *other* common "2 GPU" machine, and the hardest:
  the dGPU has **no display output of its own** (it renders and copies to the
  iGPU), so passthrough *requires* **Looking Glass** to shovel guest frames back to
  the host, and laptop dGPUs carry quirks (NVIDIA "Error 43", ACPI power-off). Doable
  but not free.

**Bind modes** (the "give Windows the dGPU when necessary" ask):

- *Static* — dGPU claimed by `vfio-pci` at boot; Linux never uses it. Simplest,
  always VM-ready; Linux forfeits the dGPU.
- *Dynamic* — Linux uses the dGPU normally; on VM launch, unbind from the Linux
  driver → bind `vfio-pci` → start guest → rebind on shutdown. Flexible, but
  nothing on Linux may be holding the dGPU at handover.

**What the engine automates (the whole chain, from detected hardware):**

1. Enumerate GPUs and **IOMMU groups**; classify the topology (desktop / single /
   muxless). The passed GPU must be cleanly isolated in its own group — many
   consumer boards are, some need the **ACS-override patch** (which has real
   security caveats and must be a loud, explicit opt-in, never silent).
2. Generate the kernel cmdline (`intel_iommu=on` / `amd_iommu=on iommu=pt`),
   `vfio-pci` binding by PCI vendor:device ID, initramfs modules, and (muxless)
   the Looking Glass host/client + shared-memory device.
3. Wire dynamic bind/unbind hooks around VM start/stop.
4. **Fail gracefully** — if IOMMU is off, groups are unisolated, or there's no
   spare GPU, refuse with a clear explanation rather than producing a broken boot.
   Never leave the machine unbootable in pursuit of passthrough.

### 14.4 VM source: managed image vs. attach the dual-boot partition

- **Managed image (default, robust).** ManifestOS provisions a dedicated Windows
  VM (its own qcow2, its own license/activation, virtio drivers pre-injected).
  Clean, reproducible, no interference with a native Windows.
- **Attach the existing dual-boot install (advanced, fragile).** ManifestOS
  already detects Windows at install (`detect_windows`, the dual-boot carve), so
  handing KVM the physical Windows partition/disk (raw passthrough) to boot the
  user's *real*, licensed apps is a natural extension. Caveats to spell out and
  guard: Windows sees new hardware → **reactivation** nags; Win11 needs **vTPM +
  OVMF** for Secure Boot/TPM 2.0; **VirtIO drivers** must be present in the
  existing install; and it **cannot run in the VM while also booted natively** —
  the engine must enforce that mutual exclusion.

### 14.5 Manifest shape (sketch)

```json
"windows": {
  "source": { "type": "managed", "iso": "…", "disk_gib": 120 },
  "gpu_passthrough": { "mode": "auto", "bind": "dynamic" },
  "apps": [
    { "name": "SolidWorks", "expose": "solidworks",
      "path": "C:\\Program Files\\SOLIDWORKS\\sldworks.exe" }
  ]
}
```

`source.type`: `managed` | `attach-partition`. `gpu_passthrough.mode`: `auto`
(detect topology) | `off` | explicit PCI id; `bind`: `static` | `dynamic`. Each
`apps[]` entry yields a FreeRDP RemoteApp launcher on `/strata/.bin` (or a
Windows-specific dir) and a `.desktop` menu entry — the same "expose" ergonomics
as a Linux stratum. `manifest windows add <app>` would mirror `strata add`.

### 14.6 Open questions (before any Phase 6 code)

1. Base FreeRDP RemoteApp integration on **WinApps** (license-permitting) or build
   the launcher/`.desktop` generation fresh?
2. File sharing: RDP drive redirect vs. virtiofs/9p — latency vs. simplicity for
   CAD's large files.
3. Managed-image acquisition: user-supplied ISO only (licensing), or a guided
   fetch? (Windows licensing constrains what we can automate.)
4. Marketplace/security: a `windows` block spins up a VM with disk/GPU access — a
   large new attack surface for `scan.py` to reason about (untrusted ISO, raw
   partition access, passthrough).
5. How much of §14.3's auto-detection is safe to run unattended vs. requires an
   explicit "yes, reconfigure my bootloader for passthrough" confirmation.

[Waydroid]: https://waydro.id
[WinApps]: https://github.com/winapps-org/winapps

## 15. Adjacent — `nix` as a native package source (like `flatpak`, **not** a stratum)

> Status: **idea / design.** Deliberately *not* part of strata — it's a sibling to
> the existing `flatpak` block. Nothing built.

**Why it's not a stratum.** Debian/Fedora binaries need their own distro's
`ld.so`/libs, which is *why* strata chroot. Nix doesn't: every package in
`/nix/store` carries its full dependency closure with absolute rpaths, so a Nix
binary runs **natively on Arch** with zero glibc-skew — no chroot, no rootfs, no
shims. Nix's isolation is the *store*, not a namespace. So Nix belongs beside
`flatpak` as a first-class **native package source** declared in the manifest,
never under strata. (This is also why "NixOS as a stratum" in §10/§12 is
pointless — you don't virtualize Nix, you just install it.)

**Why add it.** nixpkgs is the largest package set in existence (~100k+),
reproducible, per-user (no root for user installs), and never conflicts with
pacman (separate store + profile). It's the ideal complement: Flatpak covers
sandboxed GUI apps; Nix covers the long tail of CLI/dev tools **and**
reproducibility. Fits the repo thesis — declare all your software in one manifest.

**Shape (mirrors `flatpak.rs` / the strata `packages`+`snapshot` pattern):**

```json
"nix": {
  "packages": ["ripgrep", "fd", "hello"],
  "pin": "nixpkgs/<rev>"   // optional — reproducibility, like strata's `snapshot`
}
```

**Engine steps (thin orchestration of standard tools — same discipline as the
rest of the engine):**

1. **Ensure Nix.** `pacman -S --needed nix` (Arch packages it — stays true to
   "orchestrate Arch tools" rather than piping the upstream installer to a shell),
   enable `nix-daemon.service` (multi-user), add the primary user to `nix-users`.
2. **Enable flakes + nix-command** in `/etc/nix/nix.conf` so `nix profile install`
   works.
3. **Install each package into the primary user's profile** —
   `nix profile install nixpkgs#<pkg>` (or pinned: `nixpkgs/<rev>#<pkg>`),
   per-user (like flatpak `--user` / strata user-mode), run as the manifest's
   primary account.
4. **PATH** is already handled — the `nix` package's `/etc/profile.d/nix.sh` puts
   `~/.nix-profile/bin` on PATH.

**Reproducibility.** A `pin` (nixpkgs commit / flake ref) makes installs
bit-reproducible — Nix's whole point — mirroring strata's `snapshot`.

**Marketplace/security (`scan.py`).** A `nix` block that installs from **nixpkgs**
is trusted-ish; an arbitrary **flake ref** (`github:someone/repo#pkg`) is
unreviewed remote code and must be flagged HIGH, the same way a non-canonical
strata `mirror` is. `pin` values and package names are otherwise low-risk.

**Deliverable.** A `src/nix.rs` shaped like `src/flatpak.rs`; a `Nix` block in
`manifest.rs` (with `is_empty()`); wired into `install.rs::apply` next to the
flatpak step; `diff`/`export`/scan.py support. No new subsystem — it's a fifth
package source (pacman, AUR/paru, flatpak, strata, **nix**), the smallest kind of
addition this engine takes.
