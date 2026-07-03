//! Config snippets — insert fragments into existing files, not replace them.
//!
//! The `files` field owns whole files; `snippets` is for the other case: you
//! found a great waybar launch setup and want *just that piece* dropped into
//! the right section of your existing niri/Hyprland config, leaving the rest
//! of the file alone.
//!
//! Each snippet is wrapped in comment **markers** carrying its `id`:
//!
//! ```text
//! // >>> manifest:waybar-bind >>>
//! Mod+W { spawn "waybar"; }
//! // <<< manifest:waybar-bind <<<
//! ```
//!
//! so re-applying a manifest *replaces the block in place* — idempotent, never
//! duplicated, position preserved. The comment style follows the file type
//! (`//` for KDL/JSONC, `/* … */` for CSS, `#` for everything else).
//!
//! Where it lands, in priority order:
//!   1. An existing marker block with the same id → replaced in place.
//!   2. `section` names a **brace block** (`binds {` … `}` — niri, Hyprland,
//!      sway-with-braces) → inserted just before that block's closing brace.
//!   3. `section` names an **INI section** (`[section]`) → inserted at the end
//!      of that section (before the next `[header]` or EOF).
//!   4. No `section` (or not found — with a warning) → appended to the end.
//!
//! Missing target files are created (a snippet into a not-yet-existing config
//! is fine). Ownership follows the same rule as everything user-level: with a
//! declared primary user the file is written to `/home/<user>/…` and chowned.

use crate::exec::Ctx;
use crate::files;
use crate::manifest::Snippet;
use anyhow::Result;

pub fn apply(snippets: &[Snippet], primary_user: Option<&str>, ctx: &Ctx) -> Result<()> {
    for s in snippets {
        if s.id.trim().is_empty() {
            println!("  · warning: snippet for {} has no id — skipping", s.path);
            continue;
        }
        // Resolve `~/` the same way files::home_spec does, so reads and writes
        // agree on the path (absolute paths pass through untouched).
        let rel = s.path.strip_prefix("~/");
        let out_spec = |content: String| match rel {
            Some(r) => files::home_spec(primary_user, r, content),
            None => crate::manifest::FileSpec {
                path: s.path.clone(),
                content,
                mode: None,
                owner: None,
            },
        };
        let path = out_spec(String::new()).path;

        // Current content (missing file = empty; created on write). In dry-run
        // reads are skipped — the plan is shown against an empty file.
        let current = if ctx.dry_run { String::new() } else { read_existing(&path, ctx) };

        let updated = upsert(&current, s);
        println!(
            "  · snippet `{}` → {}{}",
            s.id,
            path,
            s.section.as_deref().map(|x| format!(" (section {x})")).unwrap_or_default()
        );
        files::apply(&[out_spec(updated)], ctx)?;
    }
    Ok(())
}

/// Read the target file's current content, as root if needed (user configs on
/// a live system are usually readable, but a fresh chroot install writes as
/// root). Missing file → empty string.
fn read_existing(path: &str, ctx: &Ctx) -> String {
    if let Ok(c) = std::fs::read_to_string(path) {
        return c;
    }
    ctx.output(true, "cat", &[path]).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The pure engine
// ---------------------------------------------------------------------------

/// Comment open/close for a path's file type: `//` (KDL/JSON-family), `/* */`
/// (CSS), `#` (default).
fn comment_style(path: &str) -> (&'static str, &'static str) {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "kdl" | "json" | "jsonc" | "js" | "rasi" => ("//", ""),
        "css" | "scss" => ("/*", " */"),
        _ => ("#", ""),
    }
}

fn markers(path: &str, id: &str) -> (String, String) {
    let (open, close) = comment_style(path);
    (
        format!("{open} >>> manifest:{id} >>>{close}"),
        format!("{open} <<< manifest:{id} <<<{close}"),
    )
}

/// Every managed block in `content`, as `(id, inner content)` pairs — used by
/// the Designer to rebuild snippet nodes from what's really on disk. Matching
/// is comment-style agnostic (it looks for the `>>> manifest:<id> >>>` core,
/// whatever comment characters surround it).
pub fn extract_blocks(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    for id in block_ids(content) {
        if let Some((a, b)) = block_bounds(content, &id) {
            // Both markers on one line (a == b) is a malformed but survivable
            // block: treat it as empty rather than panicking on a[+1]..b.
            let inner = lines.get(a + 1..b).unwrap_or_default().join("\n");
            out.push((id, inner));
        }
    }
    out
}

