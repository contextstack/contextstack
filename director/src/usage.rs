//! Usage added up: each run's recorded usage (NODE-PROTOCOL.md §7.2) summed per source, project,
//! conversation, intent or node, over a local day, a local month (the Director host's, as for budgets) or all time.
//!
//! Sources are shown for what they are:
//! - **pay as you go** (`api`): tokens and the dollars actually charged;
//! - **subscription**: tokens, and the allowance the agent last reported, with whether it was reached;
//! - **local** (`free_local`): tokens and any actual provider charge.
//!
//! An estimated API price (`est_usd`) is never shown: it is not a charge.

use crate::budget::{self, Micros};
use crate::store::ScopedConnection;
use chrono::NaiveDate;
#[cfg(test)]
use rusqlite::params;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The token figures a run may report, in output order.
const TOKENS: [&str; 5] = [
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_creation_tokens",
    "reasoning_tokens",
];

/// What a roll-up groups by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum By {
    /// `{class, model}` of the run's source.
    Source,
    /// The project of its intent or conversation.
    Project,
    /// The conversation whose turn or summary it ran.
    Conversation,
    /// The intent whose job it ran.
    Intent,
    /// The node that ran it.
    Node,
    /// The schedule ID carried by the job.
    Schedule,
}

impl By {
    /// A grouping by its query word.
    pub fn named(s: &str) -> Option<Self> {
        Some(match s {
            "source" => Self::Source,
            "project" => Self::Project,
            "conversation" => Self::Conversation,
            "intent" => Self::Intent,
            "node" => Self::Node,
            "schedule" => Self::Schedule,
            _ => return None,
        })
    }

    fn field(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Project => "project_id",
            Self::Conversation => "conversation_id",
            Self::Intent => "intent_id",
            Self::Node => "node",
            Self::Schedule => "schedule_id",
        }
    }
}

/// The span added up: `period` is `day`, `month` or `all`; `from`/`to` bound `received_at` (`to` exclusive).
#[derive(Debug, Clone)]
pub struct Span {
    period: &'static str,
    from: Option<String>,
    to: Option<String>,
}

impl Span {
    /// The span for `period` (`day` | `month` | `all`) holding `date` (`YYYY-MM-DD` or `YYYY-MM`; default today).
    /// Days and months are local to the Director's host, as budget periods are (`budget::Period`); `from` and `to`
    /// are those local midnights as UTC stamps.
    pub fn new(period: &str, date: Option<&str>) -> Result<Self, String> {
        Self::new_in(period, date, &chrono::Local)
    }

    /// As `new`, in the time zone `tz`.
    pub fn new_in<Tz: chrono::TimeZone>(
        period: &str,
        date: Option<&str>,
        tz: &Tz,
    ) -> Result<Self, String> {
        let day = match date {
            None => chrono::Utc::now().with_timezone(tz).date_naive(),
            Some(d) => NaiveDate::parse_from_str(d, "%Y-%m-%d")
                .or_else(|_| NaiveDate::parse_from_str(&format!("{d}-01"), "%Y-%m-%d"))
                .map_err(|_| format!("date {d:?}: want YYYY-MM-DD or YYYY-MM"))?,
        };
        let (period, from, to) = match period {
            "all" => ("all", None, None),
            unit @ ("day" | "month") => {
                let (from, to) = crate::budget::span(unit, day, tz).ok_or("bad date")?;
                (
                    if unit == "day" { "day" } else { "month" },
                    Some(from),
                    Some(to),
                )
            }
            other => return Err(format!("period {other:?}: want day, month or all")),
        };
        Ok(Self { period, from, to })
    }
}

/// Only runs of this project, conversation, intent or node.
#[derive(Debug, Clone, Default)]
pub struct Only {
    /// A lead credential whose visible runs bound the roll-up.
    pub lead: Option<crate::access::Client>,
    /// A project id.
    pub project: Option<String>,
    /// A conversation id.
    pub conversation: Option<String>,
    /// An intent id.
    pub intent: Option<String>,
    /// A node name.
    pub node: Option<String>,
}

/// What one kind of source added up to.
#[derive(Debug, Default)]
struct Bucket {
    runs: i64,
    tokens: [Option<i64>; 5],
    charged: Micros,
    charge_unreported: i64,
    allowance: Option<(String, Value)>,
    models: BTreeSet<String>,
}

