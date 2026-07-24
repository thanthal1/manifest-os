#!/usr/bin/env python
"""scan.py — security + sanity scanner for Manifest OS marketplace submissions.

A shared manifest.json, when installed, runs with root privileges on someone
else's machine. Before a marketplace ever auto-serves one, it must be reviewed
for the ways a manifest can do harm. This is the static gate: it parses a
manifest and reports every risky or broken thing, ranked by severity, so a
reviewer (or an auto-approver policy) can decide.

It does NOT execute anything — it's pure static analysis of the JSON. The
*dynamic* check (a real install + boot in a throwaway VM) is a separate, heavier
stage; see DESIGN.md. This scanner is the cheap first pass and the source of
truth the web UI renders.

Usage:
    python scan.py manifest.json              # human-readable report
    python scan.py manifest.json --json       # machine-readable (for the web UI / CI)
    python scan.py --check-packages m.json    # also resolve packages against pacman (Arch only)
    cat m.json | python scan.py -             # from stdin

Exit code: 0 if nothing above the --fail-on threshold (default: critical),
else 1 — so CI can gate on it.

Severities: CRITICAL > HIGH > MEDIUM > LOW > INFO
"""
import argparse
import json
import re
import sys

SEV_ORDER = ["INFO", "LOW", "MEDIUM", "HIGH", "CRITICAL"]


class Report:
    def __init__(self):
        self.findings = []

    def add(self, severity, category, title, detail, where=""):
        self.findings.append({
            "severity": severity, "category": category, "title": title,
            "detail": detail, "where": where,
        })

    def max_severity(self):
        if not self.findings:
            return None
        return max((f["severity"] for f in self.findings), key=SEV_ORDER.index)


# ---------------------------------------------------------------------------
# Pattern tables — the heart of the review policy
# ---------------------------------------------------------------------------

# A file written to one of these is a privilege / persistence / trust concern.
# (regex on the destination path, case-insensitive)
SENSITIVE_PATHS = [
    (r"/etc/sudoers(\.d/|$)",              "CRITICAL", "sudoers modification",
     "Writing to sudoers can silently grant passwordless root."),
    (r"\.ssh/authorized_keys$",            "CRITICAL", "SSH authorized_keys",
     "Adding an SSH key here is a remote-access backdoor."),
    (r"/etc/pacman\.conf$|/etc/pacman\.d/", "CRITICAL", "pacman config / repo injection",
     "Editing pacman config can add an attacker-controlled package repo + key."),
    (r"/etc/pam\.d/",                      "CRITICAL", "PAM configuration",
     "PAM controls authentication; a change here can bypass login checks."),
    (r"/etc/systemd/system/|/usr/lib/systemd/system/|\.config/systemd/user/",
     "HIGH", "systemd unit",
     "A unit file (esp. combined with `services`) gives persistent code execution."),
    (r"/etc/profile(\.d/|$)|\.bashrc$|\.zshrc$|\.bash_profile$|\.profile$|\.zshenv$|\.zprofile$",
     "MEDIUM", "shell startup file",
     "Shell rc / profile files run on every login — legitimate for a rice, but also a "
     "persistence/exec vector, so the content is reviewed separately below."),
    (r"/etc/cron|/var/spool/cron|\.config/systemd/user/.*\.timer$",
     "HIGH", "scheduled task",
     "Cron/timer entries run code on a schedule (persistence)."),
    (r"/etc/udev/rules\.d/|/etc/polkit-1/|/usr/share/polkit-1/",
     "MEDIUM", "udev / polkit rule",
     "These can run code on device events or widen privileges."),
    (r"^/usr/local/bin/|^/usr/bin/|^/bin/|\.local/bin/",
     "MEDIUM", "executable on PATH",
     "A file placed on PATH can shadow a real command."),
    (r"/etc/environment$|/etc/ld\.so|\.so$",
     "MEDIUM", "linker / environment file",
     "Library-path or environment tampering can hijack processes."),
]

# Substrings that mark a URL as pointing at user-controlled code/paste hosting —
# not inherently malicious, but the linked content must be reviewed by a human.
CODE_HOSTS = [
    "github.com", "githubusercontent.com", "gist.github", "gitlab.com",
    "bitbucket.org", "codeberg.org", "sourceforge", "pastebin.com", "paste.",
    "hastebin", "termbin", "transfer.sh", "0x0.st", "ghostbin", "rentry.co",
    "discord", "t.me", "bit.ly", "tinyurl", "ngrok",
]