/// `content` with the managed block `id` (markers included) removed. Returns
/// the input unchanged when the block isn't present.
pub fn remove_block(content: &str, id: &str) -> String {
    let Some((a, b)) = block_bounds(content, id) else {
        return content.to_string();
    };
    let lines: Vec<&str> = content.lines().collect();
    let mut out: Vec<String> = lines[..a].iter().map(|l| l.to_string()).collect();
    out.extend(lines[b + 1..].iter().map(|l| l.to_string()));
    // Collapse a doubled blank line the removal may leave behind.
    join(out)
}

/// The ids of every managed block in `content`, in order of appearance.
fn block_ids(content: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for line in content.lines() {
        if let Some(rest) = line.split(">>> manifest:").nth(1) {
            if let Some(id) = rest.split(" >>>").next() {
                if !id.is_empty() && !ids.iter().any(|x| x == id) {
                    ids.push(id.to_string());
                }
            }
        }
    }
    ids
}

/// Start/end line indexes of block `id`'s markers, comment-style agnostic.
fn block_bounds(content: &str, id: &str) -> Option<(usize, usize)> {
    let start_tag = format!(">>> manifest:{id} >>>");
    let end_tag = format!("<<< manifest:{id} <<<");
    let lines: Vec<&str> = content.lines().collect();
    let a = lines.iter().position(|l| l.contains(&start_tag))?;
    let b = lines.iter().position(|l| l.contains(&end_tag))?;
    (a <= b).then_some((a, b))
}

/// Insert or replace snippet `s` in `current`, returning the new file content.
/// Public so the Designer (node-graph editor) can apply edits directly.
pub fn upsert(current: &str, s: &Snippet) -> String {
    let (start, end) = markers(&s.path, &s.id);
    let block = format!("{start}\n{}\n{end}", s.content.trim_end());

    // 1) Existing managed block → replace in place, keeping the indentation
    //    the original insertion gave it (a block inside `binds { … }` was
    //    indented to match; the replacement must be too or re-applying
    //    un-indents it).
    if let (Some(a), Some(b)) = (find_line(current, &start), find_line(current, &end)) {
        if a <= b {
            let lines: Vec<&str> = current.lines().collect();
            let indent: String = lines[a].chars().take_while(|c| c.is_whitespace()).collect();
            let indented = block
                .lines()
                .map(|l| if l.is_empty() { String::new() } else { format!("{indent}{l}") })
                .collect::<Vec<_>>()
                .join("\n");
            let mut out: Vec<String> = lines[..a].iter().map(|l| l.to_string()).collect();
            out.push(indented);
            out.extend(lines[b + 1..].iter().map(|l| l.to_string()));
            return join(out);
        }
    }

    // 2/3) Section anchor.
    if let Some(section) = s.section.as_deref() {
        if let Some(at) = brace_section_close(current, section) {
            return insert_at_line(current, at, &indent_block(&block, current, at));
        }
        if let Some(at) = ini_section_end(current, section) {
            return insert_at_line(current, at, &block);
        }
        println!("  · note: section `{section}` not found in {} — appending at the end", s.path);
    }

    // 4) Append.
    let mut out = current.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(&block);
    out.push('\n');
    out
}

/// Line index of the first line equal to `needle` (trimmed).
fn find_line(hay: &str, needle: &str) -> Option<usize> {
    hay.lines().position(|l| l.trim() == needle)
}

/// For a brace config, the line index of the closing `}` of the block opened
/// by a line like `section {` (depth-tracked, so nested braces inside are
/// fine). The snippet gets inserted *before* that line.
fn brace_section_close(content: &str, section: &str) -> Option<usize> {
    let lines: Vec<&str> = content.lines().collect();
    let open_re = |l: &str| {
        let t = l.trim();
        t.strip_prefix(section)
            .map(|rest| rest.trim_start().starts_with('{'))
            .unwrap_or(false)
    };
    let start = lines.iter().position(|l| open_re(l))?;
    let mut depth = 0i32;
    for (i, l) in lines.iter().enumerate().skip(start) {
        depth += l.matches('{').count() as i32;
        depth -= l.matches('}').count() as i32;
        if depth == 0 && i >= start {
            return Some(i);
        }
    }
    None
}

