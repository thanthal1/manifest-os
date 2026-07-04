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
    (r"/etc/hosts$",                       "MEDIUM", "/etc/hosts",
     "Editing hosts can redirect domains (update servers, telemetry, auth)."),
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
                    "A baked-in password is a known credential and shouldn't ship in a "
                    "public manifest — use a survey `secret`.", f"users[{i}]")


def scan_sources(m, rep):
    urls = []
    w = m.get("wallpaper")
    if isinstance(w, str):
        urls.append(("wallpaper", w))
    elif isinstance(w, dict) and w.get("source"):
        urls.append(("wallpaper.source", w["source"]))
    df = m.get("dotfiles")
    if isinstance(df, dict) and df.get("source"):
        src = df["source"]
        urls.append(("dotfiles.source", src))
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


def scan_packages(m, rep, check=False):
    pkgs = m.get("packages", []) or []
    if not check:
        if pkgs:
            rep.add("INFO", "packages", f"{len(pkgs)} package(s) declared",
                    "Review the list; AUR packages run arbitrary build scripts. "
                    "Re-run with --check-packages on an Arch box to flag AUR/unknown names.",
                    ", ".join(pkgs[:12]) + (" …" if len(pkgs) > 12 else ""))
        return
    import subprocess
    for p in pkgs:
        repo_hit = subprocess.run(["pacman", "-Si", p], stdout=subprocess.DEVNULL,
                                  stderr=subprocess.DEVNULL).returncode == 0
        if repo_hit:
            continue
        aur = subprocess.run(["paru", "-Si", p], stdout=subprocess.DEVNULL,
                             stderr=subprocess.DEVNULL).returncode == 0
        if aur:
            rep.add("MEDIUM", "AUR package", f"`{p}` is an AUR package",
                    "AUR packages build from an author-supplied PKGBUILD that runs "
                    "arbitrary code at build time. Review it.", "packages")
        else:
            rep.add("HIGH", "unknown package", f"`{p}` not found in repos or AUR",
                    "A package name that resolves to nothing will break the install — "
                    "or, worse, could be a typosquat target.", "packages")


def shorten(s, n=90):
    s = " ".join(str(s).split())
    return s if len(s) <= n else s[:n - 1] + "…"


# ---------------------------------------------------------------------------
# Driver
# ---------------------------------------------------------------------------

def scan(manifest, check_packages=False):
    rep = Report()
    if not isinstance(manifest, dict):
        rep.add("CRITICAL", "schema", "not a JSON object", "The manifest must be a JSON object.")
        return rep
    scan_hooks(manifest, rep)
    scan_files(manifest, rep)
    scan_users(manifest, rep)
    scan_sources(manifest, rep)
    scan_repos_boot(manifest, rep)
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
