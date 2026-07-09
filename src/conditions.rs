//! Facts, conditions, and conditional resolution — the engine behind `when`.
//!
//! A **fact** is a `key → value` string the run knows about itself: a survey
//! answer, a `variables` entry, or an auto-detected hardware trait (`gpu`,
//! `cpu`, `virt`, `is_vm`, `firmware`). A [`Condition`] (a manifest `when`)
//! is checked against the [`Facts`]; [`resolve`] then folds every matching
//! conditional overlay — and drops every `when`-gated file that doesn't match —
//! into a plain [`Manifest`] the rest of the pipeline applies unchanged.
//!
//! Detection is read-only (`lspci`, `/proc/cpuinfo`, `systemd-detect-virt`,
//! `/sys/firmware/efi`), so it runs the same in `--dry-run` — a preview shows
//! exactly which conditionals would fire. On a non-Linux dev box the probes
//! find nothing and every hardware fact reads `unknown`/`none`/`bios`.

use crate::manifest::{Condition, Manifest};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

/// The run's known facts — survey answers, `variables`, and detected hardware —
/// as lowercase strings, the single map every condition evaluates against.
#[derive(Debug, Default, Clone)]
pub struct Facts {
    map: BTreeMap<String, String>,
}

impl Facts {
    /// Auto-detect the standard hardware facts, then apply manifest `detect`
    /// overrides (any value other than `"auto"` pins that fact to a literal —
    /// used to test a manifest as if on different hardware).
    pub fn detect(overrides: &BTreeMap<String, String>) -> Facts {
        let mut map = BTreeMap::new();
        map.insert("gpu".into(), detect_gpu());
        map.insert("cpu".into(), detect_cpu());
        let virt = detect_virt();
        map.insert("is_vm".into(), (virt != "none").to_string());
        map.insert("virt".into(), virt);
        map.insert(
            "firmware".into(),
            if Path::new("/sys/firmware/efi").exists() { "uefi" } else { "bios" }.into(),
        );
        for (k, v) in overrides {
            if v != "auto" && !v.trim().is_empty() {
                map.insert(k.clone(), v.to_ascii_lowercase());
            }
        }
        Facts { map }
    }

    /// A fact's value, lowercased.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.map.get(key).map(String::as_str)
    }

    /// Add facts (survey answers / variables), overriding auto-detected values
    /// with the same key so a manifest can shadow a detected fact deliberately.
    pub fn overlay(&mut self, entries: impl IntoIterator<Item = (String, String)>) {
        for (k, v) in entries {
            self.map.insert(k, v.to_ascii_lowercase());
        }
    }

    #[cfg(test)]
    fn from_pairs(pairs: &[(&str, &str)]) -> Facts {
        let mut f = Facts::default();
        f.overlay(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        f
    }
}

impl Condition {
    /// Whether this condition holds given the run's facts. An object condition
    /// is an AND over its keys; an array value is an OR for that key. Both
    /// sides are compared case-insensitively. A referenced fact that doesn't
    /// exist makes the condition false (rather than erroring).
    pub fn holds(&self, facts: &Facts) -> bool {
        match self {
            Condition::Match(map) => map.iter().all(|(key, want)| {
                facts.get(key).map(|got| value_matches(want, got)).unwrap_or(false)
            }),
            Condition::Expr(expr) => eval_expr(expr, facts),
        }
    }
}

/// Fold every matching conditional overlay into the manifest and drop every
/// `when`-gated file whose condition doesn't hold, leaving a plain manifest.
/// Idempotent-safe: downstream install steps already de-dup packages and
/// overwrite generated files, so a merged duplicate is harmless.
pub fn resolve(manifest: &mut Manifest, facts: &Facts) {
    // 1) Overlays whose `when` holds contribute their slice of manifest.
    for ov in std::mem::take(&mut manifest.conditional) {
        if !ov.when.holds(facts) {
            continue;
        }
        manifest.packages.extend(ov.packages);
        manifest.files.extend(ov.files);
        manifest.services.system.extend(ov.services.system);
        manifest.services.user.extend(ov.services.user);
        manifest.snippets.extend(ov.snippets);
        manifest.keybindings.extend(ov.keybindings);
        manifest.pre_install.extend(ov.pre_install);
        manifest.post_install.extend(ov.post_install);
        if let Some(f) = ov.flatpak {
            match &mut manifest.flatpak {
                Some(existing) => {
                    existing.remotes.extend(f.remotes);
                    existing.apps.extend(f.apps);
                }
                None => manifest.flatpak = Some(f),
            }
        }
        // A base choice wins; an overlay only fills an unset desktop/theme.
        if manifest.desktop.is_none() {
            manifest.desktop = ov.desktop;
        }
        if manifest.theme.is_none() {
            manifest.theme = ov.theme;
        }
    }

    // 2) Per-file `when` (including files just merged from overlays).
    manifest
        .files
        .retain(|f| f.when.as_ref().map(|c| c.holds(facts)).unwrap_or(true));
}

