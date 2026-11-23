//! The local model gateway's source budgets (WIRE.md, budgets lines), seen and set through the Director.
//!
//! - `GET /v1/budgets` is read as it is; each source is then labelled for a client: "off, set a budget to use",
//!   active, or "budget used up".
//! - `PUT /v1/budgets` replaces the gateway's `limits.json`. The gateway takes it only with its owner token from a
//!   local peer. The Director reads that token from its file on this machine (`--gateway-owner-token`, default
//!   `~/.contextstack/config/gateway/owner-token`) when its own owner asks, and sends it only to a loopback address.
//!   The token is never logged, stored or answered.
//!
//! The gateway must be on this machine: `--gateway` takes only a plain-HTTP loopback address, so the owner token
//! never leaves it. When the gateway has a shared bearer token (`CSGATEWAY_AUTH_TOKEN` in the Director's
//! environment), it is sent as well.

use serde_json::{Map, Value, json};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

/// The default gateway address.
pub const DEFAULT_URL: &str = "http://127.0.0.1:9200";
/// How long a gateway call may take.
const TIMEOUT: Duration = Duration::from_secs(5);
/// The largest answer read.
const MAX_ANSWER: u64 = 4 * 1024 * 1024;
/// The largest budgets document sent (the gateway's own limit).
pub const MAX_DOCUMENT: usize = 64 * 1024;

/// Where the gateway is and where its owner token lives.
#[derive(Debug, Clone)]
pub struct Gateway {
    /// A loopback address.
    pub addr: SocketAddr,
    /// The owner token's file.
    pub owner_token_file: PathBuf,
}

/// Why a gateway call failed.
#[derive(Debug)]
pub enum Fault {
    /// It could not be reached or answered nonsense.
    Unavailable(String),
    /// The owner token could not be read.
    Token(String),
}

impl Gateway {
    /// A gateway at `url`, which must be `http://` on a loopback address.
    pub fn new(url: &str, owner_token_file: PathBuf) -> Result<Self, String> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| format!("--gateway {url:?}: want http://127.0.0.1:PORT"))?
            .trim_end_matches('/');
        let rest = rest.replace("localhost", "127.0.0.1");
        let addr: SocketAddr = rest
            .parse()
            .map_err(|_| format!("--gateway {url:?}: want http://127.0.0.1:PORT"))?;
        if !addr.ip().is_loopback() {
            return Err(format!(
                "--gateway {url:?}: only a gateway on this machine (a loopback address) is used"
            ));
        }
        Ok(Self {
            addr,
            owner_token_file,
        })
    }

    /// The owner token, read now from a file only its owner may read.
    fn owner_token(&self) -> Result<String, Fault> {
        let path = &self.owner_token_file;
        let meta = std::fs::metadata(path).map_err(|e| {
            Fault::Token(format!("the gateway owner token ({}): {e}", path.display()))
        })?;
        if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 {
            return Err(Fault::Token(format!(
                "the gateway owner token ({}) must be a file only its owner can read",
                path.display()
            )));
        }
        let token = std::fs::read_to_string(path).map_err(|e| {
            Fault::Token(format!("the gateway owner token ({}): {e}", path.display()))
        })?;
        let token = token.trim().to_string();
        if token.is_empty() || token.contains(['\r', '\n']) {
            return Err(Fault::Token(
                "the gateway owner token file is empty or malformed".into(),
            ));
        }
        Ok(token)
    }

    /// `GET /v1/budgets`: `(status, body)`.
    pub fn budgets(&self) -> Result<(u16, Value), Fault> {
        self.get("/v1/budgets")
    }

    /// `GET` under `/v1/…` on the gateway (no owner token): `(status, body)`.
    pub fn get(&self, path: &str) -> Result<(u16, Value), Fault> {
        self.call("GET", path, &[], b"", TIMEOUT)
    }

    /// `PUT /v1/budgets` with the owner token: `(status, body)`.
    pub fn put_budgets(&self, document: &[u8]) -> Result<(u16, Value), Fault> {
        let token = self.owner_token()?;
        self.call(
            "PUT",
            "/v1/budgets",
            &[("X-ContextStack-Owner-Token", token.as_str())],
            document,
            TIMEOUT,
        )
    }

    pub(crate) fn model_call(&self, body: &Value) -> Result<(u16, Value), Fault> {
        self.call(
            "POST",
            "/v1/agent/model.call",
            &[],
            body.to_string().as_bytes(),
            Duration::from_secs(12),
        )
    }

    fn call(
        &self,
        method: &str,
        path: &str,
        extra: &[(&str, &str)],
        body: &[u8],
        timeout: Duration,
    ) -> Result<(u16, Value), Fault> {
        let down = |e: &dyn std::fmt::Display| {
            Fault::Unavailable(format!("the gateway at {}: {e}", self.addr))
        };
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: application/json\r\n",
            self.addr,
        );
        if method != "GET" {
            let _ = write!(
                head,
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            );
        }
        if let Ok(t) = std::env::var("CSGATEWAY_AUTH_TOKEN")
            && !t.is_empty()
            && !t.contains(['\r', '\n'])
        {
            let _ = write!(head, "Authorization: Bearer {t}\r\n");
        }
        for (k, v) in extra {
            let _ = write!(head, "{k}: {v}\r\n");
        }
        head.push_str("\r\n");
        let mut stream = TcpStream::connect_timeout(&self.addr, TIMEOUT).map_err(|e| down(&e))?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|e| down(&e))?;
        stream
            .set_write_timeout(Some(TIMEOUT))
            .map_err(|e| down(&e))?;
        stream
            .write_all(head.as_bytes())
            .and_then(|()| stream.write_all(body))
            .map_err(|e| down(&e))?;
        let mut raw = Vec::new();
        stream
            .take(MAX_ANSWER)
            .read_to_end(&mut raw)
            .map_err(|e| down(&e))?;
        parse_answer(&raw).map_err(|e| down(&e))
    }
}

