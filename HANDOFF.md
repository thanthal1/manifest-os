# Manifest OS — Handoff

> *Declare it. Share it. Deploy it.*
> Snapshot for picking this project up cold. Last updated 2026-07-25.
> Repo: https://github.com/thanthal1/manifest-os

## What this is

Three things in one repo, all over one engine:

1. **The `manifest` CLI** — reads a single `manifest.json` (the source of truth)
   and reproduces a complete Arch system: kernel, repos, packages, a full
   desktop, users, config files/snippets, theme, keybindings, wallpaper,
   services, dotfiles, bootloader.
2. **Manifest OS** — a bootable Arch-derived distro (archiso profile + the CLI +
   a **graphical installer** and a text TUI) that boots straight into a friendly
   install (blank disk, alongside Windows, or LUKS/LVM/RAID).
3. **System Snapshots** (`manifest-center`) — a desktop app on the installed
   system to save/restore setups, apply a shared one, and edit config visually
   (the node-graph **Designer**).

Not a fork of Arch — a derivative distro (like CachyOS/EndeavourOS): archiso +
our package selection + our tools.

## Repo layout

```
src/                     the engine + CLI (Rust)
  main.rs                clap CLI (install/verify/export/diff/sync/reconfigure/history/
                         rollback/desktops/kernels/tui/provision/strata/paru/android/
                         update) + finish_and_reboot()
  manifest.rs            the manifest.json schema (serde) + validation
  install.rs             install pipeline orchestration (order of steps)
  pacman.rs              repos (multilib/cachyos), -Syu, source-paru bootstrap, install
  strata.rs              foreign-distro strata: bootstrap Debian/Ubuntu/Fedora/Alpine
                         rootfs, chroot-shim binaries onto host PATH, command-not-found +
                         .deb/.rpm open-to-install (docs/strata-design.md)
  android.rs             Android apps via Waydroid: container + lazy lifecycle +
                         android-install (.apk/.apkm/.apks/.xapk) + fuzzel launchers
  flatpak.rs update.rs   Flatpak remotes+apps  /  `manifest update` across every source
  kernel.rs / boot.rs    kernel catalog + headers  /  bootloader (systemd-boot, grub, microcode)
  desktop.rs             25 desktop/WM recipes + display managers
  system.rs users.rs files.rs   hostname/locale/tz/keymap  /  useradd+sudoers+chpasswd  /  declarative writes
  dotfiles.rs snippets.rs       clone+place repos (list, subdir/into)  /  marker-block config fragments
  theming.rs keybindings.rs wallpaper.rs scaling.rs   cross-desktop theme / universal keybinds / wallpaper / HiDPI scale
  gestures.rs            cross-desktop touchpad gestures (native workspace_swipe / niri built-in, else libinput-gestures + auto-pkg)
  survey.rs conditions.rs     author questions, {{id}} injection  /  Facts + when/conditional/detect engine
  plugins.rs             expand custom blocks (docker/tailscale/…) into core primitives; inline or from plugins/
  export.rs diff.rs history.rs  capture running system  /  preview changes (+ requires_full_apply) /  git-backed history + rollback
  installer.rs           the disk EXECUTOR: partition->format->mount->pacstrap->manifest install
  probe.rs               InstallPlan + disk/network/manifest/existing-OS probing (shared by TUI+GUI)
  tui.rs                 the Ratatui guided installer
  exec.rs                Ctx: run/sudo/shell/write_root/write_user/set_password/cryptsetup/check + --dry-run
  bin/manifest-gui/      GTK4 graphical installer (feature `gui`) — i18n catalogs in i18n/
  bin/manifest-center/   System Snapshots app (feature `gui`): main.rs, snapshots.rs, designer.rs, settings.rs (post-install settings panel)
iso/
  manifest-os/           archiso profile (derived from releng, rebranded)
  build.sh               bakes binaries+examples, fixes CRLF + mangled symlinks, runs mkarchiso
scripts/
  audit-examples.sh      FAST static audit of examples: URL liveness, package existence, config validity
  audit-vms.sh           full unattended VM installs of every example (deep, slow)
marketplace/             submission-review tooling (scanner + web UI + boot-test + cache) — see its README
docker/Dockerfile        Arch container for fast engine testing
examples/*.json          4 flagship desktops (tokyonight-aurora/catppuccin-plasma/niri-rice/sway-pro); reference/ = feature demos + smaller configs
plugins/*.json           bundled plugins (docker/tailscale/ollama/k3s/steam) — new manifest blocks, baked to /usr/share/manifest-os/plugins
dist/                    build artifacts (gitignored): ISOs + screenshots
```

