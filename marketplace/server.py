#!/usr/bin/env python
"""server.py — backend for the marketplace review console.

Turns the static review web UI into a live one: it serves the page, runs the
scanner server-side (with `--check-packages`), boots a throwaway VM to actually
install a submission **using the package cache**, and keeps that cache fresh.

    python marketplace/server.py            # http://localhost:8770

Environment (all optional — sensible defaults):
    VBOX        path to VBoxManage           (default: the standard Windows path)
    ISO         install ISO for boot tests   (default: newest dist/manifestos-*.iso)
    CACHE_PORT  port of the host package-cache proxy    (default: 9129)

The package cache is marketplace/cache-proxy.py running on THIS host —
auto-started on demand. Test VMs stay on plain NAT and reach it at 10.0.2.2
(needs `--nat-localhostreachable1 on`, set at VM creation; VBox ≥6.1.28
refuses guest→host-loopback traffic without it). The VM-side plumbing mirrors
marketplace/boot-test.sh, which is verified end-to-end. Endpoints:

    POST /api/scan            body = manifest JSON  -> scanner findings (JSON)
    GET  /api/cache/status    -> proxy up? cache size, package count, cache URL
    POST /api/cache/refresh   -> no-op for compat (proxy refetches DBs live)
    POST /api/boot-test       body = manifest JSON  -> {job} (starts a VM install)
    GET  /api/boot-test?job=  -> {status, exit, log}   (poll for progress)
    POST /api/boot-test/stop  body = {job}            -> cancel + tear the VM down
"""
import base64
import hashlib
import http.server
import json
import os
import re
import shutil
import socketserver
import subprocess
import sys
import threading
import time
import urllib.request
import uuid
from urllib.parse import parse_qs, urlparse

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
WEB = os.path.join(HERE, "web")

VBOX = os.environ.get("VBOX", r"C:\Program Files\Oracle\VirtualBox\VBoxManage.exe")
PORT_CACHE = int(os.environ.get("CACHE_PORT", "9129"))
PY = shutil.which("python") or "python"


def _iso():
    if os.environ.get("ISO"):
        return os.environ["ISO"]
    d = os.path.join(REPO, "dist")
    isos = sorted((f for f in os.listdir(d) if re.match(r"manifestos-.*\.iso$", f)),
                  reverse=True) if os.path.isdir(d) else []
    return os.path.join(d, isos[0]) if isos else ""


def vbox(*args, timeout=120):
    return subprocess.run([VBOX, *args], capture_output=True, text=True, timeout=timeout)


def guest(vm, script, timeout=180):
    """Run a bash line in the live env of `vm` (root, empty password)."""
    return subprocess.run(
        [VBOX, "guestcontrol", vm, "run", "--username", "root", "--password", "",
         "--exe", "/usr/bin/bash", "--", "-lc", script],
        capture_output=True, text=True, timeout=timeout)


def guest_up(vm):
    try:
        return "READY" in guest(vm, "echo READY", timeout=20).stdout
    except Exception:
        return False


# ---------------------------------------------------------------------------
# Package cache: cache-proxy.py on THIS host (see that file for the design).
# VMs reach it at http://10.0.2.2:PORT_CACHE over plain NAT.
# ---------------------------------------------------------------------------
CACHE_DIR = os.path.join(HERE, "pkg-cache")
CACHE_URL_GUEST = f"http://10.0.2.2:{PORT_CACHE}/repo/archlinux"


def _cache_ping():
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{PORT_CACHE}/ping", timeout=3) as r:
            return r.status == 200
    except OSError:
        return False


def cache_ensure_running():
    if _cache_ping():
        return True, "already running"
    logf = open(os.path.join(HERE, "cache-proxy.log"), "ab")
    kwargs = ({"creationflags": subprocess.DETACHED_PROCESS | subprocess.CREATE_NEW_PROCESS_GROUP}
              if os.name == "nt" else {"start_new_session": True})
    subprocess.Popen([sys.executable, os.path.join(HERE, "cache-proxy.py"),
                      "--port", str(PORT_CACHE)],
                     stdout=logf, stderr=logf, stdin=subprocess.DEVNULL, **kwargs)
    time.sleep(2)
    return (_cache_ping(), "started") if _cache_ping() else (False, "failed to start")


