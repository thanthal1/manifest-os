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
  - `usr/local/bin/manifest`, `manifest-gui`, `manifest-center` — the three
    binaries, **baked in at build time** by `build.sh` (not committed)
  - `root/.zlogin` — boots straight into the **graphical installer**
    (`manifest-gui` under `cage`/`seatd`, software-render fallback), with the
    text TUI (`manifest tui`) as the fallback path
  - example manifests staged under `usr/share/manifest-os/examples`

## Building (on an Arch host / the VM, as root)

```bash
cargo build --release --features gui   # CLI + GTK installer + System Snapshots
sudo ./iso/build.sh                    # mkarchiso -> iso/out/manifestos-*.iso
```

`--features gui` is required — `build.sh` bakes all three binaries and skips the
desktop app if it isn't next to the CLI. `mkarchiso` needs Arch + root + the
`archiso` package — it can't run on Windows. Build it in the `manifest-build`
VM, then boot the resulting ISO. `build.sh` also repairs the Windows-checkout
hazards (mangled systemd symlinks, CRLF) — see HANDOFF's Gotchas.

## The install flow (implemented)

The live image boots into the guided installer, which owns the parts that can't
live in a manifest — then hands off to `manifest install` for the declarative
part:

1. **Network** — wired + WiFi join (`iwd`, with rfkill-unblock)
2. **Disk** — blank-disk erase, alongside-Windows dual-boot, or LUKS/LVM/RAID;
   filesystem + swap
3. **Manifest** — pick a bundled example, enter a URL, or load from USB
4. **Install** — run the survey/settings, then `manifest install` into the new
   root, set up users + bootloader, and reboot

`manifest provision` is the same flow, unattended/headless (scriptable for CI).

[archiso]: https://gitlab.archlinux.org/archlinux/archiso
