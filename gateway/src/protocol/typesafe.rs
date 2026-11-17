//! `TypeSafe`'s typed-question API (System One models such as Jev): `POST {base}/systemone` with
//! `Authorization: Bearer <key>`. It answers typed questions (noul, choice, score) about a state with
//! calibrated probabilities; it is not a chat model, so `model.call` never routes here. The gateway's
//! `POST /v1/ai/typed-question` forwards a caller's `TypeSafe` request body unchanged and reads back only
//! `usage` (input and output tokens are priced at the catalog rate per 1k).

use super::{Http, transport, until};
use crate::errors::CallError;
use bytes::Bytes;
use tokio::time::Instant;

/// The most of an upstream answer the gateway reads (answers are small; a request may be 32 MiB).
const MAX_ANSWER: usize = 8 * 1024 * 1024;

/// `TypeSafe`'s answer, whatever its status.
#[derive(Debug)]
pub struct Answer {
    /// Upstream HTTP status.
    pub status: u16,
    /// Upstream content type (JSON for answers and errors alike).
    pub content_type: String,
    /// Upstream body, unchanged.
    pub body: Bytes,
    /// `usage.input_tokens` of a 2xx answer, when it reports one.
    pub input_tokens: Option<i64>,
    /// `usage.output_tokens` of a 2xx answer, when it reports one.
    pub output_tokens: Option<i64>,
    /// Wall time of the upstream exchange.
    pub latency_ms: i64,
}

/// `usage.{input,output}_tokens` of an answer body, each `None` when absent or not a count.
pub fn usage(body: &[u8]) -> (Option<i64>, Option<i64>) {
    let v: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
    let count = |k: &str| v["usage"][k].as_i64().filter(|n| *n >= 0);
    (count("input_tokens"), count("output_tokens"))
}

/// Post `body` (a `TypeSafe` request, unchanged) to `{base}/systemone`.
pub async fn post(
    http: &Http,
    base: &str,
    key: &str,
    body: Bytes,
    deadline: Instant,
) -> Result<Answer, CallError> {
    let url = format!("{}/systemone", base.trim_end_matches('/'));
    let start = std::time::Instant::now();
    until(deadline, "Post", &url, async {
        let resp = http
            .client()
            .post(&url)
            .bearer_auth(key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| transport("Post", &url, &e))?;
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/json")
            .to_string();
        let body = resp
            .bytes()
            .await
            .map_err(|e| super::transport_after_response("Post", &url, &e))?;
        if body.len() > MAX_ANSWER {
            return Err(CallError::transport_local(format!(
                "Post {url:?}: answer over {MAX_ANSWER} bytes"
            )));
        }
        let (input_tokens, output_tokens) = if (200..300).contains(&status) {
            usage(&body)
        } else {
            (None, None)
        };
        Ok(Answer {
            status,
            content_type,
            body,
            input_tokens,
            output_tokens,
            latency_ms: i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX),
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_reads_counts_only() {
        let ok = br#"{"model":"jev-1.14.0","answers":{"q":{"type":"noul","noul":0.9}},"usage":{"input_tokens":120,"output_tokens":3}}"#;
        assert_eq!(usage(ok), (Some(120), Some(3)));
        assert_eq!(usage(br#"{"usage":{"input_tokens":-1}}"#), (None, None));
        assert_eq!(usage(b"not json"), (None, None));
    }
}
