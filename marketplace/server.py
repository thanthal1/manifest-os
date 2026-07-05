#!/usr/bin/env python
"""server.py — backend for the marketplace review console.

Turns the static review web UI into a live one: it serves the page, runs the
scanner server-side (with `--check-packages`), boots a throwaway VM to actually
install a submission **using the package cache**, and keeps that cache fresh.

    python marketplace/server.py            # http://localhost:8770

Environment (all optional — sensible defaults):
    VBOX        path to VBoxManage           (default: the standard Windows path)
    CACHE_VM    VM that runs the pacoloco cache        (default: manifest-build)
    NATNET      VBox NAT Network name shared by cache + test VMs (default: manifestnet)
    ISO         install ISO for boot tests   (default: newest dist/manifestos-*.iso)
    PACOLOCO_PORT                             (default: 9129)

The heavy lifting (VBox lifecycle, guestcontrol) mirrors scripts/audit-vms.sh
and marketplace/boot-test.sh; this keeps it in one process so the UI can stream
progress and drive the cache. Endpoints:

    POST /api/scan            body = manifest JSON  -> scanner findings (JSON)
    GET  /api/cache/status    -> pacoloco up? cache size, package count, cache host
    POST /api/cache/refresh   -> refresh repo DBs so version resolution is current
    POST /api/boot-test       body = manifest JSON  -> {job} (starts a VM install)
    GET  /api/boot-test?job=  -> {status, exit, log}   (poll for progress)
    POST /api/boot-test/stop  body = {job}            -> cancel + tear the VM down
"""
import http.server
import json
import os
import re
import shutil
import socketserver
import subprocess
import threading
import time
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
WEB = os.path.join(HERE, "web")

VBOX = os.environ.get("VBOX", r"C:\Program Files\Oracle\VirtualBox\VBoxManage.exe")
CACHE_VM = os.environ.get("CACHE_VM", "manifest-build")
NATNET = os.environ.get("NATNET", "manifestnet")
PORT_CACHE = int(os.environ.get("PACOLOCO_PORT", "9129"))
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
# Package cache (pacoloco on CACHE_VM). pacoloco is installed in the VM's
# disk-backed Arch (/mnt); we run it from the live env pointing at that binary +
# config so it survives, with the cache on disk at /mnt/var/cache/pacoloco.
# ---------------------------------------------------------------------------
CACHE_CFG = "/mnt/etc/pacoloco.yaml"
CACHE_DIR = "/mnt/var/cache/pacoloco"


def cache_ensure_running():
    if not guest_up(CACHE_VM):
        return False, "cache VM is not reachable (is it running?)"
    r = guest(CACHE_VM, f"ss -tlnp 2>/dev/null | grep -q ':{PORT_CACHE}' && echo UP || echo DOWN")
    if "UP" in r.stdout:
        return True, "already running"
    # start pacoloco from the live env (setsid so it survives this call)
    guest(CACHE_VM, f"setsid /mnt/usr/bin/pacoloco -config {CACHE_CFG} "
                    f">/tmp/pacoloco.log 2>&1 </dev/null & disown; sleep 2; true")
    r = guest(CACHE_VM, f"ss -tlnp 2>/dev/null | grep -q ':{PORT_CACHE}' && echo UP || echo DOWN")
    return ("UP" in r.stdout), ("started" if "UP" in r.stdout else "failed to start")


def cache_host_ip():
    # the CACHE_VM's address on the shared NAT network (10.0.2.0/24)
    r = guest(CACHE_VM, "ip -4 -o addr show | awk '{print $4}' | grep -E '^10\\.0\\.2\\.' | cut -d/ -f1 | head -1")
    return (r.stdout or "").strip()


def cache_status():
    up, msg = cache_ensure_running()
    size = guest(CACHE_VM, f"du -sh {CACHE_DIR} 2>/dev/null | cut -f1").stdout.strip() or "0"
    count = guest(CACHE_VM, f"find {CACHE_DIR} -name '*.pkg.tar.zst' 2>/dev/null | wc -l").stdout.strip() or "0"
    return {"running": up, "message": msg, "host": CACHE_VM, "ip": cache_host_ip(),
            "port": PORT_CACHE, "cache_size": size, "cached_packages": int(count or 0),
            "url": f"http://{cache_host_ip() or '<cache-ip>'}:{PORT_CACHE}/repo/archlinux/$repo/os/$arch"}


def cache_refresh():
    """Refresh the repo databases so package version resolution is current.
    (Package *files* self-update on a cache miss; this updates the indexes.)"""
    r = guest(CACHE_VM, "arch-chroot /mnt /usr/bin/bash -lc 'pacman -Sy --noconfirm >/dev/null 2>&1 && echo OK || echo FAIL'", timeout=180)
    ok = "OK" in r.stdout
    return {"ok": ok, "message": "repo databases refreshed" if ok else "refresh failed",
            **cache_status()}


# ---------------------------------------------------------------------------
# Boot test — install a submission in a throwaway VM, using the cache.
# ---------------------------------------------------------------------------
JOBS = {}  # id -> {status, log[], exit, vm, thread}


