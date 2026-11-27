//! Record a task lane landing reported by the node (`tests/director-spec/NODE-PROTOCOL.md` §9.6).

use crate::live;
use crate::records;
use crate::server::{Call, Reply, State, internal, refusal};
use crate::store::ScopedConnection;
use crate::store::{Record, touch};
use crate::util::{self, now_utc};
use crate::work::{self, No};
use rusqlite::{OptionalExtension, params};
use serde_json::{Map, Value, json};

fn opt_text(m: &Map<String, Value>, field: &str, max: usize) -> Result<Option<String>, No> {
    match m.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.len() <= max => Ok(Some(s.clone())),
        Some(_) => Err(No::invalid(&format!(
            "{field} must be a string of at most {max} bytes"
        ))),
    }
}

/// Validate and normalise a land report body for idempotent storage.
pub fn parse_body(m: &Map<String, Value>) -> Result<Value, No> {
    let state = m
        .get("state")
        .and_then(Value::as_str)
        .filter(|s| matches!(*s, "landed" | "failed" | "refused"))
        .ok_or_else(|| No::invalid("state must be landed, failed or refused"))?;
    let need = |f: &str| {
        m.get(f)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .ok_or_else(|| No::invalid(&format!("{f} is required")))
    };
    let repo = need("repo")?;
    let lane = need("lane")?;
    let main = need("main")?;
    let landed_by = need("landed_by")?;
    let commit = opt_text(m, "commit", 128)?;
    let reason = opt_text(m, "reason", 4000)?;
    if state == "landed" {
        if commit.is_none() {
            return Err(No::invalid("commit is required when state is landed"));
        }
        if reason.is_some() {
            return Err(No::invalid("reason must be null when state is landed"));
        }
    } else {
        if commit.is_some() {
            return Err(No::invalid("commit must be null unless state is landed"));
        }
        if reason.is_none() {
            return Err(No::invalid(
                "reason is required when state is failed or refused",
            ));
        }
    }
    let mut body = json!({"state": state, "repo": repo, "lane": lane, "main": main, "commit": commit,
                          "reason": reason, "landed_by": landed_by});
    if let Some(coverage) = m.get("host_probe_coverage") {
        let valid = coverage.as_object().is_some_and(|obj| {
            obj.len()
                == if obj.contains_key("sandbox_sha256") {
                    3
                } else {
                    2
                }
                && obj.get("sandbox_sha256").is_none_or(|digest| {
                    digest.as_str().is_some_and(|sha| {
                        sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit())
                    })
                })
                && ["covered", "uncovered"].iter().all(|field| {
                    obj.get(*field)
                        .and_then(Value::as_array)
                        .is_some_and(|names| {
                            names.len() <= 256
                                && names.iter().all(|name| {
                                    name.as_str()
                                        .is_some_and(|name| !name.is_empty() && name.len() <= 256)
                                })
                        })
                })
        });
        if !valid {
            return Err(No::invalid(
                "host_probe_coverage requires covered and uncovered name lists and accepts a sandbox SHA-256",
            ));
        }
        body["host_probe_coverage"] = coverage.clone();
    }
    Ok(body)
}

fn run_owner(
    db: &ScopedConnection,
    node_id: i64,
    run_id: &str,
) -> rusqlite::Result<Option<(String, Option<String>)>> {
    db.query_row(
        "SELECT COALESCE(r.job_id, (SELECT job_id FROM jobs WHERE +org_id = :org_id AND run_id = r.run_id LIMIT 1)),
                (SELECT intent_id FROM jobs WHERE +org_id = :org_id AND job_id = COALESCE(r.job_id,
                 (SELECT job_id FROM jobs WHERE +org_id = :org_id AND run_id = r.run_id LIMIT 1)))
         FROM runs r WHERE +org_id = :org_id AND r.run_id = ?1 AND r.node_id = ?2",
        params![run_id, node_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

/// The run's current land record, when it has one.
struct OpenLand {
    land_id: String,
    state: String,
    reason: Option<String>,
    landed_at: String,
    attempts: String,
}

fn open_land(db: &ScopedConnection, run_id: &str) -> rusqlite::Result<Option<OpenLand>> {
    db.query_row(
        "SELECT land_id, state, reason, landed_at, attempts FROM lands
         WHERE +org_id = :org_id AND run_id = ?1 AND current = 1",
        params![run_id],
        |r| {
            Ok(OpenLand {
                land_id: r.get(0)?,
                state: r.get(1)?,
                reason: r.get(2)?,
                landed_at: r.get(3)?,
                attempts: r.get(4)?,
            })
        },
    )
    .optional()
}

/// Attempts a replacement stores: the previous list, then the record being replaced.
/// `time` is that record's `landed_at`.
fn next_attempts(
    previous: &str,
    state: &str,
    reason: Option<&str>,
    land_id: &str,
    time: &str,
) -> String {
    let (attempts, _) = decode_attempts(previous);
    let mut list = attempts.as_array().cloned().unwrap_or_default();
    list.push(json!({
        "state": state,
        "reason": reason,
        "land_id": land_id,
        "time": time,
    }));
    serde_json::to_string(&list).unwrap_or_else(|_| "[]".to_string())
}

// The existing attempts JSON stores a bare array on older records.
pub(crate) fn decode_attempts(raw: &str) -> (Value, Option<Value>) {
    match serde_json::from_str::<Value>(raw) {
        Ok(value) if value.is_array() => (value, None),
        Ok(Value::Object(mut data)) => (
            data.remove("attempts")
                .filter(Value::is_array)
                .unwrap_or_else(|| json!([])),
            data.remove("host_probe_coverage"),
        ),
        _ => (json!([]), None),
    }
}

fn text<'a>(body: &'a Value, field: &str) -> &'a str {
    body.get(field).and_then(Value::as_str).unwrap_or("")
}

struct Incoming<'a> {
    key: &'a str,
    sha: &'a str,
    node_id: i64,
    run_id: &'a str,
    job_id: &'a str,
    body: &'a Value,
    attempts: &'a str,
    supersede: Option<&'a str>,
}

