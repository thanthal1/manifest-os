# server.py — status

**Done and verified (2026-07-06).** The old plan on this page — pacoloco in the
`manifest-build` cache VM, a shared VBox NAT Network, wiring checklists — is
obsolete: the cache moved to `cache-proxy.py` on the host (see that file), test
VMs stay on plain NAT and reach it at `10.0.2.2` (`--nat-localhostreachable1
on`, set per-VM at creation). Everything below is done:

- `python marketplace/server.py` (or the `marketplace-web` launch config)
  serves the UI at http://localhost:8770 and reaps stale `review-*` VMs on
  startup.
- `POST /api/scan` runs `scan.py --json --check-packages` **and** `manifest
  verify` (host-built binary); the UI shows both.
- `GET /api/cache/status` reports the host cache (size, package count);
  `POST /api/cache/refresh` is a compat no-op (the proxy refetches repo DBs
  live with a 60s TTL).
- `POST /api/boot-test` installs the submission in a throwaway VM through the
  cache — with the mirrorlist pinned read-only (rank_mirrors() overwrites it
  otherwise), the submission sha256-verified in-guest (copyto can fail
  silently), provision logging to `/root/install.log` (the GUI live session
  owns `/tmp/manifest-install.log`), and a guest-side cache preflight that
  falls back to real mirrors. One job at a time (409 otherwise, verified);
  polling + Stop wired into the UI.
- Verified end-to-end from the browser: upload niri-rice.json → server scan +
  verify render → Boot test button → live log panel → install.

Still open (nice-to-haves from DESIGN.md): stage-2 behavioural capture
(outbound connections, listeners, fs diff, compositor errors on login) and
evidence attach in the reviewer console.
