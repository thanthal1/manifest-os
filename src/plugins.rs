//! Plugins — third-party manifest blocks, without bloating the core.
//!
//! A **plugin** teaches the manifest a new top-level block (`docker`,
//! `tailscale`, `ollama`, `kubernetes`, …) by declaring how that block *expands*
//! into the primitives the engine already understands: `packages`, `services`,
//! `files`, `users`, `pre_install`/`post_install`, `conditional`, and so on.
//! The core engine never learns what "docker" means — by the time it parses a
//! manifest, [`expand`] has already folded every plugin block into ordinary
//! core fields and removed it. New capabilities grow at the edges; the core
//! stays small.
//!
//! A plugin is itself **declarative and reviewable** — no arbitrary code runs at
//! expand time. Its `expands` is a slice of manifest, and its `conditional`
//! rules reuse the exact same [`Condition`]/`when` engine as the rest of the
//! schema, evaluated against the *block's own fields* (so `docker: {rootless:
//! true}` can pull in extra packages). Anything genuinely imperative goes
//! through the plugin's `pre_install`/`post_install`, which are reviewed like
//! any other hook.
//!
//! Plugins come from two places:
//!   * **Inline** — a manifest's own `plugins: [ … ]` array, so a shared
//!     manifest is fully self-contained (the reviewer sees the expansion source
//!     right next to its use). Inline wins on name.
//!   * **A plugins directory** — `*.json` under `/usr/share/manifest-os/plugins`
//!     (bundled), `/etc/manifest/plugins`, `~/.config/manifest/plugins`, or a
//!     repo-local `plugins/` for development.
//!
//! ## Field interpolation
//! Inside `expands`/`conditional`, `{{field}}` is replaced by the block's field
//! of that name — typed when the whole string is a single token
//! (`"port": "{{port}}"` → the number `8080`), stringified when embedded
//! (`"…--authkey {{authkey}}"`).
//!
//! ## Two layers of conditions
//! A plugin's own `conditional` rules see **block fields** and resolve here, at
//! expand time. To branch on *hardware* instead, a plugin simply emits a normal
//! top-level `conditional` in its `expands`; that flows into the manifest and is
//! resolved later by [`crate::conditions`] with the real hardware facts.

use crate::conditions::Facts;
use crate::manifest::{Condition, Manifest};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;

/// A plugin descriptor: how a custom block expands into core primitives.
#[derive(Debug, Clone, Deserialize)]
pub struct Plugin {
    /// The plugin's name (and default block key it claims).
    pub plugin: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// The block key(s) this plugin handles. Defaults to `[plugin]`.
    #[serde(default)]
    pub provides: Vec<String>,
    /// Block fields that must be present (validation before expansion).
    #[serde(default)]
    pub requires: Vec<String>,
    /// The always-applied slice of manifest this block expands into.
    #[serde(default)]
    pub expands: Map<String, Value>,
    /// Extra slices applied only when their `when` holds against block fields.
    #[serde(default)]
    pub conditional: Vec<Rule>,
}

/// One `when`-gated slice inside a plugin, evaluated against the block's fields.
#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    pub when: Condition,
    #[serde(flatten)]
    pub body: Map<String, Value>,
}

impl Plugin {
    fn keys(&self) -> Vec<String> {
        if self.provides.is_empty() {
            vec![self.plugin.clone()]
        } else {
            self.provides.clone()
        }
    }
}

/// Expand every plugin block in a manifest JSON string into core primitives,
/// returning the rewritten JSON. A no-op (returns the input untouched) when the
/// manifest has neither unknown blocks nor inline plugins. Errors if a block
/// has no plugin to claim it — that's a typo or a missing plugin, not something
/// to silently drop.
pub fn expand(json: &str) -> Result<String> {
    let manifest = Manifest::from_str(json)?;
    if manifest.extensions.is_empty() && manifest.plugins.is_empty() {
        return Ok(json.to_string());
    }

    let registry = load_registry(&manifest.plugins)?;
    let mut root: Value =
        serde_json::from_str(json).context("re-parsing manifest as JSON for plugin expansion")?;
    let obj = root
        .as_object_mut()
        .context("manifest root must be a JSON object")?;

    // Expand each unknown block, collecting fragments to merge after removal so
    // a plugin can't accidentally see another plugin's half-merged output.
    let mut fragments: Vec<Map<String, Value>> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    for key in manifest.extensions.keys() {
        match registry.get(key) {
            Some(plugin) => {
                let block = obj.get(key).cloned().unwrap_or(Value::Null);
                let frag = render(plugin, &block)
                    .with_context(|| format!("expanding plugin block `{key}`"))?;
                fragments.push(frag);
                obj.remove(key);
            }
            None => unknown.push(key.clone()),
        }
    }
    if !unknown.is_empty() {
        bail!(
            "unknown manifest block(s): {} — the core schema has no such field and no plugin provides them. \
             Define a plugin (inline `plugins` or a plugins directory) or fix the block name.",
            unknown.join(", ")
        );
    }

    // Inline plugin defs have done their job; drop them so the result is a plain
    // core manifest (rollback/replay needs no plugins).
    obj.remove("plugins");
    for frag in fragments {
        deep_merge(obj, frag);
    }

    serde_json::to_string_pretty(&root).context("serializing expanded manifest")
}