/// Insert the current land row. The earlier row stays, no longer current, so a replay of its
/// idempotency key still renders the original response.
fn write_land(db: &mut ScopedConnection, land: &Incoming<'_>) -> Result<String, String> {
    let id = format!("lnd_{}", util::ulid()?.to_lowercase());
    let at = now_utc();
    let tx = crate::store::write_tx(db).map_err(|e| e.to_string())?;
    if let Some(old) = land.supersede {
        tx.execute(
            "UPDATE lands SET current = 0 WHERE +org_id = :org_id AND (land_id = ?1)",
            params![old],
        )
        .map_err(|e| e.to_string())?;
    }
    let attempts = match land.body.get("host_probe_coverage") {
        Some(coverage) => json!({
            "attempts": decode_attempts(land.attempts).0,
            "host_probe_coverage": coverage,
        })
        .to_string(),
        None => land.attempts.to_owned(),
    };
    tx.execute(
        "INSERT INTO lands (org_id, land_id, run_id, state, repo, lane, main, \"commit\", reason, landed_by,
                            landed_at, attempts, current)
         VALUES (:org_id, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 1)",
        params![
            id,
            land.run_id,
            text(land.body, "state"),
            text(land.body, "repo"),
            text(land.body, "lane"),
            text(land.body, "main"),
            land.body.get("commit").and_then(Value::as_str),
            land.body.get("reason").and_then(Value::as_str),
            text(land.body, "landed_by"),
            at,
            attempts,
        ],
    )
    .map_err(|e| e.to_string())?;
    touch(&tx, Record::Land, &id).map_err(|e| e.to_string())?;
    touch(&tx, Record::Run, land.run_id).map_err(|e| e.to_string())?;
    touch(&tx, Record::Job, land.job_id).map_err(|e| e.to_string())?;
    work::remember_node(&tx, land.key, land.sha, Record::Land, &id, land.node_id)
        .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(id)
}

