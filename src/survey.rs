//! The survey system: author-defined first-run questions.
//!
//! A manifest can declare a `survey` of typed questions (text / secret /
//! boolean / select / multiselect / number / path). Their answers are injected
//! anywhere `{{id}}` appears in the manifest and drive `conditional_packages`.
//!
//! Substitution happens on the *raw* manifest text before the manifest is
//! parsed. Tokens live inside JSON strings (`"{{hostname}}"`, a command like
//! `"nmcli ... '{{wifi_ssid}}'"`) so the file stays valid JSON. Number/boolean
//! answers are still usable — in conditions (`swap_gb == 8`) and string fields.
//!
//! Answer precedence: `--answers` file > question `default` > interactive
//! prompt (only on a TTY) > error if `required`.

use crate::manifest::{ConditionalPackages, Question};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::Path;

/// Collected answers, plus which ids were `secret` (so values can be kept out
/// of logs by callers).
pub struct Answers {
    values: HashMap<String, String>,
    secrets: Vec<String>,
}

impl Answers {
    pub fn is_secret(&self, id: &str) -> bool {
        self.secrets.iter().any(|s| s == id)
    }

    /// Every resolved (id, value) — survey answers and variables — as owned
    /// pairs, for seeding the [`crate::conditions::Facts`] a `when` evaluates
    /// against.
    pub fn pairs(&self) -> impl Iterator<Item = (String, String)> + '_ {
        self.values.iter().map(|(k, v)| (k.clone(), v.clone()))
    }

    /// Seed lower-priority facts (auto-detected hardware: `gpu`, `scale`, …) so
    /// they fill `{{id}}` tokens too — but only where a survey answer or
    /// variable hasn't already set that id (those always win).
    pub fn add_base_facts(&mut self, facts: impl IntoIterator<Item = (String, String)>) {
        for (k, v) in facts {
            self.values.entry(k).or_insert(v);
        }
    }
}

/// Parse the `variables` and `survey` blocks out of the raw manifest (ignoring
/// everything else), then resolve every value.
///
/// `variables` are author-defined constants — a fixed accent colour, a
/// username — that fill the same `{{id}}` slots as survey answers but need no
/// prompting. They seed the substitution map first; a survey answer with the
/// same id overrides its variable (interactive input beats a static default).
pub fn collect(raw: &str, answers_path: Option<&Path>) -> Result<Answers> {
    #[derive(serde::Deserialize)]
    struct Blocks {
        #[serde(default)]
        survey: Vec<Question>,
        #[serde(default)]
        variables: std::collections::BTreeMap<String, serde_json::Value>,
    }
    let blocks = serde_json::from_str::<Blocks>(raw).context("reading variables/survey blocks")?;
    let survey = blocks.survey;

    let provided: HashMap<String, serde_json::Value> = match answers_path {
        Some(p) => {
            let txt = std::fs::read_to_string(p)
                .with_context(|| format!("reading answers file {}", p.display()))?;
            serde_json::from_str(&txt).context("answers file must be a JSON object")?
        }
        None => HashMap::new(),
    };

    let interactive = std::io::stdin().is_terminal();
    let mut values = HashMap::new();
    let mut secrets = Vec::new();

    // Variables first, so a like-named survey answer (resolved below) wins.
    for (id, v) in &blocks.variables {
        values.insert(id.clone(), value_to_string(v));
    }

    for q in &survey {
        if q.qtype == "secret" {
            secrets.push(q.id.clone());
        }
        let answer = if let Some(v) = provided.get(&q.id) {
            let a = value_to_string(v);
            validate(q, &a).with_context(|| format!("answer for `{}` (from --answers)", q.id))?;
            a
        } else if let Some(d) = &q.default {
            let a = value_to_string(d);
            validate(q, &a).with_context(|| format!("default for `{}`", q.id))?;
            a
        } else if interactive {
            prompt_valid(q)?
        } else if q.required {
            bail!(
                "survey question `{}` is required but unanswered (provide --answers or a default)",
                q.id
            );
        } else {
            String::new()
        };
        values.insert(q.id.clone(), answer);
    }
    Ok(Answers { values, secrets })
}

/// Replace every `{{id}}` in the raw manifest with its (JSON-escaped) answer.
pub fn substitute(raw: &str, answers: &Answers) -> String {
    let mut out = raw.to_string();
    for (id, val) in &answers.values {
        let token = format!("{{{{{id}}}}}");
        out = out.replace(&token, &json_escape_inner(val));
    }
    out
}

