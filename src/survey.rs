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
}

/// Parse just the `survey` out of the raw manifest (ignoring everything else),
/// then resolve every answer.
pub fn collect(raw: &str, answers_path: Option<&Path>) -> Result<Answers> {
    #[derive(serde::Deserialize)]
    struct SurveyOnly {
        #[serde(default)]
        survey: Vec<Question>,
    }
    let survey = serde_json::from_str::<SurveyOnly>(raw)
        .context("reading survey block")?
        .survey;

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

    for q in &survey {
        if q.qtype == "secret" {
            secrets.push(q.id.clone());
        }
        let answer = if let Some(v) = provided.get(&q.id) {
            value_to_string(v)
        } else if let Some(d) = &q.default {
            value_to_string(d)
        } else if interactive {
            prompt(q)?
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
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
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
