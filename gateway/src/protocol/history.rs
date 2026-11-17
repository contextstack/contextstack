//! Rules every protocol shares: native tool-call history must be complete and unambiguous (never
//! silently flattened to text), and the neutral image block (WIRE Images).

use crate::wire::Message;
use serde_json::{Map, Value};

/// Refuse a broken native tool exchange; legacy observation-only messages pass.
pub fn validate_tool_history(messages: &[Message]) -> Result<(), String> {
    let mut pending: Vec<&str> = Vec::new();
    for m in messages {
        if !m.tool_call_id.is_empty() {
            let open = pending.iter().position(|p| *p == m.tool_call_id);
            match open {
                Some(i) if m.role == "tool" && m.tool_calls.is_empty() => {
                    pending.remove(i);
                }
                _ => {
                    return Err(format!(
                        "tool history: unmatched result {:?}",
                        m.tool_call_id
                    ));
                }
            }
            continue;
        }
        if !pending.is_empty() {
            return Err(format!("tool history: missing result before {}", m.role));
        }
        for call in &m.tool_calls {
            let object_args = call
                .args
                .as_ref()
                .is_some_and(|a| serde_json::from_str::<Map<String, Value>>(a.get()).is_ok());
            if m.role != "assistant"
                || call.id.is_empty()
                || call.name.is_empty()
                || pending.contains(&call.id.as_str())
                || !object_args
            {
                return Err(format!("tool history: invalid call {:?}", call.id));
            }
            pending.push(&call.id);
        }
    }
    if pending.is_empty() {
        Ok(())
    } else {
        Err("tool history: missing final result".into())
    }
}

/// A neutral image block's parts: `Ok(None)` when the block is not neutral (native, or not an image).
fn neutral(block: &Map<String, Value>) -> Result<Option<(String, String)>, String> {
    if block.get("type").and_then(Value::as_str) != Some("image") || block.contains_key("source") {
        return Ok(None);
    }
    if !block.contains_key("media_type") && !block.contains_key("base64") {
        return Ok(None);
    }
    let media = block
        .get("media_type")
        .and_then(Value::as_str)
        .unwrap_or("");
    let data = block.get("base64").and_then(Value::as_str).unwrap_or("");
    if media.is_empty() || data.is_empty() {
        return Err("neutral image block requires non-empty media_type and base64".into());
    }
    Ok(Some((media.to_string(), data.to_string())))
}

fn blocks(content: &Value) -> impl Iterator<Item = &Map<String, Value>> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
}

/// Refuse a malformed neutral image block before any provider call.
pub fn validate_images(messages: &[Message]) -> Result<(), String> {
    for (i, m) in messages.iter().enumerate() {
        for b in blocks(&m.content) {
            neutral(b).map_err(|e| format!("messages[{i}] (role {}): {e}", m.role))?;
        }
    }
    Ok(())
}

/// Whether any message carries an image block, native or neutral (the images capability).
pub fn has_images(messages: &[Message]) -> bool {
    messages
        .iter()
        .any(|m| blocks(&m.content).any(|b| b.get("type").and_then(Value::as_str) == Some("image")))
}

/// Translate neutral image blocks to Anthropic's `source` shape; everything else passes untouched.
pub fn anthropic_images(content: &Value) -> Value {
    let Some(parts) = content.as_array() else {
        return content.clone();
    };
    Value::Array(
        parts
            .iter()
            .map(|p| match p.as_object().map(neutral) {
                Some(Ok(Some((media, data)))) => serde_json::json!({
                    "type": "image",
                    "source": {"type": "base64", "media_type": media, "data": data},
                }),
                _ => p.clone(),
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msgs(v: Value) -> Vec<Message> {
        serde_json::from_value::<Vec<serde_json::Value>>(v)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|m| serde_json::from_value(m).ok())
            .collect()
    }

    #[test]
    fn history_rules() {
        let ok = msgs(serde_json::json!([
            {"role":"user","content":"go"},
            {"role":"assistant","content":"","tool_calls":[{"id":"c1","name":"f","args":{"a":1}}]},
            {"role":"tool","tool_call_id":"c1","content":"x"}]));
        assert!(validate_tool_history(&ok).is_ok());
        let unmatched =
            msgs(serde_json::json!([{"role":"tool","tool_call_id":"zz","content":"x"}]));
        assert!(
            validate_tool_history(&unmatched)
                .is_err_and(|e| e.starts_with("tool history: unmatched"))
        );
        let bad_args = msgs(serde_json::json!([
            {"role":"assistant","tool_calls":[{"id":"c1","name":"f","args":[1]}]},
            {"role":"tool","tool_call_id":"c1","content":"x"}]));
        assert!(validate_tool_history(&bad_args).is_err());
        let open = msgs(
            serde_json::json!([{"role":"assistant","tool_calls":[{"id":"c1","name":"f","args":{}}]}]),
        );
        assert_eq!(
            validate_tool_history(&open),
            Err("tool history: missing final result".into())
        );
    }

    #[test]
    fn neutral_images() {
        let m = msgs(
            serde_json::json!([{"role":"user","content":[{"type":"image","media_type":"image/png","base64":"AA"}]}]),
        );
        assert!(validate_images(&m).is_ok() && has_images(&m));
        let t = anthropic_images(&m[0].content);
        assert_eq!(t[0]["source"]["data"], "AA");
        let bad =
            msgs(serde_json::json!([{"role":"tool","content":[{"type":"image","base64":"AA"}]}]));
        assert!(validate_images(&bad).is_err());
    }
}
