# Hooks Promoted To First-Class Blocks

The manifest prefers **declared state** over shell (see the README's
"Declarative config (instead of hooks)"). The bundled examples try to use *zero*
`pre_install` / `post_install` hooks. Everything should go through declarative
blocks such as `files`, `snippets`, `theme`, `wallpaper`, `keybindings`,
`desktop`, `users`, `system`, `services`, `flatpak`, and `defaults`.

When an example needs a shell hook, record it here so the capability can later
become a first-class manifest block.

| Status | Example uses zero hooks? |
|---|---|
| `hyprland-pro.json` | yes |
| `sway-pro.json` | yes |
| `dev-station.json` | yes |

---

## 1. Flatpak / Flathub Apps -> `flatpak`

Former hook in `dev-station.json`:

```sh
flatpak remote-add --if-not-exists flathub https://flathub.org/repo/flathub.flatpakrepo
flatpak install -y --noninteractive flathub com.visualstudio.code || true
```

Schema:

```json
"flatpak": {
  "remotes": [
    { "name": "flathub", "url": "https://flathub.org/repo/flathub.flatpakrepo" }
  ],
  "apps": ["com.visualstudio.code", "md.obsidian.Obsidian"]
}
```

Implemented behavior:

- Ensures `flatpak` is installed.
- Adds each remote with `remote-add --system --if-not-exists`.
- Installs or updates each app id system-wide with `install --system -y --noninteractive --or-update`.
- Adds Flathub implicitly when apps are declared without remotes.

Implementation: `src/flatpak.rs`, wired into `install::apply` after package
installation.

## 2. Default Applications / MIME Associations -> `defaults`

Former hook in `dev-station.json`:

```sh
sudo -u dev xdg-settings set default-web-browser firefox.desktop
```

Schema:

```json
"defaults": {
  "browser": "firefox.desktop",
  "mime": {
    "image/png": "org.gnome.eog.desktop",
    "application/pdf": "org.gnome.Evince.desktop"
  }
}
```

Implemented behavior:

- Writes the primary user's `~/.config/mimeapps.list` directly.
- Expands `browser` to standard browser handlers.
- Adds every `mime` pair as a `[Default Applications]` entry.

Implementation: `src/defaults.rs`, wired into `install::apply` after `files`
and `snippets`.

---

## Notes / Non-Candidates

- **Dotfiles** are already first-class (`dotfiles` block, `dev-station.json`).
- **Git global config**, **shell prompt**, and **aliases** need no hook; they
  are plain `files` writes.
- **Default shell** is `users[].shell`.
- **Wallpaper** is the `wallpaper` block; on window managers the WM config calls
  `manifest-wallpaper`.