/// An HTTP/1.1 answer: its status and JSON body (content-length, chunked, or read to the end).
fn parse_answer(raw: &[u8]) -> Result<(u16, Value), String> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("no complete answer")?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| "a malformed answer")?;
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or("a malformed status line")?;
    let mut body = raw[split + 4..].to_vec();
    let header = |name: &str| {
        head.split("\r\n").skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
    };
    if header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        body = dechunk(&body)?;
    } else if let Some(n) = header("content-length").and_then(|v| v.parse::<usize>().ok()) {
        body.truncate(n);
    }
    let value = serde_json::from_slice(&body).unwrap_or_else(
        |_| json!({"error": String::from_utf8_lossy(&body).chars().take(500).collect::<String>()}),
    );
    Ok((status, value))
}

fn dechunk(mut body: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let end = body
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("a malformed chunk")?;
        let size = std::str::from_utf8(&body[..end])
            .ok()
            .and_then(|s| usize::from_str_radix(s.split(';').next().unwrap_or("").trim(), 16).ok())
            .ok_or("a malformed chunk size")?;
        body = &body[end + 2..];
        if size == 0 {
            return Ok(out);
        }
        out.extend_from_slice(body.get(..size).ok_or("a short chunk")?);
        body = body.get(size + 2..).ok_or("a short chunk")?;
    }
}

/// The gateway's report with each source labelled for a client: "no budget set" is not "budget used up".
pub fn labelled(mut report: Value) -> Value {
    if let Some(rows) = report["sources"].as_array_mut() {
        for row in rows {
            let shown = match row["state"].as_str() {
                Some("off") => "off, set a budget to use".to_string(),
                Some("exhausted") => match row["resets_at"].as_str() {
                    Some(at) => format!("budget used up until {at}"),
                    None => "budget used up".into(),
                },
                Some("active") => match row["remaining_usd"]
                    .as_f64()
                    .and_then(crate::budget::micros)
                {
                    Some(r) => format!("active: {} left", crate::budget::dollars(r)),
                    None => "active".into(),
                },
                Some("invalid") => "the gateway's budget file is invalid; nothing paid runs".into(),
                _ => "unknown".into(),
            };
            row["shown"] = json!(shown);
        }
    }
    report
}

/// The shared budget's key in `limits.json`: it applies to every pay-as-you-go source.
pub const SHARED: &str = "*";

