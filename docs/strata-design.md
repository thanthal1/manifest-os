# Strata — multi-distro package access (design)

> Status: **Phase 1 built + VM-validated** (2026-07-22). Arch + Debian bookworm
> stratum bootstrapped via debootstrap in the `manifest-build` VM; `dpkg` 1.21.23
> (Debian) and `pacman` v7.1.0 (Arch) confirmed running from one PATH; enter-helper
> mounts auto-clean with no leaks; DNS share works. Draft 1. Owner: (you).
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
/bedrock/strata/arch      ← the host itself (the implicit "init" stratum)
/bedrock/strata/debian    ← debootstrap'd Debian rootfs      (glibc)
/bedrock/strata/alpine    ← Alpine rootfs   (musl — Phase 3, hardest)

/bedrock/bin/apt   ─┐
/bedrock/bin/dpkg   ├─ generated exec-shims, on the host PATH, each chroots
/bedrock/bin/gcc   ─┘   into its stratum and execs the real binary
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
ExecStart=/usr/bin/arch-chroot /bedrock/strata/debian /usr/bin/foo --foreground
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
| `name` | stratum id → dir name (`/bedrock/strata/<name>`) and shim namespace |
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
  bootstrap if `/bedrock/strata/<name>/etc/os-release` exists; `--needed`-style
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

**Phase 2 — ergonomics + a second glibc distro.**
Ubuntu stratum (proves the debootstrap backend generalizes), GUI foreign app via
shared Wayland/X socket (proves `share`), `diff`/`reconfigure` support, `export`
captures existing strata, System Snapshots UI awareness.

**Phase 3 — Fedora (dnf backend) + Alpine (musl, self-contained).**
Second bootstrap backend; musl stratum used only through its own shims (no
cross-musl-to-glibc promise).

**Phase 4 — crossfs (only if Phases 1–2 proved demand).**
Transparent `/usr/lib`/`/etc`/`/usr/share` + per-file resolution for GUI polish.
Separately-installed **GPL** component behind a hard boundary — never vendored
into `src/`. Reassess whether shims already covered the real use cases.

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
  no PID-1 takeover). We borrow the *strata* idea and the `/bedrock/strata` layout
  convention, nothing more.
- Not a way to *boot* another distro — strata are never init targets.
- Not a general containerization story — no isolation is the *point*; foreign
  binaries share the host's namespaces and `$HOME`. If you want isolation, that's
  `systemd-nspawn`/Docker/Distrobox, not this.
- Not (in v1) a foreign-*service* manager — see §3.
