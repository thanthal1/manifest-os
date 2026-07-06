#!/usr/bin/env python3
"""cache-proxy.py — Arch package cache proxy that runs on the (Windows) host.

pacoloco (cache-setup.sh) is the preferred cache but needs an Arch box to run
on. This is the same idea in stdlib Python so the dev host itself can be the
cache: test VMs point their mirrorlist at

    Server = http://10.0.2.2:9129/repo/archlinux/$repo/os/$arch

(10.0.2.2 is the VirtualBox NAT alias for the host's loopback) and every
package a VM downloads is kept in marketplace/pkg-cache/. The first boot test
downloads ~2 GB once; every later test is served from disk in milliseconds.

URL layout is pacoloco-compatible (/repo/<name>/<path>), so PACOLOCO_URL works
unchanged whether it points here or at a real pacoloco.

    python cache-proxy.py [--port 9129] [--cache-dir DIR] [--purge-days 30]

Behaviour:
  - *.pkg.tar.* (+ .sig) are immutable: cached forever, served from disk.
  - repo databases (.db/.files) are re-fetched (60s TTL); if the mirror is
    unreachable a stale cached copy is served, so re-checks work offline.
  - Range requests are honoured (pacman's curl XferCommand resumes with -C -).
  - GET /ping returns 200 — boot-test.sh uses it as a health check.
"""

import argparse
import email.utils
import os
import re
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

REPOS = {
    # same upstreams as cache-setup.sh's pacoloco.yaml
    "archlinux": [
        "https://geo.mirror.pkgbuild.com",
        "https://mirror.rackspace.com/archlinux",
    ],
    "cachyos": [
        "https://mirror.cachyos.org/repo/x86_64/cachyos",
    ],
}

# Content-addressed artifacts: name-version-rel.arch.pkg.tar.zst never changes.
IMMUTABLE = re.compile(r"\.(pkg|src)\.tar\.[a-z0-9]+(\.sig)?$")

# Package epochs put a ':' in filenames (zlib-1:1.3.2-...), which NTFS silently
# treats as an alternate-data-stream marker — the download "succeeds" into a
# stream on a file called 'zlib-1' and the rename then fails (WinError 87).
# Percent-escape everything Windows can't have in a filename before touching disk.
_BAD = set('%<>:"|?*')


def _safe(component: str) -> str:
    return "".join(f"%{ord(c):02X}" if c in _BAD or ord(c) < 32 else c for c in component)
DB_TTL = 60  # seconds a fetched .db/.files stays "fresh enough"
CHUNK = 256 * 1024

CACHE_DIR = ""  # set in main()

_locks_guard = threading.Lock()
_locks: dict[str, threading.Lock] = {}


def _lock_for(path: str) -> threading.Lock:
    with _locks_guard:
        return _locks.setdefault(path, threading.Lock())


def log(msg: str) -> None:
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


def fetch(upstreams: list[str], rel: str, dest: str) -> str:
    """Download rel from the first upstream that works, atomically into dest.
    Returns "ok", "404" (every upstream said not-found), or "fail"."""
    os.makedirs(os.path.dirname(dest), exist_ok=True)
    part = dest + ".part"
    all_404 = True
    for base in upstreams:
        url = f"{base}/{rel}"
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "manifestos-cache-proxy/1"})
            with urllib.request.urlopen(req, timeout=200) as resp, open(part, "wb") as f:
                while True:
                    chunk = resp.read(CHUNK)
                    if not chunk:
                        break
                    f.write(chunk)
            os.replace(part, dest)
            return "ok"
        except (urllib.error.URLError, OSError, TimeoutError) as e:
            if not (isinstance(e, urllib.error.HTTPError) and e.code == 404):
                all_404 = False
                log(f"MISS  {rel} — upstream {base} failed: {e}")
            try:
                os.remove(part)
            except OSError:
                pass
    return "404" if all_404 else "fail"


