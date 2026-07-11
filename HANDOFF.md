# Manifest OS — Handoff

> *Declare it. Share it. Deploy it.*
> Snapshot for picking this project up cold. Last updated 2026-07-10.
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
  main.rs                clap CLI (install/verify/export/diff/sync/history/rollback/
                         desktops/kernels/tui/provision) + finish_and_reboot()
  manifest.rs            the manifest.json schema (serde) + validation
  install.rs             install pipeline orchestration (order of steps)
  pacman.rs              repos (multilib/cachyos), -Syu, source-paru bootstrap, install
  kernel.rs / boot.rs    kernel catalog + headers  /  bootloader (systemd-boot, grub, microcode)
  desktop.rs             25 desktop/WM recipes + display managers
  system.rs users.rs files.rs   hostname/locale/tz/keymap  /  useradd+sudoers+chpasswd  /  declarative writes
  dotfiles.rs snippets.rs       clone+place repos (list, subdir/into)  /  marker-block config fragments
  theming.rs keybindings.rs wallpaper.rs scaling.rs   cross-desktop theme / universal keybinds / wallpaper / HiDPI scale
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
`files`, `snippets`, `flatpak`, `defaults`, `wallpaper`, `keybindings`, `theme`,
`display` (HiDPI `scale`), and `pre_install`/`post_install` (the escape hatch —
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
| System Snapshots app (save/restore/apply/Designer/settings) | ✅ | VM (cage software-render) |
| export / diff / sync / reconfigure / history / rollback | ✅ | VM + dry-run |
| WiFi list+connect (rfkill-unblock included) | ✅ | real HW (laptop) |
| Install-log to USB on a real-HW failure | ✅ fixed | needs a real failing USB to re-confirm |
| marketplace boot-test **server** (`server.py`) | ⏳ WIP, unverified | see marketplace/SERVER-TODO.md |

## How to build & test

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

## Known gaps / next steps

- **Marketplace review pipeline** ([`marketplace/`](marketplace/)): scanner +
  web UI + package cache are **done + verified**; the live **`server.py`** (UI →
  boot a VM → test with the cache) is a **WIP draft, unverified** — pick-up list
  in [`marketplace/SERVER-TODO.md`](marketplace/SERVER-TODO.md). Still to build:
  stage-2 behavioural capture (outbound conns / listeners / fs-diff), resource
  pinning + manifest signing at approval.
- **Real hardware:** WiFi connect and dual-boot alongside Windows are now
  confirmed on real HW; still to re-confirm the install-log-to-USB fix on a real
  failing USB (not testable in VBox).
- **Catalog/site + a real `manifest-os-release` package + signing key** instead
  of the executor writing branding inline.
- **Move dev to real Arch** for native builds, real GPU, and dogfooding (no VM
  round-trips).

## One-line mental model

`manifest.json` is the source of truth. The engine is a thin orchestrator of
standard Arch tools (pacman, paru, systemctl, sed, bootctl…) — no bespoke magic.
The TUI/GUI + archiso turn that engine into a bootable OS; System Snapshots turns
it into a friendly lifecycle app; `marketplace/` gates sharing.