def cache_status():
    up, msg = cache_ensure_running()
    size = n = 0
    for root, _, files in os.walk(CACHE_DIR):
        for f in files:
            try:
                size += os.path.getsize(os.path.join(root, f))
            except OSError:
                continue
            if ".pkg.tar." in f:
                n += 1
    return {"running": up, "message": msg, "host": "this machine (cache-proxy.py)",
            "port": PORT_CACHE, "cache_size": f"{size / 1e9:.1f} GB" if size >= 1e8 else f"{size / 1e6:.0f} MB",
            "cached_packages": n, "url": f"{CACHE_URL_GUEST}/$repo/os/$arch"}


def cache_refresh():
    """Kept for UI compat: the proxy refetches repo databases live (60s TTL),
    so there is nothing to refresh by hand."""
    return {"ok": True, "message": "repo databases are fetched live by the proxy — always current",
            **cache_status()}


# ---------------------------------------------------------------------------
# Boot test — install a submission in a throwaway VM, using the cache.
# ---------------------------------------------------------------------------
JOBS = {}  # id -> {status, log[], exit, vm, thread}


def reap_stale_vms():
    """Delete review-* VMs left behind by a killed server (they're throwaway)."""
    out = vbox("list", "vms").stdout or ""
    for name in re.findall(r'"(review-[0-9a-f]+)"', out):
        vbox("controlvm", name, "poweroff")
        time.sleep(1)
        vbox("unregistervm", name, "--delete")
        print(f"  reaped stale VM {name}")
    # Loose review-* files, but NOT ones a still-registered VM uses — a kept VM
    # (renamed to kept-*) keeps its original review-*.vdi, which must survive.
    inuse = (vbox("list", "hdds").stdout or "").lower()
    for f in os.listdir(HERE):
        if re.match(r"\.?review-[0-9a-f]+\.(json|vdi)$", f) and f.lower() not in inuse:
            try:
                os.remove(os.path.join(HERE, f))
            except OSError:
                pass


def _kept_name(manifest_text, jid):
    """A friendly, reaper-safe VM name derived from the manifest's meta.name."""
    try:
        name = (json.loads(manifest_text).get("meta") or {}).get("name") or "manifest"
    except Exception:
        name = "manifest"
    slug = re.sub(r"[^a-z0-9]+", "-", name.lower()).strip("-")[:32] or "manifest"
    return f"kept-{slug}-{jid[:6]}"