/// Render one plugin against its block value, producing the manifest fragment it
/// contributes (fields substituted, matching conditionals folded in).
fn render(plugin: &Plugin, block: &Value) -> Result<Map<String, Value>> {
    let empty = Map::new();
    let fields = block.as_object().unwrap_or(&empty);

    for req in &plugin.requires {
        if !fields.contains_key(req) {
            bail!(
                "plugin `{}` requires field `{}` in the `{}` block",
                plugin.plugin,
                req,
                plugin.plugin
            );
        }
    }

    let mut out = Map::new();
    if let Value::Object(rendered) = subst(&Value::Object(plugin.expands.clone()), fields) {
        deep_merge(&mut out, rendered);
    }

    let facts = block_facts(fields);
    for rule in &plugin.conditional {
        if rule.when.holds(&facts) {
            if let Value::Object(rendered) = subst(&Value::Object(rule.body.clone()), fields) {
                deep_merge(&mut out, rendered);
            }
        }
    }
    Ok(out)
}

/// Block fields as `Facts` for the plugin's own `when` rules (scalars only).
fn block_facts(fields: &Map<String, Value>) -> Facts {
    let mut f = Facts::default();
    f.overlay(
        fields
            .iter()
            .filter_map(|(k, v)| scalar_str(v).map(|s| (k.clone(), s))),
    );
    f
}

/// Recursively substitute `{{field}}` tokens in a value from the block's fields.
fn subst(v: &Value, fields: &Map<String, Value>) -> Value {
    match v {
        Value::String(s) => subst_string(s, fields),
        Value::Array(a) => Value::Array(a.iter().map(|e| subst(e, fields)).collect()),
        Value::Object(o) => {
            Value::Object(o.iter().map(|(k, val)| (k.clone(), subst(val, fields))).collect())
        }
        other => other.clone(),
    }
}

fn subst_string(s: &str, fields: &Map<String, Value>) -> Value {
    // A string that is exactly one token → the field's *typed* value, so
    // `"port": "{{port}}"` yields a JSON number, not a quoted string.
    if let Some(inner) = s.strip_prefix("{{").and_then(|x| x.strip_suffix("}}")) {
        let key = inner.trim();
        if !key.contains("{{") && !key.contains("}}") {
            return fields.get(key).cloned().unwrap_or(Value::String(String::new()));
        }
    }
    // Otherwise textual replacement of each scalar token embedded in the string.
    let mut out = s.to_string();
    for (k, val) in fields {
        if let Some(sc) = scalar_str(val) {
            out = out.replace(&format!("{{{{{k}}}}}"), &sc);
        }
    }
    Value::String(out)
}

/// A JSON scalar as a string, or `None` for arrays/objects/null.
fn scalar_str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Merge `src` into `target`: arrays concatenate, objects merge recursively,
/// and on a scalar/type conflict the existing (`target`) value wins — so a
/// plugin adds to the manifest but never overrides what the author set.
fn deep_merge(target: &mut Map<String, Value>, src: Map<String, Value>) {
    for (k, v) in src {
        match target.get_mut(&k) {
            None => {
                target.insert(k, v);
            }
            Some(existing) => merge_value(existing, v),
        }
    }
}

fn merge_value(a: &mut Value, b: Value) {
    match (a, b) {
        (Value::Object(ao), Value::Object(bo)) => deep_merge(ao, bo),
        (Value::Array(aa), Value::Array(ba)) => aa.extend(ba),
        // scalar, or a type mismatch: keep what's already there (author wins).
        _ => {}
    }
}

/// Build the block-key → plugin map from directory plugins (lower priority) then
/// inline definitions (which override on name).
fn load_registry(inline: &[Value]) -> Result<HashMap<String, Plugin>> {
    let mut plugins: Vec<Plugin> = Vec::new();
    for dir in search_dirs() {
        load_dir(&dir, &mut plugins);
    }
    for v in inline {
        let p: Plugin = serde_json::from_value(v.clone())
            .context("parsing an inline `plugins` entry")?;
        plugins.push(p);
    }

    let mut map = HashMap::new();
    for p in plugins {
        for key in p.keys() {
            map.insert(key, p.clone());
        }
    }
    Ok(map)
}