# Dangerous shell idioms in hooks / file content (regex, case-insensitive).
SHELL_DANGER = [
    (r"\b(curl|wget)\b[^|]*\|\s*(sudo\s+)?(sh|bash|zsh|python|perl)\b",
     "CRITICAL", "remote code execution",
     "Piping a download straight into a shell runs unreviewed remote code as root."),
    (r"\bbase64\s+-?-?d(ecode)?\b|\bbase64\b[^|]*\|\s*(sh|bash)",
     "CRITICAL", "base64-decoded execution",
     "Decoding then running base64 is a classic way to hide a payload."),
    (r">\s*/dev/tcp/|nc\s+-e|ncat\s+-e|/dev/tcp/\d",
     "CRITICAL", "reverse shell / raw socket",
     "Raw TCP redirection or netcat -e is a reverse-shell primitive."),
    (r"\brm\s+-rf\s+(/|\$HOME|~)(\s|$)",
     "CRITICAL", "destructive delete",
     "`rm -rf` on / or a home root can wipe the system."),
    (r"\beval\b|\bexec\b\s+\d?<|\bsource\s+/dev|\bxxd\s+-r",
     "HIGH", "dynamic code evaluation",
     "eval / hex-reverse / process-substitution execution can hide intent."),
    (r"chmod\s+(u\+s|\+s|[0-7]*[4-7][0-7]{3})",
     "HIGH", "setuid bit",
     "Setting the setuid bit on a file is a privilege-escalation technique."),
    (r"\b(curl|wget)\b",
     "MEDIUM", "network download in a hook",
     "The hook fetches something from the network at install time — review the source."),
    (r"\buseradd\b|\bpasswd\b|\bchpasswd\b|\busermod\b.*-G\s*\w*wheel",
     "HIGH", "user/credential manipulation in a hook",
     "Creating users or setting passwords from a raw hook bypasses the reviewed `users` block."),
]

# Long base64/hex blobs hiding in content.
B64_BLOB = re.compile(r"[A-Za-z0-9+/]{120,}={0,2}")
HEX_BLOB = re.compile(r"(?:\\x[0-9a-fA-F]{2}){20,}")

# --- DNS control / spoofing ------------------------------------------------
# A manifest that can change how names resolve can silently redirect update
# servers, package mirrors, keyservers, auth/telemetry endpoints — pointing the
# machine at attacker-controlled infrastructure. Every DNS-touching capability
# is surfaced so a reviewer can confirm it isn't spoofing.
DNS_PATHS = [
    (r"/etc/resolv\.conf$", "HIGH", "resolv.conf — DNS server override",
     "Pins the system's DNS servers; an attacker resolver can spoof any domain."),
    (r"/etc/systemd/resolved\.conf(\.d/|$)", "HIGH", "systemd-resolved DNS config",
     "Sets system DNS servers — can redirect all name resolution."),
    (r"/etc/NetworkManager/(conf\.d/|system-connections/|NetworkManager\.conf)",
     "HIGH", "NetworkManager DNS / connection config",
     "Can set DNS servers or a connection profile that redirects name resolution."),
    (r"/etc/nsswitch\.conf$", "HIGH", "nsswitch — name-resolution order",
     "Reordering/adding resolution modules can hijack how hostnames resolve."),
    (r"/etc/dnsmasq\.conf$|/etc/dnsmasq\.d/|/etc/unbound/|/etc/named\.conf$|/etc/bind/|/etc/dnscrypt|/etc/stubby/",
     "MEDIUM", "local DNS server / resolver config",
     "Configures a local DNS forwarder/server; review its upstreams and per-domain overrides."),
]
# Commands (in hooks or written scripts) that repoint resolution at runtime.
DNS_CMD = re.compile(
    r"resolvectl\s+dns|systemd-resolve\b[^\n]*dns|nmcli\b[^\n]*\bdns\b|"
    r">\s*/etc/resolv\.conf|tee\s+/etc/resolv\.conf|nameserver\s+\d{1,3}(\.\d{1,3}){3}", re.I)
DNS_PKGS = {"dnsmasq", "unbound", "bind", "dnscrypt-proxy", "stubby", "adguardhome",
            "coredns", "pihole", "pi-hole", "blocky", "https-dns-proxy", "smartdns"}