/// Packages contributed by conditions that hold given the answers.
pub fn conditional_packages(conds: &[ConditionalPackages], answers: &Answers) -> Vec<String> {
    let mut out = Vec::new();
    for c in conds {
        if eval(&c.condition, answers) {
            out.extend(c.packages.iter().cloned());
        }
    }
    out
}

/// Evaluate a simple `id == value` / `id != value` condition.
fn eval(cond: &str, answers: &Answers) -> bool {
    let (id, want, negate) = if let Some((l, r)) = cond.split_once("==") {
        (l.trim(), r.trim(), false)
    } else if let Some((l, r)) = cond.split_once("!=") {
        (l.trim(), r.trim(), true)
    } else {
        return false;
    };
    let got = answers.values.get(id).map(String::as_str).unwrap_or("");
    let want = want.trim_matches(|c| c == '"' || c == '\'');
    (got == want) != negate
}

fn prompt(q: &Question) -> Result<String> {
    let hint = match q.qtype.as_str() {
        "boolean" => " [true/false]".to_string(),
        "select" | "multiselect" => format!(" [{}]", q.options.join("/")),
        _ => String::new(),
    };
    print!("{}{hint}: ", q.label);
    std::io::stdout().flush().ok();
    let mut line = String::new();
    // read_line returning 0 means EOF — bail rather than loop forever on a
    // closed stdin (a required question with no more input).
    if std::io::stdin().read_line(&mut line)? == 0 {
        bail!("input closed before `{}` was answered", q.id);
    }
    Ok(line.trim().to_string())
}

/// Prompt until the answer validates (or is an accepted empty for an optional
/// question). Re-prompts with the reason on invalid input.
fn prompt_valid(q: &Question) -> Result<String> {
    loop {
        let a = prompt(q)?;
        if a.is_empty() {
            if q.required {
                println!("  · this one is required — please enter a value");
                continue;
            }
            return Ok(a);
        }
        match validate(q, &a) {
            Ok(()) => return Ok(a),
            Err(e) => println!("  · {e}"),
        }
    }
}

/// Check an answer against the question's declared validation — type, `min`/
/// `max` (numeric range or text length), `options` (enum) and `pattern`
/// (anchored regex). An empty answer to an optional question is fine.
fn validate(q: &Question, answer: &str) -> Result<()> {
    if answer.is_empty() {
        return Ok(());
    }
    match q.qtype.as_str() {
        "number" => {
            let n: f64 = answer
                .parse()
                .map_err(|_| anyhow::anyhow!("`{answer}` is not a number"))?;
            if let Some(min) = q.min {
                if n < min {
                    bail!("must be at least {min}");
                }
            }
            if let Some(max) = q.max {
                if n > max {
                    bail!("must be at most {max}");
                }
            }
        }
        "select" => enum_check(q, answer)?,
        "multiselect" => {
            for item in answer.split_whitespace() {
                enum_check(q, item)?;
            }
        }
        _ => {
            // text / secret / path / boolean — bound the length.
            let len = answer.chars().count() as f64;
            if let Some(min) = q.min {
                if len < min {
                    bail!("must be at least {} character(s)", min as u64);
                }
            }
            if let Some(max) = q.max {
                if len > max {
                    bail!("must be at most {} character(s)", max as u64);
                }
            }
        }
    }
    if let Some(pat) = &q.pattern {
        let re = regex::Regex::new(&format!("^(?:{pat})$"))
            .map_err(|e| anyhow::anyhow!("invalid pattern for `{}`: {e}", q.id))?;
        if !re.is_match(answer) {
            bail!("doesn't match the required format ({pat})");
        }
    }
    Ok(())
}

fn enum_check(q: &Question, item: &str) -> Result<()> {
    if !q.options.is_empty() && !q.options.iter().any(|o| o == item) {
        bail!("`{item}` must be one of: {}", q.options.join(", "));
    }
    Ok(())
}

/// Render a JSON value as the string to inject (bool/number → bare literal,
/// string → its text, array → space-joined).
fn value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Array(a) => a
            .iter()
            .map(value_to_string)
            .collect::<Vec<_>>()
            .join(" "),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Escape a value for substitution *inside* an existing JSON context. Plain
