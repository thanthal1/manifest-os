# Packaging — updatable components via a signed pacman repo

This turns the Manifest OS components into ordinary Arch packages served from
a GitHub release, so **`pacman -Syu` is the updater**. No bespoke self-update
code: GitHub is just the file host, pacman does everything else (versioning,
upgrade/downgrade, rollback via its cache, dependency resolution).

## The packages

| Package | Contents |
|---|---|
| `manifest-os` | the engine + CLI (`/usr/bin/manifest`) |
| `manifest-os-gui` | graphical installer + System Snapshots app |
| `manifest-os-plugins` | the bundled plugins (docker/tailscale/ollama/k3s/steam) |
| `manifest-os-examples` | flagship + reference example manifests |
| `manifest-os-keyring` | the repo's **public** signing key for pacman |

Code + data packages come from one split `pkg/PKGBUILD` (one source tarball,
one cargo build). The keyring is separate (`keyring/PKGBUILD`) — it changes
only if the key rotates.

## How package verification works (30-second version)

Signing uses a **key pair**:

* The **private key** can *create* signatures. It lives in `~/.gnupg` on the
  maintainer machine and nowhere else — not in this repo, not in the build VM,
  not on GitHub. `gen-key.sh` creates it; nothing here ever exports it.
* The **public key** can only *check* signatures. It ships to everyone inside
  `manifest-os-keyring` and is committed here as `keyring/manifest-os.asc`.
  That's safe by design: a public key cannot sign anything.

For each artifact we publish a detached `.sig` file — a statement that "the
private-key holder vouches for exactly these bytes". When a user's pacman
downloads `manifest-os-1.0-1-x86_64.pkg.tar.zst`, it also fetches the `.sig`
and checks it against the trusted public key (`SigLevel = Required`). Any
tampering — a corrupted mirror, a hijacked download, a modified GitHub asset —
changes the bytes, the signature no longer matches, and pacman refuses the
package. GitHub is therefore just *storage*; it doesn't need to be trusted.

**Leak checklist** (what would be bad, and why it can't happen here):
* `.sig` files, `.asc` public key, packages, the db — all public by design. ✅
* The private key — never leaves `~/.gnupg`; `.gitignore` also blocks
  `*.sec.asc`/secret-export names as a belt-and-suspenders. ✅
* The build VM never sees the key: packages are built unsigned in the VM and
  signed afterwards on the host. ✅
* If the maintainer machine were ever compromised: publish the revocation
  certificate GnuPG saved in `~/.gnupg/openpgp-revocs.d/`, generate a fresh
  key, ship a new keyring package.

## Release flow

```
# 0. once, ever: create the signing key (private stays in ~/.gnupg)
bash packaging/gen-key.sh

# 1. host: source tarball of exactly HEAD
V=1.0.0
git archive --format=tar.gz --prefix=manifest-os-$V/ -o packaging/pkg/manifest-os-$V.tar.gz HEAD

# 2. Arch (the manifest-build VM chroot): copy packaging/ to /home/pkgwork, then
MANIFEST_PKGVER=$V bash /home/pkgwork/build-repo.sh
#    -> /home/pkgwork/out/*.pkg.tar.zst + manifest-os.db  (unsigned)

# 3. host: copy out/ back to packaging/out/, then sign + publish
bash packaging/sign-repo.sh
bash packaging/publish-repo.sh        # gh release upload to the fixed `repo` tag
```

## What users do

Every third-party repo has a one-time trust bootstrap (pacman won't install a
`SigLevel = Required` repo until it trusts the key). The key isn't on a public
keyserver, so import it straight from this repo:

```
FPR=770823575E30BC11FC390790A61B8164C861A598
curl -fsSL https://raw.githubusercontent.com/thanthal1/manifest-os/main/packaging/keyring/manifest-os.asc \
  | sudo pacman-key --add -
sudo pacman-key --lsign-key "$FPR"

# add the repo (packaging/manifest-os.conf) to /etc/pacman.conf, then:
sudo pacman -Sy manifest-os-keyring   # pins the key as a package so it survives
sudo pacman -Syu manifest-os manifest-os-gui manifest-os-plugins
```

(On a Manifest OS install, the ISO can ship `manifest-os-keyring` pre-trusted so
users skip the bootstrap entirely — that's the ISO-integration step.) From then
on every component updates through normal `pacman -Syu`.

> **Publishing a new version:** bump `$V`, re-run the build → sign → publish flow
> above. pacman compares `pkgver-pkgrel`, so a higher version is picked up by
> `pacman -Syu` automatically; the `repo` release tag and Server URL never
> change. (Publishing the key to `keys.openpgp.org` too would let users bootstrap
> with `pacman-key --recv-keys $FPR` instead of the curl above — optional.)
