# Manifest OS

> *Declare it. Share it. Deploy it.*

Declare a complete Arch Linux system — packages, kernel, repos, services,
dotfiles, and pre/post hooks — in a single `manifest.json`, and reproduce it
exactly on any machine with one command.

```bash
manifest install ./my-setup.json
```

## Status

**Phase 1 — Core CLI.** This repo currently implements the local install
pipeline against an already-running Arch system. The ISO/TUI installer, the
versioned schema loader, the survey system, and the catalog come later (see the
project plan).

The install flow that exists today:

| Step | What it does |
|------|--------------|
| Enable repos | multilib / CachyOS (CachyOS implied by `kernel: "cachy"`) |
| Bootstrap paru | the one hardcoded AUR helper |
| `pre_install` hooks | author shell, run first |
| Install packages | one `paru -S` for official + AUR + kernel |
| Dotfiles | `git clone` (placement coming) |
| Enable services | systemd system + user units |
| `post_install` hooks | author shell, run last |

> Network, disk, partitioning and filesystem are **not** the manifest's job —
> they belong to the ISO's TUI layer.

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
with `"display_manager"`. See [`examples/hyprland-rice.json`](examples/hyprland-rice.json)
and [`examples/gnome.json`](examples/gnome.json).

> All 118 packages across the catalog are validated against the real Arch repos
> in CI-style container runs — no broken package names.

## Try it (safely, anywhere)

The `--dry-run` flag prints every step without touching the system, so it works
even on a non-Arch dev machine:

```bash
cargo run -- verify  examples/minimal.json
cargo run -- install examples/minimal.json --dry-run
```

For a real install, run it inside a **throwaway Arch VM with snapshots** — the
install is destructive and meant to be rolled back during development.

## Commands

```bash
manifest install <file.json> [--dry-run]   # apply a manifest
manifest verify  <file.json>               # validate structure + schema version
manifest desktops                          # list supported desktops / WMs
manifest export | sync | diff              # Phase 5 (not yet implemented)
```

## Schema

See [`examples/minimal.json`](examples/minimal.json) for a complete v1.0.0
manifest. The schema is defined in [`src/manifest.rs`](src/manifest.rs).

## License

MIT
