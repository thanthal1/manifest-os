# marketplace/ — submission review tooling

Tooling to review a shared `manifest.json` **before** it runs on someone's
machine — the security gate an eventual Manifest OS marketplace needs. A
manifest installs with root privileges, so a submission is untrusted code until
proven otherwise.

See [`DESIGN.md`](DESIGN.md) for the full three-stage pipeline and threat model.
This README is how to *use* what's here.

## What's here

| File | What it is | State |
|---|---|---|
| [`scan.py`](scan.py) | Static security + sanity scanner (CLI). The source of truth. | ✅ works |
| [`web/index.html`](web/index.html) | Web UI — paste/drop a manifest, see findings + flagged sources; with server.py running it adds `manifest verify`, the real `--check-packages` scan, and a **Boot test in a VM** button with live log. | ✅ works |
| [`server.py`](server.py) | Backend for the web UI: server-side scan + verify, cache status, VM boot tests. `python marketplace/server.py` → :8770. | ✅ verified end-to-end |
| [`boot-test.sh`](boot-test.sh) | Review one submission: static scan, then (opt) a real VM install + boot. | ✅ verified end-to-end (niri-rice, 2026-07-05) |
| [`cache-proxy.py`](cache-proxy.py) | Package cache that runs on the (Windows) host — auto-started by boot-test.sh. | ✅ works |
| [`cache-setup.sh`](cache-setup.sh) | Alternative: pacoloco cache on an Arch cache host (62× warm speedup). | ✅ works |
| [`DESIGN.md`](DESIGN.md) | The full pipeline: static → boot test → sign-off, package cache, signing. | design |

## The scanner (`scan.py`)

Static analysis only — it never executes anything. It flags the ways a manifest
can do harm, ranked CRITICAL → INFO:

- **Code execution:** any `pre_install`/`post_install` hook; `curl | sh`,
  base64-decoded exec, reverse shells, `rm -rf /`.
- **Persistence / privilege:** writes to `sudoers`, `~/.ssh/authorized_keys`,
  `pacman.conf`, PAM, systemd units, cron, shell rc / `profile.d`; `users` with
  `sudo`/`wheel`/root/hardcoded passwords.
- **DNS / spoofing:** anything that can repoint name resolution — `/etc/hosts`
  redirects (flagged harder when they target update/mirror/keyserver/auth
  domains), `resolv.conf`, `systemd-resolved`, NetworkManager DNS, `nsswitch`,
  a bundled `dnsmasq`/`unbound`, or a hook running `resolvectl`/`nmcli … dns`.
- **Untrusted sources:** custom/third-party repos, AUR packages, plain-HTTP
  URLs, and **links to code/paste hosting (GitHub, gists, pastebin, …)** whose
  content can change after review.
- **Obfuscation:** embedded base64 / hex blobs.
- **Broken:** invalid JSON; with `--check-packages`, package names that resolve
  to nothing (typosquat risk) or only exist in the AUR.

```bash
python marketplace/scan.py submission.json              # human report
python marketplace/scan.py submission.json --json       # for the web UI / CI
python marketplace/scan.py submission.json --check-packages   # + AUR/typosquat (pacman, or the Arch web APIs)
cat submission.json | python marketplace/scan.py -      # from stdin
```

Exit code is non-zero if any finding is at/above `--fail-on` (default
`CRITICAL`), so CI can gate on it.

## The web UI (`web/index.html`)

Open it in a browser — no build, no server, no dependencies. Paste or drop a
`manifest.json` and it renders a verdict (BLOCK / MANUAL REVIEW / REVIEW / LOOKS
CLEAN), the findings colour-coded by severity, and an **"external sources to
review"** panel that lists every URL the installer would fetch, tagging the ones
pointing at user-controlled hosting (GitHub etc.). Click **"Load a risky
sample"** to see it flag a deliberately-malicious manifest.

It mirrors `scan.py`'s rules for an instant client-side look. The definitive
scan (including `--check-packages`) is `scan.py` on an Arch box; a deployed
marketplace would call `scan.py --json` from its backend and render the same
JSON.

## Reviewing a submission (`boot-test.sh`)

```bash
marketplace/boot-test.sh submission.json                 # static gate only
marketplace/boot-test.sh submission.json --boot -i dist/manifestos-*.iso   # + full VM boot test
```

Stage 1 (static scan) runs everywhere. Stage 2 (`--boot`) does a real
`manifest provision` install in a throwaway VirtualBox VM and needs the runner
host. Every package the VM downloads goes through a caching proxy
(`cache-proxy.py`, auto-started on the host; or set `PACOLOCO_URL` to a real
pacoloco — see cache-setup.sh), so only the first test downloads from the
mirrors and re-tests are served from `pkg-cache/`. To check *your own* manifest
end-to-end in one command, use `scripts/check-manifest.sh <file.json>`. The
behavioural capture (outbound connections, new listeners, fs diff, compositor
config errors on login) is the stage-2 work still to build.

> A clean static scan is **necessary but not sufficient** — the boot test is
> what proves a manifest both safe and working. Neither replaces a human
> sign-off for anything the scan flags above the auto-approve threshold.