# A hosts line mapping an IP to a real (non-loopback) domain = a static DNS override.
HOSTS_ENTRY = re.compile(
    r"^\s*(?:\d{1,3}(?:\.\d{1,3}){3}|[0-9a-fA-F:]{3,})\s+(?!localhost|ip6-|::1\b)([A-Za-z0-9.-]+\.[A-Za-z]{2,}\S*)",
    re.M)
# Domains whose spoofing is especially dangerous (updates, mirrors, keys, auth).
SENSITIVE_DOMAINS = ("github", "archlinux", "aur.", "mirror", "kernel.org", "gnu.org",
                     "google", "cloudflare", "microsoft", "apple", "pgp", "keyserver",
                     "gpg", "letsencrypt", "pypi", "npmjs")


# ---------------------------------------------------------------------------
# Checks
# ---------------------------------------------------------------------------

def scan_hooks(m, rep):
    for phase in ("pre_install", "post_install"):
        for i, line in enumerate(m.get(phase, []) or []):
            rep.add("HIGH", "shell hook", f"{phase} runs a shell command",
                    "Author-supplied shell runs as root during install — the single "
                    "biggest review item. Prefer declarative blocks (files/users/services).",
                    f"{phase}[{i}]: {shorten(line)}")
            scan_shell_text(line, rep, f"{phase}[{i}]")


def scan_shell_text(text, rep, where):
    for rx, sev, title, detail in SHELL_DANGER:
        if re.search(rx, text, re.I):
            rep.add(sev, "shell pattern", title, detail, f"{where}: {shorten(text)}")
    if B64_BLOB.search(text):
        rep.add("HIGH", "obfuscation", "embedded base64 blob",
                "A long base64 string may be a hidden payload — decode and review it.",
                where)
    if HEX_BLOB.search(text):
        rep.add("MEDIUM", "obfuscation", "embedded hex-escaped blob",
                "Long \\xNN sequences can hide code or data.", where)


def scan_files(m, rep):
    for i, f in enumerate(m.get("files", []) or []):
        path = str(f.get("path", ""))
        content = str(f.get("content", ""))
        owner = f.get("owner", "")
        mode = f.get("mode", "")
        for rx, sev, title, detail in SENSITIVE_PATHS:
            if re.search(rx, path, re.I):
                rep.add(sev, "sensitive file", f"{title}: {path}", detail, f"files[{i}]")
        # world-writable or setuid modes on a written file
        if re.match(r"0?[0-7]*[2367]$", str(mode)) or (str(mode).startswith("4") and len(str(mode)) >= 4):
            rep.add("MEDIUM", "file mode", f"unusual mode {mode} on {path}",
                    "World-writable or setuid modes are rarely legitimate.", f"files[{i}]")
        # dangerous shell idioms inside content that lands somewhere executable
        scan_shell_text(content, rep, f"files[{i}].content ({path})")
    for i, s in enumerate(m.get("snippets", []) or []):
        scan_shell_text(str(s.get("content", "")), rep, f"snippets[{i}].content")


def scan_dns(m, rep):
    """DNS-control / spoofing capabilities — see DNS_* tables above."""
    for i, f in enumerate(m.get("files", []) or []):
        path = str(f.get("path", ""))
        content = str(f.get("content", ""))
        for rx, sev, title, detail in DNS_PATHS:
            if re.search(rx, path, re.I):
                rep.add(sev, "DNS", title, detail, f"files[{i}]")
        if re.search(r"/etc/hosts$", path, re.I):
            hits = HOSTS_ENTRY.findall(content)
            if hits:
                hot = any(any(d in h.lower() for d in SENSITIVE_DOMAINS) for h in hits)
                rep.add("HIGH" if hot else "MEDIUM", "DNS",
                        f"/etc/hosts statically redirects {len(hits)} host(s)",
                        "Hosts entries override DNS for specific domains — a spoofing vector "
                        "(redirect update/mirror/keyserver/auth endpoints)."
                        + (" One targets a sensitive domain." if hot else "")
                        + " Entries: " + ", ".join(hits[:6]), f"files[{i}]")
            else:
                rep.add("MEDIUM", "DNS", "writes /etc/hosts",
                        "Editing hosts can redirect domains — review the entries.", f"files[{i}]")
        if DNS_CMD.search(content):
            rep.add("HIGH", "DNS", f"a written file changes DNS resolution ({path})",
                    "The content repoints which resolver the system uses — a spoofing vector.",
                    f"files[{i}]")
    for phase in ("pre_install", "post_install"):
        for i, line in enumerate(m.get(phase, []) or []):
            if DNS_CMD.search(line):
                rep.add("HIGH", "DNS", f"{phase} changes DNS resolution",
                        "A hook sets nameservers / resolv.conf — can point the machine at an "
                        "attacker resolver that spoofs any domain.", f"{phase}[{i}]: {shorten(line)}")
    for p in m.get("packages", []) or []:
        if p in DNS_PKGS:
            rep.add("MEDIUM", "DNS", f"installs a DNS server/resolver (`{p}`)",
                    "A local DNS server/forwarder intercepts all name resolution — review its "
                    "config for spoofed or overridden domains.", "packages")