// ---------------------------------------------------------------------------
// condition matching
// ---------------------------------------------------------------------------

/// Whether the condition value (a scalar, or an array = "any of") matches the
/// fact string. Case-insensitive; JSON scalars are stringified (`true`, `8`).
fn value_matches(want: &serde_json::Value, got: &str) -> bool {
    match want {
        serde_json::Value::Array(items) => items.iter().any(|it| value_matches(it, got)),
        other => scalar_to_string(other).eq_ignore_ascii_case(got),
    }
}

fn scalar_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Evaluate a legacy `id == value` / `id != value` expression against facts.
fn eval_expr(expr: &str, facts: &Facts) -> bool {
    let (id, want, negate) = if let Some((l, r)) = expr.split_once("==") {
        (l.trim(), r.trim(), false)
    } else if let Some((l, r)) = expr.split_once("!=") {
        (l.trim(), r.trim(), true)
    } else {
        return false;
    };
    let got = facts.get(id).unwrap_or("");
    let want = want.trim_matches(|c| c == '"' || c == '\'');
    got.eq_ignore_ascii_case(want) != negate
}

// ---------------------------------------------------------------------------
// hardware detection (read-only; safe in dry-run)
// ---------------------------------------------------------------------------

fn cmd_stdout(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// GPU vendor: nvidia / amd / intel / virtio / vmware / qemu / vbox / unknown.
/// Prefers `lspci`; falls back to PCI vendor ids under `/sys/class/drm`.
fn detect_gpu() -> String {
    if let Some(out) = cmd_stdout("lspci", &[]) {
        for line in out.lines() {
            let l = line.to_ascii_lowercase();
            if l.contains("vga compatible controller")
                || l.contains("3d controller")
                || l.contains("display controller")
            {
                if let Some(v) = gpu_vendor_from_text(&l) {
                    return v.into();
                }
            }
        }
    }
    // Fallback: PCI vendor id of the first DRM card.
    if let Ok(cards) = std::fs::read_dir("/sys/class/drm") {
        for card in cards.flatten() {
            let vid = card.path().join("device/vendor");
            if let Ok(id) = std::fs::read_to_string(&vid) {
                if let Some(v) = gpu_vendor_from_pci_id(id.trim()) {
                    return v.into();
                }
            }
        }
    }
    "unknown".into()
}

fn gpu_vendor_from_text(l: &str) -> Option<&'static str> {
    if l.contains("nvidia") {
        Some("nvidia")
    } else if l.contains("amd") || l.contains("advanced micro devices") || l.contains("ati ") || l.contains("radeon") {
        Some("amd")
    } else if l.contains("intel") {
        Some("intel")
    } else if l.contains("vmware") {
        Some("vmware")
    } else if l.contains("virtio") || l.contains("red hat") {
        Some("virtio")
    } else if l.contains("virtualbox") || l.contains("innotek") {
        Some("vbox")
    } else if l.contains("qxl") || l.contains("cirrus") || l.contains("bochs") {
        Some("qemu")
    } else {
        None
    }
}

fn gpu_vendor_from_pci_id(id: &str) -> Option<&'static str> {
    match id.to_ascii_lowercase().as_str() {
        "0x10de" => Some("nvidia"),
        "0x1002" | "0x1022" => Some("amd"),
        "0x8086" => Some("intel"),
        "0x15ad" => Some("vmware"),
        "0x1af4" => Some("virtio"),
        "0x80ee" => Some("vbox"),
        "0x1234" | "0x1b36" => Some("qemu"),
        _ => None,
    }
}

/// CPU vendor: intel / amd / unknown (from `/proc/cpuinfo`).
fn detect_cpu() -> String {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    for line in cpuinfo.lines() {
        if let Some(v) = line.strip_prefix("vendor_id") {
            let v = v.trim_start_matches([':', ' ', '\t']).trim();
            return match v {
                "GenuineIntel" => "intel".into(),
                "AuthenticAMD" => "amd".into(),
                other if !other.is_empty() => other.to_ascii_lowercase(),
                _ => "unknown".into(),
            };
        }
    }
    "unknown".into()
}

