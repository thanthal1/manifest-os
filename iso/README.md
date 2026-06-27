# Manifest OS — the ISO

This is what turns the `manifest` CLI into an **operating system**: a bootable
Arch-derived live image that launches into the Manifest OS installer.

It is **not** a fork of Arch — there's no such thing. Like CachyOS or
EndeavourOS, Manifest OS is a *derivative distribution*: an [archiso] profile
(`manifest-os/`, derived from archiso's `releng`) + our tools + (later) our own
package repo.

## What's in the profile

- `manifest-os/profiledef.sh` — ISO identity (rebranded to Manifest OS)
- `manifest-os/packages.x86_64` — packages baked into the live system (Arch's
  releng set + our additions). WiFi is handled by `iwd`, already included.
- `manifest-os/airootfs/` — the live filesystem overlay:
  - `usr/local/bin/manifest-welcome` — auto-launches on tty1 login; brings the
    network up (incl. WiFi via `iwctl`) and points at the install flow
  - `usr/local/bin/manifest` — the CLI, **baked in at build time** by `build.sh`
    (not committed)
  - `root/.zlogin` — runs the welcome on boot

## Building (on an Arch host / the VM, as root)

```bash
cargo build --release          # produce the Linux binary
sudo ./iso/build.sh            # mkarchiso -> iso/out/manifestos-*.iso
```

`mkarchiso` needs Arch + root + the `archiso` package — it can't run on
Windows. Build it in the Arch VM, then boot the resulting ISO.

## What still belongs in the real TUI

`manifest-welcome` is a placeholder. The full Ratatui TUI will own the parts
that can't live in a manifest:

1. **Network** — WiFi join (stubbed here via `iwctl`)
2. **Disk** — partitioning, filesystem, swap (the steps hand-scripted during VM
   testing — that flow is the TUI's spec)
3. **Manifest** — browse catalog / enter URL / load from USB
4. **Install** — run the survey, then `manifest install` into the new root

[archiso]: https://gitlab.archlinux.org/archlinux/archiso
