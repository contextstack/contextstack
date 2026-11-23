//! Live output for streamed CLI work orders: the executor's stdout lines, read as the coding agent's
//! own streaming output, mapped to delta text. Only text is mapped; every other event (tool use,
//! usage, the agent's final result, the executor's own report) yields nothing, and the run's final
//! reply still carries the whole stdout.
//!
//! Dialects, chosen by the work order's executor (its inventory entry's binary, else its name):
//! - claude (`--output-format stream-json`): `stream_event` text deltas (with
//!   `--include-partial-messages`), else the text blocks of each `assistant` message.
//! - codex (`exec --json`): `item.completed` with an `agent_message` item; the older
//!   `{"msg":{"type":"agent_message"}}` form too.
//! - gemini (`--output-format stream-json`): `message` events from the assistant.
//! - cursor-agent and its model templates (`cli_binary` cursor-agent, `--output-format stream-json`):
//!   Claude's event shape, so the claude mapping (dialect follows the binary, not each template name).
//! - antigravity (`agy --output-format stream-json`): events keyed `event`; `step_update` text deltas
//!   on `agent_response` steps.
//!
//! Any other executor has no streaming mode: `stream: true` is refused `gateway.streaming_unsupported`.

use serde_json::Value;

/// A coding agent's streaming dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// Claude Code stream-json.
    Claude,
    /// Codex `--json` events.
    Codex,
    /// Gemini CLI stream-json.
    Gemini,
    /// Antigravity CLI (`agy`) stream-json.
    Antigravity,
}

fn by_binary(binary: &str) -> Option<Dialect> {
    let base = binary.rsplit('/').next().unwrap_or(binary);
    match base {
        "claude" | "cursor-agent" => Some(Dialect::Claude),
        "agy" => Some(Dialect::Antigravity),
        "codex" => Some(Dialect::Codex),
        "gemini" => Some(Dialect::Gemini),
        _ => None,
    }
}

/// The dialect of an executor: a CLI inventory entry of that name decides by its binary; otherwise a
/// well-known executor name (`claude-code`, `codex`, `gemini-cli`) or a `<name>-` variant of one.
pub fn dialect(executor: &str, inventory: &crate::inventory::Inventory) -> Option<Dialect> {
    if let Some(e) = inventory.by_name(executor).filter(|e| e.is_cli()) {
        let binary = if e.cli_binary.is_empty() {
            &e.name
        } else {
            &e.cli_binary
        };
        return by_binary(binary);
    }
    crate::inventory::WELL_KNOWN_CLI
        .iter()
        .find(|(name, _, _)| {
            executor == *name
                || executor
                    .strip_prefix(name)
                    .is_some_and(|r| r.starts_with('-'))
        })
        .and_then(|(_, binary, _)| by_binary(binary))
}

/// Maps one run's stdout lines to delta text.
#[derive(Debug)]
pub struct Mapper {
    dialect: Dialect,
    /// Claude: partial-message deltas were seen, so whole assistant messages would repeat them.
    partial: bool,
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

impl Mapper {
    /// A mapper for one run.
    pub fn new(dialect: Dialect) -> Self {
        Self {
            dialect,
            partial: false,
        }
    }

    /// The delta text one stdout line carries (empty when none).
    pub fn text(&mut self, line: &[u8]) -> String {
        let Ok(v) = serde_json::from_slice::<Value>(line.trim_ascii()) else {
            return String::new();
        };
        match self.dialect {
            Dialect::Claude => self.claude(&v),
            Dialect::Codex => codex(&v),
            Dialect::Antigravity => {
                if s(&v["event"]) == "step_update" && s(&v["step_type"]) == "agent_response" {
                    s(&v["text_delta"]).to_string()
                } else {
                    String::new()
                }
            }
            Dialect::Gemini => {
                if s(&v["type"]) == "message" && s(&v["role"]) == "assistant" {
                    s(&v["content"]).to_string()
                } else {
                    String::new()
                }
            }
        }
    }

