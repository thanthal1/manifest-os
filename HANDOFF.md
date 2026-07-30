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
  painted onto the desktop over FreeRDP — by default in a **kiosk session**
  (one connection, our stub as the session's shell, so no taskbar and no
  per-window X clients; §21), falling back to **WinApps** RemoteApp. Same lazy
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
5. **RDP's alternate shell (`/shell:`) was a dead end, and stopped being one —
   see §21.** It was banned for five releases because dockur signed the user in
   at the console: client editions are single-session, so an RDP connect *took
   over* that session instead of creating one, the alternate shell was ignored,
   and you got a plain Windows desktop that also looks "successful" to any
   duration check. **§20 removed the premise** — the console sits at a lock
   screen now, so our connect is what *creates* the session, and a created
   session honours its alternate shell. It is the default launch path as of
   §21. The failure mode is unchanged and still silent, so the ban was replaced
   by *proof*, not by trust.
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
10. **The same goes for `manifest windows-vm` itself.** `windows-vm-run` re-enters
    it to heal a half-finished setup, from that same TTY-less launcher. It now
    asks for root only when a package, service or group actually *changes* —
    `pacman -S --needed` is a no-op but still prompts, and that alone aborted
    the whole setup with "sudo: a terminal is required".
11. **Nothing here is checked by `sh -n` unless you make it.** The release loop
    pipes `manifest __script <name>` through `sh -n`, which only covers what
    `__script` exposes — the inline `ctx.shell()` fragments in `setup()` were
    checked by nothing, and one of them (`then : fi`, where `fi` parses as an
    argument to `:`) was a **parse error that aborted setup for every user with
    debloat on**, one step before `compose.yaml` is written. Every fragment is a
    pure function with an `sh -n` test now. Keep it that way.
12. **The guest's password cannot be changed from out here.** It is written into
    Windows while it installs, so regenerating one on a re-run doesn't rotate
    anything — it locks you out of the guest you have, and FreeRDP's only symptom
    is `ERRCONNECT_CONNECT_TRANSPORT_FAILED` / connection reset, which reads like
    a network fault. An existing compose's password is reused.
13. **`storage/` is `root:root`** — dockur creates it. A plain `rm -rf` from the
    user removes *nothing*, and with the error swallowed a "reinstall" boots the
    very same Windows 40 minutes later. Wipe it with the privilege that made it
    (`docker run -v …:/wipe`), and check the result before claiming anything.
