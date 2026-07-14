# Working in this repo (agent notes)

ManifestOS is a declarative Arch derivative: one `manifest.json` reproduces a
whole system. The engine is a Rust crate — logic in `src/`, curated example
manifests in `examples/`, bundled plugins in `plugins/`, and the live-ISO
mkarchiso profile in `iso/manifest-os/`.

## Local dev (host = Windows)

`cargo build` / `cargo test` / `cargo clippy` all run on the Windows host and
are the fast inner loop — use them for every code change. The GUI bits need
`--features gui`. `manifest verify examples/<x>.json` validates a manifest.

You **cannot** produce the Linux ISO binaries on Windows. Anything that has to
run on Linux — building the shipped binaries, `mkarchiso` — happens in the VM
(below).

## Building the ISO

The ISO is built inside the **`manifest-build`** VirtualBox VM (an Arch box
with `mkarchiso`). Drive it with `VBoxManage guestcontrol`. This path has
several traps — every one below has bitten a build:

VBoxManage lives at `C:\Program Files\Oracle\VirtualBox\VBoxManage.exe`.
Guest commands take the form (note `MSYS_NO_PATHCONV=1` so MSYS doesn't mangle
guest paths, and `-- -lc`, letting bash set argv0 — don't repeat the command):

```
MSYS_NO_PATHCONV=1 VBoxManage guestcontrol manifest-build \
  --username root --password root run --exe /usr/bin/bash -- -lc "…"
```

### The gotchas (in the order you hit them)

1. **The VM boots the Arch live ISO, not the installed system.** After
   `startvm`, guest control comes up in the live environment (`/run/archiso`),
   which has no toolchain. The real build box is the installed system on
   **`/dev/sda1`** — mount it and `arch-chroot` in:
   ```
   mount /dev/sda1 /mnt/sys
   arch-chroot /mnt/sys /bin/bash -c "…"
   ```
   (This is the "disc-swap gotcha". You *could* detach the IDE optical drive to
   boot the disk directly, but the chroot path is what works reliably.)

2. **The source tree is at `/root/build`, not `/root/manifest-os`.**
   `/root/manifest-os` is only a stale copy of the mkarchiso *profile* — ignore
   it. `/root/build` is the full checkout (`src/`, `iso/`, `target/` …), but it
   has **no `.git`** and is synced manually, so it is usually stale.

3. **Sync the exact commit in by tarball** (there's no git remote in the VM and
   no shared folder). On the host:
   ```
   git archive --format=tar -o <scratch>/mos-src.tar HEAD
   ```
   Copy it in and extract *over* `/root/build` — this preserves `target/` so the
   Rust build stays incremental:
   ```
   VBoxManage guestcontrol … copyto --target-directory "/mnt/sys/root/" <host tar>
   arch-chroot /mnt/sys /bin/bash -c "cd /root/build && tar -xf /root/mos-src.tar"
   ```
   Verify a known-new string landed (e.g. `grep native_scaling src/manifest.rs`).

4. **Don't background jobs *inside* `arch-chroot`.** When the `arch-chroot`
   command returns it tears down mounts and kills its children, so a `nohup … &`
   launched inside it dies immediately. Instead run the build **synchronously**
   in a single `arch-chroot` invocation and background the whole *`guestcontrol`*
   call on the host side (the harness `run_in_background`), polling a logfile.

5. **`tar` extraction drops the exec bit on `iso/build.sh`.** Invoke it as
   `bash ./iso/build.sh`, not `./iso/build.sh` (which gives `Permission denied`,
   exit 126).

6. **The build itself**, from `/root/build`, as root, in the chroot:
   ```
   cargo build --release --features gui   # builds manifest + manifest-gui + manifest-center
   bash ./iso/build.sh                     # bakes binaries + examples + plugins, runs mkarchiso
   ```
   `build.sh` writes to `iso/out/`.

7. **The ISO filename is dated** `manifestos-YYYY.MM.DD-x86_64.iso` (UTC in the
   VM). If a build straddles midnight the name rolls over — look for *today's*
   file, not the one you saw at the start. A "stale binary" scare is almost
   always this, not a real bug.

8. **Trust the guard, not vibes.** `build.sh` unmounts leftover work mounts
   before `rm -rf` and then `cmp`s the ISO-embedded binary against the one it
   baked, aborting if they differ (the classic stale-squashfs trap). It prints
   `ISO written to:` **only after** that check passes, so that line + `EXIT=0`
   is the real success signal.

9. **`sync` before powering the VM off.** The build box is ext4; a hard
   power-off loses the last write — i.e. the ISO you just built.

## Shipping the ISO to the host

Pull it out with `guestcontrol … copyfrom` into `dist/` on the host (it's
~1.8 GB, so it takes a bit). `dist/` is also where `build.sh` looks for a
fallback prebuilt binary.