/// The gateway's limits (`sources` of `limits.json`) as its `GET /v1/budgets` report shows them, normalised:
/// `source → {usd, period}`. The report carries no revision and not the file itself, so the document is rebuilt
/// from it: `total` is the shared budget (`"*"`); a source row's own `budget` is its entry; and a source shown
/// `off` while a shared budget is active has its own $0 entry (the gateway's way to keep one source off). Entries
/// with no effect are left out ($0 for `"*"`; a $0 source with no shared budget), so two reports that mean the
/// same compare equal. An invalid file is refused: it is replaced whole with `PUT source-budgets`.
pub fn limits_of(report: &Value) -> Result<Map<String, Value>, String> {
    if let Some(e) = report.get("error").filter(|e| !e.is_null()) {
        return Err(format!(
            "the gateway's budgets file is invalid ({e}); replace it whole with PUT source-budgets"
        ));
    }
    let rows = report["sources"]
        .as_array()
        .ok_or("the gateway's budgets report lists no sources")?;
    let entry = |b: &Value| -> Result<Value, String> {
        let usd = b["usd"].as_f64().filter(|u| *u >= 0.0);
        let period = b["period"]
            .as_str()
            .filter(|p| matches!(*p, "day" | "month"));
        match (usd, period) {
            (Some(usd), Some(period)) => Ok(json!({"usd": usd, "period": period})),
            _ => Err(format!(
                "the gateway's budgets report has a budget without usd and period: {b}"
            )),
        }
    };
    let mut out = Map::new();
    let shared = report.get("total").filter(|t| !t.is_null());
    if let Some(t) = shared {
        out.insert(SHARED.into(), entry(t)?);
    }
    for row in rows {
        let source = row["source"]
            .as_str()
            .ok_or("the gateway's budgets report has a row without a source")?
            .to_ascii_lowercase();
        if row["state"] == "invalid" {
            return Err(
                "the gateway's budgets file is invalid; replace it whole with PUT source-budgets"
                    .into(),
            );
        }
        if let Some(b) = row.get("budget").filter(|b| !b.is_null()) {
            out.insert(source, entry(b)?);
        } else if shared.is_some() && row["state"] == "off" {
            out.insert(source, json!({"usd": 0.0, "period": "day"}));
        }
    }
    Ok(normalised(out))
}

/// Entries with no effect left out: a $0 shared budget, and $0 sources when no shared budget is active.
fn normalised(mut limits: Map<String, Value>) -> Map<String, Value> {
    let zero = |b: &Value| b["usd"].as_f64().is_none_or(|u| u <= 0.0);
    if limits.get(SHARED).is_some_and(zero) {
        limits.remove(SHARED);
    }
    if !limits.contains_key(SHARED) {
        limits.retain(|_, b| !zero(b));
    }
    limits
}

/// The limits with one source's budget set (`Some((usd, period))`) or turned off (`None`). Off for the shared
/// budget removes it; off for a source removes its own entry and, while a shared budget is active, gives it $0 so
/// the shared budget does not cover it.
pub fn edited(
    mut limits: Map<String, Value>,
    source: &str,
    set: Option<(f64, &str)>,
) -> Map<String, Value> {
    if let Some((usd, period)) = set {
        limits.insert(source.into(), json!({"usd": usd, "period": period}));
    } else {
        limits.remove(source);
        if source != SHARED && limits.contains_key(SHARED) {
            limits.insert(source.into(), json!({"usd": 0.0, "period": "day"}));
        }
    }
    normalised(limits)
}

