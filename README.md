# Manifest OS

> *Declare it. Share it. Deploy it.*

Declare a complete Arch Linux system — packages, kernel, repos, services,
dotfiles, and pre/post hooks — in a single `manifest.json`, and reproduce it
exactly on any machine with one command.

```bash
manifest install ./my-setup.json
```

## Status

Working end-to-end: the **`manifest` CLI** applies a manifest to a running Arch
system; a **bootable ISO** boots straight into a **graphical installer**
(`manifest-gui`, GTK4) or a text TUI and installs to a blank disk, alongside
Windows, or onto encrypted/LVM/RAID layouts; and an installed system gets
**System Snapshots** (`manifest-center`) — a friendly desktop app to save/restore
setups and edit config visually (the node-graph "Designer"). The manifest
lifecycle (`export` / `diff` / `sync` / `history` / `rollback`) is implemented.
See [HANDOFF.md](HANDOFF.md) for the full map and what's verified.

The core install flow:

| Step | What it does |
|------|--------------|
| Enable repos | multilib / CachyOS (CachyOS implied by `kernel: "cachy"`) |
| Bootstrap paru | the one hardcoded AUR helper |
| `pre_install` hooks | author shell, run first |
| Install packages | one `paru -S` for official + AUR + kernel |
| Dotfiles | `git clone` + per-file placement (one repo or a list) |
| Enable services | systemd system + user units |
| `post_install` hooks | author shell, run last |

> Network, disk, partitioning and filesystem are **not** the manifest's job —
> they belong to the ISO's TUI layer.

## Bootloader

A `boot` block installs and configures the bootloader so a non-default kernel
actually boots. UEFI/BIOS is auto-detected; the root device and CPU microcode
are resolved automatically.

```json
"boot": {
  "loader": "systemd-boot",      // or "grub"
  "cmdline": ["quiet", "splash"],
  "timeout": 4
}
```

- **systemd-boot** (UEFI): `bootctl install`, writes `loader.conf` + a
  per-kernel entry with `root=PARTUUID=…` and the microcode initrd.
- **grub** (UEFI or BIOS): sets `/etc/default/grub`, runs `grub-install` for the
  detected firmware, then `grub-mkconfig`.

Opt-in by presence — omit `boot` and the installer leaves the bootloader alone.
This step is designed for the ISO's chroot context and must be tested in a VM.

## Dotfiles

```json
"dotfiles": {
  "source": "https://github.com/you/dotfiles",
  "branch": "main",
  "method": "symlink"            // or "copy"
}
```

The repo is treated as a mirror of `$HOME` and placed **per file** — a repo's
`.config/nvim/init.lua` lands at `~/.config/nvim/init.lua` without replacing the
whole `~/.config`. `symlink` links each file back to a persistent clone (so
editing the repo updates the live config); `copy` copies them. `.git`, README
and LICENSE are skipped.

**`subdir` / `into`** retarget where files are read from and written to, for
repos that don't mirror `$HOME`. `subdir:"config", into:"~/.config"` maps
`<repo>/config/*` → `~/.config/*`.

**`dotfiles` may also be a list** — map several repo directories (or several
repos) to different targets in one manifest. Each unique repo is cloned only
once, even when mapped multiple times:

```json
"dotfiles": [
  { "source": "https://github.com/JaKooLit/Hyprland-Dots", "subdir": "config",       "into": "~/.config",            "method": "copy" },
  { "source": "https://github.com/JaKooLit/Hyprland-Dots", "subdir": "assets/.icons", "into": "~/.icons",             "method": "copy" },
  { "source": "https://github.com/JaKooLit/Wallpaper-Bank", "subdir": "wallpapers",   "into": "~/Pictures/wallpapers", "method": "copy" }
]
```

## Declarative config (instead of hooks)

The manifest prefers **declared state** over shell. Common things that would
otherwise be `post_install` commands have first-class blocks the CLI executes:

```json
"users": [
  { "name": "matt", "groups": ["wheel","video"], "shell": "/bin/zsh", "sudo": true }
],
"files": [
  { "path": "/etc/sysctl.d/99-swappiness.conf", "content": "vm.swappiness=10\n", "mode": "644" },
  { "path": "~/.config/foo/bar.conf", "content": "..." }
],
"flatpak": {
  "apps": ["com.visualstudio.code"]
},
"defaults": {
  "browser": "firefox.desktop"
}
```

