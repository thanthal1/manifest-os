# Hooks to promote to first-class blocks

The manifest prefers **declared state** over shell (see the README's
"Declarative config (instead of hooks)"). The bundled examples try to use *zero*
`pre_install` / `post_install` hooks — everything goes through `files`,
`snippets`, `theme`, `wallpaper`, `keybindings`, `desktop`, `users`, `system`,
`services`.

When an example still needs a shell hook, it's recorded here so the capability
can later become a first-class manifest block (the same way `files`, `snippets`,
`theme`, `wallpaper`, and `keybindings` each replaced a class of hook).

| Status | Example uses zero hooks? |
|---|---|
| `hyprland-pro.json` | ✅ zero |
| `sway-pro.json` | ✅ zero |
| `dev-station.json` | ⚠️ 3 hook lines (2 features below) |

---

## 1. Flatpak / Flathub apps  → proposed `flatpak` block

**Hook used today** (`dev-station.json` `post_install`):

```sh
flatpak remote-add --if-not-exists flathub https://flathub.org/repo/flathub.flatpakrepo
flatpak install -y --noninteractive flathub com.visualstudio.code || true
```

**Proposed schema:**

```json
"flatpak": {
  "remotes": [
    { "name": "flathub", "url": "https://flathub.org/repo/flathub.flatpakrepo" }
  ],
  "apps": ["com.visualstudio.code", "md.obsidian.Obsidian"]
}
```

**Behavior:** ensure `flatpak` is installed, add each remote
(`--if-not-exists`), install each app id (`-y --noninteractive`). Idempotent —
safe to re-run on sync. `flathub` could be the implicit default remote so
`"apps": [...]` alone works. New module `flatpak.rs`, wired into `install::apply`
after packages; add a `Flatpak` field to `Manifest`.

## 2. Default applications / MIME associations  → proposed `defaults` block

**Hook used today** (`dev-station.json` `post_install`):

```sh
sudo -u dev xdg-settings set default-web-browser firefox.desktop
```

**Proposed schema:**

```json
"defaults": {
  "browser": "firefox.desktop",
  "mime": {
    "image/png": "org.gnome.eog.desktop",
    "application/pdf": "org.gnome.Evince.desktop"
  }
}
```

**Behavior:** run as the primary user (like `theme`/`keybindings` do): `browser`
→ `xdg-settings set default-web-browser`; each `mime` pair →
`xdg-mime default <app> <type>`. Writes `~/.config/mimeapps.list`, so it could
alternatively be implemented as a generated `files` entry (no shell at all) —
worth doing, since that removes the last hook from `dev-station.json`. New
module `defaults.rs` (or fold into `files`), `Defaults` field on `Manifest`,
applied at user level after `files`.

---

### Notes / non-candidates

- **Dotfiles** are already first-class (`dotfiles` block, `dev-station.json`).
- **git global config**, **shell prompt**, **aliases** need no hook — they're
  plain `files` writes (`~/.gitconfig`, `~/.config/starship.toml`, `~/.zshrc`).
- **Default shell** is `users[].shell` (no `chsh` hook).
- **Wallpaper** is the `wallpaper` block; on window managers the WM config calls
  `manifest-wallpaper` (see `hyprland-pro.json` / the `snippets` in
  `dev-station.json`).