class Server(ThreadingHTTPServer):
    def handle_error(self, request, client_address):
        # pacman/curl drop connections constantly (retries, aborts) — don't
        # spew a traceback for those, only for real bugs
        et = sys.exc_info()[0]
        if et and issubclass(et, (ConnectionError, TimeoutError)):
            return
        super().handle_error(request, client_address)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):  # quiet the default per-request stderr line
        pass

    def do_GET(self):
        if self.path == "/ping":
            body = b"ok\n"
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        m = re.match(r"^/repo/([^/]+)/(.+)$", self.path.split("?")[0])
        if not m or m.group(1) not in REPOS:
            self.send_error(404, "unknown repo — expected /repo/<name>/<path>")
            return
        repo, rel = m.group(1), m.group(2)
        # never let the path escape the cache dir
        parts = [p for p in rel.split("/") if p not in ("", ".", "..")]
        rel = "/".join(parts)
        cached = os.path.join(CACHE_DIR, repo, *(_safe(p) for p in parts))

        name = parts[-1]
        immutable = bool(IMMUTABLE.search(name))

        with _lock_for(cached):
            have = os.path.isfile(cached)
            fresh = have and (immutable or time.time() - os.path.getmtime(cached) < DB_TTL)
            if fresh:
                log(f"HIT   {repo}/{rel}")
                if immutable:
                    now = time.time()  # bump mtime = last-served, for --purge-days
                    os.utime(cached, (now, now))
            else:
                got = fetch(REPOS[repo], rel, cached)
                if got == "ok":
                    log(f"MISS  {repo}/{rel} — fetched")
                elif have:
                    log(f"STALE {repo}/{rel} — all upstreams down, serving cached copy")
                elif got == "404":
                    # e.g. *.db.sig — Arch repo dbs are unsigned; pass the 404
                    # through (pacman tolerates it) instead of a scary 502
                    self.send_error(404, "not found upstream")
                    return
                else:
                    self.send_error(502, "all upstreams failed and nothing cached")
                    return

        self.serve_file(cached)

    def serve_file(self, path: str):
        size = os.path.getsize(path)
        start, end = 0, size - 1
        status = 200
        rng = self.headers.get("Range")
        if rng:
            m = re.match(r"bytes=(\d+)-(\d*)$", rng.strip())
            if m:
                start = int(m.group(1))
                if m.group(2):
                    end = min(int(m.group(2)), size - 1)
                if start >= size:
                    self.send_response(416)
                    self.send_header("Content-Range", f"bytes */{size}")
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                status = 206

        self.send_response(status)
        self.send_header("Content-Length", str(end - start + 1))
        self.send_header("Accept-Ranges", "bytes")
        self.send_header(
            "Last-Modified", email.utils.formatdate(os.path.getmtime(path), usegmt=True)
        )
        if status == 206:
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.end_headers()
        with open(path, "rb") as f:
            f.seek(start)
            left = end - start + 1
            while left > 0:
                chunk = f.read(min(CHUNK, left))
                if not chunk:
                    break
                try:
                    self.wfile.write(chunk)
                except (ConnectionError, OSError):
                    return  # client hung up (pacman retry/abort) — not our problem
                left -= len(chunk)


def purge(days: int) -> None:
    """Drop packages not served in `days` days so the cache doesn't grow forever."""
    cutoff = time.time() - days * 86400
    freed = n = 0
    for root, _, files in os.walk(CACHE_DIR):
        for f in files:
            p = os.path.join(root, f)
            try:
                st = os.stat(p)
                if st.st_mtime < cutoff:
                    os.remove(p)
                    n += 1
                    freed += st.st_size
            except OSError:
                pass
    if n:
        log(f"purged {n} files ({freed / 1e9:.1f} GB) untouched for {days}+ days")


def main():
    global CACHE_DIR
    here = os.path.dirname(os.path.abspath(__file__))
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--port", type=int, default=int(os.environ.get("CACHE_PORT", 9129)))
    ap.add_argument("--cache-dir", default=os.path.join(here, "pkg-cache"))
    ap.add_argument("--bind", default="0.0.0.0",
                    help="must be 0.0.0.0: VBox NAT on Windows does NOT deliver the "
                         "guest's 10.0.2.2 traffic to the host loopback (verified — a "
                         "127.0.0.1 bind gets 'connection refused' from the guest)")
    ap.add_argument("--purge-days", type=int, default=30, help="0 disables the startup purge")
    args = ap.parse_args()

    CACHE_DIR = args.cache_dir
    os.makedirs(CACHE_DIR, exist_ok=True)
    if args.purge_days:
        purge(args.purge_days)

    srv = Server((args.bind, args.port), Handler)
    log(f"cache proxy on http://{args.bind}:{args.port} — cache dir {CACHE_DIR}")
    log(f"VM mirrorlist line: Server = http://10.0.2.2:{args.port}/repo/archlinux/$repo/os/$arch")
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        sys.exit(0)


if __name__ == "__main__":
    main()