/// `POST runs/{id}/land`: record what happened when the machine landed a lane (node protocol 2).
/// A record that is not `landed` may be replaced. `landed` is final.
pub fn post_land(state: &State, call: &Call, node_id: i64, node_name: &str, run_id: &str) -> Reply {
    let (key, m) = match (work::key(call), work::body(call)) {
        (Ok(k), Ok(m)) => (k, m),
        (Err(r), _) | (_, Err(r)) => return r.reply(),
    };
    if !util::plain_name(run_id, 200) || run_id.contains("..") {
        return refusal(
            422,
            "invalid_request",
            "run_id must be a plain name",
            Value::Null,
        );
    }
    let body = match parse_body(&m) {
        Ok(b) => b,
        Err(r) => return r.reply(),
    };
    let sha = work::content("land", run_id, &body);
    let mut db = state.store.db();
    match work::prior(&db, &key, &sha) {
        Ok(Some((kind, id))) => return work::render(&db, 200, kind, &id, true),
        Ok(None) => {}
        Err(r) => return r.reply(),
    }
    let (job_id, intent_id) = match run_owner(&db, node_id, run_id) {
        Ok(Some(pair)) => pair,
        Ok(None) => {
            return refusal(404, "not_found", "no such run for this node", Value::Null);
        }
        Err(e) => return internal(&e),
    };
    let (attempts, supersede) = match open_land(&db, run_id) {
        Err(e) => return internal(&e),
        Ok(Some(row)) if row.state == "landed" => {
            let record = records::one(&db, Record::Land, &row.land_id).ok().flatten();
            return refusal(
                409,
                "already_landed",
                "this run is already landed",
                json!({"land": record}),
            );
        }
        Ok(Some(row)) => {
            let attempts = next_attempts(
                &row.attempts,
                &row.state,
                row.reason.as_deref(),
                &row.land_id,
                &row.landed_at,
            );
            (attempts, Some(row.land_id))
        }
        Ok(None) => ("[]".to_string(), None),
    };
    let made = write_land(
        &mut db,
        &Incoming {
            key: &key,
            sha: &sha,
            node_id,
            run_id,
            job_id: &job_id,
            body: &body,
            attempts: &attempts,
            supersede: supersede.as_deref(),
        },
    );
    match made {
        Ok(land_id) => {
            let record = records::one(&db, Record::Land, &land_id).ok().flatten();
            let mut stream =
                json!({"run_id": run_id, "job_id": job_id, "node": node_name, "land": record});
            if let Some(i) = intent_id {
                stream["intent_id"] = json!(i);
            }
            if let Some((conversation, field, cid)) = live::conversation_tag(&db, &job_id) {
                stream["conversation_id"] = json!(conversation);
                stream[field] = json!(cid);
            }
            state.hub.land_events(vec![stream]);
            crate::logf!(
                "run {run_id}: land {} ({land_id})",
                body["state"].as_str().unwrap_or("?")
            );
            work::render(&db, 201, Record::Land, &land_id, false)
        }
        Err(e) => internal(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Map, Value, json};

    fn land_body(value: &Value) -> Map<String, Value> {
        let Some(m) = value.as_object().cloned() else {
            panic!("expected JSON object");
        };
        m
    }

    #[test]
    fn landed_requires_commit() {
        let m = land_body(
            &json!({"state": "landed", "repo": "r", "lane": "l", "main": "main", "landed_by": "owner"}),
        );
        assert!(parse_body(&m).is_err());
    }

    #[test]
    fn failed_requires_reason() {
        let m = land_body(
            &json!({"state": "failed", "repo": "r", "lane": "l", "main": "main", "landed_by": "owner"}),
        );
        assert!(parse_body(&m).is_err());
    }

    #[test]
    fn landed_shape() {
        let m = land_body(
            &json!({"state": "landed", "repo": "ctx", "lane": "lane/x", "main": "main", "commit": "abc",
                                 "landed_by": "the owner"}),
        );
        let Ok(b) = parse_body(&m) else {
            panic!("expected valid landed body");
        };
        assert_eq!(b["state"], "landed");
        assert_eq!(b["commit"], "abc");
        assert!(b["reason"].is_null());
        assert!(b.get("host_probe_coverage").is_none());
    }

    #[test]
    fn sandbox_land_carries_probe_coverage() {
        let mut m = land_body(&json!({
            "state": "refused", "repo": "ctx", "lane": "probe", "main": "main",
            "reason": "needs owner host probes", "landed_by": "owner",
            "host_probe_coverage": {"covered": ["probe-a"], "uncovered": ["probe-b"]},
        }));
        let Ok(body) = parse_body(&m) else {
            panic!("expected valid probe coverage");
        };
        assert_eq!(body["host_probe_coverage"], m["host_probe_coverage"]);
        m["host_probe_coverage"]["sandbox_sha256"] = json!("a".repeat(64));
        assert_eq!(
            parse_body(&m)
                .ok()
                .map(|body| body["host_probe_coverage"].clone()),
            Some(m["host_probe_coverage"].clone()),
        );
        m["host_probe_coverage"]["sandbox_sha256"] = json!("invalid");
        assert!(parse_body(&m).is_err());
        m.insert("host_probe_coverage".into(), json!({"covered": true}));
        assert!(parse_body(&m).is_err());
    }

    #[test]
    fn replacement_appends_the_earlier_attempt() {
        let first = next_attempts("[]", "failed", Some("check failed"), "lnd_old", "t1");
        let Ok(parsed) = serde_json::from_str::<Value>(&first) else {
            panic!("attempts JSON must parse");
        };
        assert_eq!(
            parsed,
            json!([{"state": "failed", "reason": "check failed", "land_id": "lnd_old", "time": "t1"}])
        );
        let second = next_attempts(&first, "refused", Some("dirty main"), "lnd_mid", "t2");
        let Ok(parsed) = serde_json::from_str::<Value>(&second) else {
            panic!("attempts JSON must parse");
        };
        assert_eq!(parsed.as_array().map(Vec::len), Some(2));
        assert_eq!(parsed[0]["land_id"], "lnd_old");
        assert_eq!(parsed[1]["state"], "refused");
        assert_eq!(parsed[1]["time"], "t2");
    }
}
