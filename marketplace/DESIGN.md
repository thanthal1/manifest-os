# Marketplace review pipeline — design

A marketplace serves `manifest.json` files that, when installed, run with **root
privileges on a stranger's machine**. Trust is the whole product. This folder is
the review pipeline that gates submissions. It has three stages, cheapest first:

```
 submission ─▶  1. STATIC SCAN  ─▶  2. BOOT TEST  ─▶  3. HUMAN SIGN-OFF  ─▶ published
                (scan.py, ~1s)      (VM, minutes)      (only if 1/2 flag)
```

A submission that fails stage 1 never reaches stage 2. Most submissions that
pass 1 and 2 cleanly can be auto-approved; anything with HIGH+ findings is queued
for a human.

---

## Stage 1 — Static scan  *(built: `scan.py`, `web/index.html`)*

Pure static analysis of the JSON — no execution. Catches the compromise vectors
a manifest has: shell hooks, writes to `sudoers`/`authorized_keys`/`pacman.conf`,
`curl | sh`, base64 payloads, sudo/root users, untrusted repos, code-host URLs,
etc. Emits severity-ranked findings (`scan.py --json`) that the web UI renders
and CI gates on (`--fail-on`). **This is the fast filter and is done.**

What it *cannot* know: whether a package name actually resolves, whether a
dependency pulls something nasty, whether the config boots. That's stage 2.

## Stage 2 — Boot test  *(skeleton: `boot-test.sh`; infra below)*

The only real proof a manifest is safe **and** works: install it end-to-end in a
throwaway VM and boot the result, watching what it does.

```
 build InstallPlan  ─▶  manifest provision <json> --disk /dev/sda --no-reboot
                        (real pacstrap + paru + packages + config, in a fresh VM)
                    ─▶  reboot the installed disk
                    ─▶  observe: did it boot? DM up? compositor config errors?
                                 unexpected outbound network? unexpected listeners?
                    ─▶  tear the VM down; report pass/fail + evidence
```

This reuses the existing rig — `scripts/audit-vms.sh` already spins up throwaway
VBox VMs and runs `manifest provision` per manifest. The review pipeline extends
it with:

- **Behavioural capture** (the security half). During install + first boot, record:
  - **outbound connections** the manifest makes beyond the package mirrors
    (a hook phoning home, a config exfiltrating). A NAT with logging, or
    `nftables` logging on the host-only net, gives the connection list.
  - **new listeners / services** on the installed system (`ss -tlnp`, enabled
    units diffed against the recipe's expected set).
  - **compositor config errors** — boot the DM, log in, run the compositor's
    own validator (`hyprctl configerrors`, `niri validate`, `sway -C`). This is
    exactly the check `scripts/audit-examples.sh -c` already does; wire its
    result into the verdict. *(This is what would have caught the Hyprland
    config-banner regression before it shipped.)*
  - **filesystem diff** — what landed outside the paths the manifest declared.

- **A hard timeout + snapshot rollback** so a hostile manifest can't wedge the
  runner. VMs are disposable and never reused between submissions.

### The package cache  *(the "don't install every time" ask)*

Every boot test otherwise re-downloads the same ~2 GB of packages. Two options;
the pipeline should use the **caching proxy**, which is transparent and scales:

1. **Caching proxy (recommended).** Run [`pacoloco`](https://github.com/anatol/pacoloco)
   (or a plain caching reverse-proxy) on the runner host. Point each VM's
   `/etc/pacman.d/mirrorlist` at `http://<host>:9129/repo/...`. First download
   populates the cache; every later VM hits the local copy. Works for `pacstrap`,
   the chroot's `pacman -Syu`, and package installs alike — no per-VM setup, and
   it caches *all* repos including CachyOS. AUR builds still compile from source
   (that's inherent), but their *dependencies* come from cache.

2. **Shared cache dir.** Mount a persistent host directory as the VM's
   `/var/cache/pacman/pkg` (VBox shared folder, or a dedicated data disk reused
   across runs). Simpler, but only caches the final `pacman -S` step, not
   `pacstrap`, and needs the mount wired before the install starts.

A warm cache turns a ~15-minute cold boot test into a few minutes — enough to
make per-submission testing practical.

## Stage 3 — Human sign-off

Only reached when stage 1 or 2 raised anything above the auto-approve threshold.
The reviewer sees the scan findings, the boot-test evidence (screenshots, the
outbound-connection list, the fs diff) and approves or rejects. The web UI is
the reviewer's console; the boot-test evidence attaches to the same view.

---

## Security posture (why this is the hard part)

- **Never trust the submission.** The runner treats every manifest as hostile:
  disposable VM, network egress logged and rate-limited, hard timeout, no shared
  state between runs, the runner host never on the same trust domain as the VM.
- **Pin what's published.** A submission's `dotfiles.source` / wallpaper / any
  URL can change *after* review (stage-1 flags code-host URLs for exactly this
  reason). The marketplace should snapshot/mirror external resources at approval
  time and serve the pinned copy, not re-fetch the live URL at install time.
- **Signing.** Published manifests should be content-hashed and signed by the
  marketplace so the installer can verify integrity before applying.
- **Re-scan on change.** Any edit to a published manifest re-enters stage 1.

## Status

| Stage | State |
|---|---|
| 1. Static scan (`scan.py` + web UI) | ✅ built |
| 1. `--check-packages` (AUR/typosquat) | ✅ built (needs Arch + synced DB) |
| 2. Boot test harness | ⏳ `scripts/audit-vms.sh` exists; `boot-test.sh` is the review-wrapper skeleton |
| 2. Behavioural capture (net/listeners/fs-diff) | ❌ to build |
| 2. Package caching proxy | ❌ to deploy (pacoloco) |
| 3. Reviewer console + evidence attach | ⏳ web UI renders findings; boot evidence not yet wired |
| Pin/mirror + signing | ❌ to build |
