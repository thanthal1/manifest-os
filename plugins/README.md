# Plugins — new manifest blocks without touching the core

A **plugin** teaches the manifest a new top-level block (`docker`, `tailscale`,
`ollama`, `k3s`, `steam`, …) by declaring how that block *expands* into the
primitives the engine already understands: `packages`, `services`, `files`,
`users`, `repos`, `pre_install`/`post_install`, `conditional`, and so on.

The core engine never learns what "docker" means. Before it parses a manifest,
`manifest::plugins::expand` folds every plugin block into ordinary core fields
and removes it. New capabilities grow at the edges; the core stays small — and
because expansion is pure data (no code runs at expand time), a plugin is as
reviewable as any other manifest.

## Using a plugin

```json
{
  "schema_version": "1.0.0",
  "meta": { "name": "Container host" },
  "docker": { "compose": true },
  "tailscale": {},
  "ollama": { "webui": true }
}
```

Each block's fields drive the plugin. `docker: { "compose": true }` pulls in
`docker-compose`; `docker: {}` just installs the engine.

## Where plugins live

Loaded lowest-priority first, later definitions win on name:

1. `plugins/` next to the repo / binary (development)
2. `/usr/share/manifest-os/plugins/` (bundled on the ISO)
3. `/etc/manifest/plugins/` (system-wide)
4. `~/.config/manifest/plugins/` (per-user)
5. A manifest's own inline `plugins: [ … ]` array (**highest** — makes a shared
   manifest fully self-contained; the reviewer sees the expansion right there)

## Writing a plugin

```json
{
  "plugin": "docker",
  "version": "1.0.0",
  "description": "Docker Engine + socket, optional Compose/Buildx/rootless.",
  "provides": ["docker"],
  "requires": [],
  "expands": {
    "packages": ["docker"],
    "services": { "system": ["docker.socket"] }
  },
  "conditional": [
    { "when": { "compose": true },  "packages": ["docker-compose"] },
    { "when": { "rootless": true }, "packages": ["docker-rootless-extras"] }
  ]
}
```

- **`plugin`** — name, and the block key it claims by default.
- **`provides`** — extra block keys it handles (defaults to `[plugin]`).
- **`requires`** — block fields that must be present, else expansion errors.
- **`expands`** — the always-applied slice of manifest.
- **`conditional`** — slices applied only when their `when` holds against the
  **block's own fields** (same `when` engine as the core schema).

### Field interpolation
Inside `expands`/`conditional`, `{{field}}` is replaced by the block's field of
that name — **typed** when the whole string is one token (`"timeout":
"{{timeout}}"` → the number), **stringified** when embedded in text.

### Two layers of conditions
A plugin's own `conditional` rules see **block fields** and resolve at expand
time. To branch on *hardware* instead, emit a normal top-level `conditional` in
`expands`; it flows into the manifest and is resolved later against the real
hardware facts (`gpu`, `cpu`, `is_vm`, …).

### Rules of thumb
- Arrays add up (a plugin's packages are appended to the author's); on a scalar
  conflict the **author wins** (a plugin never overrides what you set).
- Keep secrets out. Anything a plugin puts in `post_install` lands in the
  saved manifest — don't template auth keys or tokens into it. Leave
  interactive/secret steps (`tailscale up`, a k3s join token) to the user.

## Bundled plugins

| Block | Effect | Flags |
|---|---|---|
| `docker` | Docker Engine + `docker.socket` | `compose`, `buildx`, `rootless` |
| `tailscale` | Tailscale client + `tailscaled` | — |
| `ollama` | Ollama LLM runtime + service | `webui` (Open WebUI) |
| `k3s` | Single-node Kubernetes | — |
| `steam` | Steam (enables multilib) | `gamemode`, `mangohud`, `gamescope` |
