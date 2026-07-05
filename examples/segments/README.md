# Segments — shareable config fragments

A **segment** is a small piece of config someone can share, and you can drop
onto your setup in **System Snapshots → Designer** without editing any files by
hand. Open a `.json` like the ones here, then drag its card onto the matching
part of your setup (a waybar segment onto your bar, a niri segment onto niri).
The Designer refuses a mismatched drop, scans the content for anything risky,
and only writes it on **Apply** (after auto-saving a snapshot).

## Format

```json
{
  "id": "fancy-clock",
  "label": "Fancy Clock",
  "description": "A styled clock module for your bar.",
  "applies_to": "waybar",
  "section": "modules-right",
  "content": "\"clock\": { \"format\": \"{:%H:%M}\" }"
}
```

| Field | Meaning |
|---|---|
| `id` | Names the managed block. Re-dropping replaces it in place (idempotent). |
| `label` | Friendly name shown on the draggable card. |
| `description` | One line about what it does. |
| `applies_to` | What it fits — a **kind** (`waybar`, `niri`, `hyprland`, `sway`, `i3`, `mako`, `foot`, `kitty`), a **family** (`wm` = any window manager, `bar`, `notifications`, `terminal`), or `any`. This is what lets the Designer stop you dropping it in the wrong place. |
| `section` | *(optional)* where in the file it goes — a brace block like `binds`, or an INI/JSON section. Omitted → appended to the end. |
| `content` | The fragment itself. |

The Designer supplies the **path** from wherever you drop it, so a segment isn't
tied to one person's file layout. Under the hood a placed segment becomes a
manifest [`snippet`](../../src/snippets.rs) wrapped in marker comments, so it's
inserted in place, updated on re-apply, and removed cleanly — never overwriting
the rest of your file.

> A single `applies_to: "wm"` segment fits *any* window manager, but remember
> config **syntax** differs between them (niri's `spawn-at-startup` vs Hyprland's
> `exec-once`). Tag a segment `wm` only when its content is genuinely portable;
> otherwise tag the specific WM (like `niri-screenshot.json` here).