def scan_users(m, rep):
    for i, u in enumerate(m.get("users", []) or []):
        name = u.get("name", "")
        if name == "root" or u.get("uid") == 0:
            rep.add("CRITICAL", "user", f"user `{name}` is root / uid 0",
                    "A manifest defining the root account can seize the machine.", f"users[{i}]")
        if u.get("sudo"):
            rep.add("HIGH", "user", f"user `{name}` granted sudo",
                    "This account gets passwordless-prompt root via a sudoers drop-in.", f"users[{i}]")
        if "wheel" in (u.get("groups") or []):
            rep.add("HIGH", "user", f"user `{name}` added to `wheel`",
                    "wheel members can escalate to root (sudo).", f"users[{i}]")
        if u.get("password"):
            rep.add("MEDIUM", "credential", f"hardcoded password for `{name}`",
                    "Fine for a personal install-script manifest, but a baked-in credential "
                    "shouldn't ship in a *public marketplace* listing — use a survey `secret` "
                    "there so each install sets its own.", f"users[{i}]")


def scan_sources(m, rep):
    urls = []
    w = m.get("wallpaper")
    if isinstance(w, str):
        urls.append(("wallpaper", w))
    elif isinstance(w, dict) and w.get("source"):
        urls.append(("wallpaper.source", w["source"]))
    # dotfiles may be a single object or an array of mappings.
    df = m.get("dotfiles")
    df_list = df if isinstance(df, list) else [df] if isinstance(df, dict) else []
    for i, entry in enumerate(df_list):
        if isinstance(entry, dict) and entry.get("source"):
            src = entry["source"]
            urls.append((f"dotfiles[{i}].source", src))
            rep.add("MEDIUM", "dotfiles", "dotfiles cloned from a URL",
                    "Every file in the repo is placed into the user's $HOME — the whole repo "
                    "is trusted code/config. Review it.", f"dotfiles.source: {src}")
    for phase in ("pre_install", "post_install"):
        for line in m.get(phase, []) or []:
            urls += [(phase, u) for u in re.findall(r"https?://[^\s\"'|)]+", line)]

    for where, u in urls:
        u = u.rstrip(").,'\"")
        if not u.startswith("http"):
            continue
        if u.startswith("http://"):
            rep.add("MEDIUM", "insecure URL", "plain-HTTP resource",
                    "http:// is unauthenticated and MITM-able; require https://.", f"{where}: {u}")
        host_hit = next((h for h in CODE_HOSTS if h in u.lower()), None)
        if host_hit:
            rep.add("LOW", "external source", f"links to {host_hit}",
                    "User-controlled code/paste hosting — the content can change after "
                    "review. A human should inspect the linked source.", f"{where}: {u}")


def scan_repos_boot(m, rep):
    repos = m.get("repos") or {}
    if repos.get("cachyos") or m.get("system", {}).get("kernel") == "cachy":
        rep.add("MEDIUM", "third-party repo", "enables the CachyOS repository",
                "Pulls a third-party repo and signing key — trusted, but not official Arch.",
                "repos.cachyos / system.kernel")
    for key, val in repos.items():
        if key not in ("multilib", "cachyos", "cachy_optimized_packages") and val:
            rep.add("HIGH", "custom repo", f"enables an unrecognized repo `{key}`",
                    "A non-standard repo is an untrusted package source.", "repos")
    boot = m.get("boot") or {}
    for arg in boot.get("cmdline", []) or []:
        if re.match(r"(init|rd\.|systemd\.|module_blacklist|security)=", str(arg)):
            rep.add("MEDIUM", "boot cmdline", f"sensitive kernel parameter `{arg}`",
                    "Changing init / security / module params can alter what boots.", "boot.cmdline")