Three binaries: **`manifest`** (CLI, always), **`manifest-gui`** and
**`manifest-center`** (both need `--features gui`; the ISO build compiles with it).

## The manifest (what a JSON can declare)

`system`, `repos`, `packages`, `services`, `dotfiles` (one repo or a list, with
`subdir`/`into` retargeting), `desktop` + `display_manager`, `boot`, `users`,
`files`, `snippets`, `flatpak`, `defaults`, `wallpaper`, `keybindings`,
`gestures` (touchpad — native-first, else auto-installed libinput-gestures), `theme`,
`display` (HiDPI `scale`), `login` (greeter theme — bundled SDDM theme styled by
`accent`/`background`/…, or select another; tuigreet colours),
`strata` (foreign-distro binary access — see below), `android` (Android apps via
Waydroid), and `pre_install`/`post_install` (the escape hatch —
everything else is declarative). Plus the **adaptive** layer: `variables` +
`survey`/`settings` questions (`{{token}}` substitution), auto-detected `detect`
facts (gpu/cpu/virt/is_vm/firmware/scale), and `when`-gated `conditional`
overlays + `conditional_packages`. Plus **plugins**: `docker`/`tailscale`/etc.
blocks that a plugin (bundled in `plugins/`, or inline in the manifest's own
`plugins` array) expands into core primitives before parsing — the core never
learns what they mean. Schema: [`src/manifest.rs`](src/manifest.rs); facts/
conditions engine: [`src/conditions.rs`](src/conditions.rs); plugin expander:
[`src/plugins.rs`](src/plugins.rs); complete example:
[`examples/tokyonight-aurora.json`](examples/tokyonight-aurora.json).

## Foreign software — run non-Arch apps beside pacman

Full design: [`docs/strata-design.md`](docs/strata-design.md). Shipped in the
`manifest-os` package (`pacman -Syu`).

- **Strata** (`strata` block / `manifest strata add <distro>`): a full
  **Debian/Ubuntu/Fedora/Alpine** rootfs under `/strata/<name>`, *never booted* —
  entered via a private-mount-namespace chroot, with per-binary **shims** on the
  host PATH so an `apt`-installed and a `pacman`-installed binary run from one
  shell. CLI + TUI (host terminfo bound) + **GUI** foreign apps; `apt`/`dnf`/`apk`
  install **auto-exposes** binaries + mirrors their `.desktop` to the menu; GUI
  apps launch one-click passwordless (scoped sudoers). **Command-not-found**:
  typing `apt`/`dnf`/`apk` (or `paru`, or `waydroid`) offers to set it up. And
  **open-to-install**: double-click a `.deb`/`.rpm` → `strata-install` puts it in
  the matching stratum.
- **Android** (`android` block / `manifest android`): Android apps via **Waydroid**
  (a container on the host kernel, not a VM). **Lazy lifecycle** — nothing runs at
  boot; `waydroid-launch` brings it up on first app launch and a `waydroid-idle`
  timer stops it after `idle_minutes` (default 45) unused. `android-install
  <apk|.apkm|.apks|.xapk|fdroid-id>` installs single APKs, **split bundles**, or
  F-Droid ids; bundles/`.apk*` also **open-to-install**. F-Droid ships in-container.
- **Windows — wine tier** (`windows` block / `windows-install <file.exe>`): a
  per-app `WINEPREFIX`, winetricks verbs, and a `.desktop` launcher. A
  compatibility oracle (`src/wincompat.rs`) scans the installer's bytes for
  markers (kernel anti-cheat, dongles, MSIX) and only *blocks* on marker
  evidence, never on a fuzzy name match — when it is unsure it asks. Wine is
  installed on first use, not at install time. **Works: Notepad++ on real HW.**
