# Manifest OS — Handoff

> *Declare it. Share it. Deploy it.*
> Snapshot for picking this project up cold. Last updated 2026-06-27.
> Repo: https://github.com/thanthal1/manifest-os

## What this is

Two things in one repo:

1. **The `manifest` CLI** — reads a single `manifest.json` (the source of truth)
   and reproduces a complete Arch system: kernel, repos, packages, a full
   desktop, users, config files, services, dotfiles, bootloader.
2. **Manifest OS** — a bootable Arch-derived distribution (an archiso profile +
   the CLI + a guided installer TUI) that boots straight into a friendly
   installer.

It is **not** a fork of Arch — it's a derivative distro (like CachyOS/Endeavour):
archiso + our package selection + our tools.

## Repo layout

```
src/                     the manifest CLI (Rust, one binary)
  main.rs                clap CLI + finish_and_reboot()
  manifest.rs            the manifest.json schema (serde structs) + validation
  install.rs             install pipeline orchestration (the order of steps)
  pacman.rs              repos (multilib/cachyos), pacman -Syu, paru bootstrap, package install
  kernel.rs              kernel catalog (linux/lts/zen/hardened/cachyos) + headers
  desktop.rs             25 desktop/WM recipes + display managers (resolve/apply)
  system.rs              hostname/locale/timezone/keymap (file ops, chroot-safe)
  users.rs               useradd + validated sudoers + chpasswd (never logged)
  files.rs               declarative file writes (~/ as user, /etc as root)
  dotfiles.rs            clone repo + per-file symlink/copy into $HOME
  boot.rs                bootloader: systemd-boot (UEFI) / grub (UEFI+BIOS) + microcode
  survey.rs              author questions, {{id}} injection, conditional_packages
  installer.rs           the TUI's install EXECUTOR (disk -> pacstrap -> manifest install)
  tui.rs                 the Ratatui guided installer (Welcome->Network->Disk->Manifest->Confirm)
  exec.rs                Ctx: run/sudo/shell/output/write_root/write_user/set_password/check + --dry-run
  logo.txt              embedded Manifest OS logo (fastfetch)
iso/
  manifest-os/           archiso profile (derived from releng, rebranded)
  build.sh               bakes binary+examples, fixes CRLF + mangled symlinks, runs mkarchiso
  README.md
docker/Dockerfile        Arch container for fast engine testing
examples/*.json          sample manifests (niri-rice is the full riced demo)
dist/                    build artifacts (gitignored): ISOs, extracted binary, test screenshots
```

## CLI commands

```
manifest install <file> [--dry-run] [--answers a.json]   apply a manifest
manifest verify  <file>                                  validate
manifest desktops                                        list 25 desktops/WMs
manifest kernels                                         list kernels
manifest tui [--dry-run]                                 guided installer (used on the ISO)
manifest export | sync | diff                            Phase 5, NOT implemented
```

`--dry-run` prints every command without executing — works on any OS (incl.
Windows) and is the safe way to inspect behavior.

## Status — what's built and how it was verified

| Area | State | Verified on |
|---|---|---|
| Manifest schema + parse | ✅ | unit/dry-run |
| repos, paru bootstrap (source paru), packages | ✅ | Docker (real Arch) |
| 25 desktop/WM recipes | ✅ all 118 pkgs resolve | Docker |
| kernel + headers, CachyOS repo trigger | ✅ | Docker |
| system block (hostname/locale/tz/keymap) | ✅ | Docker + VM |
| users, files (declarative) | ✅ | VM (real Arch) |
| dotfiles symlink/copy | ✅ | Docker |
| survey + {{}} + conditional_packages | ✅ | dry-run |
| bootloader (GRUB/BIOS) installs **and boots** | ✅ | VM |
| guided TUI (all screens, navigation) | ✅ | VM console |
| full TUI install -> reboot into installed system | ✅ | VM (`niri-rice`) |
| ISO builds, boots, branded, auto-launches TUI | ✅ | VM |
| **WiFi list-all + connect-verify** | ⚠️ code only | VBox has no wireless — needs real HW |
| **systemd-boot (UEFI) path** | ⚠️ code only | VM is BIOS — needs UEFI HW |
| **USB `manifests/` scan (RM=1)** | ⚠️ logic verified, filter untestable | VBox shows RM=0 — needs real USB |
| export / sync / diff (Phase 5) | ❌ stubbed | — |