/// The kind a run's `source.class` names.
fn kind_of(class: Option<&str>) -> &'static str {
    match class {
        Some("api") => "pay_as_you_go",
        Some("subscription") => "subscription",
        Some("free_local") => "local",
        _ => "unknown",
    }
}

impl Bucket {
    fn add(&mut self, usage: &Value, at: &str) {
        self.runs += 1;
        for (slot, key) in self.tokens.iter_mut().zip(TOKENS) {
            if let Some(n) = usage[key].as_i64() {
                *slot = Some(slot.unwrap_or(0) + n);
            }
        }
        match budget::amount(&usage["charged_usd"]) {
            Some(m) => self.charged += m,
            None => self.charge_unreported += 1,
        }
        if let Some(m) = usage["source"]["reported_model"].as_str() {
            self.models.insert(m.to_string());
        }
        if usage["allowance"].is_object()
            && self
                .allowance
                .as_ref()
                .is_none_or(|(when, _)| when.as_str() <= at)
        {
            self.allowance = Some((at.to_string(), usage["allowance"].clone()));
        }
    }

    /// The figures, shown for what the source is.
    fn render(&self, kind: &str) -> Map<String, Value> {
        let mut out = Map::new();
        out.insert("runs".into(), json!(self.runs));
        for (key, n) in TOKENS.iter().zip(self.tokens) {
            out.insert((*key).into(), json!(n));
        }
        out.insert("reported_models".into(), json!(self.models));
        if kind == "pay_as_you_go" || self.charged > 0 {
            out.insert("charged_usd".into(), budget::usd(self.charged));
        }
        match kind {
            "pay_as_you_go" => {
                out.insert(
                    "charge_unreported_runs".into(),
                    json!(self.charge_unreported),
                );
            }
            "subscription" => {
                let latest = self.allowance.as_ref();
                let reached = latest.is_some_and(|(_, a)| a["rejected"] == true);
                out.insert(
                    "allowance".into(),
                    latest.map_or(Value::Null, |(at, a)| {
                        let mut a = a.clone();
                        a["reported_at"] = json!(at);
                        a
                    }),
                );
                out.insert("allowance_reached".into(), json!(reached));
            }
            _ => {}
        }
        out.insert("shown".into(), json!(self.shown(kind)));
        out
    }

    fn shown(&self, kind: &str) -> String {
        if kind != "pay_as_you_go" && self.charged > 0 {
            return format!("{kind}: {} charged", budget::dollars(self.charged));
        }
        match kind {
            "pay_as_you_go" => format!("pay as you go: {} charged", budget::dollars(self.charged)),
            "subscription" => match &self.allowance {
                Some((_, a)) if a["rejected"] == true => "subscription: allowance reached".into(),
                Some((_, a)) => a["windows"]
                    .as_array()
                    .and_then(|w| w.iter().find(|w| w["used_pct"].is_number()))
                    .map_or_else(
                        || "subscription: usage only; the allowance left is not reported".into(),
                        |w| {
                            format!(
                                "subscription: {}% of the {} allowance used",
                                w["used_pct"],
                                w["window"].as_str().unwrap_or("reported")
                            )
                        },
                    ),
                None => "subscription: usage only; the allowance left is not reported".into(),
            },
            "local" => "local: usage only, no cost".into(),
            _ => "source not reported: usage only".into(),
        }
    }
}

/// One run with what it is attributed to.
struct Row {
    usage: Value,
    at: String,
    keys: [Option<String>; 4],
    node: String,
}