- **`users`** — creates accounts (idempotent), sets groups/shell, drops a
  validated `/etc/sudoers.d` file for `sudo: true`, and sets `password` via
  `chpasswd` over stdin (never logged; prefer a survey `secret`).
- **`files`** — writes a file verbatim: `~/...` as the user, absolute paths as
  root, creating parent dirs and applying `mode`/`owner`. Replaces
  `mkdir`/`echo >`/`cat >` hooks.
- **`flatpak`** - installs Flatpak if needed, adds remotes such as Flathub, and
  installs or updates declared app ids.
- **`defaults`** - writes the primary user's `mimeapps.list` for browser and
  MIME defaults without needing a desktop session.

`pre_install` / `post_install` remain only as an escape hatch for the genuinely
one-off.

## Adaptive manifests (variables, conditions, machine detection)

A manifest can adapt to the machine and to the person installing it, instead of
being a fixed snapshot:

```json
"variables": { "editor": "nvim" },
"survey":  [ { "id": "laptop", "type": "bool", "label": "Is this a laptop?" } ],
"detect":  { "gpu": "auto", "is_vm": "auto" },
"conditional": [
  { "when": { "gpu": "nvidia" }, "packages": ["nvidia-dkms"] },
  { "when": { "laptop": true },  "packages": ["tlp", "brightnessctl"] }
],
"files": [
  { "path": "~/.config/foo.conf", "content": "editor={{editor}}", "when": { "is_vm": false } }
]
```

- **`variables`** — author-defined values, substituted as `{{name}}` anywhere.
- **`survey`** — questions asked at install; answers become tokens too, with
  optional `pattern` / `min` / `max` validation.
- **`detect`** — auto-detected facts (`gpu`, `cpu`, `virt`, `is_vm`, `firmware`,
  `scale`) exposed as tokens.
- **`when`** — gate a `conditional` package/file/service overlay, or any single
  `files` entry, on those facts.

## Display scaling & settings app

`display.scale` scales the desktop, cursor and lock screen for HiDPI panels; set
it to `"{{scale}}"` to auto-detect from the panel resolution. A manifest can also
expose a **`settings`** block — the same question format as `survey`, but editable
*after* install from the System Snapshots app, so a well-authored manifest doubles
as a settings panel (scale, accent colour, wallpaper, opacity, …):

```json
"display":  { "scale": "{{scale}}" },
"settings": [ { "id": "scale", "type": "scale", "label": "Display scale", "default": 1.0 } ]
```

## Plugins (new blocks without bloating the core)

The core schema stays small; new capabilities grow at the edges. A **plugin**
teaches the manifest a new top-level block — `docker`, `tailscale`, `ollama`,
`k3s`, `steam`, … — by declaring how it *expands* into primitives the engine
already knows (`packages`, `services`, `files`, `repos`, hooks, `conditional`):

```json
"docker":    { "compose": true },
"tailscale": {},
"ollama":    { "webui": true },
"steam":     { "gamemode": true }
```

The engine never learns what "docker" means — plugin blocks are folded into core
fields *before* it parses the manifest. Plugins load from a plugins directory
(bundled ones live in [`plugins/`](plugins)) or, so a shared manifest is fully
self-contained, from the manifest's own inline `plugins: [ … ]` array. Expansion
is pure data — no code runs — so a plugin is as reviewable as any other block,
and the marketplace scanner vets what it expands into. See
[`plugins/README.md`](plugins/README.md) to write one, and
[`examples/reference/plugins-demo.json`](examples/reference/plugins-demo.json)
for bundled + inline plugins together.

## System basics

The `system` block sets the machine's identity and localization. All applied
with file operations (so they also work inside `arch-chroot`):

```json
"system": {
  "hostname": "archbox",
  "locale":   "en_US.UTF-8",
  "timezone": "America/New_York",
  "keymap":   "us",
  "kernel":   "linux-zen"
}
```

| Field | Effect |
|---|---|
| `hostname` | writes `/etc/hostname` + the `/etc/hosts` loopback block |
| `locale` | uncomments it in `/etc/locale.gen`, runs `locale-gen`, sets `/etc/locale.conf` |
| `timezone` | symlinks `/etc/localtime` (validated), syncs the RTC (best-effort) |
| `keymap` | writes `/etc/vconsole.conf` |
| `kernel` | see below |

Every field is optional and idempotent.

## Kernels (always one, switchable)

Every install gets a kernel — without one the system won't boot. Omit
`system.kernel` and you get stock Arch **`linux`** (the bootstrap default);
set it to switch. The matching `*-headers` are always installed too, so DKMS
drivers (e.g. `nvidia-dkms`) build cleanly.