/// A source name as `limits.json` keys it: lower-cased, 1 to 128 of `a-z 0-9 . _ - :`, or `*` for the shared
/// budget.
pub fn source_name(raw: &str) -> Option<String> {
    let s = raw.to_ascii_lowercase();
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':');
    (s == SHARED || (!s.is_empty() && s.len() <= 128 && s.chars().all(plain))).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback() {
        let f = PathBuf::from("/nonexistent");
        assert!(Gateway::new("http://127.0.0.1:9200", f.clone()).is_ok());
        assert!(Gateway::new("http://localhost:9200/", f.clone()).is_ok());
        assert!(Gateway::new("http://[::1]:9200", f.clone()).is_ok());
        assert!(Gateway::new("http://192.0.2.5:9200", f.clone()).is_err());
        assert!(Gateway::new("https://127.0.0.1:9200", f.clone()).is_err());
        assert!(Gateway::new("http://gateway.example:9200", f).is_err());
    }

    #[test]
    fn answers() -> Result<(), String> {
        let (s, v) = parse_answer(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\n{\"a\":1}\nxx")?;
        assert_eq!((s, v), (200, json!({"a": 1})));
        let (s, v) = parse_answer(b"HTTP/1.1 403 Forbidden\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"b\"\r\n3\r\n:2}\r\n0\r\n\r\n")?;
        assert_eq!((s, v), (403, json!({"b": 2})));
        assert!(parse_answer(b"garbage").is_err());
        Ok(())
    }

    #[test]
    fn a_token_others_can_read_is_refused() -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("owner-token");
        std::fs::write(&path, "tok\n")?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
        let g = Gateway::new("http://127.0.0.1:1", path.clone())?;
        assert!(matches!(g.owner_token(), Err(Fault::Token(_))));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        assert_eq!(g.owner_token().ok().as_deref(), Some("tok"));
        Ok(())
    }

    #[test]
    fn labels() {
        let r = labelled(json!({"sources": [
            {"source": "a", "state": "off"},
            {"source": "b", "state": "exhausted", "resets_at": "2026-11-11T00:00:00-05:00"},
            {"source": "c", "state": "active", "remaining_usd": 1.5},
            {"source": "d", "state": "active", "remaining_usd": 0.1983}]}));
        assert_eq!(r["sources"][0]["shown"], "off, set a budget to use");
        assert!(
            r["sources"][1]["shown"]
                .as_str()
                .is_some_and(|s| s.starts_with("budget used up"))
        );
        assert_eq!(r["sources"][2]["shown"], "active: $1.50 left");
        // $0.20 with $0.0017 spent is not shown as "$0.20 left".
        assert_eq!(r["sources"][3]["shown"], "active: $0.198 left");
    }

    fn report(total: &Value, rows: &[Value]) -> Value {
        json!({"schema_version": "csgateway.budgets.v1", "sources": rows, "total": total})
    }

    #[test]
    fn limits_are_rebuilt_from_the_report() -> Result<(), String> {
        let r = report(
            &json!({"usd": 10.0, "period": "month", "spent_usd": 1.0}),
            &[
                json!({"source": "openai", "state": "active", "budget": {"usd": 5.0, "period": "day", "spent_usd": 0}}),
                json!({"source": "anthropic", "state": "active", "budget": null, "shared": true}),
                json!({"source": "OpenRouter", "state": "off", "budget": null}),
            ],
        );
        let l = limits_of(&r)?;
        assert_eq!(
            Value::Object(l.clone()),
            json!({"*": {"usd": 10.0, "period": "month"}, "openai": {"usd": 5.0, "period": "day"},
                   "openrouter": {"usd": 0.0, "period": "day"}})
        );
        // Without a shared budget, an off source has no entry.
        let r = report(
            &Value::Null,
            &[json!({"source": "openai", "state": "off", "budget": null})],
        );
        assert!(limits_of(&r)?.is_empty());
        assert!(limits_of(&json!({"sources": [], "error": "bad json"})).is_err());
        assert!(
            limits_of(&report(
                &Value::Null,
                &[json!({"source": "a", "state": "invalid"})]
            ))
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn one_source_is_set_or_turned_off() {
        let l: Map<String, Value> = serde_json::from_value(
            json!({"*": {"usd": 10.0, "period": "month"}, "openai": {"usd": 5.0, "period": "day"}}),
        )
        .unwrap_or_default();
        let set = edited(l.clone(), "anthropic", Some((2.5, "day")));
        assert_eq!(set["anthropic"], json!({"usd": 2.5, "period": "day"}));
        assert_eq!(set["openai"], l["openai"], "the others are kept");
        let off = edited(l.clone(), "openai", None);
        assert_eq!(off["openai"]["usd"], 0.0, "off under a shared budget is $0");
        let no_shared = edited(l, "*", None);
        assert_eq!(
            Value::Object(no_shared.clone()),
            json!({"openai": {"usd": 5.0, "period": "day"}})
        );
        assert!(
            edited(no_shared, "openai", None).is_empty(),
            "off is no entry"
        );
        assert_eq!(source_name("OpenAI").as_deref(), Some("openai"));
        assert_eq!(source_name("*").as_deref(), Some("*"));
        assert_eq!(source_name("a b"), None);
        assert_eq!(source_name(""), None);
    }
}