/// numbers/booleans pass through unchanged; strings get quotes/backslashes
/// escaped so they stay valid JSON.
fn json_escape_inner(s: &str) -> String {
    let quoted = serde_json::Value::String(s.to_string()).to_string();
    quoted[1..quoted.len() - 1].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ConditionalPackages;

    #[test]
    fn variables_fill_substitution_tokens() {
        let raw = r##"{"schema_version":"1.0.0",
            "variables":{"accent":"#ff5d5d","username":"matt"},
            "meta":{"name":"{{username}}'s rice"},
            "files":[{"path":"~/.config/x","content":"color={{accent}}"}]}"##;
        let ans = collect(raw, None).unwrap();
        let out = substitute(raw, &ans);
        assert!(out.contains("matt's rice"), "{out}");
        assert!(out.contains("color=#ff5d5d"), "{out}");
        assert!(!out.contains("{{accent}}") && !out.contains("{{username}}"));
        // The substituted manifest still parses.
        assert!(crate::manifest::Manifest::from_str(&out).is_ok());
    }

    #[test]
    fn survey_answer_overrides_like_named_variable() {
        // variable user=matt, but a survey default resolves the same id — the
        // survey answer (interactive/default) wins over the static variable.
        let raw = r#"{"schema_version":"1.0.0",
            "variables":{"user":"matt"},
            "survey":[{"id":"user","type":"text","label":"User","default":"alice"}]}"#;
        let ans = collect(raw, None).unwrap();
        assert_eq!(substitute("{{user}}", &ans), "alice");
    }

    #[test]
    fn variables_drive_conditional_packages() {
        // A variable can gate conditional packages just like a survey answer.
        let raw = r#"{"schema_version":"1.0.0","variables":{"gpu":"nvidia"}}"#;
        let ans = collect(raw, None).unwrap();
        let conds = vec![ConditionalPackages {
            condition: "gpu == nvidia".into(),
            packages: vec!["nvidia-dkms".into()],
        }];
        assert_eq!(conditional_packages(&conds, &ans), vec!["nvidia-dkms".to_string()]);
    }

    #[test]
    fn number_and_bool_variables_inject_as_bare_literals() {
        let raw = r#"{"schema_version":"1.0.0","variables":{"gaps":8,"blur":true}}"#;
        let ans = collect(raw, None).unwrap();
        // Bare (unquoted) in a JSON numeric/boolean slot.
        assert_eq!(substitute(r#"{"n":{{gaps}},"b":{{blur}}}"#, &ans), r#"{"n":8,"b":true}"#);
    }

    #[test]
    fn no_variables_block_is_fine() {
        let ans = collect(r#"{"schema_version":"1.0.0"}"#, None).unwrap();
        assert_eq!(substitute("nothing to do", &ans), "nothing to do");
    }

    fn q(json: &str) -> Question {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn number_range_is_enforced() {
        let port = q(r#"{"id":"port","type":"number","label":"Port","min":1,"max":65535}"#);
        assert!(validate(&port, "8080").is_ok());
        assert!(validate(&port, "0").is_err());
        assert!(validate(&port, "70000").is_err());
        assert!(validate(&port, "notnum").is_err());
    }

    #[test]
    fn text_length_and_pattern_are_enforced() {
        let user = q(r#"{"id":"user","type":"text","label":"User","min":2,"max":32,"pattern":"[a-z_][a-z0-9_-]*"}"#);
        assert!(validate(&user, "matt").is_ok());
        assert!(validate(&user, "a").is_err()); // too short
        assert!(validate(&user, "1bad").is_err()); // pattern (must start lowercase/_)
        assert!(validate(&user, "has space").is_err()); // anchored: space not allowed
    }

    #[test]
    fn select_answer_must_be_an_option() {
        let de = q(r#"{"id":"de","type":"select","label":"DE","options":["gnome","plasma","niri"]}"#);
        assert!(validate(&de, "plasma").is_ok());
        assert!(validate(&de, "xfce").is_err());
        let multi = q(r#"{"id":"apps","type":"multiselect","label":"Apps","options":["firefox","kitty"]}"#);
        assert!(validate(&multi, "firefox kitty").is_ok());
        assert!(validate(&multi, "firefox chrome").is_err());
    }

    #[test]
    fn empty_optional_answer_skips_validation() {
        let opt = q(r#"{"id":"x","type":"text","label":"X","min":5}"#);
        assert!(validate(&opt, "").is_ok());
    }

    #[test]
    fn collect_rejects_an_invalid_default() {
        // A default that violates its own validation fails fast at load time.
        let raw = r#"{"schema_version":"1.0.0","survey":[
            {"id":"port","type":"number","label":"Port","default":99999,"max":65535}]}"#;
        assert!(collect(raw, None).is_err());
    }
}
