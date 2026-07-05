# server.py — implementation notes & TODO (WIP, UNVERIFIED)

`marketplace/server.py` is a **draft backend** to make the review UI live: boot a
VM, install a submission **using the package cache**, and keep the cache fresh.
It is **written but not run or verified** — do not trust it works yet. This file
is the pick-up-where-we-left-off list.

## What server.py already has (draft)

Runs on the **host** (Windows Python), serves `web/`, drives VirtualBox directly
in Python (no bash dependency), manages the cache in `CACHE_VM` (default
`manifest-build`) via guestcontrol. Endpoints:

- `POST /api/scan` — runs `scan.py - --json` on the posted manifest.
- `GET  /api/cache/status` — pacoloco up? cache size, package count, cache IP.
- `POST /api/cache/refresh` — `pacman -Sy` in the cache VM (refresh repo DBs).
- `POST /api/boot-test` → `{job}`; `GET /api/boot-test?job=` polls status+log;
  `POST /api/boot-test/stop` cancels. Creates a fresh UEFI VM on a VBox **NAT
  Network**, points its mirrorlist at the cache, runs `manifest provision`,
  streams the log, tears the VM down.

## TODO (do these next)

1. **Wire the web UI** (`web/index.html`) to the backend — none of this is done:
   - Feature-detect the backend on load (`GET /api/cache/status`); if it fails,
     stay the static-only tool and hide the boot-test/cache controls.
   - When backend present: route the scan through `POST /api/scan` (so it gets
     `--check-packages` / AUR-typosquat detection) instead of client-side only.
   - A **cache widget**: show status (running, size, N packages, cache URL) +
     a **"Refresh cache"** button (`POST /api/cache/refresh`).
   - A **"Boot test in VM"** button that appears after a scan and is enabled
     unless the verdict is BLOCK. On click: `POST /api/boot-test`, then poll
     `GET /api/boot-test?job=` on a timer, rendering the streamed `log` + `step`
     in a live panel, with a **Stop** button (`POST /api/boot-test/stop`).
2. **launch.json**: point the `marketplace-web` config at
   `python marketplace/server.py` (PORT 8770) instead of `http.server`, so the
   preview serves the full app.
3. **Verify** (checklist below).

## Gotchas to resolve during verification (the risky bits)

- **pacoloco persistence.** It's installed in the cache VM's *disk-backed* Arch
  at `/mnt` (chroot), NOT the live archiso env. `cache_ensure_running()` starts
  it from the **live env** as `/mnt/usr/bin/pacoloco -config /mnt/etc/pacoloco.yaml`
  with `setsid` so it survives the guestcontrol call. **Unverified** that the Go
  binary runs from the live env (glibc/paths ok?) and that `cache_dir`
  `/mnt/var/cache/pacoloco` is writable from there. NOTE: things started *inside*
  `arch-chroot` die when the call returns (PID namespace) — that's why we start
  it from the live env directly. Confirm it actually listens + persists.
- **NAT-network reachability (the crux of "test with cached packages").** The
  test VM must reach the cache VM's pacoloco. `ensure_natnet()` creates a VBox
  NAT Network `manifestnet` (10.0.2.0/24) and puts `CACHE_VM` on it **only when
  powered off** (a live NIC switch is riskier). `manifest-build` is currently on
  **plain NAT** (for internet + our guestcontrol), so today `cache_host_ip()`
  returns "" and boot_test **falls back to the normal mirrors (uncached but
  still works)**. To actually cache: put `manifest-build` on `manifestnet`, then
  verify (a) it keeps internet — a NAT Network provides it, should be fine;
  (b) guestcontrol still works (it's over the VBox channel, not network — should
  be unaffected); (c) a test VM on `manifestnet` can `curl
  http://<cache-ip>:9129/...`. This is the one integration I never proved.
- **bash-free by design.** server.py drives VBoxManage + scan.py directly, no
  bash shell-out, so it doesn't depend on Git Bash on the host. Keep it that way.
- **Orphan VMs.** boot_test's `finally` powers off + unregisters the `review-*`
  VM. But if the server is killed mid-job, `review-*` VMs linger — add a reaper
  that deletes stale `review-*` VMs on startup.
- **Host load / concurrency.** Each boot test is a 6 GB VM; running several at
  once overcommits the host (the RCU-stall / clean-poweroff issue from the VM
  memory). Add a job queue or max-1-concurrent guard before exposing it.
- **Full boot test is slow (~minutes, even warm).** The UI must poll/stream,
  never block. (Draft already does JOBS + polling.)
- **"Update the cache when needed"** = `cache_refresh()` runs `pacman -Sy` to
  refresh repo DBs so version resolution is current; package *files* self-update
  on a cache miss (new version → new filename → pacoloco fetches it; old files
  purged after 30 days per `pacoloco.yaml`). Consider adding a **pre-warm**
  action: install a reference desktop manifest once to populate the cache with
  the common package set so the first real boot test is fast.

## Verification checklist (fast → slow)

- [ ] `python marketplace/server.py` starts; `http://localhost:8770` serves the UI.
- [ ] `POST /api/scan` (a manifest) returns findings matching `scan.py`.
- [ ] Put `manifest-build` on NAT network `manifestnet`; confirm internet +
      guestcontrol still work; read its `10.0.2.x` IP.
- [ ] pacoloco starts from the live env, listens on 9129, and **persists**
      across separate guestcontrol calls.
- [ ] `GET /api/cache/status` → `running:true`, ip, size, count.
- [ ] `POST /api/cache/refresh` → `pacman -Sy` ok.
- [ ] Boot a throwaway VM on `manifestnet`; from it
      `curl http://<cache-ip>:9129/repo/archlinux/core/os/x86_64/core.db` → 200.
- [ ] `POST /api/boot-test` with `minimal.json`: job runs, log streams, mirrorlist
      rewritten to the cache, install shows cache hits
      (`grep 'serving cached file'`), completes exit 0, VM torn down.

## Status of the rest of the pipeline

Static scan (`scan.py`) + DNS detection + web UI (client-side) + `cache-setup.sh`
(pacoloco, 62× verified) are **done**. This server is the piece that joins the UI
to a live boot test + cache. See `DESIGN.md` for the whole picture.