/// Virtualization technology, or `none` on bare metal (`systemd-detect-virt`).
/// `systemd-detect-virt` prints `none` (exit 1) on bare metal, so stdout is
/// authoritative regardless of exit status.
fn detect_virt() -> String {
    match cmd_stdout("systemd-detect-virt", &[]) {
        Some(s) if !s.trim().is_empty() => s.trim().to_ascii_lowercase(),
        _ => "none".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;

    fn parse(json: &str) -> Manifest {
        Manifest::from_str(json).unwrap()
    }

    #[test]
    fn object_condition_is_an_and_over_keys() {
        let facts = Facts::from_pairs(&[("gpu", "nvidia"), ("is_vm", "false")]);
        let m = parse(r#"{"schema_version":"1.0.0","files":[
            {"path":"a","when":{"gpu":"nvidia","is_vm":false}},
            {"path":"b","when":{"gpu":"nvidia","is_vm":true}}]}"#);
        assert!(m.files[0].when.as_ref().unwrap().holds(&facts));
        assert!(!m.files[1].when.as_ref().unwrap().holds(&facts));
    }

    #[test]
    fn array_value_is_an_or() {
        let facts = Facts::from_pairs(&[("gpu", "amd")]);
        let m = parse(r#"{"schema_version":"1.0.0","files":[
            {"path":"a","when":{"gpu":["nvidia","amd"]}}]}"#);
        assert!(m.files[0].when.as_ref().unwrap().holds(&facts));
    }

    #[test]
    fn legacy_expr_form_still_works() {
        let facts = Facts::from_pairs(&[("gpu", "nvidia")]);
        let m = parse(r#"{"schema_version":"1.0.0","files":[
            {"path":"a","when":"gpu == nvidia"},
            {"path":"b","when":"gpu != nvidia"}]}"#);
        assert!(m.files[0].when.as_ref().unwrap().holds(&facts));
        assert!(!m.files[1].when.as_ref().unwrap().holds(&facts));
    }

    #[test]
    fn missing_fact_is_false_not_an_error() {
        let facts = Facts::default();
        let m = parse(r#"{"schema_version":"1.0.0","files":[{"path":"a","when":{"gpu":"nvidia"}}]}"#);
        assert!(!m.files[0].when.as_ref().unwrap().holds(&facts));
    }

    #[test]
    fn resolve_drops_unmatched_files_and_folds_matching_overlays() {
        let facts = Facts::from_pairs(&[("gpu", "nvidia")]);
        let mut m = parse(r#"{"schema_version":"1.0.0",
            "packages":["base"],
            "files":[
                {"path":"keep-always"},
                {"path":"nvidia-only","when":{"gpu":"nvidia"}},
                {"path":"amd-only","when":{"gpu":"amd"}}],
            "conditional":[
                {"when":{"gpu":"nvidia"},"packages":["nvidia-dkms"],
                 "files":[{"path":"from-overlay"}],
                 "post_install":["nvidia-xconfig"]},
                {"when":{"gpu":"amd"},"packages":["should-not-appear"]}]}"#);
        resolve(&mut m, &facts);
        let paths: Vec<&str> = m.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["keep-always", "nvidia-only", "from-overlay"]);
        assert!(m.packages.contains(&"nvidia-dkms".to_string()));
        assert!(!m.packages.contains(&"should-not-appear".to_string()));
        assert_eq!(m.post_install, vec!["nvidia-xconfig".to_string()]);
        assert!(m.conditional.is_empty());
    }

    #[test]
    fn overlay_only_fills_an_unset_desktop() {
        let facts = Facts::from_pairs(&[("is_vm", "true")]);
        let mut base_set = parse(r#"{"schema_version":"1.0.0","desktop":"gnome",
            "conditional":[{"when":{"is_vm":true},"desktop":"niri"}]}"#);
        resolve(&mut base_set, &facts);
        assert_eq!(base_set.desktop.as_deref(), Some("gnome")); // base wins

        let mut base_unset = parse(r#"{"schema_version":"1.0.0",
            "conditional":[{"when":{"is_vm":true},"desktop":"niri"}]}"#);
        resolve(&mut base_unset, &facts);
        assert_eq!(base_unset.desktop.as_deref(), Some("niri")); // overlay fills it
    }

    #[test]
    fn detect_overrides_pin_a_fact() {
        let mut ov = BTreeMap::new();
        ov.insert("gpu".to_string(), "nvidia".to_string());
        ov.insert("cpu".to_string(), "auto".to_string());
        let facts = Facts::detect(&ov);
        assert_eq!(facts.get("gpu"), Some("nvidia")); // pinned
        assert!(facts.get("firmware").is_some()); // still auto-detected
    }
}
