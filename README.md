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
manifest export | sync | diff              # Phase 5 (not yet implemented)
```

## Schema

See [`examples/minimal.json`](examples/minimal.json) for a complete v1.0.0
manifest. The schema is defined in [`src/manifest.rs`](src/manifest.rs).

## License

MIT