def _pkgs_via_pacman(pkgs):
    """On an Arch box: classify each name via pacman -Si / paru -Si."""
    import subprocess
    out = {}
    for p in pkgs:
        if subprocess.run(["pacman", "-Si", p], stdout=subprocess.DEVNULL,
                          stderr=subprocess.DEVNULL).returncode == 0:
            out[p] = "repo"
        elif subprocess.run(["paru", "-Si", p], stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL).returncode == 0:
            out[p] = "aur"
        else:
            out[p] = "missing"
    return out


def _pkgs_via_web(pkgs):
    """No pacman on this host (the scan also runs on the Windows dev box):
    ask the Arch web APIs instead — archlinux.org per name for the official
    repos, then one batched AUR RPC call for whatever's left."""
    import urllib.parse
    import urllib.request

    def get(url):
        req = urllib.request.Request(url, headers={"User-Agent": "manifestos-scan/1"})
        with urllib.request.urlopen(req, timeout=20) as r:
            return json.load(r)

    out = {}
    for p in pkgs:
        hit = get("https://archlinux.org/packages/search/json/?name=" + urllib.parse.quote(p))
        out[p] = "repo" if hit.get("results") else "missing"
    rest = [p for p, v in out.items() if v == "missing"]
    if rest:
        q = "&".join("arg[]=" + urllib.parse.quote(p) for p in rest)
        found = {r["Name"] for r in get("https://aur.archlinux.org/rpc/v5/info?" + q).get("results", [])}
        for p in rest:
            if p in found:
                out[p] = "aur"
    return out


def scan_packages(m, rep, check=False):
    pkgs = m.get("packages", []) or []
    if not pkgs:
        return
    status = None
    if check:
        import shutil
        try:
            status = _pkgs_via_pacman(pkgs) if shutil.which("pacman") else _pkgs_via_web(pkgs)
        except OSError as e:
            rep.add("INFO", "packages", "package check skipped",
                    "No pacman on this host and the Arch web APIs were unreachable, "
                    "so package names could not be verified this run.", shorten(e))
    if status is None:
        rep.add("INFO", "packages", f"{len(pkgs)} package(s) declared",
                "Review the list; AUR packages run arbitrary build scripts. "
                "Re-run with --check-packages to flag AUR/unknown names.",
                ", ".join(pkgs[:12]) + (" …" if len(pkgs) > 12 else ""))
        return
    for p, st in status.items():
        if st == "aur":
            rep.add("MEDIUM", "AUR package", f"`{p}` is an AUR package",
                    "AUR packages build from an author-supplied PKGBUILD that runs "
                    "arbitrary code at build time. Review it.", "packages")
        elif st == "missing":
            rep.add("HIGH", "unknown package", f"`{p}` not found in repos or AUR",
                    "A package name that resolves to nothing will break the install — "
                    "or, worse, could be a typosquat target.", "packages")


def shorten(s, n=90):
    s = " ".join(str(s).split())
    return s if len(s) <= n else s[:n - 1] + "…"


# ---------------------------------------------------------------------------
# Driver
# ---------------------------------------------------------------------------

# Core top-level manifest keys the engine understands natively. Anything else is
# a plugin block (or a typo). Keep loosely in sync with src/manifest.rs.
CORE_KEYS = {
    "schema_version", "meta", "system", "repos", "packages", "services",
    "dotfiles", "desktop", "display_manager", "boot", "variables", "survey",
    "settings", "conditional_packages", "conditional", "detect", "users",
    "files", "snippets", "flatpak", "strata", "defaults", "wallpaper", "keybindings",
    "gestures", "theme", "display", "login", "pre_install", "post_install", "plugins",
}