def boot_test(job_id, manifest_text):
    j = JOBS[job_id]
    head, inst = [], []          # harness lines / cleaned install-log lines
    def rebuild(): j["log"] = (head + inst)[-500:]
    def log(m): head.append(m); rebuild()
    vm = f"review-{job_id[:8]}"
    j["vm"] = vm
    iso = _iso()
    try:
        if not iso or not os.path.exists(iso):
            log("ERROR: no install ISO found in dist/"); j["result"] = "failed"; j["exit"] = 2; return
        up, msg = cache_ensure_running()
        log(f"[cache] proxy {msg} on :{PORT_CACHE}" if up
            else f"[cache] WARNING: proxy {msg} — install will use real mirrors")
        mfile = os.path.join(HERE, f".{vm}.json")
        # newline="": Windows text mode would write \r\n, and the sha256
        # arrival check below compares against the *original* bytes
        with open(mfile, "w", encoding="utf-8", newline="") as fh:
            fh.write(manifest_text)

        log(f"[vm] creating {vm} (fresh UEFI, NAT)")
        vdi = os.path.join(HERE, f"{vm}.vdi")
        vbox("createvm", "--name", vm, "--ostype", "ArchLinux_64", "--register")
        # --nat-localhostreachable1: VBox >=6.1.28 NAT refuses guest traffic to
        # 10.0.2.2 (host loopback) by default — without it the cache is
        # unreachable from the VM (instant "connection refused").
        # --accelerate3d on + 128MB VRAM: Wayland compositors (Hyprland/niri/
        # sway) use wlroots' GLES renderer, which has NO software fallback — with
        # 3D off they crash on login and bounce back to the greeter. Needed for a
        # kept VM's desktop to actually render.
        vbox("modifyvm", vm, "--memory", "6144", "--cpus", "4", "--firmware", "efi",
             "--nic1", "nat", "--nat-localhostreachable1", "on",
             "--graphicscontroller", "vmsvga", "--accelerate3d", "on", "--vram", "128",
             "--boot1", "dvd", "--boot2", "disk")
        vbox("createmedium", "disk", "--filename", vdi, "--size", "25000")
        vbox("storagectl", vm, "--name", "SATA", "--add", "sata", "--controller", "IntelAhci")
        vbox("storageattach", vm, "--storagectl", "SATA", "--port", "0", "--device", "0", "--type", "hdd", "--medium", vdi)
        vbox("storageattach", vm, "--storagectl", "SATA", "--port", "1", "--device", "0", "--type", "dvddrive", "--medium", iso)
        vbox("startvm", vm, "--type", "headless")

        log("[vm] waiting for the live environment…")
        t0 = time.time()
        while not guest_up(vm):
            if j.get("cancel") or time.time() - t0 > 360:
                log("ERROR: live env never came up" if not j.get("cancel") else "cancelled")
                j["status"] = "failed"; j["exit"] = 1; return
            time.sleep(8)

        if up:
            # Pin the mirrorlist to the cache with a READ-ONLY BIND MOUNT: the
            # installer's rank_mirrors() overwrites /etc/pacman.d/mirrorlist
            # early in every provision, which would silently bypass the cache.
            # The RO mount makes that overwrite fail harmlessly (rank_mirrors
            # is best-effort), and pacstrap copies the pinned mirrorlist into
            # the target so the chrooted installs are cached too.
            pin = guest(vm, f"echo 'Server = {CACHE_URL_GUEST}/$repo/os/$arch' > /root/mirrorlist.cache"
                            " && cp /root/mirrorlist.cache /etc/pacman.d/mirrorlist"
                            " && mount --bind /root/mirrorlist.cache /etc/pacman.d/mirrorlist"
                            " && mount -o remount,ro,bind /etc/pacman.d/mirrorlist && echo PINNED").stdout
            if "PINNED" in pin:
                log("[vm] mirrorlist pinned to the package cache")
            # Preflight from inside the guest; fall back to real mirrors now
            # rather than dying 10 minutes into pacstrap.
            if "ok" not in guest(vm, f"curl -sf -m 5 http://10.0.2.2:{PORT_CACHE}/ping").stdout:
                log("[vm] WARNING: cache unreachable from the guest — using real mirrors")
                guest(vm, "umount /etc/pacman.d/mirrorlist 2>/dev/null;"
                          " printf 'Server = https://geo.mirror.pkgbuild.com/$repo/os/$arch\\n'"
                          " > /etc/pacman.d/mirrorlist")

        # Get the submission in and PROVE it arrived (a silent copy failure
        # makes provision mis-resolve the path as a catalog name): copyto,
        # base64 fallback over guestcontrol, then sha256 compare.
        subprocess.run([VBOX, "guestcontrol", vm, "copyto", mfile, "/root/submission.json",
                        "--username", "root", "--password", ""], capture_output=True, text=True)
        want = hashlib.sha256(manifest_text.encode("utf-8")).hexdigest()
        got = guest(vm, "sha256sum /root/submission.json 2>/dev/null | cut -d' ' -f1").stdout.strip()
        if got != want:
            log(f"[vm] copyto failed (sha {got[:12] or '(no file)'} != {want[:12]}) — pushing base64 through guestcontrol")
            b64 = base64.b64encode(manifest_text.encode("utf-8")).decode()
            guest(vm, ": > /root/submission.b64")
            # small chunks: guestcontrol rejects args of a few KB and up with
            # VERR_NOT_SUPPORTED (verified — 5.5 KB already fails)
            for i in range(0, len(b64), 2000):
                r = guest(vm, f"printf %s '{b64[i:i + 2000]}' >> /root/submission.b64")
                if r.returncode != 0:
                    log(f"ERROR: base64 push failed at offset {i}: {r.stderr.strip()[:150]}")
                    j["result"] = "failed"; j["exit"] = 1; return
            guest(vm, "base64 -d /root/submission.b64 > /root/submission.json && rm -f /root/submission.b64")
            got = guest(vm, "sha256sum /root/submission.json 2>/dev/null | cut -d' ' -f1").stdout.strip()
        if got != want:
            log("ERROR: submission never arrived intact in the VM")
            j["result"] = "failed"; j["exit"] = 1; return
        log("[vm] submission copied in (sha256 verified)")

        log("[install] manifest provision (this is the real thing — minutes)…")
        guest(vm, "rm -f /tmp/prov.exit; setsid bash -c 'manifest provision /root/submission.json "
                  "--disk /dev/sda --user reviewer --password review1234 --no-reboot "
                  ">/root/install.log 2>&1; echo $? >/tmp/prov.exit' </dev/null >/dev/null 2>&1 & echo launched")
        while True:
            if j.get("cancel"): log("cancelled"); j["result"] = "cancelled"; j["exit"] = 1; return
            raw = guest(vm, "cat /tmp/prov.exit 2>/dev/null").stdout.strip()
            # Faithful stream: read the whole (small) log, normalise \r-redraws,
            # and drop curl's progress-meter noise (the "0  0  0 …" lines — curl
            # in the resilient XferCommand isn't silent). Rebuild rather than
            # append, so the panel always matches the real /root/install.log.
            raw_log = guest(vm, "cat /root/install.log 2>/dev/null").stdout
            cleaned = []
            for ln in raw_log.replace("\r", "\n").split("\n"):
                s = ln.rstrip()
                if re.match(r"^\s*\d[\d.%]*\s+\d", s) or "% Total" in s or "Dload" in s:
                    continue                       # curl progress meter — skip
                if not s and cleaned and not cleaned[-1]:
                    continue                       # collapse blank runs
                cleaned.append(s)
            inst[:] = cleaned; rebuild()
            step = guest(vm, "grep -aE '^\\[' /root/install.log 2>/dev/null | tail -1").stdout.strip()
            if step: j["step"] = step
            if raw.isdigit():
                j["exit"] = int(raw)
                j["result"] = "passed" if raw == "0" else "failed"
                log("[install] OK — completed." if raw == "0" else f"[install] FAILED (exit {raw}).")
                return
            time.sleep(12)
    except Exception as e:
        log(f"ERROR: {e}"); j["result"] = "failed"; j["exit"] = j.get("exit") or 1
    finally:
        result = j.get("result") or "failed"
        keep = bool(j.get("keep")) and not j.get("cancel") and result in ("passed", "failed")
        try: os.remove(os.path.join(HERE, f".{vm}.json"))
        except OSError: pass
        if keep:
            j["step"] = "saving the VM…"
            try: guest(vm, "sync", timeout=30)   # flush ext4 before power-off
            except Exception: pass
            if result == "passed":
                # drop the install ISO so it boots into the installed system
                vbox("storageattach", vm, "--storagectl", "SATA", "--port", "1",
                     "--device", "0", "--type", "dvddrive", "--medium", "none")
            vbox("controlvm", vm, "poweroff", timeout=30); time.sleep(2)
            newname = _kept_name(manifest_text, job_id)
            vbox("modifyvm", vm, "--name", newname)   # out of the review-* reap namespace
            j["kept_vm"] = newname
            log(f"[vm] KEPT as '{newname}' (powered off).")
            if result == "passed":
                # provision renames the manifest's primary user to the install
                # account (--user reviewer) and moves the rice onto it, so THAT
                # is the only login — not the manifest's original user.
                log("      Open VirtualBox → start it → log in as  reviewer / review1234  (has the desktop).")
            else:
                log("      Install FAILED — kept in the live environment; read /root/install.log inside it.")
        else:
            if result in ("passed", "failed"):
                log("[vm] tearing down.")
            try:
                vbox("controlvm", vm, "poweroff", timeout=30)
                time.sleep(1)
                vbox("unregistervm", vm, "--delete", timeout=60)
            except Exception:
                pass
        # publish the terminal status last, so a poll never sees "passed"
        # before the kept-VM rename/kept_vm field is in place
        j["step"] = ""
        j["status"] = result