| `system.kernel` | Package | Notes |
|---|---|---|
| *(unset)* / `linux` | `linux` | Mainline, Arch default |
| `linux-lts` | `linux-lts` | Long-term support |
| `linux-zen` | `linux-zen` | Desktop-tuned |
| `linux-hardened` | `linux-hardened` | Security mitigations |
| `cachy` | `linux-cachyos` | BORE/EEVDF, x86-64-v3/v4; pulls the CachyOS repo + key |

`manifest kernels` lists them. Installing a kernel auto-regenerates the
initramfs (pacman's mkinitcpio hook); adding a *bootloader* entry is the ISO
boot step's job, not the manifest's.

## Desktops (one field, full setup)

Add a single `"desktop"` field and the installer expands it into the entire
environment — core packages, a display manager, XDG portals, a polkit agent,
companion apps (terminal, notifications, launcher, bar, wallpaper, applets),
services, and any greeter/session config that has to be written:

```json
{ "schema_version": "1.0.0", "meta": { "name": "x" }, "desktop": "hyprland" }
```

25 environments are supported out of the box (`manifest desktops` to list):

- **Desktops:** GNOME, KDE Plasma, Xfce, Cinnamon, MATE, LXQt, LXDE, Budgie,
  Deepin, COSMIC
- **Wayland WMs:** Hyprland, Sway, Niri, River, labwc, Wayfire
- **X11 WMs:** i3, bspwm, awesome, Qtile, Openbox, xmonad, herbstluftwm,
  Fluxbox, IceWM

Each recipe picks a sensible display manager (GDM/SDDM/LightDM/greetd/ly/
cosmic-greeter) and writes its config automatically. Override it per manifest
with `"display_manager"`. See the four flagship desktops in
[`examples/`](examples): [`tokyonight-aurora.json`](examples/tokyonight-aurora.json)
(Hyprland), [`catppuccin-plasma.json`](examples/catppuccin-plasma.json) (KDE Plasma),
[`niri-rice.json`](examples/niri-rice.json) (Niri) and
[`sway-pro.json`](examples/sway-pro.json) (Sway). Smaller feature-demos and extra
configs live in [`examples/reference/`](examples/reference).

> Every package name across the catalog is validated against the real Arch repos
> in CI-style container runs — no broken package names.

## Try it (safely, anywhere)

The `--dry-run` flag prints every step without touching the system, so it works
even on a non-Arch dev machine:

```bash
cargo run -- verify  examples/reference/minimal.json
cargo run -- install examples/reference/minimal.json --dry-run
```

For a real install, run it inside a **throwaway Arch VM with snapshots** — the
install is destructive and meant to be rolled back during development.

## Commands

```bash
manifest install <file.json> [--dry-run] [--answers a.json]   # apply a manifest
manifest verify  <file.json>               # validate structure + schema version
manifest export  [-o out.json]             # capture the running system into a manifest
manifest diff    <file.json>               # preview what applying would change
manifest sync    <file.json>               # re-apply an edited manifest
manifest history                           # list applied manifests (git-backed)
manifest rollback [<ref>]                  # revert to a previous manifest
manifest desktops | kernels                # list supported desktops/WMs / kernels
manifest tui | provision …                 # guided (ISO) / unattended headless installer
```

`--dry-run` prints every command without executing — safe to inspect on any OS.

## Schema

See [`examples/reference/minimal.json`](examples/reference/minimal.json) for a
minimal v1.0.0 manifest, or [`examples/tokyonight-aurora.json`](examples/tokyonight-aurora.json)
for a complete all-declarative desktop. The schema is defined in
[`src/manifest.rs`](src/manifest.rs).

## Sharing manifests (marketplace tooling)

A manifest installs as root, so a shared one is untrusted until reviewed.
[`marketplace/`](marketplace/) has the review pipeline: a static security
scanner (`scan.py` — flags shell hooks, sudoers/SSH/DNS-spoofing/RCE patterns,
untrusted sources), a self-contained web console, and a package-cached VM
boot-test. See [`marketplace/README.md`](marketplace/README.md).

Two auditors keep the bundled examples honest:
[`scripts/audit-examples.sh`](scripts/audit-examples.sh) (fast: URL liveness,
package existence, compositor-config validity) and
[`scripts/audit-vms.sh`](scripts/audit-vms.sh) (full VM installs).

## License

MIT