## How to build & test

**Fast loop — the engine (Docker, on any host):**
```
docker build -f docker/Dockerfile -t manifest-test .
docker run --rm manifest-test install examples/bootstrap.json --dry-run
docker run --rm manifest-test install examples/bootstrap.json        # real, in a throwaway Arch container
```
Covers everything except the bootloader booting and services on a real init.

**The ISO (needs Arch + root — can't build on Windows):**
```
cargo build --release
sudo ./iso/build.sh        # -> iso/out/manifestos-*.iso
```
Built in the `manifest-build` VirtualBox VM (60 GB disk). `build.sh` repairs the
two Windows-checkout hazards automatically (see Gotchas).

**Write the ISO to USB:** balenaEtcher, or Rufus in "DD Image mode". It's an
isohybrid image. Disable Secure Boot on the target.

## The VirtualBox test rig (how this was driven)

Dev happened on Windows driving VirtualBox via `VBoxManage.exe`:
- VM **`arch`** = install target (BIOS, 8 GB). VM **`manifest-build`** = ISO builder (60 GB).
- The live Arch ISO bundles `virtualbox-guest-utils`, so `guestcontrol` works as
  `--username root --password ""`.
- In Git Bash, prefix everything with `export MSYS_NO_PATHCONV=1` or guest paths
  get mangled to Windows paths.
- Detached long jobs need `setsid bash -lc "..."` (login shell) or PATH is lost
  inside `arch-chroot` (gives exit 127).
- The **installed** system has no guest additions: drive it with
  `controlvm <vm> keyboardputstring "..."` / `keyboardputscancode 1c 9c` (Enter)
  and read the screen with `controlvm <vm> screenshotpng`.

## Gotchas (these cost real time — read before building)

1. **Windows mangles symlinks.** A Windows git checkout turns the airootfs
   `*.wants/*.service` symlinks into text files, so `pacman-init`, `vboxservice`,
   `networkd` never run on the built ISO → empty keyring → pacstrap fails.
   `build.sh` re-links them. `installer.rs::ensure_keyring()` is a second safety net.
2. **CRLF.** A Windows checkout adds CRLF; `mkarchiso` chokes sourcing
   `profiledef.sh` (`$'\r': command not found`). `build.sh` strips it;
   `.gitattributes` forces LF.
3. **pacstrap provider prompts.** `base` pulls virtual deps (`initramfs`,
   iptables) with multiple providers → an interactive prompt. The executor names
   `mkinitcpio` explicitly; other providers take pacstrap's `--noconfirm` default.
4. **paru-bin is ABI-fragile.** We bootstrap *source* `paru` (links against the
   installed libalpm) — never `paru-bin`.
5. **Wayland needs a GPU in VBox.** Hyprland/niri need 3D accel:
   `VBoxManage modifyvm <vm> --accelerate3d on --vram 128`. Not needed on real HW.

## Known gaps / next steps

- **Verify on real hardware:** WiFi (list+connect), UEFI/systemd-boot, USB
  `manifests/` scan — none are testable in VBox.
- **Phase 5:** implement `export` / `sync` / `diff`. The installed system already
  keeps its manifest at `/etc/manifest-install.json` as the hook.
- **Packaging:** make a real `manifest-os-release` package + own pacman repo +
  signing key, instead of the executor writing branding inline.
- **Catalog (Phase 3):** the GitHub-based manifest catalog + static site.
- **Move dev to real Arch** for native builds, real GPU, and dogfooding (ideally
  run the dev environment itself on Arch so no VM round-trips).

## One-line mental model

`manifest.json` is the source of truth. The CLI is a thin orchestrator of
standard Arch tools (pacman, paru, systemctl, sed, bootctl…) — no bespoke magic.
The TUI + archiso turn that engine into a bootable OS.