14. **A guest without `RDPApps.reg` does not refuse the launch — it serves the
    console session.** Verified on real hardware, with a screenshot: you get a
    full Windows desktop carrying an *"Another user is signed in"* prompt
    (dockur is already signed in there), and it sits on screen indefinitely, so
    the 5-second duration check reads it as a **successful launch**. This is the
    alternate-shell trap (#5) arrived at from the other side. The attempt is
    therefore *gated* on `.remoteapp-enabled` — never attempt what the guest
    cannot do, or you get a false success, a stray desktop, and a launcher for
    an app nobody installed.
15. **dockur accepts a custom answer file** at `$STORAGE/custom.xml` (also
    `/custom.xml`, `/run/assets/custom.xml` — see its `run/answer.sh`). Its stock
    `win11x64.xml` autologons with `LogonCount 65432`.
16. **It takes THREE things at once, which is why no single fix ever showed
    progress.** This is the answer to the question that was open for five
    releases, and the tier works now — verified end to end, screenshotted:
    a borderless RemoteApp window tiled by Hyprland as a native client, with no
    Windows desktop anywhere.
    1. **`RDPApps.reg`** (`fDisabledAllowList=1`) — permission to run an
       arbitrary program as a RemoteApp. Necessary, and on its own it changes
       *nothing*; a guest with it still just serves the desktop.
    2. **Automatic sign-in off** (§20). Windows client editions are
       **single-session** and dockur autologons the same user at the console, so
       the request collides with the session that already exists: you get the
       desktop plus *"Another user is signed in"*, then `ERRINFO_LOGOFF_BY_USER`
       ~30 s later when nobody answers. Permission was never the whole story —
       a *free session* is.
    3. **Reach the file by `Z:`, not `\\tsclient\home`.** `\\tsclient` exists
       only if the **client** enables drive redirection, and we do not pass
       `+home-drive`, so that path doesn't resolve, the app never starts, and no
       window is created. `Z:` is dockur's `/shared` mount — always there.
       **`windows-vm-run` tries `Z:` first, and that order is load-bearing:**
       the failing `\\tsclient` attempt still holds the connection ~20 s, which
       the duration check reads as success, so the working path is never
       reached.
17. **Do not trust `windows.boot` as "installed".** dockur writes it from
    `markWindowsBooted`, called only out of `finish()` — i.e. on container
    **shutdown**. It is absent for the entire life of a guest that has never
    been stopped, so gating anything on it (the idle watchdog, say) disables
    that thing permanently.
18. **There is no cheap "is Windows installing?" signal — stop looking for one.**
    Three were tried for the idle watchdog and two shipped broken in opposite
    directions: the activity file alone (killed an install 16 min in),
    `windows.boot` (§17, always absent → watchdog disabled for good), and
    `nc -z 127.0.0.1 3389` (**always true** — docker publishes the port when the
    *container* starts, so it answers for the whole 40 min Windows installs).
    The answerable question is *"has anything **used** this VM since it
    started?"* — local, always knowable, and false during an install by
    construction. `should_stop()` is a shell function precisely so a test can
    drive it with fabricated clocks; both broken versions pass every structural
    assertion and fail that table.
19. **To know whether Windows is really serving RDP, make it prove it.** Send an
    X.224 Connection Request; a real RDP server replies with a Connection
    Confirm (`03 00 …`), docker's proxy accepts and returns nothing. That is the
    only dependable readiness check here — the image may report no healthcheck
    at all. `windows-vm-run`'s `rdp_ready()` does this.
20. **Windows' automatic sign-in has to be off** (§16). Appended to the
    `C:\OEM\install.bat` we already write — `AutoAdminLogon=0`, and delete
    `DefaultPassword` + `AutoLogonCount` (dockur sets `LogonCount` 65432). It
    runs as a **FirstLogonCommand**, i.e. inside the session it disables, so it
    takes effect on the guest's *next* boot: the first restart after
    installation is what frees the console. Cost: `http://localhost:8006` lands
    on a lock screen, so setup says why.
21. **The kiosk session is the default launch path; RemoteApp is the fallback.**
    RemoteApp is per-window by construction — RAIL surfaces every guest
    top-level as its own X client — so the *host* compositor ends up owning a
    set of windows it cannot relate to each other. That is the whole of the
    fragility: a file dialog or a sign-in browser arrives as a new client and
    takes a tile over whatever you were using, and `float_windows` is a 24-second
    poll racing window creation to undo it. It is also why `Chromium renders
    blank` and why Firefox-family doorhangers arrive as blank 1920x1091
    transients (§ the Waterfox note in the source) — those are FreeRDP RAIL
    bugs, reachable only through RAIL.

    The kiosk path connects **once**, with `/shell:` naming a PowerShell stub
    (`KIOSK_SHELL_PS1`) as the session's shell. No explorer means no taskbar, no
    Start menu, no desktop icons — nothing was started. The app runs maximized
    inside that session and every further window it opens is composited *by the
    guest*, so Linux sees exactly one client that never changes count, and
    `/dynamic-resolution` makes resizing it resize the guest.

    Things that are not obvious and cost something to rediscover:
    - **The stub proves it ran; nothing else can.** Windows ignores `/shell:`
      whenever the session already exists and serves an ordinary desktop, which
      passes a duration check, an exit status *and* FreeRDP's log identically —
      §5's trap, arrived at from the third side. The stub therefore touches
      `Z:\.manifest-shell-alive`; `kiosk_launch` deletes it, connects, and waits
      for it to come back. **Do not remove that check to simplify the function.**
    - **The heartbeat is periodic, not write-once.** A *reconnect* to a session
      the stub is already running has to prove itself too, and there is no
      second first-run to be had.
    - **A guest is only condemned on a live connection.** FreeRDP dying outright
      says nothing about the shell (the VM may still be booting), so only "still
      connected, no heartbeat" writes `.kiosk-unsupported`. Stamping on any
      failure permanently demotes a guest for having been slow once. The stamp
      is cleared on reinstall, and by `setup_oem_step` — it records what one
      *particular* Windows could not do.
    - **Several apps, one session.** Client editions still allow one interactive
      session, so a second launch must not open a second connection — it would
      take the session away from the first app. Instead it writes the path to
      `Z:\.manifest-launch`, which the stub is already polling, and raises the
      existing window. The stub *renames* the queue file before reading it, so
      the file disappearing is also how the host knows that session is alive and
      consuming; if it doesn't disappear, that session is gone however healthy
      its pid looks.
    - **Both launch paths block.** On a second launch the window belongs to
      another `windows-vm-run`, so `wait` is unavailable and it polls instead.
      Returning as soon as the app *started* would run the caller's "what did it
      install?" step against an installer still on its first page, and announce
      that the user's installer had closed while they were looking at it.
    - **Everything lives at the root of `Z:`.** The guest lists
      `Z:\Windows Transfer` stale — a file written from Linux a moment ago reads
      as "does not exist", which is why the app enumerator passes its script
      `-EncodedCommand`. Root-level files opened by exact path are the one shape
      of this shown to work here (the disk-grow script runs that way). A stub
      written to the subdirectory yields a session with **no shell at all**.
    - **A session whose app never started looks exactly like one whose app was
      quit** — it idles out in 30 s and closes cleanly either way. So the stub
      writes `Z:\.manifest-shell-error` when `Start-Process` throws, and
      `kiosk_launch` returns 1 on finding it. Without that, `windows-vm-run`
      reads the clean close as "the installer ran and was closed" and writes a
      launcher for an app the guest never managed to start.
    - **Escape hatch:** `MANIFEST_WINVM_MODE=remoteapp windows-vm-run …` forces
      the old path, for comparing the two without a rebuild.
    - **The stub is not fully syntax-checked.** There is no PowerShell on the dev
      host, same as `debloat.ps1` and `manifest-browser.ps1`. What exists is a
      *balance* smoke test over every generated PowerShell — depth returns to zero
      and never goes negative, for both `{}` and `()`. That is §11's `then : fi`
      in the other language, and it matters most here because this script is the
      session's shell: a parse error is a session with nothing in it. It is not a
      parser. It works because the only braces inside string literals are `-f`
      placeholders (`"{0}`t{1}"`), which balance; a literal unmatched brace in a
      string would need a real one.
22. **The dev box runs niri, and every window-management branch missed it.**
    `float_windows` tests `HYPRLAND_INSTANCE_SIGNATURE`, `SWAYSOCK`, `I3SOCK`,
    then `wmctrl` — niri sets none of the three, and `wmctrl` is X11-only and
    not installed — so it hit its `else break` on the *first* pass and did
    nothing at all. Silently: the loop just exits. Any RemoteApp launch made on
    this machine has therefore had **zero** window management, which is a large
    part of what "it breaks when windows pop in front" actually was here. Worth
    remembering before attributing the next layout problem to RAIL.

    **`float_windows` is now gone entirely, and should not come back.** Floating
    was the wrong call — it existed because Hyprland once stretched a small
    dialog to a full tile, but tiling these windows behaves better in practice.
    The loop was also its own bug: it re-issued float+focus for *every* matching
    window on *every* pass, so a session surfacing a dozen windows got well over
    a hundred focus dispatches across 24 s — from the user's side,
    indistinguishable from windows spawning and stealing focus by themselves.

    `niri_ids` survives for `kiosk_raise`, because **niri has no class
    selector**: unlike hyprctl, swaymsg and i3-msg, every action it exposes
    takes `--id`, so the window is looked up by App ID first (`niri msg
    windows`, parsed with awk — no `jq` on the box). Verified against a live
    window, not assumed. Note a kiosk session is an ordinary desktop client
    FreeRDP names `xfreerdp` / `FreeRDP:<host>` — never `RAIL:<hex>`.
22a. **No X server means no window, and it looks identical to a failed launch.**
    `xfreerdp3` is an **X11** client. On niri, XWayland is a separate service
    (`xwayland-satellite`) that ships **disabled**, and when it is not running an
    app launch produces no window at all — the user sees their desktop
    background and reports "it just refuses". Two traps: the stale
    `/tmp/.X11-unix/X0` socket **outlives** the server, so checking for the
    socket proves nothing (`pgrep -x Xwayland` is the real test), and the
    failure is completely silent because a launcher has no terminal.
    `manifest-freerdp` now checks, prefers `sdl-freerdp3` when there is no X,
    and `notify-send`s the fix rather than dying quietly.

    Unblock: `systemctl --user enable --now xwayland-satellite`.
22b. **The FreeRDP log used to truncate to EMPTY at 256 KB, and destroyed
    evidence mid-investigation.** A kiosk launch was being diagnosed; the log
    rolled; `grep -c 'shell:powershell'` then returned 0 and was briefly taken as
    proof the kiosk path had never run — when in fact the entry had simply been
    erased. It now keeps the most recent 256 KB of a 512 KB cap. **Any log a
    diagnosis depends on must never be emptied**, only trimmed.
22c. **The "window spam" on opening an app is RAIL surfacing the whole desktop,
    and no client-side rule can fix it.** RAIL surfaces every top-level window in
    the *session*, and on the RemoteApp path explorer.exe is running behind the
    RemoteApp — so a connect surfaces the desktop with it. Measured in
    `windows-vm-freerdp.log`: **31–111 `xf_rail_monitored_desktop` events and 7
    tray icons per connection.** Not running a shell in the session is the fix,
    which is what the kiosk path is.
22d. **A stamp from an abandoned experiment silently disabled the kiosk path for
    two days.** `.kiosk-unsupported` was found dated *2026-07-28 02:42*,
    alongside a 2791-byte `~/.manifest-shell.ps1` — an earlier, uncommitted
    attempt at the same idea. It predated the shipped kiosk code by two days and
    gated it out completely: `grep -c 'shell:powershell'` over the FreeRDP log
    returned **0**, so the feature had never once executed while appearing to be
    live.

    The stamp now carries a **reason** (`no-heartbeat`) and the gate greps for
    it, so an empty or foreign file counts as absent and the path is retried.
    Both directions are behaviour-tested. **General rule this re-teaches:** a
    marker file whose meaning is "existence" cannot distinguish *its own* writer
    from anyone else's, and the failure mode is silence — the feature simply
    never runs, and every test still passes.
23. **The console windows that flashed up after every launch were the app scan.**
    `enum_apps_step` runs its enumerator as a RemoteApp — `/app:program:cmd.exe,
    cmd:/C powershell …` — so RAIL surfaced a **cmd console and then a PowerShell
    console** as real windows, every time an app closed, plus whatever
    `manifest windows-vm --link` opened to do the same job worse.

    On the kiosk path there is no round trip at all now: the stub ends every
    session by writing `Z:\.manifest-apps.tsv` itself, from inside the session,
    already hidden — and that is exactly the right moment, because the app has
    closed so anything it installed is on disk. `windows-vm-run` then calls
    `manifest __winvm-apps --from-share`, which only turns that file into
    launchers. `--link` is skipped with it: it writes entries solely for apps in
    WinApps' own hardcoded catalog, which the scan supersedes.

    The RemoteApp fallback still uses the RDP one-shot, unchanged. The two
    remaining one-shots (`GUEST_POLICY_CMD`, disk-grow) still flash consoles but
    fire **once per guest** and once per size change — and they are on verified
    paths (§16.3), so leave them be unless there is a reason.
24. **Why launching two apps from Linux breaks, while an app opening another app
    does not.** Reported as "opening multiple apps breaks things sometimes, which
    is odd, because Inventor opening Waterfox to sign in is fine". Both halves are
    true and the difference is structural:
    - **Inventor → Waterfox** happens *inside the guest*. No new RDP connection —
      the already-connected client is simply told about a new window. One session,
      one connection, nothing to contend for.
    - **Two `windows-vm-run` launches** on the RemoteApp path are two RDP
      connections. Windows client editions allow **one** interactive session, so
      the second does not add a window: it *takes the session* from the first.

    The evidence is in `~/.local/state/windows-vm-freerdp.log`: 20 FreeRDP
    processes over three minutes with routinely overlapping lifetimes, nine
    ending `ERRINFO_LOGOFF_BY_USER` — the same code §16.2 identifies as session
    contention. A useful trap while reading it: a connection with no `rail` lines
    is **not** a desktop session, it is a RAIL launch that died before window
    negotiation. Classifying on that mistakenly makes it look like something else
    is opening plain desktops.

    **And one collision became a cascade, through our own code.** `run_wa`
    retried under `sg docker` on *any* non-zero exit. `winapps manual` blocks on
    `wait $FREERDP_PID` (§7), so a launch that Windows logged off exits non-zero
    too — and the retry then opened a *second* connection while the first was
    still dying. That is the pair of connections **261 ms apart** in the log:
    far too fast to be a person clicking twice. The retry is now gated on the
    failure actually looking like the docker-group problem it exists for, and
    both branches are behaviour-tested, not just asserted on.

    The RemoteApp path also takes a **single-flight claim** (`.rail.pid`) for the
    length of a launch, so a second one refuses with an explanation instead of
    stealing the session. Released before the "what got installed?" step, which
    can run for a while; `rail_busy` checks the pid is alive rather than that the
    file exists, so a crash cannot wedge it.

    **The kiosk session has none of this by construction** — a second launch goes
    through the queue the resident stub is already polling, which is precisely
    what Inventor-starting-Waterfox does. Generalising that is the fix; the
    single-flight claim is damage control for the fallback.
25. **Debloat covers more than apps now.** Added: the third-party preinstall
    stubs (Spotify/Netflix/TikTok/… — matched by *wildcard*, since their package
    names are vendor-specific and region-dependent, so the exact-name list cannot
    catch them), OneDrive, Paint 3D / 3D Viewer / Copilot / Mail+Calendar,
    notification and suggestion toasts, idle services, telemetry and defrag
    scheduled tasks, hibernation, System Restore, and **visual effects**.

    Visual effects are the biggest single win and the least obvious: animations,
    shadows and transparency are pixels that have to cross RDP, so they cost
    latency on every frame rather than GPU the guest hasn't got.

    Kept, deliberately, each pinned by a unit test — **Defender** (`Z:` is the
    user's real `$HOME` mounted in, and the guest runs arbitrary downloaded
    installers, so it is not a sandbox and its AV is not redundant), **Print
    Spooler** (apps enumerate printers at startup and some hang without it;
    "Print to PDF" is useful when the host is Linux), **WebView2/Edge**
    (installers embed it to draw their own UI), **Windows Update**, and the
    **pagefile** (safe to drop only with RAM to spare, and it fails as an app
    dying mid-install with nothing that says why).

    **`manifest windows-vm --debloat` applies it to a guest already installed.**
    Setup-time debloat is a FirstLogonCommand, and §3's rule bites: no re-running
    setup, so a guest built before the list grew keeps the old one forever. This
    is the way out that isn't a 40-minute reinstall. Explicit, not automatic —
    removing app packages takes minutes, and prepending that to someone's app
    launch is worse than asking; `windows-vm-run` prints a one-line hint instead.

    The privilege split is the whole difficulty, and it is why the runtime script
    is not just the setup one re-run:
    - **An RDP logon can get a filtered token.** Nothing in the guest disables
      UAC (checked — WinApps' own `install.bat` self-elevates with `fltmc` +
      `RunAs`, which is the tell). So the machine-scope half — services,
      scheduled tasks, HKLM policy, provisioned packages, hibernation, System
      Restore — may be refused. With `$ErrorActionPreference = 'SilentlyContinue'`
      over the whole script, refused means **silent**. Same class of bug as §5.
    - **It does not self-elevate**, though WinApps does. An elevated process gets
      a different logon session, and **mapped drives are per-session**, so the
      elevated copy would see no `Z:` — it could neither read the script nor
      write its result back. A UAC consent dialog arriving as a RAIL window is
      also the shape that paints blank.
    - **So it reports instead.** `$admin` is checked, not assumed; the
      machine-scope half is *guarded* by it; `$skipped` names what was refused;
      and the result goes back through the share, the direction that works.
      `.debloat-applied` is stamped **only** on a clean run — stamping a partial
      one would hide the single thing the user needs to know and withdraw the
      offer.
    - **The unelevated half is most of what is felt**: visual effects,
      transparency, animations, notification toasts and per-user app removal are
      all user-scope. A partial run is genuinely useful, which is why it happens
      rather than being refused outright.
    - **The guest turned out NOT to be token-filtered.** First real run on the
      KVM box reported `ok` — i.e. `$skipped` was empty, so `$admin` was true and
      the machine-scope half (services, tasks, HKLM policy, provisioned packages,
      hibernation, System Restore) really did apply over RDP. Good news, and it
      answers a question that was guesswork. The guard stays anyway: it costs
      nothing, it depends on how dockur happens to set the account up, and the
      failure it protects against is silent.
    - **CRLF ate the first run's result.** `Set-Content` writes `ok\r\n`, and
      `$(cat …)` strips the trailing newline but **not** the carriage return — so
      the value was `ok\r`, every arm of the `case` missed, and a completely
      successful debloat was reported as an unrecognised result and left
      unstamped. Read it with `tr -d '\r\n'`. This is the CRLF trap from the
      other direction: elsewhere in this tier the danger is *forgetting* CRLF for
      a batch file Windows reads, and here it is forgetting that anything Windows
      *writes* carries it.
    - **It starts the VM first.** The guest is stopped after `idle_minutes`, so
      on any normal day this command finds it down; without a start-and-wait every
      run would fail as *"the debloat did not report back"*, which reads like the
      script is broken. `rdp_ready` (§19) is now shared between `windows-vm-run`
      and this rather than duplicated. It also touches the activity file, or the
      idle watchdog can stop the VM underneath a debloat that takes minutes.
    - **One body, two callers.** `DEBLOAT_BODY` is shared by `debloat_ps1` and
      `debloat_runtime_ps1` so the lists cannot drift; a test asserts both carry
      it, and that the machine-scope calls sit inside an `$admin` guard rather
      than merely after the check.

    Two traps worth naming. The `Get-AppxPackage *x* | Remove-AppxPackage` form
    every debloat gist uses removes a package **for the calling user only** and
    leaves the provisioned copy, so the next profile Windows creates gets all of
    it back — both removals need `-AllUsers`, and the provisioned one is separate.
    And the exclusion *comments* trip any test that greps for a service name, so
    those assertions match the quoted list-entry form instead; the comments are
    the part worth keeping, because without them the next person re-adds them.

    **Not yet verified on real hardware** — see the open-issues list.

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
| **Windows VM tier** (dockur + WinApps RemoteApp) | ✅ **single-window launch works** — needs all three of §16 | real HW (KVM): setup → install → borderless RemoteApp window, screenshotted |
| **Windows VM tier — kiosk session** (`/shell:`, §21, now the default) | ⚠️ built + unit-tested, **not yet run on real HW** | unit only — see the open-issues list for what to check |
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
7. Prove it from the outside, not from `packaging/out/`: `curl` the package **and**
   its `.sig` from the release URL and `gpg --verify` both, then look inside the
   downloaded package for whatever the release was for. Two minutes, and it is the
   only check that covers the upload itself.

> **VBoxManage `copyfrom` in a bash loop:** write the host destination with
> forward slashes (`C:/…/out/$f`). In a double-quoted string `"…out\\$f"` collapses
> to `out\$f` — an *escaped* dollar — so every file silently lands in one file
> literally named `out$f`, with exit 0 and a 100% progress bar each time.

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
- ~~**Opening a `.exe` left a window that never closed**~~ — **fixed** (0.1.0-69),
  reported on niri. Two independent causes, both in `android::gui_install_script`:
  1. the progress dialog was fed by `tool | zenity --progress`, so it lived until
     the **write end of that pipe** closed — which every child the tool leaves
     behind inherits. Wine daemonises `wineserver`, so "Opening x.exe..." stayed
     up after the install finished. Now the tool logs to a file and the dialog is
     driven by a loop watching the tool's **PID**, ending with `echo 100` (under
     `--pulsate`, `--auto-close` has no other way to know it's done).
  2. `windows-install` draws its **own** dialogs and then hands over to the
     program's real installer window — there is no moment a wrapper can call
     "finished". `.exe`/`.msi` now set `MOS_OWN_UI=1` and skip the progress
     dialog entirely.
  Also: the terminal fallback closed only on `press Enter`, even after a
  successful install — it now closes itself on success and waits only on failure.
  **General rule:** never tie a dialog's lifetime to a pipe a tool's children can
  inherit, and never wrap a tool that has its own UI.
- ~~**Every pacman run failed on a fresh install**~~ — **fixed** (0.1.0-69).
  `call to execv failed (No such file or directory)` after
  `Snapshotting package versions (Manifest OS)...`, on a brand-new install.
  The chroot install writes `/etc/pacman.d/hooks/96-manifest-versions.hook` while
  the only `manifest` is `/usr/local/bin/manifest`, so that path gets baked in —
  and `configure_updates` deletes that binary minutes later once the packages
  install. It removed the *export* hook alongside it but not this one.
  (`repair_hook` didn't catch it either: it runs from the package's integration
  hook *during* that same transaction, when the binary is still there.) Now the
  package owns `96-manifest-versions.hook` too (`/usr/share/libalpm/hooks`,
  pointing at `/usr/bin/manifest`), the installer removes the runtime override,
  and `repair_hook` drops any leftover override once the packaged copy exists.
  **Rule this re-teaches:** a hook that has to name an absolute path belongs in
  the *package*, not in something written at setup time.
- ~~**Windows VM tier — does a RemoteApp paint?**~~ — **done.** It needed all
  three of §16 together (registry permission, automatic sign-in off, and the
  `Z:` share tried first), which is why five releases of registry-only work
  showed nothing. Verified end to end on this KVM box and screenshotted.
  Remaining polish, none of it blocking:
  - **The duration heuristic is gone from the default path** — the kiosk session
    (§21) is judged by the guest's own heartbeat file, which is a real signal
    rather than a stopwatch. It still governs the **RemoteApp fallback**, where
    it remains wrong: it reports success for a launch that painted nothing. The
    reliable signal there is the X11 window class — a real RemoteApp is
    `RAIL:<hex>`, a desktop session is `xfreerdp` / `FreeRDP: <host>` — and
    FreeRDP's log cannot tell them apart (`Invalid appWindow` and
    `xf_rail_monitored_desktop` appear identically in both). Costs an
    `xdotool`-class dependency, hence still deferred.
  - ~~**RemoteApp windows land tiled.**~~ — addressed by §21 rather than fixed:
    the kiosk session presents one client, so there is nothing per-window for a
    compositor to tile. `float_windows` stays for the RemoteApp fallback, where
    the underlying problem is unchanged.
  - `--link` app detection is untested since the cert-pin fix.
- **The kiosk session (§21) has not run on real hardware.** Unit tests cover the
  ordering, the proof-before-success rule, the stamping guard and that both
  paths block; none of that can tell you whether Windows honours `/shell:` on
  *this* guest. What to check on the KVM box, in order: a launch produces one
  window with **no taskbar**; `Z:\.manifest-shell-alive` appears within ~45 s
  (if it never does, `.kiosk-unsupported` gets stamped and you silently fall
  back to RemoteApp — check for that file before concluding anything); a file
  dialog opens *inside* the window rather than as a second client; a second app
  launched while the first is open lands in the same window; and closing the
  window ends the session rather than leaving one to time out 30 s later.
  Compare against `MANIFEST_WINVM_MODE=remoteapp`.
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
