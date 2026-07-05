//! Shareable config **segments** — the thing a non-technical user downloads and
//! drops onto a matching config in the Designer, without ever touching a path,
//! a section, or JSON.
//!
//! A segment is a superset of a manifest [`Snippet`](crate::manifest::Snippet):
//! it carries what *kind* of config it fits (`applies_to`) plus a friendly
//! `label`/`description`, and it has **no path of its own** — the drop target
//! supplies that. So a "waybar" segment can only land on a waybar config, a
//! "wm" segment on any window-manager config, and the UI can refuse a mismatched
//! drop instead of letting someone misconfigure their system.
//!
//! On apply the Designer combines the segment with the target's path into a
//! real `Snippet` and writes it through the same idempotent marker-block engine
//! ([`crate::snippets`]).

use anyhow::{bail, Context, Result};

/// Config targets the Designer knows how to place segments into:
/// (friendly title, `$HOME`-relative path suffix, kind). `kind` drives
/// compatibility matching.
pub const KNOWN_TARGETS: &[(&str, &str, &str)] = &[
    ("Niri", ".config/niri/config.kdl", "niri"),
    ("Hyprland", ".config/hypr/hyprland.conf", "hyprland"),
    ("Sway", ".config/sway/config", "sway"),
    ("i3", ".config/i3/config", "i3"),
    ("River", ".config/river/init", "river"),
    ("Waybar", ".config/waybar/config.jsonc", "waybar"),
    ("Waybar", ".config/waybar/config", "waybar"),
    ("Waybar style", ".config/waybar/style.css", "waybar-style"),
    ("Mako", ".config/mako/config", "mako"),
    ("Foot", ".config/foot/foot.ini", "foot"),
    ("Kitty", ".config/kitty/kitty.conf", "kitty"),
];

/// The kind of a known config path, e.g. `~/.config/waybar/config.jsonc` →
/// `"waybar"`. `None` for a path the Designer doesn't recognize.
pub fn kind_of_path(path: &str) -> Option<&'static str> {
    KNOWN_TARGETS
        .iter()
        .find(|(_, rel, _)| path.ends_with(rel))
        .map(|(_, _, kind)| *kind)
}

/// Friendly title for a known config path.
pub fn title_of_path(path: &str) -> Option<&'static str> {
    KNOWN_TARGETS
        .iter()
        .find(|(_, rel, _)| path.ends_with(rel))
        .map(|(title, _, _)| *title)
}

/// The families a target `kind` belongs to. A segment's `applies_to` matches if
/// it equals the kind or names one of these families. Every kind is in `"any"`.
pub fn target_families(kind: &str) -> Vec<&'static str> {
    let mut fams = vec!["any"];
    match kind {
        "niri" | "hyprland" | "sway" | "i3" | "river" | "labwc" | "wayfire" => fams.push("wm"),
        "waybar" | "waybar-style" => fams.push("bar"),
        "mako" | "dunst" => fams.push("notifications"),
        "foot" | "kitty" | "alacritty" => fams.push("terminal"),
        _ => {}
    }
    fams
}

/// Whether a segment tagged `applies_to` can be dropped onto a target of `kind`.
/// An empty / `any` / `*` tag fits everywhere (the UI should still flag those as
/// "untagged — review before dropping"). Case- and whitespace-insensitive.
pub fn segment_fits(applies_to: &str, kind: &str) -> bool {
    let a = applies_to.trim().to_ascii_lowercase();
    if a.is_empty() || a == "any" || a == "*" {
        return true;
    }
    a == kind || target_families(kind).contains(&a.as_str())
}

/// A downloadable segment package. Parsed from a segment `.json` (see
/// [`Segment::from_json`]); the `path` where it lands comes from the drop
/// target, not from the file.
#[derive(Debug, Clone)]
pub struct Segment {
    /// Managed-block id (re-applying replaces it in place).
    pub id: String,
    /// Friendly name for the browse/drop UI.
    pub label: String,
    /// One-line description of what it does.
    pub description: String,
    /// What kind of config it fits: `"waybar"`, `"wm"`, `"niri"`, `"any"`, …
    pub applies_to: String,
    /// Section to insert into (brace block or INI `[section]`); appended if unset.
    pub section: Option<String>,
    /// The fragment itself.
    pub content: String,
}