def ensure_natnet():
    r = vbox("natnetwork", "list")
    if NATNET not in (r.stdout or ""):
        vbox("natnetwork", "add", "--netname", NATNET, "--network", "10.0.2.0/24", "--enable", "--dhcp", "on")
    # make sure the cache VM is on it (so the test VM can reach the cache)
    info = vbox("showvminfo", CACHE_VM, "--machinereadable").stdout
    if f'natnet1="{NATNET}"' not in info:
        st = "poweroff" in info
        if st:  # only when powered off; a live switch is possible but risky
            vbox("modifyvm", CACHE_VM, "--nic1", "natnetwork", "--nat-network", NATNET)


def boot_test(job_id, manifest_text):
    j = JOBS[job_id]
    def log(m): j["log"].append(m); j["log"][:] = j["log"][-400:]
    vm = f"review-{job_id[:8]}"
    j["vm"] = vm
    iso = _iso()
    try:
        if not iso or not os.path.exists(iso):
            log("ERROR: no install ISO found in dist/"); j["status"] = "failed"; j["exit"] = 2; return
        ensure_natnet()
        up, msg = cache_ensure_running()
        cache_ip = cache_host_ip()
        log(f"[cache] pacoloco {msg}; host {cache_ip or '?'}:{PORT_CACHE}")
        mfile = os.path.join(HERE, f".{vm}.json")
        open(mfile, "w", encoding="utf-8").write(manifest_text)

        log(f"[vm] creating {vm} (fresh UEFI, NAT network {NATNET})")
        vdi = os.path.join(HERE, f"{vm}.vdi")
        vbox("createvm", "--name", vm, "--ostype", "ArchLinux_64", "--register")
        vbox("modifyvm", vm, "--memory", "6144", "--cpus", "4", "--firmware", "efi",
             "--nic1", "natnetwork", "--nat-network", NATNET,
             "--graphicscontroller", "vmsvga", "--vram", "64", "--boot1", "dvd", "--boot2", "disk")
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

        if cache_ip:
            guest(vm, f"echo 'Server = http://{cache_ip}:{PORT_CACHE}/repo/archlinux/$repo/os/$arch' > /etc/pacman.d/mirrorlist")
            log(f"[vm] mirrorlist -> cache http://{cache_ip}:{PORT_CACHE}  (installs pull from cache)")
        subprocess.run([VBOX, "guestcontrol", vm, "copyto", mfile, "/root/submission.json",
                        "--username", "root", "--password", ""], capture_output=True, text=True)

        log("[install] manifest provision (this is the real thing — minutes)…")
        guest(vm, "rm -f /tmp/prov.exit; setsid bash -c 'manifest provision /root/submission.json "
                  "--disk /dev/sda --user reviewer --password review1234 --no-reboot "
                  ">/tmp/manifest-install.log 2>&1; echo $? >/tmp/prov.exit' </dev/null >/dev/null 2>&1 & echo launched")
        seen = 0
        while True:
            if j.get("cancel"): log("cancelled"); j["status"] = "failed"; j["exit"] = 1; return
            raw = guest(vm, "cat /tmp/prov.exit 2>/dev/null").stdout.strip()
            tail = guest(vm, "tail -n 3 /tmp/manifest-install.log 2>/dev/null").stdout.splitlines()
            for line in tail[seen:] if len(tail) > seen else []:
                log("  " + line)
            seen = len(tail)
            # surface the phase headers as they happen
            step = guest(vm, "grep -E '^\\[' /tmp/manifest-install.log 2>/dev/null | tail -1").stdout.strip()
            if step: j["step"] = step
            if raw.isdigit():
                j["exit"] = int(raw)
                if raw == "0":
                    hits = guest(vm, "grep -c 'serving cached file' /tmp/manifest-install.log 2>/dev/null").stdout.strip()
                    log(f"[install] OK — completed."); j["status"] = "passed"
                else:
                    log(f"[install] FAILED (exit {raw})."); j["status"] = "failed"
                return
            time.sleep(12)
    except Exception as e:
        log(f"ERROR: {e}"); j["status"] = "failed"; j["exit"] = 1
    finally:
        if j.get("status") in ("passed", "failed"):
            log("[vm] tearing down.")
        try:
            vbox("controlvm", vm, "poweroff", timeout=30)
            time.sleep(1)
            vbox("unregistervm", vm, "--delete", timeout=60)
        except Exception:
            pass
        try:
            os.remove(os.path.join(HERE, f".{vm}.json"))
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
                               "step": j.get("step", ""), "log": j["log"], "vm": j.get("vm")})
        return super().do_GET()

    def do_POST(self):
        body = self._body()
        if self.path.startswith("/api/scan"):
            p = subprocess.run([PY, os.path.join(HERE, "scan.py"), "-", "--json"],
                               input=body, capture_output=True, text=True)
            try:
                return self._json(json.loads(p.stdout))
            except Exception:
                return self._json({"error": p.stderr or "scan failed", "findings": []}, 500)
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
            jid = uuid.uuid4().hex
            JOBS[jid] = {"status": "running", "log": [], "exit": None}
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
    print(f"Marketplace review console: http://localhost:{port}")
    print(f"  cache VM: {CACHE_VM}   NAT network: {NATNET}   ISO: {os.path.basename(_iso()) or '(none)'}")
    Server(("127.0.0.1", port), Handler).serve_forever()