def scan_plugins(m, rep):
    """Fold inline plugin content into `m` so every other scanner covers it, and
    flag blocks that rely on a plugin shipped outside this manifest.

    A plugin `expands` a custom block into the same primitives (hooks, files,
    users, repos, dotfiles) the rest of the scanner already vets — so an inline
    plugin sneaking in a root hook must not escape review just because it's one
    level down.
    """
    inline = m.get("plugins") or []
    provided = set()
    for p in inline:
        if isinstance(p, dict):
            provided.add(p.get("plugin"))
            provided.update(p.get("provides") or [])

    # Blocks with no core field and no inline definition = external plugin.
    for key in list(m.keys()):
        if key in CORE_KEYS or key in provided:
            continue
        rep.add("MEDIUM", "plugins", f"external plugin block `{key}`",
                "This block isn't a core field and isn't defined inline — it expands "
                "via a plugin shipped separately, so what it installs can't be reviewed "
                "from this manifest alone. Prefer an inline `plugins` definition.", key)

    # Merge each inline plugin's contributed primitives into the manifest view.
    for p in inline:
        if not isinstance(p, dict):
            continue
        bodies = [p.get("expands") or {}]
        for r in (p.get("conditional") or []):
            if isinstance(r, dict):
                bodies.append({k: v for k, v in r.items() if k != "when"})
        for b in bodies:
            if not isinstance(b, dict):
                continue
            for phase in ("pre_install", "post_install"):
                m.setdefault(phase, []).extend(b.get(phase) or [])
            for key in ("files", "snippets", "users"):
                m.setdefault(key, []).extend(b.get(key) or [])
            df = b.get("dotfiles")
            if df:
                cur = m.get("dotfiles")
                m["dotfiles"] = (cur if isinstance(cur, list) else [cur] if cur else []) \
                    + (df if isinstance(df, list) else [df])
            if isinstance(b.get("repos"), dict):
                m.setdefault("repos", {}).update(b["repos"])


# Canonical mirror/snapshot hosts per distro. A stratum pulling packages from
# anywhere else is an untrusted foreign package source (supply-chain risk), the
# same way a custom pacman repo is.
STRATA_TRUSTED_HOSTS = {
    "debian": ("deb.debian.org", "snapshot.debian.org", "ftp.debian.org",
               "security.debian.org"),
    "ubuntu": ("archive.ubuntu.com", "ports.ubuntu.com", "security.ubuntu.com",
               "snapshot.ubuntu.com"),
    "fedora": ("dl.fedoraproject.org", "download.fedoraproject.org",
               "mirrors.fedoraproject.org"),
    "alpine": ("dl-cdn.alpinelinux.org",),
}
# Exposing one of these from a foreign stratum onto the host PATH puts a
# privilege/identity path in front of the host's own tooling — worth a flag.
STRATA_RISKY_EXPOSE = {"sudo", "su", "doas", "passwd", "sh", "bash", "dash",
                       "zsh", "env", "chroot", "mount", "pkexec"}


def scan_strata(m, rep):
    strata = m.get("strata") or []
    if not strata:
        return
    for i, s in enumerate(strata):
        if not isinstance(s, dict):
            rep.add("HIGH", "strata", f"strata[{i}] is not an object",
                    "Each stratum must be a JSON object.", "strata")
            continue
        name = s.get("name", f"#{i}")
        distro = str(s.get("distro", "")).strip().lower()
        where = f"strata[{i}] ({name})"

        # A foreign rootfs runs its own package manager as root at install time —
        # inherently a broad new trust surface, always worth surfacing.
        rep.add("MEDIUM", "strata", f"bootstraps a `{distro or '?'}` stratum",
                "Installs a full foreign-distro rootfs and runs its package "
                "manager as root — a new, non-Arch trust surface. Confirm the "
                "distro and mirror are intended.", where)

        # Mirror / snapshot host trust: anything off the canonical hosts is an
        # untrusted package source.
        trusted = STRATA_TRUSTED_HOSTS.get(distro, ())
        for field in ("mirror", "snapshot"):
            val = s.get(field)
            # snapshot is usually a bare timestamp (fine); only URL-shaped values
            # carry a host to check.
            if isinstance(val, str) and "://" in val:
                host = re.sub(r"^\w+://([^/]+).*", r"\1", val).lower()
                if trusted and not any(host == t or host.endswith("." + t) for t in trusted):
                    rep.add("HIGH", "strata", f"untrusted {field} host `{host}`",
                            f"Stratum `{name}` pulls packages from a host that "
                            f"isn't a canonical {distro} mirror — an untrusted "
                            "package source.", f"{where}.{field}")
            if isinstance(val, str) and val.startswith("http://"):
                rep.add("MEDIUM", "insecure URL", f"plain-HTTP {field} for stratum `{name}`",
                        "Foreign packages fetched over unencrypted HTTP.", f"{where}.{field}")

        # Never let a manifest disable the bootstrap's signature verification.
        blob = json.dumps(s)
        if "no-check-gpg" in blob or "--no-check-gpg" in blob or s.get("check_gpg") is False:
            rep.add("CRITICAL", "strata", f"stratum `{name}` disables GPG verification",
                    "Bootstrapping without signature checks accepts tampered "
                    "packages — a supply-chain hole.", where)

        # Risky exposed binaries.
        for b in s.get("expose", []) or []:
            if str(b).lower() in STRATA_RISKY_EXPOSE:
                rep.add("HIGH", "strata", f"exposes `{b}` from stratum `{name}` onto the host PATH",
                        "A shell/privilege/identity binary from a foreign stratum "
                        "shadows or fronts the host's own — a privilege path the "
                        "host tooling doesn't audit.", f"{where}.expose")