    fn claude(&mut self, v: &Value) -> String {
        match s(&v["type"]) {
            "stream_event" => {
                let e = &v["event"];
                if s(&e["type"]) == "content_block_delta" && s(&e["delta"]["type"]) == "text_delta"
                {
                    self.partial = true;
                    return s(&e["delta"]["text"]).to_string();
                }
                String::new()
            }
            "assistant" if !self.partial => v["message"]["content"]
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter(|b| s(&b["type"]) == "text")
                        .map(|b| s(&b["text"]))
                        .collect()
                })
                .unwrap_or_default(),
            _ => String::new(),
        }
    }
}

fn codex(v: &Value) -> String {
    if s(&v["type"]) == "item.completed" && s(&v["item"]["type"]) == "agent_message" {
        return s(&v["item"]["text"]).to_string();
    }
    if s(&v["msg"]["type"]) == "agent_message" {
        return s(&v["msg"]["message"]).to_string();
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fold(d: Dialect, lines: &[&str]) -> String {
        let mut m = Mapper::new(d);
        lines.iter().map(|l| m.text(l.as_bytes())).collect()
    }

    #[test]
    fn dialects_map_text_only() {
        let claude = [
            r#"{"type":"system","subtype":"init"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hel"},{"type":"tool_use","name":"x"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"lo"}]}}"#,
            r#"{"type":"result","result":"Hello"}"#,
            "not json",
        ];
        assert_eq!(fold(Dialect::Claude, &claude), "Hello");
        let partial = [
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hi"}]}}"#,
        ];
        assert_eq!(fold(Dialect::Claude, &partial), "Hi");
        let codex = [
            r#"{"type":"thread.started"}"#,
            r#"{"type":"item.completed","item":{"type":"reasoning","text":"no"}}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"done"}}"#,
            r#"{"msg":{"type":"agent_message","message":"!"}}"#,
        ];
        assert_eq!(fold(Dialect::Codex, &codex), "done!");
        let gemini = [
            r#"{"type":"message","role":"user","content":"q"}"#,
            r#"{"type":"message","role":"assistant","content":"a","delta":true}"#,
        ];
        assert_eq!(fold(Dialect::Gemini, &gemini), "a");
        let agy = [
            r#"{"event":"init","conversation_id":"c"}"#,
            r#"{"event":"step_update","step_type":"agent_response","state":"ACTIVE","text_delta":"An"}"#,
            r#"{"event":"step_update","step_type":"tool","text_delta":"no"}"#,
            r#"{"event":"step_update","step_type":"agent_response","state":"DONE","text_delta":"ti."}"#,
            r#"{"event":"result","result":{"status":"SUCCESS","response":"Anti."}}"#,
        ];
        assert_eq!(fold(Dialect::Antigravity, &agy), "Anti.");
    }

    #[test]
    fn executors_choose_dialects() {
        let inv = crate::inventory::Inventory::default();
        assert_eq!(dialect("claude-code-sonnet", &inv), Some(Dialect::Claude));
        assert_eq!(dialect("codex", &inv), Some(Dialect::Codex));
        assert_eq!(dialect("gemini-cli", &inv), Some(Dialect::Gemini));
        assert_eq!(dialect("codexy", &inv), None);
        assert_eq!(dialect("fake-cli", &inv), None);
        let cli = |name: &str, binary: &str| crate::inventory::ModelEntry {
            name: name.into(),
            executor: "cli".into(),
            cli_binary: binary.into(),
            ..crate::inventory::ModelEntry::default()
        };
        let inv = crate::inventory::Inventory::build(
            Some(vec![
                cli("cursor-agent", "cursor-agent"),
                cli("cursor-sol", "cursor-agent"),
                cli("cursor-codex", "cursor-agent"),
                cli("cursor-composer", "cursor-agent"),
                cli("cursor-grok", "cursor-agent"),
                cli("cursor-sonnet", "cursor-agent"),
                cli("antigravity", "agy"),
                cli("antigravity-hydrogen", "agy"),
            ]),
            &crate::inventory::Observed::default(),
        );
        assert_eq!(dialect("cursor-agent", &inv), Some(Dialect::Claude));
        for name in [
            "cursor-sol",
            "cursor-codex",
            "cursor-composer",
            "cursor-grok",
            "cursor-sonnet",
        ] {
            assert_eq!(dialect(name, &inv), Some(Dialect::Claude), "{name}");
        }
        for name in ["antigravity", "antigravity-hydrogen"] {
            assert_eq!(dialect(name, &inv), Some(Dialect::Antigravity), "{name}");
        }
    }
}
