//! Strict legacy spec loading for the language-only runner comparison.

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

/// A single experiment's execution contract; durable identifiers retain their legacy names.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Spec {
    pub intent: Option<String>,
    #[serde(deserialize_with = "default_on_null")]
    pub id: String,
    #[serde(deserialize_with = "default_on_null")]
    pub model: String,
    #[serde(deserialize_with = "default_on_null")]
    pub system: String,
    #[serde(deserialize_with = "string_list")]
    pub context: Vec<String>,
    #[serde(deserialize_with = "string_list")]
    pub tools: Vec<String>,
    #[serde(deserialize_with = "string_list")]
    pub mcp: Vec<String>,
    #[serde(deserialize_with = "string_list")]
    pub post_turn: Vec<String>,
    #[serde(deserialize_with = "string_list")]
    pub pre_tool: Vec<String>,
    #[serde(deserialize_with = "string_list")]
    pub env: Vec<String>,
    #[serde(deserialize_with = "default_on_null")]
    pub check_window: i64,
    #[serde(deserialize_with = "default_on_null")]
    pub budget: Budget,
    #[serde(deserialize_with = "string_list")]
    pub tasks: Vec<String>,
    #[serde(deserialize_with = "default_on_null")]
    pub trials: i64,
    #[serde(deserialize_with = "default_on_null")]
    pub hypothesis: String,
    #[serde(skip)]
    pub path: String,
    #[serde(skip)]
    pub sha: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Budget {
    #[serde(deserialize_with = "default_on_null")]
    pub turns: i64,
    #[serde(deserialize_with = "default_on_null")]
    pub tool_calls: i64,
    #[serde(deserialize_with = "default_on_null")]
    pub usd: f64,
    #[serde(deserialize_with = "default_on_null")]
    pub seconds: i64,
}

// Go leaves concrete fields at their default when YAML or JSON contains null.
pub(crate) fn default_on_null<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

// yaml.v3 omits null sequence entries when decoding a slice of strings.
fn string_list<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    let entries = Option::<Vec<Option<String>>>::deserialize(deserializer)?;
    Ok(entries.unwrap_or_default().into_iter().flatten().collect())
}