def scan(manifest, check_packages=False):
    rep = Report()
    if not isinstance(manifest, dict):
        rep.add("CRITICAL", "schema", "not a JSON object", "The manifest must be a JSON object.")
        return rep
    scan_plugins(manifest, rep)
    scan_hooks(manifest, rep)
    scan_files(manifest, rep)
    scan_dns(manifest, rep)
    scan_users(manifest, rep)
    scan_sources(manifest, rep)
    scan_repos_boot(manifest, rep)
    scan_strata(manifest, rep)
    scan_packages(manifest, rep, check=check_packages)
    return rep


COLORS = {"CRITICAL": "\033[41;97m", "HIGH": "\033[31m", "MEDIUM": "\033[33m",
          "LOW": "\033[36m", "INFO": "\033[90m", "_": "\033[0m"}


def print_report(rep, meta_name):
    order = sorted(rep.findings, key=lambda f: -SEV_ORDER.index(f["severity"]))
    print(f"\n  Manifest OS — security scan: {meta_name}\n")
    if not order:
        print("  \033[32mNo findings.\033[0m Nothing risky detected (still no substitute "
              "for a boot test).\n")
        return
    counts = {}
    for f in order:
        counts[f["severity"]] = counts.get(f["severity"], 0) + 1
    summary = "  ".join(f"{COLORS[s]}{counts[s]} {s}{COLORS['_']}"
                        for s in reversed(SEV_ORDER) if s in counts)
    print("  " + summary + "\n")
    for f in order:
        c = COLORS.get(f["severity"], "")
        print(f"  {c}{f['severity']:>8}{COLORS['_']}  {f['title']}")
        print(f"            {f['detail']}")
        if f["where"]:
            print(f"            \033[90m↳ {f['where']}{COLORS['_']}")
        print()


def main():
    ap = argparse.ArgumentParser(description="Static security scan of a Manifest OS manifest.")
    ap.add_argument("manifest", help="path to manifest.json, or - for stdin")
    ap.add_argument("--json", action="store_true", help="emit findings as JSON")
    ap.add_argument("--check-packages", action="store_true",
                    help="resolve packages against pacman/paru (Arch only)")
    ap.add_argument("--fail-on", default="CRITICAL", choices=SEV_ORDER,
                    help="exit non-zero if any finding is at/above this severity (default CRITICAL)")
    args = ap.parse_args()

    # The report uses a couple of Unicode glyphs; force UTF-8 so a Windows
    # console (cp1252) doesn't crash the tool.
    try:
        sys.stdout.reconfigure(encoding="utf-8")
    except Exception:
        pass

    raw = sys.stdin.read() if args.manifest == "-" else open(args.manifest, encoding="utf-8").read()
    try:
        manifest = json.loads(raw)
    except json.JSONDecodeError as e:
        if args.json:
            print(json.dumps({"error": f"invalid JSON: {e}", "findings": []}))
        else:
            print(f"\n  \033[41;97mERROR\033[0m  not valid JSON: {e}\n")
        sys.exit(1)

    rep = scan(manifest, check_packages=args.check_packages)
    name = (manifest.get("meta") or {}).get("name") or args.manifest

    if args.json:
        print(json.dumps({"name": name, "max_severity": rep.max_severity(),
                          "findings": rep.findings}, indent=2))
    else:
        print_report(rep, name)

    worst = rep.max_severity()
    if worst and SEV_ORDER.index(worst) >= SEV_ORDER.index(args.fail_on):
        sys.exit(1)


if __name__ == "__main__":
    main()
