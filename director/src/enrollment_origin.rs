use crate::server::{Call, Reply, State, json};
use crate::store::ScopedConnection;
use crate::work::{No, body};
use rusqlite::OptionalExtension;
use serde_json::{Value, json as value};

fn read(db: &ScopedConnection) -> Result<Option<String>, No> {
    db.query_row(
        "SELECT value FROM org_settings WHERE +org_id = :org_id AND key = 'advertised_url'",
        rusqlite::params![],
        |row| row.get(0),
    )
    .optional()
    .map_err(|e| No::internal(&e))
}

fn origin(raw: &str) -> Option<String> {
    if raw.contains('#') {
        return None;
    }
    let uri = raw.parse::<hyper::Uri>().ok()?;
    let scheme = uri
        .scheme_str()
        .filter(|s| matches!(*s, "http" | "https"))?;
    let authority = uri
        .authority()
        .filter(|a| !a.host().is_empty() && !a.as_str().contains('@'))?;
    (uri.path() == "/" && uri.query().is_none()).then(|| format!("{scheme}://{authority}"))
}

pub fn link(db: &ScopedConnection, call: &Call, kind: &str, code: &str) -> Result<String, No> {
    let url = read(db)?
        .or_else(|| call.header("x-contextstack-invite-origin").and_then(origin))
        .ok_or_else(|| No::invalid("enrollment origin must be an HTTP or HTTPS origin"))?;
    let mut encoded = String::new();
    for byte in url.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    Ok(format!("contextstack://{kind}?url={encoded}&code={code}"))
}

pub fn settings(state: &State, call: &Call) -> Reply {
    let result = (|| -> Result<Option<String>, No> {
        let db = state.store.db();
        if call.method == "GET" {
            return read(&db);
        }
        let mut input = body(call)?;
        if input.len() != 1 {
            return Err(No::invalid("expected a single url field"));
        }
        match input.remove("url") {
            Some(Value::Null) => {
                db.execute(
                    "DELETE FROM org_settings WHERE +org_id = :org_id AND key = 'advertised_url'",
                    rusqlite::params![],
                )
                .map_err(|e| No::internal(&e))?;
                Ok(None)
            }
            Some(Value::String(raw)) => {
                let url = origin(&raw)
                    .ok_or_else(|| No::invalid("url must be an HTTP or HTTPS origin"))?;
                db.execute(
                    "INSERT INTO org_settings (org_id,key,value) VALUES (:org_id,'advertised_url',?1)
                     ON CONFLICT(org_id,key) DO UPDATE SET value=excluded.value WHERE +org_settings.org_id = :org_id",
                    [&url],
                ).map_err(|e| No::internal(&e))?;
                Ok(Some(url))
            }
            _ => Err(No::invalid("url must be an HTTP or HTTPS origin or null")),
        }
    })();
    match result {
        Ok(url) => json(200, &value!({"url":url})),
        Err(no) => no.reply(),
    }
}