/// For an INI config, the line index just past the end of `[section]`'s body
/// (i.e. the next `[header]` line, or one past the last line).
fn ini_section_end(content: &str, section: &str) -> Option<usize> {
    let lines: Vec<&str> = content.lines().collect();
    let header = format!("[{section}]");
    let start = lines.iter().position(|l| l.trim() == header)?;
    for (i, l) in lines.iter().enumerate().skip(start + 1) {
        let t = l.trim();
        if t.starts_with('[') && t.ends_with(']') {
            return Some(i);
        }
    }
    Some(lines.len())
}

/// Indent `block` to match the body of the brace section closing at line
/// `close_at` (one level deeper than the closing brace's indent).
fn indent_block(block: &str, content: &str, close_at: usize) -> String {
    let close_line = content.lines().nth(close_at).unwrap_or("");
    let base: String = close_line.chars().take_while(|c| c.is_whitespace()).collect();
    let inner = format!("{base}    ");
    block
        .lines()
        .map(|l| if l.is_empty() { String::new() } else { format!("{inner}{l}") })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Insert `text` as new line(s) before line index `at`.
fn insert_at_line(content: &str, at: usize, text: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut out: Vec<String> = lines[..at].iter().map(|l| l.to_string()).collect();
    out.push(text.to_string());
    out.extend(lines[at..].iter().map(|l| l.to_string()));
    join(out)
}

fn join(lines: Vec<String>) -> String {
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snip(id: &str, path: &str, section: Option<&str>, content: &str) -> Snippet {
        Snippet {
            id: id.into(),
            path: path.into(),
            section: section.map(String::from),
            content: content.into(),
        }
    }

    const NIRI: &str = "input {\n    keyboard { }\n}\n\nbinds {\n    Mod+Return { spawn \"foot\"; }\n}\n";

    #[test]
    fn inserts_into_brace_section_before_closing_brace() {
        let s = snip("waybar", "config.kdl", Some("binds"), "Mod+W { spawn \"waybar\"; }");
        let out = upsert(NIRI, &s);
        // Inside binds { }, after the existing bind, before the closing brace.
        let binds_close = out.lines().position(|l| l == "}").map(|_| ()).is_some();
        assert!(binds_close);
        assert!(out.contains("// >>> manifest:waybar >>>"));
        let idx_existing = out.find("Mod+Return").unwrap();
        let idx_new = out.find("Mod+W").unwrap();
        let idx_close = out.rfind('}').unwrap();
        assert!(idx_existing < idx_new && idx_new < idx_close);
        // The untouched parts survive verbatim.
        assert!(out.contains("input {"));
    }

    #[test]
    fn reapplying_replaces_the_block_in_place_not_duplicates() {
        let s1 = snip("waybar", "config.kdl", Some("binds"), "Mod+W { spawn \"waybar\"; }");
        let once = upsert(NIRI, &s1);
        let s2 = snip("waybar", "config.kdl", Some("binds"), "Mod+B { spawn \"waybar\"; }");
        let twice = upsert(&once, &s2);
        assert_eq!(twice.matches(">>> manifest:waybar >>>").count(), 1);
        assert!(twice.contains("Mod+B"));
        assert!(!twice.contains("Mod+W"));
    }

    #[test]
    fn ini_section_inserts_before_next_header() {
        let ini = "[General]\nkey=1\n\n[Other]\nx=2\n";
        let s = snip("extra", "app.conf", Some("General"), "added=yes");
        let out = upsert(ini, &s);
        let general = out.find("[General]").unwrap();
        let added = out.find("added=yes").unwrap();
        let other = out.find("[Other]").unwrap();
        assert!(general < added && added < other);
        assert!(out.contains("# >>> manifest:extra >>>"));
    }

    #[test]
    fn missing_section_appends_with_warning_path() {
        let s = snip("x", "config.kdl", Some("nope"), "line");
        let out = upsert("top { }\n", &s);
        assert!(out.trim_end().ends_with("// <<< manifest:x <<<"));
    }

    #[test]
    fn no_section_appends_to_end_and_missing_file_is_created() {
        let s = snip("boot", "config.kdl", None, "spawn-at-startup \"waybar\"");
        let out = upsert("", &s);
        assert!(out.starts_with("// >>> manifest:boot >>>"));
        assert!(out.contains("spawn-at-startup"));
    }

    #[test]
    fn comment_style_follows_file_type() {
        assert_eq!(markers("a.kdl", "i").0, "// >>> manifest:i >>>");
        assert_eq!(markers("style.css", "i").0, "/* >>> manifest:i >>> */");
        assert_eq!(markers("hyprland.conf", "i").0, "# >>> manifest:i >>>");
    }

    #[test]
    fn hyprland_style_section_works_too() {
        let hypr = "general {\n    gaps_in = 5\n}\n";
        let s = snip("gaps", "hyprland.conf", Some("general"), "gaps_out = 10");
        let out = upsert(hypr, &s);
        let a = out.find("gaps_in").unwrap();
        let b = out.find("gaps_out").unwrap();
        let close = out.rfind('}').unwrap();
        assert!(a < b && b < close);
    }

    #[test]
    fn nested_braces_do_not_confuse_the_close_finder() {
        let kdl = "binds {\n    Mod+T { spawn \"x\"; }\n    Mod+Y { spawn \"y\"; }\n}\nafter { }\n";
        let close = brace_section_close(kdl, "binds").unwrap();
        // Closing brace of binds is line 3 (0-indexed), not one of the inline pairs.
        assert_eq!(close, 3);
    }

    #[test]
    fn extract_blocks_round_trips_what_upsert_wrote() {
        let s = snip("waybar", "config.kdl", Some("binds"), "Mod+W { spawn \"waybar\"; }");
        let content = upsert(NIRI, &s);
        let blocks = extract_blocks(&content);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].0, "waybar");
        assert!(blocks[0].1.contains("Mod+W"));
    }

    #[test]
    fn extract_handles_all_comment_styles() {
        let css = "/* >>> manifest:bar-style >>> */\n.bar { color: red; }\n/* <<< manifest:bar-style <<< */\n";
        let blocks = extract_blocks(css);
        assert_eq!(blocks[0].0, "bar-style");
        assert!(blocks[0].1.contains("color: red"));
    }

    #[test]
    fn reapplying_inside_a_section_keeps_the_indentation() {
        let s1 = snip("waybar", "config.kdl", Some("binds"), "Mod+W { spawn \"waybar\"; }");
        let once = upsert(NIRI, &s1);
        let s2 = snip("waybar", "config.kdl", Some("binds"), "Mod+B { spawn \"waybar\"; }");
        let twice = upsert(&once, &s2);
        // The replaced block's marker keeps the indent the insertion gave it.
        let marker_line = twice.lines().find(|l| l.contains(">>> manifest:waybar >>>")).unwrap();
        assert!(marker_line.starts_with("    "), "lost indentation: {marker_line:?}");
        let content_line = twice.lines().find(|l| l.contains("Mod+B")).unwrap();
        assert!(content_line.starts_with("    "), "lost indentation: {content_line:?}");
    }

    #[test]
    fn extract_blocks_survives_markers_on_one_line() {
        // A malformed, hand-mangled block: both markers on the same line.
        let bad = "# >>> manifest:x >>> # <<< manifest:x <<<\nrest\n";
        let blocks = extract_blocks(bad);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].1, "");
    }

    #[test]
    fn remove_block_deletes_only_the_named_block() {
        let s1 = snip("a", "x.conf", None, "one");
        let s2 = snip("b", "x.conf", None, "two");
        let content = upsert(&upsert("base\n", &s1), &s2);
        let out = remove_block(&content, "a");
        assert!(!out.contains("manifest:a"));
        assert!(!out.contains("one"));
        assert!(out.contains("manifest:b"));
        assert!(out.contains("two"));
        assert!(out.contains("base"));
        // Removing a missing id is a no-op.
        assert_eq!(remove_block(&out, "zzz"), out);
    }
}