fn rows(db: &ScopedConnection, span: &Span, only: &Only) -> rusqlite::Result<Vec<Row>> {
    const CONV: &str = "COALESCE(b.conversation_id, (SELECT t.conversation_id FROM turns t WHERE +org_id = :org_id AND t.job_id = j.job_id), (SELECT s.conversation_id FROM summaries s WHERE +org_id = :org_id AND s.job_id = j.job_id))";
    const PROJ: &str = "COALESCE(b.project_id, (SELECT i.project_id FROM intents i WHERE +org_id = :org_id AND i.intent_id = j.intent_id))";
    let mut sql = format!(
        "SELECT r.usage, r.received_at, n.name, j.intent_id, {CONV}, {PROJ},
                CASE WHEN json_type(j.meta, '$.schedule_id') = 'text' THEN json_extract(j.meta, '$.schedule_id') END
         FROM (SELECT * FROM runs WHERE +org_id = :org_id) r JOIN (SELECT * FROM nodes WHERE +org_id = :org_id) n ON n.id = r.node_id
           LEFT JOIN (SELECT * FROM jobs WHERE +org_id = :org_id) j ON j.job_id = COALESCE(r.job_id, (SELECT x.job_id FROM jobs x WHERE +org_id = :org_id AND x.run_id = r.run_id LIMIT 1),
                                                   (SELECT l.job_id FROM live_runs l WHERE +org_id = :org_id AND l.run_id = r.run_id))
           LEFT JOIN (SELECT * FROM job_budgets WHERE +org_id = :org_id) b ON b.job_id = j.job_id
         WHERE (? IS NULL OR r.received_at >= ?) AND (? IS NULL OR r.received_at < ?)"
    );
    let mut bind: Vec<rusqlite::types::Value> = vec![
        span.from.clone().into(),
        span.from.clone().into(),
        span.to.clone().into(),
        span.to.clone().into(),
    ];
    if let Some(n) = &only.node {
        sql.push_str(" AND n.name = ?");
        bind.push(n.clone().into());
    }
    if let Some(p) = &only.project {
        sql.push_str(" AND ");
        sql.push_str(PROJ);
        sql.push_str(" = ?");
        bind.push(p.clone().into());
    }
    if let Some(c) = &only.conversation {
        sql.push_str(" AND ");
        sql.push_str(CONV);
        sql.push_str(" = ?");
        bind.push(c.clone().into());
    }
    if let Some(i) = &only.intent {
        sql.push_str(" AND j.intent_id = ?");
        bind.push(i.clone().into());
    }
    if let Some(client) = &only.lead {
        crate::lead::push_run_scope(client, &mut sql, &mut bind);
    }
    sql.push_str(" ORDER BY r.received_at");
    let mut stmt = db.prepare(&sql)?;
    let out = stmt
        .query_map(bind.iter(), |r| {
            let usage: String = r.get(0)?;
            let intent: Option<String> = r.get(3)?;
            let conversation: Option<String> = r.get(4)?;
            let project: Option<String> = r.get(5)?;
            Ok(Row {
                usage: serde_json::from_str(&usage).unwrap_or(Value::Null),
                at: r.get(1)?,
                node: r.get(2)?,
                keys: [project, conversation, intent, r.get(6)?],
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(out)
}

/// Every kind's figures for a group, and its dollars charged in all.
fn render_group(buckets: &BTreeMap<&'static str, Bucket>) -> Map<String, Value> {
    let mut out = Map::new();
    let runs: i64 = buckets.values().map(|b| b.runs).sum();
    let charged = buckets.values().map(|b| b.charged).sum();
    out.insert("runs".into(), json!(runs));
    out.insert("charged_usd".into(), budget::usd(charged));
    let kinds: Map<String, Value> = buckets
        .iter()
        .map(|(k, b)| ((*k).to_string(), Value::Object(b.render(k))))
        .collect();
    out.insert("kinds".into(), Value::Object(kinds));
    out
}

/// `GET usage`: `{by, period, from, to, groups, total}`.
pub fn rollup(db: &ScopedConnection, by: By, span: &Span, only: &Only) -> rusqlite::Result<Value> {
    let rows = rows(db, span, only)?;
    let mut total: BTreeMap<&'static str, Bucket> = BTreeMap::new();
    let mut groups: BTreeMap<String, (Value, BTreeMap<&'static str, Bucket>)> = BTreeMap::new();
    for row in &rows {
        let class = row.usage["source"]["class"].as_str();
        let kind = kind_of(class);
        let (key, label) = match by {
            By::Source => {
                let source = json!({"class": row.usage["source"]["class"], "model": row.usage["source"]["model"]});
                (source.to_string(), source)
            }
            By::Node => (row.node.clone(), json!(row.node)),
            other => {
                let i = match other {
                    By::Project => 0,
                    By::Conversation => 1,
                    By::Schedule => 3,
                    _ => 2,
                };
                let k = row.keys[i].clone();
                (k.clone().unwrap_or_default(), json!(k))
            }
        };
        let entry = groups
            .entry(key)
            .or_insert_with(|| (label, BTreeMap::new()));
        entry.1.entry(kind).or_default().add(&row.usage, &row.at);
        total.entry(kind).or_default().add(&row.usage, &row.at);
    }
    let groups: Vec<Value> = groups
        .into_values()
        .map(|(label, buckets)| {
            let mut out = Map::new();
            out.insert(by.field().into(), label.clone());
            if by == By::Source {
                // A source is one kind: its figures are shown flat, for what it is.
                let kind = kind_of(label["class"].as_str());
                out.insert("kind".into(), json!(kind));
                if let Some(b) = buckets.get(kind) {
                    out.extend(b.render(kind));
                }
            } else {
                out.extend(render_group(&buckets));
            }
            Value::Object(out)
        })
        .collect();
    Ok(
        json!({"by": by.field(), "period": span.period, "from": span.from, "to": span.to,
              "groups": groups, "total": render_group(&total)}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans() -> Result<(), String> {
        let d = Span::new_in("day", Some("2026-11-10"), &chrono::Utc)?;
        assert_eq!(
            (d.from.as_deref(), d.to.as_deref()),
            (Some("2026-11-10T00:00:00Z"), Some("2026-11-11T00:00:00Z"))
        );
        let m = Span::new_in("month", Some("2026-12"), &chrono::Utc)?;
        assert_eq!(
            (m.from.as_deref(), m.to.as_deref()),
            (Some("2026-12-01T00:00:00Z"), Some("2027-01-01T00:00:00Z"))
        );
        // A local day: in UTC−8 it runs from 08:00 to 08:00 UTC.
        let la = chrono::FixedOffset::west_opt(8 * 3600).ok_or("offset")?;
        let d = Span::new_in("day", Some("2026-11-10"), &la)?;
        assert_eq!(
            (d.from.as_deref(), d.to.as_deref()),
            (Some("2026-11-10T08:00:00Z"), Some("2026-11-11T08:00:00Z"))
        );
        assert!(Span::new("all", None)?.from.is_none());
        assert!(Span::new("week", None).is_err());
        assert!(Span::new("day", Some("soon")).is_err());
        Ok(())
    }

    #[test]
    fn each_kind_is_shown_for_what_it_is() {
        let mut api = Bucket::default();
        api.add(
            &json!({"input_tokens": 10, "output_tokens": null, "est_usd": 4.0, "charged_usd": 0.25,
                    "source": {"class": "api", "model": "m"}}),
            "2026-11-10T00:00:00Z",
        );
        let a = api.render("pay_as_you_go");
        assert_eq!(a["charged_usd"], json!(0.25));
        assert_eq!(
            (a["input_tokens"].clone(), a["output_tokens"].clone()),
            (json!(10), Value::Null)
        );
        assert!(!a.contains_key("est_usd"), "an estimate is never shown");
        let mut sub = Bucket::default();
        let allowance = |rejected: bool| {
            json!({"via": "rate_limit_event", "rejected": rejected,
                   "windows": [{"window": "five_hour", "used_pct": 80, "resets_at": "x", "status": "allowed"}]})
        };
        sub.add(
            &json!({"input_tokens": 5, "charged_usd": 0, "est_usd": 3.0, "allowance": allowance(false),
                    "source": {"class": "subscription", "model": "opus", "reported_model": "claude-opus-5-5"}}),
            "2026-11-10T01:00:00Z",
        );
        let s = sub.render("subscription");
        assert!(
            !s.contains_key("charged_usd"),
            "a subscription has no dollar charge"
        );
        assert_eq!(s["allowance_reached"], json!(false));
        assert!(
            s["shown"]
                .as_str()
                .is_some_and(|t| t.contains("80% of the five_hour"))
        );
        assert_eq!(s["reported_models"], json!(["claude-opus-5-5"]));
        sub.add(
            &json!({"charged_usd": 0, "allowance": allowance(true), "source": {"class": "subscription"}}),
            "2026-11-10T02:00:00Z",
        );
        assert_eq!(sub.render("subscription")["allowance_reached"], json!(true));
        let mut local = Bucket::default();
        local.add(
            &json!({"input_tokens": 1, "charged_usd": 0, "source": {"class": "free_local"}}),
            "2026-11-10T00:00:00Z",
        );
        let l = local.render("local");
        assert!(!l.contains_key("charged_usd") && l["shown"] == "local: usage only, no cost");
    }
    #[test]
    fn reported_charges_count_in_local_and_subscription_totals() {
        let mut buckets = BTreeMap::new();
        for (kind, charge) in [("local", 0.25), ("subscription", 0.125)] {
            let mut bucket = Bucket::default();
            bucket.add(
                &json!({"charged_usd": charge, "provider_reported_usd": charge}),
                "2026-11-21T00:00:00Z",
            );
            assert_eq!(bucket.render(kind)["charged_usd"], json!(charge));
            buckets.insert(kind, bucket);
        }
        assert_eq!(render_group(&buckets)["charged_usd"], json!(0.375));
    }

    #[test]
    fn lead_usage_rollup_excludes_foreign_runs() -> rusqlite::Result<()> {
        use crate::access::{Client, Kind};
        use crate::store::Store;
        let tmp = tempfile::tempdir()
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        let store = Store::open(&tmp.path().join("data")).map_err(|e| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e.clone())))
        })?;
        let db = store.db();
        db.execute(
            "INSERT OR IGNORE INTO nodes (org_id, id, name, token_hash, created_at) VALUES (:org_id, 1, 'estate', 'th', 'now')",
            rusqlite::params![],
        )?;
        for (iid, by) in [("mine", "lead_cred"), ("other", "owner")] {
            db.execute(
                "INSERT INTO intents (org_id, intent_id, node, spec, task, trials, meta, bundle_sha256, created_by, created_at)
                 VALUES (:org_id, ?1, 'estate', 's', 't', 1, '{}', 'sha', ?2, '2026-11-10T00:00:00Z')",
                params![iid, by],
            )?;
        }
        for (jid, iid) in [("job_mine", "mine"), ("job_other", "other")] {
            db.execute(
                "INSERT INTO jobs (org_id, job_id, node_id, spec, task, trials, bundle_sha256, meta, state, created_at, intent_id)
                 VALUES (:org_id, ?1, 1, 's', 't', 1, 'sha', '{}', 'done', '2026-11-10T00:00:00Z', ?2)",
                params![jid, iid],
            )?;
        }
        let usage = r#"{"input_tokens":1,"source":{"class":"free_local","model":"m"}}"#;
        for (rid, jid) in [("run_mine", "job_mine"), ("run_other", "job_other")] {
            db.execute(
                "INSERT INTO runs (org_id, run_id, node_id, job_id, usage, received_at) VALUES (:org_id, ?1, 1, ?2, ?3, '2026-11-10T01:00:00Z')",
                params![rid, jid, usage],
            )?;
        }
        db.execute(
            "INSERT INTO conversations (org_id, conversation_id, node, spec, spec_version, model, instructions_tokens, tools, created_by, created_at)
             VALUES (:org_id, 'conv', 'estate', 'lead', '1', 'm', 0, 0, 'owner', 'now')",
            rusqlite::params![],
        )?;
        db.execute(
            "INSERT INTO credentials (org_id, member_id, id, kind, name, token_hash, created_at, conversation_id, intent_id, node)
             VALUES (:org_id, :member_id, 'lead_cred', 'lead', 'lead', 'h', 'now', 'conv', 'mine', 'estate')",
            rusqlite::params![],
        )?;
        let lead = Client {
            id: "lead_cred".into(),
            kind: Kind::Lead,
            member: crate::access::Member::fixture(),
            conversation_id: Some("conv".into()),
            intent_id: Some("mine".into()),
            node: Some("estate".into()),
        };
        let span = Span {
            period: "all",
            from: None,
            to: None,
        };
        let only = Only {
            lead: Some(lead),
            project: None,
            conversation: None,
            intent: None,
            node: None,
        };
        let v = rollup(&db, By::Node, &span, &only)?;
        assert_eq!(v["total"]["runs"], json!(1));
        Ok(())
    }
}