/// Read every `*.json` in `dir` as a plugin. A malformed file is warned about
/// and skipped rather than breaking unrelated installs — if the manifest
/// actually needs it, expansion later fails loudly with "unknown block".
fn load_dir(dir: &PathBuf, out: &mut Vec<Plugin>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return, // dir absent — normal
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Plugin>(&s).ok())
        {
            Some(p) => out.push(p),
            None => eprintln!("  · warning: skipping malformed plugin {}", path.display()),
        }
    }
}

/// Directories searched for plugin `*.json`, lowest priority first.
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("plugins")]; // repo-local (dev)
    if let Ok(exe) = std::env::current_exe() {
        if let Some(bin) = exe.parent() {
            dirs.push(bin.join("plugins"));
            if let Some(prefix) = bin.parent() {
                dirs.push(prefix.join("share/manifest-os/plugins"));
            }
        }
    }
    dirs.push(PathBuf::from("/usr/share/manifest-os/plugins"));
    dirs.push(PathBuf::from("/etc/manifest/plugins"));
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(home).join(".config/manifest/plugins"));
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Manifest;

    /// Expand with only the manifest's inline plugins (no dir lookup surprises in
    /// tests: the tests define what they use inline).
    fn expand_inline(json: &str) -> Manifest {
        let out = expand(json).expect("expansion succeeds");
        Manifest::from_str(&out).expect("expanded manifest parses")
    }

    const DOCKER: &str = r#"{
        "plugin": "docker",
        "provides": ["docker"],
        "expands": {
            "packages": ["docker", "docker-compose"],
            "services": { "system": ["docker.socket"] }
        },
        "conditional": [
            { "when": { "rootless": true }, "packages": ["docker-rootless-extras"] }
        ]
    }"#;

    #[test]
    fn block_expands_into_core_packages_and_services() {
        let json = format!(
            r#"{{"schema_version":"1.0.0","plugins":[{DOCKER}],"docker":{{}}}}"#
        );
        let m = expand_inline(&json);
        assert!(m.packages.contains(&"docker".to_string()));
        assert!(m.packages.contains(&"docker-compose".to_string()));
        assert!(m.services.system.contains(&"docker.socket".to_string()));
        // Fully consumed: no leftover unknown block, no inline defs.
        assert!(m.extensions.is_empty());
        assert!(m.plugins.is_empty());
    }

    #[test]
    fn block_field_drives_a_conditional() {
        let on = format!(r#"{{"schema_version":"1.0.0","plugins":[{DOCKER}],"docker":{{"rootless":true}}}}"#);
        assert!(expand_inline(&on).packages.contains(&"docker-rootless-extras".to_string()));
        let off = format!(r#"{{"schema_version":"1.0.0","plugins":[{DOCKER}],"docker":{{"rootless":false}}}}"#);
        assert!(!expand_inline(&off).packages.contains(&"docker-rootless-extras".to_string()));
    }

    #[test]
    fn tokens_are_typed_when_whole_and_stringified_when_embedded() {
        let plugin = r#"{
            "plugin": "svc",
            "expands": {
                "files": [{"path":"/etc/svc.conf","content":"port={{port}} name={{name}}"}],
                "boot": {"loader":"grub","timeout":"{{timeout}}"}
            }
        }"#;
        let json = format!(
            r#"{{"schema_version":"1.0.0","plugins":[{plugin}],"svc":{{"port":8080,"name":"api","timeout":3}}}}"#
        );
        let m = expand_inline(&json);
        assert_eq!(m.files[0].content, "port=8080 name=api");
        // Whole-string token kept its JSON number type through to the typed field.
        assert_eq!(m.boot.as_ref().unwrap().timeout, Some(3));
    }

    #[test]
    fn plugin_packages_add_to_author_packages_not_replace() {
        let json = format!(
            r#"{{"schema_version":"1.0.0","packages":["vim"],"plugins":[{DOCKER}],"docker":{{}}}}"#
        );
        let m = expand_inline(&json);
        assert!(m.packages.contains(&"vim".to_string()));
        assert!(m.packages.contains(&"docker".to_string()));
    }

    #[test]
    fn required_field_is_enforced() {
        let plugin = r#"{"plugin":"tailscale","requires":["authkey"],"expands":{"packages":["tailscale"]}}"#;
        let json = format!(r#"{{"schema_version":"1.0.0","plugins":[{plugin}],"tailscale":{{}}}}"#);
        assert!(expand(&json).is_err());
    }

    #[test]
    fn unknown_block_without_a_plugin_is_an_error() {
        let json = r#"{"schema_version":"1.0.0","totally_made_up":{"x":1}}"#;
        assert!(expand(json).is_err());
    }

    #[test]
    fn no_plugins_is_an_untouched_passthrough() {
        let json = r#"{"schema_version":"1.0.0","packages":["vim"]}"#;
        assert_eq!(expand(json).unwrap(), json);
    }
}