impl Segment {
    /// Parse a segment from JSON. Accepts either a bare segment object, or a
    /// manifest carrying a `snippets`/`segments` array (the first entry is
    /// used) — so today's snippet files and shared manifests both work.
    /// `content` is required; everything else has a sensible default.
    pub fn from_json(raw: &str) -> Result<Segment> {
        let v: serde_json::Value =
            serde_json::from_str(raw).context("that isn't valid JSON")?;
        let obj = v
            .get("segments")
            .or_else(|| v.get("snippets"))
            .and_then(|s| s.as_array())
            .and_then(|a| a.first())
            .cloned()
            .unwrap_or(v);

        let get = |k: &str| obj.get(k).and_then(|x| x.as_str()).map(str::to_string);
        let content = get("content").context("no \"content\" in the segment")?;
        if content.trim().is_empty() {
            bail!("the segment has empty content");
        }
        let id = get("id").filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "segment".into());
        Ok(Segment {
            label: get("label").unwrap_or_else(|| id.clone()),
            description: get("description").unwrap_or_default(),
            applies_to: get("applies_to").unwrap_or_default(),
            section: get("section").filter(|s| !s.trim().is_empty()),
            content,
            id,
        })
    }

    /// Turn this segment + a concrete target path into a manifest [`Snippet`]
    /// the engine can write.
    pub fn to_snippet(&self, path: &str) -> crate::manifest::Snippet {
        crate::manifest::Snippet {
            id: self.id.clone(),
            path: path.to_string(),
            section: self.section.clone(),
            content: self.content.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_and_title_from_path() {
        assert_eq!(kind_of_path("/home/x/.config/waybar/config.jsonc"), Some("waybar"));
        assert_eq!(kind_of_path("/home/x/.config/niri/config.kdl"), Some("niri"));
        assert_eq!(kind_of_path("/home/x/.config/hypr/hyprland.conf"), Some("hyprland"));
        assert_eq!(kind_of_path("/home/x/.config/unknown/thing"), None);
        assert_eq!(title_of_path("/home/x/.config/niri/config.kdl"), Some("Niri"));
    }

    #[test]
    fn compatibility_matching() {
        // exact kind
        assert!(segment_fits("waybar", "waybar"));
        assert!(!segment_fits("waybar", "niri"));
        // family: a "wm" segment fits any window manager, not a bar
        assert!(segment_fits("wm", "niri"));
        assert!(segment_fits("wm", "hyprland"));
        assert!(segment_fits("wm", "sway"));
        assert!(!segment_fits("wm", "waybar"));
        // untagged / any fits everywhere
        assert!(segment_fits("", "waybar"));
        assert!(segment_fits("any", "niri"));
        assert!(segment_fits("*", "hyprland"));
        // case/space insensitive
        assert!(segment_fits("  WayBar  ", "waybar"));
        assert!(segment_fits("WM", "sway"));
        // a niri-specific segment doesn't fit hyprland
        assert!(segment_fits("niri", "niri"));
        assert!(!segment_fits("niri", "hyprland"));
    }

    #[test]
    fn parses_bare_segment() {
        let s = Segment::from_json(
            r#"{"id":"clock","label":"Fancy clock","description":"a clock",
                "applies_to":"waybar","section":"modules-right","content":"\"clock\""}"#,
        )
        .unwrap();
        assert_eq!(s.id, "clock");
        assert_eq!(s.label, "Fancy clock");
        assert_eq!(s.applies_to, "waybar");
        assert_eq!(s.section.as_deref(), Some("modules-right"));
        assert!(s.content.contains("clock"));
    }

    #[test]
    fn parses_from_snippets_array_and_defaults() {
        // a shared manifest with a snippets[] list, minimal fields
        let s = Segment::from_json(r#"{"snippets":[{"content":"Mod+W { spawn \"waybar\"; }"}]}"#)
            .unwrap();
        assert_eq!(s.id, "segment"); // default id
        assert_eq!(s.label, "segment"); // label defaults to id
        assert_eq!(s.applies_to, ""); // untagged
        assert!(s.section.is_none());
    }

    #[test]
    fn rejects_missing_or_empty_content() {
        assert!(Segment::from_json(r#"{"id":"x"}"#).is_err());
        assert!(Segment::from_json(r#"{"content":"   "}"#).is_err());
        assert!(Segment::from_json("not json").is_err());
    }

    #[test]
    fn bundled_segment_examples_parse() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/segments");
        let mut n = 0;
        for entry in std::fs::read_dir(&dir).expect("examples/segments dir") {
            let p = entry.unwrap().path();
            if p.extension().map(|e| e == "json").unwrap_or(false) {
                let raw = std::fs::read_to_string(&p).unwrap();
                let seg = Segment::from_json(&raw)
                    .unwrap_or_else(|e| panic!("{}: {e}", p.display()));
                // a shared example should be tagged so the Designer can place it safely
                assert!(!seg.applies_to.trim().is_empty(), "{} has no applies_to", p.display());
                n += 1;
            }
        }
        assert!(n >= 2, "expected bundled segment examples");
    }

    #[test]
    fn to_snippet_takes_target_path() {
        let seg = Segment::from_json(r#"{"id":"bar","applies_to":"waybar","content":"x"}"#).unwrap();
        let sn = seg.to_snippet("/home/me/.config/waybar/config.jsonc");
        assert_eq!(sn.id, "bar");
        assert_eq!(sn.path, "/home/me/.config/waybar/config.jsonc");
        assert_eq!(sn.content, "x");
    }
}