def _manifest_verify(manifest_text):
    """Run the real schema validator (`manifest verify`) if a host build of the
    binary exists. Returns {ran, ok, output}."""
    exe = next((p for p in (os.path.join(REPO, "target", "release", "manifest.exe"),
                            os.path.join(REPO, "target", "debug", "manifest.exe"),
                            os.path.join(REPO, "target", "release", "manifest"))
                if os.path.isfile(p)), None)
    if not exe:
        return {"ran": False, "ok": None, "output": "no built manifest binary (cargo build --release)"}
    tmp = os.path.join(HERE, f".verify-{uuid.uuid4().hex[:8]}.json")
    try:
        open(tmp, "w", encoding="utf-8").write(manifest_text)
        p = subprocess.run([exe, "verify", tmp], capture_output=True, text=True,
                           encoding="utf-8", errors="replace", timeout=30)
        return {"ran": True, "ok": p.returncode == 0,
                "output": (p.stdout + p.stderr).strip()}
    except Exception as e:
        return {"ran": True, "ok": False, "output": str(e)}
    finally:
        try:
            os.remove(tmp)
        except OSError:
            pass


# ---------------------------------------------------------------------------
# HTTP
# ---------------------------------------------------------------------------
class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **k):
        super().__init__(*a, directory=WEB, **k)

    def _json(self, obj, code=200):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _body(self):
        n = int(self.headers.get("Content-Length", 0))
        return self.rfile.read(n).decode("utf-8") if n else ""

    def do_GET(self):
        if self.path.startswith("/api/cache/status"):
            return self._json(cache_status())
        if self.path.startswith("/api/boot-test"):
            jid = self.path.split("job=")[-1] if "job=" in self.path else ""
            j = JOBS.get(jid)
            if not j:
                return self._json({"error": "no such job"}, 404)
            return self._json({"status": j["status"], "exit": j.get("exit"),
                               "step": j.get("step", ""), "log": j.get("log", []),
                               "vm": j.get("vm"), "kept_vm": j.get("kept_vm")})
        return super().do_GET()

    def do_POST(self):
        body = self._body()
        if self.path.startswith("/api/scan"):
            p = subprocess.run([PY, os.path.join(HERE, "scan.py"), "-", "--json", "--check-packages"],
                               input=body, capture_output=True, text=True,
                               encoding="utf-8", errors="replace")
            try:
                out = json.loads(p.stdout)
            except Exception:
                return self._json({"error": p.stderr or "scan failed", "findings": []}, 500)
            out["verify"] = _manifest_verify(body)
            return self._json(out)
        if self.path.startswith("/api/cache/refresh"):
            return self._json(cache_refresh())
        if self.path.startswith("/api/boot-test/stop"):
            jid = json.loads(body or "{}").get("job", "")
            if jid in JOBS:
                JOBS[jid]["cancel"] = True
            return self._json({"ok": True})
        if self.path.startswith("/api/boot-test"):
            try:
                json.loads(body)  # validate it's JSON
            except Exception as e:
                return self._json({"error": f"invalid JSON: {e}"}, 400)
            # one at a time: each test is a 6 GB VM; two at once overcommits
            # the host (RCU stalls, blown provisioning timeouts)
            running = [k for k, v in JOBS.items() if v["status"] == "running"]
            if running:
                return self._json({"error": "a boot test is already running — wait or stop it",
                                   "job": running[0]}, 409)
            keep = parse_qs(urlparse(self.path).query).get("keep", ["0"])[0] in ("1", "true", "yes")
            jid = uuid.uuid4().hex
            JOBS[jid] = {"status": "running", "log": [], "exit": None, "keep": keep}
            t = threading.Thread(target=boot_test, args=(jid, body), daemon=True)
            JOBS[jid]["thread"] = t
            t.start()
            return self._json({"job": jid})
        self._json({"error": "not found"}, 404)

    def log_message(self, *a):
        pass  # quiet


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True


if __name__ == "__main__":
    port = int(os.environ.get("PORT", "8770"))
    reap_stale_vms()
    print(f"Marketplace review console: http://localhost:{port}")
    print(f"  cache: cache-proxy.py :{PORT_CACHE} ({CACHE_DIR})   ISO: {os.path.basename(_iso()) or '(none)'}")
    Server(("127.0.0.1", port), Handler).serve_forever()