/// Load the first YAML document, rejecting unknown fields and missing required values.
pub fn load(path: &str) -> Result<Spec, String> {
    let raw = std::fs::read(path)
        .map_err(|error| format!("open {path}: {}", crate::process::io_error(&error)))?;
    if String::from_utf8_lossy(&raw)
        .lines()
        .all(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
    {
        return Err(format!(
            "{path}: EOF (unknown fields are refused — SCHEMA.md)"
        ));
    }
    let document = serde_yaml::Deserializer::from_slice(&raw)
        .next()
        .ok_or_else(|| format!("{path}: EOF (unknown fields are refused — SCHEMA.md)"))?;
    let mut spec: Spec = default_on_null(document).map_err(|error| {
        let text = error.to_string();
        let diagnostic = if text.contains("unknown field `") {
            let field = text.split('`').nth(1).unwrap_or("");
            let line = error.location().map_or(1, |location| location.line());
            let kind = if text.starts_with("budget:") {
                "Budget"
            } else {
                "Spec"
            };
            format!(
                "yaml: unmarshal errors:\n  line {line}: field {field} not found in type lab.{kind}"
            )
        } else {
            format!("yaml: {text}")
        };
        format!("{path}: {diagnostic} (unknown fields are refused — SCHEMA.md)")
    })?;
    if let Some(intent) = &spec.intent {
        if intent.contains('-') {
            return Err(format!(
                "{path}: intent: retired format; use a Director ULID"
            ));
        }
        if intent.len() != 26
            || !intent
                .bytes()
                .all(|c| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&c))
            || intent.as_bytes()[0] > b'7'
        {
            return Err(format!("{path}: intent must be a Director ULID"));
        }
    }
    let bad: Vec<_> = [
        ("id", spec.id.is_empty()),
        ("tasks", spec.tasks.is_empty()),
        ("trials", spec.trials <= 0),
        ("budget.turns", spec.budget.turns <= 0),
        ("budget.tool_calls", spec.budget.tool_calls < 0),
        ("budget.seconds", spec.budget.seconds < 0),
        (
            "model (required; PINNED.md pinned: is the default to copy)",
            spec.model.is_empty(),
        ),
        (
            "mcp (attach enters behind its own experiment)",
            !spec.mcp.is_empty(),
        ),
    ]
    .into_iter()
    .filter_map(|(name, invalid)| invalid.then_some(name))
    .collect();
    if !bad.is_empty() {
        return Err(format!("{path}: invalid: {}", bad.join(", ")));
    }
    spec.sha = format!("{:x}", Sha256::digest(&raw));
    path.clone_into(&mut spec.path);
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const VALID: &str = "id: x\nmodel: fake\ntasks: [tasks/fx]\ntrials: 1\nbudget: {turns: 1}\nhypothesis: 'fixture parity'\n";

    fn load_text(text: &str) -> Result<Spec, String> {
        let mut file = tempfile::NamedTempFile::new().map_err(|error| error.to_string())?;
        file.write_all(text.as_bytes())
            .map_err(|error| error.to_string())?;
        load(file.path().to_str().ok_or("non-UTF8 temporary path")?)
    }

    #[test]
    fn unknown_field_reports_go_contract() -> Result<(), String> {
        let error = load_text(&format!("{VALID}surprise: field\n"))
            .err()
            .ok_or("unknown field accepted")?;
        assert!(error.ends_with("yaml: unmarshal errors:\n  line 7: field surprise not found in type lab.Spec (unknown fields are refused — SCHEMA.md)"), "{error}");
        Ok(())
    }

    #[test]
    fn director_intent_is_optional_and_ulid_only() -> Result<(), String> {
        assert!(load_text(VALID)?.intent.is_none());
        let id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        assert_eq!(
            load_text(&format!("{VALID}intent: {id}\n"))?
                .intent
                .as_deref(),
            Some(id)
        );
        for retired in [concat!("INT", "-0001"), concat!("EXP", "-1")] {
            let error = load_text(&format!("{VALID}intent: {retired}\n"))
                .err()
                .ok_or("retired format accepted")?;
            assert!(error.contains("retired format"), "{error}");
        }
        for invalid in [
            "hello",
            "81ARZ3NDEKTSV4RRFFQ69G5FAV",
            "01ARZ3NDEKTSV4RRFFQ69G5FAI",
        ] {
            assert!(load_text(&format!("{VALID}intent: {invalid}\n")).is_err());
        }
        Ok(())
    }

    #[test]
    fn flow_and_block_environment_requests_agree() -> Result<(), String> {
        let flow = load_text(&format!("{VALID}env: [NAME] # note\n"))?;
        let block = load_text(&format!("{VALID}env:\n  - NAME\n"))?;
        assert_eq!(flow.env, block.env);
        assert!(load_text(&format!("{VALID}env: NAME\n")).is_err());
        Ok(())
    }

    #[test]
    fn specs_need_no_governance_id() -> Result<(), String> {
        load_text(
            "id: x
model: fake
tasks: [tasks/fx]
trials: 1
budget: {turns: 1}
",
        )?;
        Ok(())
    }

    #[test]
    fn load_first_document_and_hash_original_bytes() -> Result<(), String> {
        let text = format!("{VALID}---\nunknown: ignored second document\n");
        let spec = load_text(&text)?;
        assert_eq!(spec.sha, format!("{:x}", Sha256::digest(text.as_bytes())));
        assert_eq!(spec.id, "x");
        Ok(())
    }

    #[test]
    fn null_fields_and_sequence_entries_match_go_defaults() -> Result<(), String> {
        let spec = load_text(&format!(
            "{VALID}system: null\ncontext: null\ntools: [null, shell]\nmcp: null\npost_turn: null\npre_tool: null\nenv: null\ncheck_window: null\n"
        ))?;
        assert!(spec.system.is_empty() && spec.context.is_empty());
        assert_eq!(spec.tools, ["shell"]);
        assert!(spec.mcp.is_empty() && spec.post_turn.is_empty() && spec.pre_tool.is_empty());
        assert_eq!(spec.env, Vec::<String>::new());
        assert_eq!(spec.check_window, 0);
        let budget: Budget =
            serde_yaml::from_str("turns: null\ntool_calls: null\nusd: null\nseconds: null")
                .map_err(|error| error.to_string())?;
        assert_eq!((budget.turns, budget.tool_calls, budget.seconds), (0, 0, 0));
        assert!(budget.usd.abs() < f64::EPSILON);
        Ok(())
    }

    #[test]
    fn missing_spec_path_surfaces_open_error() {
        if let Err(error) = load("/nonexistent/spec.yaml") {
            assert!(error.contains("open /nonexistent/spec.yaml"));
            assert!(error.contains("no such file"));
        } else {
            panic!("expected open error");
        }
    }

    #[test]
    fn null_required_values_fail_validation_instead_of_yaml_decode() -> Result<(), String> {
        for text in [
            "null",
            "id: null\nmodel: null\ntasks: null\ntrials: null\nbudget: null\nhypothesis: null",
        ] {
            let error = load_text(text).err().ok_or("empty spec accepted")?;
            assert!(error.contains(": invalid:"), "{error}");
        }
        assert!(
            load_text("")
                .err()
                .ok_or("empty spec accepted")?
                .contains(": EOF")
        );
        Ok(())
    }

    #[test]
    fn negative_tool_calls_and_seconds_are_rejected() -> Result<(), String> {
        for (field, name) in [
            ("tool_calls: -1", "budget.tool_calls"),
            ("seconds: -1", "budget.seconds"),
        ] {
            let yaml = format!(
                "id: x
model: fake
tasks: [tasks/fx]
trials: 1
budget: {{turns: 1, {field}}}
hypothesis: 'fixture parity'
"
            );
            let error = load_text(&yaml).err().ok_or(name)?;
            assert!(error.contains(&format!("invalid: {name}")), "{error}");
        }
        Ok(())
    }
}