- **Windows — VM tier** (`windows.vm` / `manifest windows-vm`): a real Windows in
  a container (dockur/windows, downloaded from Microsoft unattended) with apps
  painted onto the desktop by **WinApps** over FreeRDP RemoteApp. Same lazy
  lifecycle as Android. Declining Wine offers this instead. **See the section
  below — this tier has sharp edges that cost five release cycles.**
- **`manifest update`**: one command updates the host (repos + AUR), every stratum
  via its own package manager, Flatpak, and the Waydroid image.

### The Windows VM tier — read this before touching it

Everything here was learned by failing on real hardware. Each item silently
produces "the window didn't open", so none of them are guessable from a log.

1. **WinApps hardcodes two things** (`bin/winapps`, not configurable):
   `readonly CONTAINER_NAME="WinApps"` and
   `readonly COMPOSE_PATH="${HOME}/.config/winapps/compose.yaml"`. Our container
   must be *named* `WinApps` and the compose must *also* live at that path, or it
   exits `no such object: WinApps` before it ever reaches RDP.
2. **Volumes must be absolute.** The same compose file lives in two directories
   now; `./storage` would resolve against each one — two disks, two Windows
   installs, neither the one the user set up.
3. **The guest must allow RemoteApp.** Windows refuses to run an arbitrary
   program as a RemoteApp unless `TSAppAllowList\fDisabledAllowList=1`. WinApps'
   `oem/RDPApps.reg` sets it, applied by dockur running `C:\OEM\install.bat`
   **during Windows setup**. A guest installed without it can never run one and
   *cannot be repaired* — there is no re-running setup. `windows-vm-run` detects
   this (a `.remoteapp-enabled` stamp) and offers an unattended reinstall.
4. **Their `oem/` files are GPL-3.0.** They are copied **at runtime** from the
   user's clone into the oem mount — never into this MIT tree. Our debloat step
   *appends to* their `install.bat` rather than replacing it (dockur runs exactly
   one, and theirs is the one that matters).
5. **RDP's alternate shell (`/shell:`) is a dead end here** — do not re-add it.
   Windows client editions are single-session and dockur signs the user in at the
   console, so an RDP connect *takes over* that session instead of creating one:
   the alternate shell is ignored, explorer runs, and you get a plain Windows
   desktop that also looks "successful" to any duration check. A unit test keeps
   it out.
6. **WinApps discards FreeRDP's output** (`$FREERDP_COMMAND … &>/dev/null &`), so
   a failed launch leaves nothing to read. `FREERDP_COMMAND` points at a
   generated `manifest-freerdp` wrapper that keeps stderr in
   `~/.local/state/windows-vm-freerdp.log`. Three rounds of guessing were the
   direct cost of not having this.
7. **`winapps manual` blocks** on `wait $FREERDP_PID`, so an instant return means
   FreeRDP died on the spot. Exit status alone proves nothing — it is 0 either
   way.
8. **A portable `.exe` installs nothing** (Rufus, for one), so an app scan finds
   nothing by construction. When no new app is detected we write a `.desktop` for
   the transferred file itself. And detection matches `.desktop` **content**, not
   filename: WinApps writes `<exe>.desktop` with no prefix of its own.
