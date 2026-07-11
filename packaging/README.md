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

```
# add the repo (packaging/manifest-os.conf) to /etc/pacman.conf, then trust the
# key once (the chicken-and-egg step every third-party repo has):
sudo pacman-key --recv-keys <FPR> ; sudo pacman-key --lsign-key <FPR>
#   — or on a Manifest OS install the ISO already shipped the keyring.
sudo pacman -Sy manifest-os-keyring
sudo pacman -Syu manifest-os manifest-os-gui manifest-os-plugins
```

From then on every component updates individually through normal
`pacman -Syu`, and the ISO can stop baking loose binaries into
`/usr/local/bin` and install these packages instead (future step).