9. **`dk()` must never use interactive `sudo`.** These scripts are what launchers
   Exec, and a launcher has no TTY — the prompt hangs forever showing nothing
   (the same trap Waydroid's launchers hit). `sudo -n` + `notify-send`; a test
   walks the generated scripts and fails on any interactive sudo.

Engine gate: strata/Android orchestration is unit + dry-run + (strata) VM-verified;
**Android/Waydroid rendering is real-hardware-only** (VBox GL 2.1 can't run gralloc).

## Install options (TUI + GUI + `provision`)

Blank-disk **erase** or **alongside** (dual-boot, shrinks Windows/Linux);
**LUKS** (full-disk or /home); **LVM**; **RAID1**; **swap** (none/zram/file/
partition); **NVIDIA** proprietary driver; **printing**; **autologin**; **root
password**; **extra users**; **static IP / VLAN / proxy**. `manifest provision`
is the unattended CLI form of all of it (what `audit-vms.sh` drives).

## Status — what's built and how it was verified

| Area | State | Verified on |
|---|---|---|
| Manifest schema + all declarative blocks | ✅ | unit/dry-run + Docker + VM |
| repos, source-paru, packages, 25 desktops | ✅ | Docker (real Arch) |
| system / users / files / snippets / theme / keybindings / wallpaper | ✅ | VM |
| dotfiles clone + per-file place (list, subdir/into) | ✅ | Docker + dry-run |
| variables / survey / `when`+conditional / detect facts | ✅ | unit + VM |
| plugins — custom blocks expand into core (inline + bundled) | ✅ | unit + dry-run |
| HiDPI `display.scale` (desktop+cursor+lock) + settings-app panel | ✅ | VM (14" 4K) |
| bootloader: GRUB (BIOS+UEFI) installs **and boots** | ✅ | VM |
| systemd-boot (UEFI) | ✅ | VM (UEFI) |
| guided TUI + **GTK GUI installer** (all screens) | ✅ | VM |
| full install → reboot into installed desktop | ✅ | VM (niri-rice, **hyprland-pro**) |
| **UEFI hands-off reboot** (efibootmgr boot-order) | ✅ | VM (UEFI) |
| dual-boot alongside Windows (shrink + reuse ESP, per-OS bootloader) | ✅ | real HW (4 concurrent installs, stable) |
| LUKS (systemd `sd-encrypt` + BIOS/UEFI) | ✅ | VM |
| System Snapshots app (save/restore/apply/Designer/settings + **strata-aware**) | ✅ | VM (cage software-render) |
| export / diff / sync / reconfigure / history / rollback | ✅ | VM + dry-run |
| **strata**: Debian/Ubuntu/Fedora/Alpine bootstrap + PATH shims + CLI/TUI/GUI | ✅ | real HW + VM |
| strata GUI foreign apps (passwordless launch, .desktop menu, fonts/terminfo) | ✅ | VM (real XWayland via weston + Xvfb-auth) |
| strata command-not-found + `.deb`/`.rpm` open-to-install | ✅ | unit + dry-run |
| `paru` command-not-found (`manifest paru`) | ✅ | unit + dry-run |
| **Android/Waydroid** (`android` block, lazy lifecycle, `android-install`, `.apkm`) | ✅ | **real HW** (install, ARM libndk, launchers, open-to-install) |
| **Windows wine tier** (`windows` block, oracle, per-app prefix, lazy wine) | ✅ | real HW (Notepad++) |
| **Windows VM tier** (dockur + WinApps RemoteApp) | ⏳ Windows installs ✅; single-window launch **unconfirmed** | real HW; needs a guest reinstalled with the RemoteApp registry file |
| `manifest update` (host + AUR + strata + Flatpak + Waydroid) | ✅ | unit + dry-run |
| WiFi list+connect (rfkill-unblock included) | ✅ | real HW (laptop) |
| Install-log to USB on a real-HW failure | ✅ fixed | needs a real failing USB to re-confirm |
| marketplace boot-test **server** (`server.py`) | ⏳ WIP, unverified | see marketplace/SERVER-TODO.md |

## How to build & test

> **Moving development onto a real Arch machine (in progress).** Everything below
> assumes the historical Windows-host + VirtualBox rig. On a native Arch box most
> of that goes away, and one long-standing blocker disappears with it:
>
> - **The ISO builds natively** — `cargo build --release --features gui` then
>   `sudo ./iso/build.sh`. No tarball-into-the-VM dance, no `arch-chroot`, none of
>   the "don't background jobs inside arch-chroot" or dated-filename traps. The
>   packages likewise: `packaging/build-repo.sh` wants `MANIFEST_PKGVER=0.1.0` and
>   a `$WORK` (default `/home/pkgwork`) holding `pkg/` + `keyring/` and a
>   `manifest-os-$PKGVER.tar.gz` **built from `git archive`, never from the
>   working tree** (the tree carries `iso/work/`, which is tens of GB and once
>   filled the build disk).
> - **The GPG signing key stays wherever the maintainer is.** `sign-repo.sh` runs
>   on the machine holding the private key and nowhere else; only signatures ship.
>   Moving machines means moving that key deliberately, not copying the repo.
> - **The big unlock: KVM.** The Windows VM tier could never be exercised here —
>   VirtualBox has no nested KVM, so *every* bug in it surfaced on the user's
>   hardware, five release cycles in a row. A real Arch box with `/dev/kvm` can
>   run `manifest windows-vm` end to end. **Do this before touching that code
>   again**; the remaining unknown (does a guest installed *with* WinApps'
>   `RDPApps.reg` actually paint a borderless window?) is one local run away.
> - Waydroid rendering also needs real hardware/GPU — same reason, now available.

**Engine (Docker, any host):** `docker build -f docker/Dockerfile -t manifest-test .`
then `docker run --rm manifest-test install examples/reference/bootstrap.json [--dry-run]`.

**Audit the examples before an ISO:** `bash scripts/audit-examples.sh` (fast:
URLs live? packages exist? add `-c` to validate compositor configs). Deeper:
`bash scripts/audit-vms.sh` (full VM installs). Run on an Arch box / the
`manifest-build` VM.

**The ISO (needs Arch + root — can't build on Windows):**
`cargo build --release --features gui` then `sudo ./iso/build.sh`
→ `iso/out/manifestos-*.iso`. Built in the `manifest-build` VirtualBox VM;
`build.sh` repairs the Windows-checkout hazards (see Gotchas).

**Write to USB:** balenaEtcher or Rufus "DD Image mode" (isohybrid). Disable
Secure Boot on the target.

## The VirtualBox test rig (how this is driven from Windows)

`VBoxManage.exe` at `/c/Program Files/Oracle/VirtualBox/`. **`manifest-build`** =
the always-on ISO builder + package cache; ephemeral `review-*`/`audit-*`/
`hyprtest` VMs are throwaway install targets. The live Arch ISO bundles
`virtualbox-guest-utils`, so `guestcontrol --username root --password ""` works.

- In Git Bash prefix with `export MSYS_NO_PATHCONV=1` (or guest paths get
  mangled). Pass Windows source paths to VBoxManage with **forward slashes**
  (`C:/Users/...`) so `$var` expands cleanly; **never** `copyto --recursive`
  onto files — it truncated every example to 0 bytes once.
- The build VM's real Arch is a **chroot at `/mnt`** (archiso live env is
  ephemeral RAM). Long/backgrounded services must be started from the *live env*
  with `setsid … & disown`, not inside `arch-chroot` (its PID namespace is killed
  when the call returns — this bit seatd/cage and pacoloco).
- The **installed** system has no guest additions: drive it with
  `controlvm keyboardputstring/keyboardputscancode 1c 9c` + `screenshotpng`.
  Its **UEFI Boot Manager menu ignores the Enter scancode** — keep a valid GRUB
  nvram entry so it boots straight through (don't rely on the boot menu).

## Gotchas (these cost real time — read before building)

1. **Windows mangles symlinks + CRLF.** A Windows checkout turns airootfs
   `*.wants/*.service` symlinks into text and adds CRLF → pacman-init/vboxservice
   never run, keyring empty, pacstrap fails; `mkarchiso` chokes on `profiledef.sh`.
   `build.sh` re-links + strips CRLF; `.gitattributes` forces LF.
2. **paru-bin is ABI-fragile** — bootstrap *source* `paru`, never `paru-bin`.
3. **Wayland needs a GPU in VBox** — `modifyvm <vm> --accelerate3d on --vram 128`.
4. **Dead URLs / stale desktop config in examples are invisible to `verify`.**
   A 404 wallpaper URL aborted installs; Hyprland 0.55 rejects `windowrule`-float
   / `togglesplit`. `audit-examples.sh` catches both now — run it before shipping.
   The running compositor's error banner is the authoritative config validator.
5. **Don't over-provision test VMs** — 6–8 GB / 4 vCPU. Big installs can OOM the
   ISO's RAM overlay; several concurrent 6 GB VMs overcommit the host.
6. **`git checkout <file>` to undo a one-line experiment reverts the whole
   file** — including everything uncommitted in it. Undo the edit, or commit
   first. This ate a full turn's work once.
7. **Anything written as a real file at setup time is stale forever.** Package
   upgrades regenerate the thin-stub commands (see the pacman hook above) but not
   files like the VM's `compose.yaml`. If a fix changes such a file, it needs a
   migration in the script that reads it — otherwise `pacman -Syu` silently
   leaves users on the broken version.

## Shipping a package update (the loop used for 0.1.0-25 → -59)

1. `cargo build --bin manifest && cargo test --lib && cargo clippy` — and for
   anything generating shell, pipe it through `sh -n`:
   `./target/debug/manifest __script <name> | sh -n`.
2. Bump `pkgrel` in [`packaging/pkg/PKGBUILD`](packaging/pkg/PKGBUILD), commit,
   push (work happens **on `main`**).
3. Build the packages on Arch (natively on the new machine; historically in the
   `manifest-build` VM). Source tarball from `git archive`, **not** the working
   tree. `find pkg -name '*.pkg.tar.zst' -delete` first — old pkgrels accumulate
   and `repo-add` will index 100+ packages that never get uploaded.
4. **Verify the built binary actually contains the change** before publishing:
   `tar -xf <pkg> usr/bin/manifest && strings usr/bin/manifest | grep <new string>`.
   Cheap, and it has caught a stale build more than once.
5. `packaging/sign-repo.sh` (needs the private key — maintainer machine only),
   then `packaging/publish-repo.sh` (gh release, fixed tag `repo`).
6. Delete the superseded pkgrel's assets so the release stays at 16.

## Known gaps / next steps

- **Marketplace review pipeline** ([`marketplace/`](marketplace/)): scanner +
  web UI + package cache are **done + verified**; the live **`server.py`** (UI →
  boot a VM → test with the cache) is a **WIP draft, unverified** — pick-up list
  in [`marketplace/SERVER-TODO.md`](marketplace/SERVER-TODO.md). Still to build:
  stage-2 behavioural capture (outbound conns / listeners / fs-diff), resource
  pinning + manifest signing at approval.
- ~~**Android/Waydroid**~~ — **done** (0.1.0-40), confirmed on real HW. Image
  pins, `export`/`diff`/Snapshots capture, `.xapk` OBB and `scan_android` in
  `scan.py` are all closed.
- ~~**Command-not-found delivery**~~ — **done** (0.1.0-38/-39). Generated commands
  are thin stubs (`exec sh -c "$(manifest __script <name>)"`) and a pacman
  PostTransaction hook (`96-manifest-integration.hook` → `manifest
  __refresh-integration`) regenerates the CNF handler, file handlers and MIME
  defaults on every upgrade. `pacman -Syu` alone now delivers behaviour changes.
  **This is why generated scripts must stay stubs** — anything written as a real
  file at setup time (e.g. the VM's `compose.yaml`) does *not* get updated by an
  upgrade and needs its own migration path.
- **Windows VM tier — one unknown left:** whether a guest installed *with*
  WinApps' `RDPApps.reg` actually paints a borderless RemoteApp window. Untestable
  on the old rig (no nested KVM in VirtualBox); one local run on a KVM box
  settles it. Everything upstream of that is fixed and shipped (0.1.0-59). If it
  works, the fully-borderless path is done; if not, the next thing to check is
  whether dockur's autologin can be disabled, since a *new* session is what
  RemoteApp wants.
- **Phase 6c — GPU passthrough (`vm-vfio`, §14.3):** not started. The CAD /
  SolidWorks path.
- **Real hardware:** WiFi connect, dual-boot alongside Windows, strata GUI and
  Android are all confirmed. Still to re-confirm the install-log-to-USB fix on a
  real failing USB (not testable in VBox).
- **Catalog/site + a real `manifest-os-release` package + signing key** instead
  of the executor writing branding inline.
- **Move dev to real Arch** — *in progress*; see the note at the top of "How to
  build & test" for what changes and what it unblocks.
- **ISO is behind the package.** Latest ISO is `2026.07.26` (0.1.0-40); the
  package is at **0.1.0-59** (all the Windows work). A consolidated ISO wants
  rebuilding — natively, on the new machine.

## One-line mental model

`manifest.json` is the source of truth. The engine is a thin orchestrator of
standard Arch tools (pacman, paru, systemctl, sed, bootctl…) — no bespoke magic.
The TUI/GUI + archiso turn that engine into a bootable OS; System Snapshots turns
it into a friendly lifecycle app; `marketplace/` gates sharing.
