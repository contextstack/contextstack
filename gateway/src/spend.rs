//! Pay-as-you-go source budgets (WIRE.md request budget): per-source day and month limits replace a single legacy daily total.
//!
//! A source is the catalog endpoint id an api-class model is served by (openrouter, openai, anthropic,
//! gemini, an overlay id). Local and subscription calls debit the same budgets when a provider
//! reports an actual charge. API admission for a source is OFF until `~/.contextstack/config/gateway/limits.json` gives it a budget:
//!
//! ```json
//! {"sources": {"openrouter": {"usd": 20, "period": "month"},
//!              "anthropic":  {"usd": 2,  "period": "day"},
//!              "*":          {"usd": 25, "period": "month"}}}
//! ```
//!
//! `"*"` is one budget shared by every source: it turns every source on, and a source with its own
//! budget must fit both. A source's own budget of $0 turns it off even under `"*"`. The legacy
//! `{"api_usd_per_day": X}` reads as `"*": {"usd": X, "period": "day"}` (both at once is invalid).
//! Periods are local calendar days and months.
//!
//! Admission, before any connection: the call's reservation is the most it can cost at list price (input
//! estimate × input rate + max output × output rate). It must fit `budget.max_usd` and every applicable
//! budget's usd − spent this period − in-flight holds (concurrent calls cannot jointly exceed). The hold is
//! released when the call settles at its true charge (provider-reported, else priced tokens; over-reservation
//! is flagged but never under-reported) or is
//! released at $0. The counter `~/.contextstack/gateway/spend.json` keeps each source's day and month
//! spend, rewritten by atomic rename under the guard's lock, so a restart keeps it and two calls cannot
//! both take the last dollars. Anything unreadable refuses: fail closed.

use crate::pricing::{self, ResolvedRates};
use chrono::{DateTime, Datelike, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

/// The adapters' `max_tokens` when the request sets none.
pub const DEFAULT_OUTPUT_TOKENS: i64 = 4096;

/// The budget key shared by every source.
pub const ALL: &str = "*";

/// Spend a legacy (single-total) counter recorded, which no source can claim.
const UNATTRIBUTED: &str = "(unattributed)";

const COUNTER_SCHEMA: &str = "csgateway.spend.v2";

/// Float noise allowed when comparing dollars.
const EPS: f64 = 1e-9;

/// How long a budget's spend accumulates before it resets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    /// A local calendar day.
    Day,
    /// A local calendar month.
    Month,
}

impl Period {
    fn name(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Month => "month",
        }
    }

    /// When the period containing `now` ends (local midnight).
    fn resets_at(self, now: DateTime<Local>) -> String {
        let d = now.date_naive();
        let next = match self {
            Self::Day => d.succ_opt(),
            Self::Month if d.month() == 12 => NaiveDate::from_ymd_opt(d.year() + 1, 1, 1),
            Self::Month => NaiveDate::from_ymd_opt(d.year(), d.month() + 1, 1),
        };
        next.and_then(|n| n.and_hms_opt(0, 0, 0))
            .and_then(|n| n.and_local_timezone(Local).earliest())
            .map(crate::log::rfc3339_local)
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Budget {
    usd: f64,
    period: Period,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitsFile {
    #[serde(default)]
    api_usd_per_day: Option<f64>,
    #[serde(default)]
    sources: BTreeMap<String, Budget>,
}

/// Budgets by lower-cased source id, `"*"` included.
type Limits = BTreeMap<String, Budget>;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
struct Spent {
    #[serde(default)]
    day_usd: f64,
    #[serde(default)]
    month_usd: f64,
}

impl Spent {
    fn of(self, p: Period) -> f64 {
        match p {
            Period::Day => self.day_usd,
            Period::Month => self.month_usd,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Counter {
    #[serde(default)]
    schema_version: String,
    #[serde(default)]
    day: String,
    #[serde(default)]
    month: String,
    #[serde(default)]
    sources: BTreeMap<String, Spent>,
    /// Legacy single daily total in older counters: read once, never written.
    #[serde(default, skip_serializing)]
    api_usd: Option<f64>,
}

impl Counter {
    /// The period's spend under a budget key: one source, or every source for `"*"`.
    fn spent(&self, key: &str, p: Period) -> f64 {
        if key == ALL {
            self.sources.values().map(|s| s.of(p)).sum()
        } else {
            self.sources.get(key).map_or(0.0, |s| s.of(p))
        }
    }

    /// Bring a counter read at `now` into the current day and month; a legacy total is unattributed.
    fn roll(mut self, now: DateTime<Local>) -> Result<Self, String> {
        let (day, month) = (
            now.format("%Y-%m-%d").to_string(),
            now.format("%Y-%m").to_string(),
        );
        if let Some(v) = self.api_usd.take()
            && self.schema_version.is_empty()
        {
            let this_month = !self.day.is_empty() && self.day.starts_with(&month);
            self.sources.insert(
                UNATTRIBUTED.into(),
                Spent {
                    day_usd: if self.day == day { v } else { 0.0 },
                    month_usd: if this_month { v } else { 0.0 },
                },
            );
            self.month.clone_from(&month);
        }
        let fine = |v: f64| v.is_finite() && v >= 0.0;
        if !self
            .sources
            .values()
            .all(|s| fine(s.day_usd) && fine(s.month_usd))
        {
            return Err("negative or non-finite spend".into());
        }
        let (new_day, new_month) = (self.day != day, self.month != month);
        for s in self.sources.values_mut() {
            if new_day {
                s.day_usd = 0.0;
            }
            if new_month {
                s.month_usd = 0.0;
            }
        }
        self.schema_version = COUNTER_SCHEMA.into();
        (self.day, self.month) = (day, month);
        Ok(self)
    }
}

/// Why an api call was refused: the additive `error.budget` block on `gateway.policy_denied`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Denial {
    /// `off` (no budget set), `exhausted` (used up this period), `ceiling` (over the request's
    /// `budget.max_usd`) or `invalid` (limit file, counter or `max_usd` unreadable).
    pub reason: &'static str,
    /// The call's source.
    pub source: String,
    /// The budget that refused: the source id or `"*"` (exhausted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<String>,
    /// `day` | `month` (exhausted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub period: Option<&'static str>,
    /// The budget (exhausted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usd: Option<f64>,
    /// Charged this period (exhausted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spent_usd: Option<f64>,
    /// Held by calls in flight (exhausted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held_usd: Option<f64>,
    /// This call's estimate (exhausted, ceiling).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimate_usd: Option<f64>,
    /// The request's ceiling (ceiling).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_usd: Option<f64>,
    /// When the period resets (exhausted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
    /// Human text, the wire error's `message`.
    #[serde(skip)]
    pub message: String,
}

impl std::fmt::Display for Denial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Denial {}

impl Denial {
    fn new(reason: &'static str, source: &str, message: String) -> Self {
        Self {
            reason,
            source: source.into(),
            message,
            ..Self::default()
        }
    }
}

/// The pre-flight hold: input estimate at the input rate plus max output at the output rate (WIRE Request budget).
pub fn estimate(rates: ResolvedRates, body_bytes: usize, max_output_tokens: i64) -> f64 {
    pricing::reserve_estimate(rates, body_bytes, max_output_tokens, DEFAULT_OUTPUT_TOKENS)
}

fn finite_non_negative(v: f64) -> bool {
    v.is_finite() && v >= 0.0
}

/// The budgets that admit a call to `source`, or why it is off.
fn applicable(limits: &Limits, source: &str) -> Result<Vec<(String, Budget)>, String> {
    let own = limits.get(source).copied();
    if own.is_some_and(|b| b.usd <= 0.0) {
        return Err(format!(
            "pay-as-you-go source {source:?} is off: its budget is $0"
        ));
    }
    let all = limits.get(ALL).copied().filter(|b| b.usd > 0.0);
    let mut out: Vec<(String, Budget)> = own.map(|b| (source.to_string(), b)).into_iter().collect();
    out.extend(all.map(|b| (ALL.to_string(), b)));
    if out.is_empty() {
        return Err(format!(
            "pay-as-you-go source {source:?} is off: no budget set (config/gateway/limits.json sources.{source})"
        ));
    }
    Ok(out)
}

/// The limit file (read on every api call, so an edit applies at once), the counter file, and the
/// estimates held by calls in flight, by source.
#[derive(Debug, Default)]
pub struct Guard {
    limits: Option<PathBuf>,
    owner: Option<PathBuf>,
    counter: Option<PathBuf>,
    held: Mutex<BTreeMap<String, f64>>,
    writing: Mutex<()>,
}

/// Why `PUT /v1/budgets` wrote nothing.
#[derive(Debug)]
pub enum SetError {
    /// The document is not a valid limit file (400).
    Invalid(String),
    /// The file could not be written (500).
    Failed(String),
}

/// Whether `given` equals the secret `want`. An empty secret matches nothing. Equal lengths are
/// compared without returning at the first differing byte (the check `Guard::is_owner` uses).
pub(crate) fn constant_time_eq(want: &[u8], given: &[u8]) -> bool {
    !want.is_empty()
        && want.len() == given.len()
        && want
            .iter()
            .zip(given)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

fn held_under(held: &BTreeMap<String, f64>, key: &str) -> f64 {
    if key == ALL {
        held.values().sum()
    } else {
        held.get(key).copied().unwrap_or(0.0)
    }
}

impl Guard {
    /// The guard under HOME: `config/gateway/limits.json` and `gateway/spend.json`. No HOME refuses every api call.
    pub fn new(home: Option<&Path>) -> Self {
        let root = home.map(|h| h.join(".contextstack"));
        Self {
            limits: root
                .as_ref()
                .map(|r| r.join("config").join("gateway").join("limits.json")),
            owner: root
                .as_ref()
                .map(|r| r.join("config").join("gateway").join("owner-token")),
            counter: root.map(|r| r.join("gateway").join("spend.json")),
            held: Mutex::new(BTreeMap::new()),
            writing: Mutex::new(()),
        }
    }

    /// Decide one api call to `source`; `max_usd` is the request's `budget.max_usd` ("" is none).
    pub fn admit(
        self: &Arc<Self>,
        source: &str,
        max_usd: &str,
        est: f64,
    ) -> Result<Reservation, Box<Denial>> {
        let source = source.to_ascii_lowercase();
        let invalid = |m: String| Box::new(Denial::new("invalid", &source, m));
        // Serialize the spent + held snapshot against the hold-to-charge transition.
        let _write = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        let limits = self.limits().map_err(invalid)?;
        let apply =
            applicable(&limits, &source).map_err(|m| Box::new(Denial::new("off", &source, m)))?;
        if !max_usd.is_empty() {
            let ceiling = max_usd
                .parse::<f64>()
                .ok()
                .filter(|v| finite_non_negative(*v))
                .ok_or_else(|| {
                    invalid(format!(
                        "budget.max_usd {max_usd:?} is not a non-negative decimal"
                    ))
                })?;
            if est > ceiling + EPS {
                let mut d = Denial::new(
                    "ceiling",
                    &source,
                    format!(
                        "this call estimates ${est:.6}, over the request's budget.max_usd ${ceiling:.6}"
                    ),
                );
                (d.estimate_usd, d.max_usd) = (Some(est), Some(ceiling));
                return Err(Box::new(d));
            }
        }
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Local::now();
        let c = self.read(now).map_err(invalid)?;
        for (key, b) in &apply {
            let (spent, h) = (c.spent(key, b.period), held_under(&held, key));
            if spent + h + EPS >= b.usd || spent + h + est > b.usd + EPS {
                let period = b.period.name();
                let mut d = Denial::new(
                    "exhausted",
                    &source,
                    format!(
                        "pay-as-you-go budget {key:?} used up: ${spent:.6} spent this {period} and ${h:.6} in flight, this call estimates ${est:.6}, budget ${:.6}; resets {}",
                        b.usd,
                        b.period.resets_at(now)
                    ),
                );
                d.limit = Some(key.clone());
                d.period = Some(period);
                (d.usd, d.spent_usd, d.held_usd) = (Some(b.usd), Some(spent), Some(h));
                d.estimate_usd = Some(est);
                d.resets_at = Some(b.period.resets_at(now));
                return Err(Box::new(d));
            }
        }
        *held.entry(source.clone()).or_default() += est;
        Ok(Reservation {
            guard: Arc::clone(self),
            source,
            est,
            done: false,
        })
    }

    pub async fn admit_async(
        self: &Arc<Self>,
        source: &str,
        max_usd: &str,
        est: f64,
    ) -> Result<Reservation, Box<Denial>> {
        let guard = Arc::clone(self);
        let source = source.to_owned();
        let max_usd = max_usd.to_owned();
        tokio::task::spawn_blocking(move || guard.admit(&source, &max_usd, est))
            .await
            .map_err(|e| Box::new(Denial::new("invalid", "", e.to_string())))?
    }

    /// Debit a provider-reported charge even when admission did not require a hold.
    pub fn debit_reported(&self, source: &str, charged: f64) {
        let _write = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        if let Err(error) = self.record_charge(&source.to_ascii_lowercase(), charged) {
            crate::logf!("paid API counter not updated: {error}");
        }
    }

    // A positive charge requires the caller to hold `writing`.
    fn record_charge(&self, source: &str, charged: f64) -> Result<(), String> {
        if !(charged > 0.0 && charged.is_finite()) {
            return Ok(());
        }
        let mut counter = self.read(Local::now())?;
        let spend = counter.sources.entry(source.to_owned()).or_default();
        spend.day_usd += charged;
        spend.month_usd += charged;
        self.write(&counter)
    }

    /// Each source's state for `GET /v1/budgets`: `known` are the pay-as-you-go sources the catalog and
    /// inventory name; configured ones are added. No key or credential is read.
    pub fn report(&self, known: impl IntoIterator<Item = String>) -> Value {
        let _write = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Local::now();
        let held = self
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let state = self.limits().and_then(|l| self.read(now).map(|c| (l, c)));
        let mut names: BTreeSet<String> =
            known.into_iter().map(|s| s.to_ascii_lowercase()).collect();
        let (limits, counter) = match state {
            Ok(s) => s,
            Err(e) => {
                let rows: Vec<Value> = names
                    .iter()
                    .map(|s| json!({"source": s, "state": "invalid"}))
                    .collect();
                return json!({"schema_version": "csgateway.budgets.v1", "sources": rows, "total": null, "error": e});
            }
        };
        names.extend(
            counter
                .sources
                .keys()
                .filter(|s| s.as_str() != UNATTRIBUTED)
                .cloned(),
        );
        names.extend(limits.keys().filter(|k| *k != ALL).cloned());
        let budget = |key: &str, b: &Budget| {
            let (spent, h) = (counter.spent(key, b.period), held_under(&held, key));
            json!({"usd": b.usd, "period": b.period.name(), "spent_usd": spent, "held_usd": h,
                   "remaining_usd": (b.usd - spent - h).max(0.0), "resets_at": b.period.resets_at(now)})
        };
        let rows: Vec<Value> = names.iter().map(|s| {
            let spent = counter.sources.get(s).copied().unwrap_or_default();
            let mut row = json!({"source": s, "state": "off", "budget": null, "shared": false,
                                 "spent": {"day_usd": spent.day_usd, "month_usd": spent.month_usd}});
            let Ok(apply) = applicable(&limits, s) else { return row };
            let views: Vec<(Value, &Budget)> = apply.iter().map(|(k, b)| (budget(k, b), b)).collect();
            let tight = views.iter().min_by(|a, b| a.0["remaining_usd"].as_f64().partial_cmp(&b.0["remaining_usd"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
            let remaining = tight.and_then(|t| t.0["remaining_usd"].as_f64()).unwrap_or(0.0);
            row["state"] = json!(if remaining <= EPS { "exhausted" } else { "active" });
            row["remaining_usd"] = json!(remaining);
            if remaining <= EPS {
                row["resets_at"] = tight.map_or(Value::Null, |t| t.0["resets_at"].clone());
            }
            row["shared"] = json!(apply.iter().any(|(k, _)| k == ALL));
            if let Some((v, _)) = apply.iter().zip(&views).find(|((k, _), _)| k == s).map(|(_, v)| v) {
                row["budget"] = v.clone();
            }
            row
        }).collect();
        let total = limits
            .get(ALL)
            .filter(|b| b.usd > 0.0)
            .map_or(Value::Null, |b| budget(ALL, b));
        json!({"schema_version": "csgateway.budgets.v1", "sources": rows, "total": total})
    }

    fn limits(&self) -> Result<Limits, String> {
        let path = self
            .limits
            .as_ref()
            .ok_or("paid API limit: no HOME, so no limit file")?;
        match std::fs::read(path) {
            Ok(raw) => parse_limits(&raw)
                .map_err(|e| format!("paid API limit file {} invalid: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Limits::new()),
            Err(e) => Err(format!("paid API limit file unreadable: {e}")),
        }
    }

    /// Create the owner token once (`config/gateway/owner-token`, 0600 in a 0700 dir) if absent:
    /// 32 random bytes as hex, never logged. `Ok(Some(path))` when this start created it.
    pub fn ensure_owner_token(&self) -> Result<Option<PathBuf>, String> {
        use std::io::{Read, Write};
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
        let path = self.owner.as_ref().ok_or("no HOME, so no owner token")?;
        let dir = path.parent().ok_or("owner token has no directory")?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| e.to_string())?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        let mut f = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };
        let mut bytes = [0u8; 32];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut r| r.read_exact(&mut bytes))
            .map_err(|e| e.to_string())?;
        let hex = bytes.iter().fold(String::with_capacity(64), |mut out, b| {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
            out
        });
        f.write_all(format!("{hex}\n").as_bytes())
            .and_then(|()| f.sync_all())
            .map_err(|e| e.to_string())?;
        Ok(Some(path.clone()))
    }

    /// Whether `given` is the owner token (read from its file on each check; constant-time compare).
    /// A missing, unreadable or empty token file matches nothing.
    pub fn is_owner(&self, given: &str) -> bool {
        let Some(want) = self
            .owner
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
        else {
            return false;
        };
        constant_time_eq(want.trim().as_bytes(), given.trim().as_bytes())
    }

    /// Replace the limit file with `raw` (`PUT /v1/budgets`): validated as the guard reads it, then
    /// written in its canonical `{"sources": …}` form (the legacy total becomes `"*"` daily) by atomic
    /// rename. An invalid document writes nothing. The next call or report reads the new budgets.
    pub fn replace(&self, raw: &[u8]) -> Result<(), SetError> {
        let limits = parse_limits(raw).map_err(SetError::Invalid)?;
        let path = self
            .limits
            .as_ref()
            .ok_or_else(|| SetError::Failed("no HOME, so no limit file".into()))?;
        let mut body = serde_json::to_vec_pretty(&json!({ "sources": limits }))
            .map_err(|e| SetError::Failed(e.to_string()))?;
        body.push(b'\n');
        let _one = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        write_atomic(path, &body).map_err(SetError::Failed)
    }

    /// The counter as of `now`: missing is $0; another day's or month's spend is $0 for that period.
    fn read(&self, now: DateTime<Local>) -> Result<Counter, String> {
        let path = self.counter.as_ref().ok_or("paid API counter: no HOME")?;
        let unreadable = |e: &dyn std::fmt::Display| {
            format!("paid API counter {} unreadable: {e}", path.display())
        };
        let c = match std::fs::read(path) {
            Ok(raw) => serde_json::from_slice::<Counter>(&raw).map_err(|e| unreadable(&e))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Counter::default(),
            Err(e) => return Err(unreadable(&e)),
        };
        c.roll(now).map_err(|e| unreadable(&e))
    }

    fn write(&self, c: &Counter) -> Result<(), String> {
        let path = self.counter.as_ref().ok_or("paid API counter: no HOME")?;
        let mut body = serde_json::to_vec(c).map_err(|e| e.to_string())?;
        body.push(b'\n');
        write_atomic(path, &body)
    }
}

/// A limit file's budgets, or why it is invalid.
fn parse_limits(raw: &[u8]) -> Result<Limits, String> {
    let v: Value = serde_json::from_slice(raw).map_err(|e| e.to_string())?;
    if !v.is_object() {
        return Err("want a JSON object".into());
    }
    let doc: LimitsFile = serde_json::from_value(v).map_err(|e| e.to_string())?;
    let mut out = Limits::new();
    for (k, b) in doc.sources {
        if !finite_non_negative(b.usd) {
            return Err(format!("sources.{k}.usd must be a non-negative number"));
        }
        if out.insert(k.to_ascii_lowercase(), b).is_some() {
            return Err(format!("sources.{k} is named twice"));
        }
    }
    if let Some(v) = doc.api_usd_per_day {
        if !finite_non_negative(v) {
            return Err("api_usd_per_day must be a non-negative number".into());
        }
        if out.contains_key(ALL) {
            return Err("api_usd_per_day and sources.\"*\" both set the shared budget".into());
        }
        out.insert(
            ALL.into(),
            Budget {
                usd: v,
                period: Period::Day,
            },
        );
    }
    Ok(out)
}

/// Write `body` to `path` by a fsynced temp file and rename: a reader sees the old file or the new one.
fn write_atomic(path: &Path, body: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| e.to_string())?;
    f.write_all(body).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// An admitted api call's hold on its source's budgets, released on drop unless settled first.
pub struct Reservation {
    guard: Arc<Guard>,
    source: String,
    est: f64,
    done: bool,
}

impl Reservation {
    /// The estimate held at admission (the settlement reference cap).
    pub fn reservation_usd(&self) -> f64 {
        self.est
    }

    /// Release the hold and return the charge recorded against its source.
    pub fn settle(mut self, charged_usd: f64, over_reservation: bool) -> f64 {
        if over_reservation {
            crate::log::row(&json!({
                "event": "spend_over_reservation",
                "source": self.source,
                "reservation_usd": self.est,
                "charged_usd": charged_usd,
            }));
        }
        if let Err(e) = self.finish(charged_usd) {
            crate::logf!("paid API counter not updated: {e}");
            0.0
        } else {
            charged_usd
        }
    }

    fn finish(&mut self, charged: f64) -> Result<(), String> {
        if self.done {
            return Ok(());
        }
        self.done = true;
        let g = &self.guard;
        let _write = (charged > 0.0 && charged.is_finite())
            .then(|| g.writing.lock().unwrap_or_else(PoisonError::into_inner));
        let mut held = g.held.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(h) = held.get_mut(&self.source) {
            *h -= self.est;
            if *h <= EPS {
                held.remove(&self.source);
            }
        }
        drop(held);
        g.record_charge(&self.source, charged)
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let _ = self.finish(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Res = Result<(), Box<dyn std::error::Error>>;

    fn guard(limits: &str) -> Result<(tempfile::TempDir, Arc<Guard>), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let g = Arc::new(Guard::new(Some(dir.path())));
        let path = g.limits.clone().ok_or("no path")?;
        std::fs::create_dir_all(path.parent().ok_or("no parent")?)?;
        std::fs::write(&path, limits)?;
        Ok((dir, g))
    }

    fn reason(r: Result<Reservation, Box<Denial>>) -> &'static str {
        r.err().map_or("admitted", |d| d.reason)
    }

    #[test]
    fn off_active_exhausted_ceiling() -> Res {
        let (_d, g) = guard(
            r#"{"sources": {"or": {"usd": 0.01, "period": "day"}, "zero": {"usd": 0, "period": "day"}}}"#,
        )?;
        assert_eq!(reason(g.admit("openai", "", 0.001)), "off", "no budget");
        assert_eq!(
            reason(g.admit("openai", "5", 0.001)),
            "off",
            "max_usd cannot turn a source on"
        );
        assert_eq!(reason(g.admit("zero", "", 0.001)), "off", "$0 is off");
        let h = g.admit("OR", "", 0.004)?;
        assert_eq!(
            reason(g.admit("or", "", 0.007)),
            "exhausted",
            "held estimates count"
        );
        h.settle(0.009, false);
        assert_eq!(
            reason(g.admit("or", "1", 0.002)),
            "exhausted",
            "max_usd never raises"
        );
        assert_eq!(reason(g.admit("or", "0.0001", 0.0005)), "ceiling");
        drop(g.admit("or", "", 0.001)?); // dropped unsettled: released, nothing counted
        assert!(g.admit("or", "", 0.001).is_ok());
        assert_eq!(reason(g.admit("or", "abc", 0.0)), "invalid");
        Ok(())
    }

    #[test]
    fn shared_budget_and_legacy() -> Res {
        let (_d, g) = guard(r#"{"api_usd_per_day": 0.01}"#)?;
        g.admit("a", "", 0.0)?.settle(0.006, false);
        assert_eq!(
            reason(g.admit("b", "", 0.005)),
            "exhausted",
            "the legacy total is shared"
        );
        let (_d, g) = guard(
            r#"{"sources": {"a": {"usd": 1, "period": "month"}, "*": {"usd": 0.01, "period": "day"}}}"#,
        )?;
        g.admit("a", "", 0.0)?.settle(0.01, false);
        let d = g.admit("a", "", 0.0).err().ok_or("admitted")?;
        assert_eq!((d.reason, d.limit.as_deref()), ("exhausted", Some(ALL)));
        let (_d, g) = guard(
            r#"{"sources": {"a": {"usd": 0, "period": "day"}, "*": {"usd": 1, "period": "day"}}}"#,
        )?;
        assert_eq!(reason(g.admit("a", "", 0.0)), "off", "own $0 wins over *");
        assert!(g.admit("b", "", 0.0).is_ok());
        Ok(())
    }

    #[test]
    fn exhausted_at_zero_estimate() -> Res {
        let (_d, g) = guard(r#"{"sources": {"a": {"usd": 0.01, "period": "day"}}}"#)?;
        g.admit("a", "", 0.0)?.settle(0.01, false);
        assert_eq!(
            reason(g.admit("a", "", 0.0)),
            "exhausted",
            "a used-up budget refuses even a $0 estimate"
        );
        Ok(())
    }

    #[test]
    fn limit_file_is_strict() -> Res {
        for body in [
            r#"{"api_usd_per_day": -1}"#,
            r#"{"x": 1}"#,
            "nope",
            r#"{"api_usd_per_day": "1"}"#,
            r#"{"sources": {"a": {"usd": 1}}}"#,
            r#"{"sources": {"a": {"usd": 1, "period": "week"}}}"#,
            r#"{"sources": {"a": {"usd": -1, "period": "day"}}}"#,
            r#"{"sources": {"a": {"usd": 1, "period": "day"}, "A": {"usd": 1, "period": "day"}}}"#,
            r#"{"api_usd_per_day": 1, "sources": {"*": {"usd": 1, "period": "day"}}}"#,
            "[]",
        ] {
            let (_d, g) = guard(body)?;
            assert_eq!(
                reason(g.admit("a", "1", 0.0)),
                "invalid",
                "{body} must refuse"
            );
        }
        assert!(Guard::new(None).limits().is_err(), "no HOME refuses");
        Ok(())
    }

    #[test]
    fn counter_rolls_and_imports_legacy() -> Res {
        let now = Local::now();
        let (day, month) = (
            now.format("%Y-%m-%d").to_string(),
            now.format("%Y-%m").to_string(),
        );
        let legacy = Counter {
            day: day.clone(),
            api_usd: Some(2.0),
            ..Counter::default()
        };
        let c = legacy.roll(now)?;
        assert!(
            (c.spent(ALL, Period::Day) - 2.0).abs() < EPS
                && (c.spent(ALL, Period::Month) - 2.0).abs() < EPS
        );
        let mut old = Counter {
            schema_version: COUNTER_SCHEMA.into(),
            day: "2000-01-01".into(),
            month: month.clone(),
            ..Counter::default()
        };
        old.sources.insert(
            "a".into(),
            Spent {
                day_usd: 1.0,
                month_usd: 3.0,
            },
        );
        let c = old.roll(now)?;
        assert_eq!(
            (c.spent("a", Period::Day), c.spent("a", Period::Month)),
            (0.0, 3.0)
        );
        assert_eq!(c.day, day);
        Ok(())
    }

    #[test]
    fn resets_at_is_the_next_boundary() {
        let now = Local::now();
        let day = Period::Day.resets_at(now);
        let month = Period::Month.resets_at(now);
        assert!(
            day.contains("T00:00:00") && month.contains("-01T00:00:00"),
            "{day} {month}"
        );
        assert!(day <= month || month.len() == day.len());
    }

    #[test]
    fn replace_validates_then_writes_canonically() -> Res {
        let (_d, g) = guard(r#"{"api_usd_per_day": 1}"#)?;
        let path = g.limits.clone().ok_or("no path")?;
        let before = std::fs::read(&path)?;
        assert!(matches!(
            g.replace(br#"{"sources": {"a": {"usd": 1}}}"#),
            Err(SetError::Invalid(_))
        ));
        assert_eq!(
            std::fs::read(&path)?,
            before,
            "an invalid document writes nothing"
        );
        g.replace(br#"{"sources": {"OR": {"usd": 2, "period": "month"}}, "api_usd_per_day": 1}"#)
            .map_err(|e| format!("{e:?}"))?;
        let doc: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        assert_eq!(
            doc,
            json!({"sources": {"*": {"usd": 1.0, "period": "day"}, "or": {"usd": 2.0, "period": "month"}}})
        );
        assert!(g.admit("or", "", 0.0).is_ok());
        Ok(())
    }

    #[test]
    fn owner_token_created_once() -> Res {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let g = Guard::new(Some(dir.path()));
        assert!(!g.is_owner(""), "no file matches nothing");
        let path = g.ensure_owner_token()?.ok_or("not created")?;
        let token = std::fs::read_to_string(&path)?;
        assert_eq!(token.trim().len(), 64);
        assert_eq!(
            std::fs::metadata(&path)?.permissions().mode() & 0o777,
            0o600
        );
        let parent = path.parent().ok_or("no parent")?;
        assert_eq!(
            std::fs::metadata(parent)?.permissions().mode() & 0o777,
            0o700
        );
        assert!(g.ensure_owner_token()?.is_none(), "created once");
        assert_eq!(std::fs::read_to_string(&path)?, token, "never replaced");
        assert!(g.is_owner(token.trim()) && !g.is_owner("") && !g.is_owner(&token.trim()[1..]));
        Ok(())
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn concurrent_settlements_sum() -> Res {
        use std::sync::Barrier;
        use std::thread::{self, JoinHandle};
        let (_d, g) = guard(r#"{"sources": {"a": {"usd": 100, "period": "day"}}}"#)?;
        let n = 32usize;
        let charge = 0.001;
        let barrier = Arc::new(Barrier::new(n));
        let mut handles: Vec<JoinHandle<()>> = Vec::with_capacity(n);
        for _ in 0..n {
            let g = Arc::clone(&g);
            let b = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                let r = g
                    .admit("a", "", 0.0)
                    .unwrap_or_else(|e| panic!("admitted: {e}"));
                b.wait();
                r.settle(charge, false);
            }));
        }
        for h in handles {
            h.join().map_err(|_| "settlement worker panicked")?;
        }
        let want = n as f64 * charge;
        let spent = g.read(Local::now())?.spent("a", Period::Day);
        assert!(
            (spent - want).abs() < EPS,
            "expected ${want} spent, got ${spent}"
        );
        Ok(())
    }

    #[test]
    fn invalid_denial_wire_has_no_path() -> Res {
        use crate::errors::WireError;
        let (_d, g) = guard("not json")?;
        let d = g
            .admit("a", "", 0.0)
            .err()
            .unwrap_or_else(|| panic!("expected denial"));
        assert_eq!(d.reason, "invalid");
        let w = WireError::budget(d);
        assert_eq!(w.budget.as_ref().map(|b| b.reason), Some("invalid"));
        assert!(!w.message.contains('/'));
        assert!(!w.message.contains("limits.json"));
        assert!(!w.message.contains("spend.json"));
        Ok(())
    }

    #[test]
    fn estimate_matches_blended_fallback() {
        let rates = ResolvedRates {
            input_per_1k: 0.01,
            output_per_1k: 0.01,
            cache_read_per_1k: 0.01,
            cache_write_per_1k: 0.01,
        };
        let want = 0.01 * 4196.0 / 1000.0;
        assert!((estimate(rates, 400, 0) - want).abs() < f64::EPSILON);
    }
}

#[cfg(test)]
mod settlement_lock_tests {
    use super::*;

    #[test]
    fn pending_counter_write_leaves_hold_lock_available() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let guard = Arc::new(Guard::new(Some(dir.path())));
        guard
            .replace(br#"{"api_usd_per_day": 1}"#)
            .map_err(|e| format!("{e:?}"))?;
        let reserved = guard.admit("api", "", 0.5)?;
        let writing = guard.writing.lock().map_err(|_| "writer lock")?;
        let (start, started) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = start.send(());
            reserved.settle(0.5, false)
        });
        started.recv()?;
        std::thread::sleep(std::time::Duration::from_millis(50));
        let hold_available = guard.held.try_lock().is_ok();
        drop(writing);
        assert_eq!(worker.join().map_err(|_| "settlement panicked")?, 0.5);
        assert!(hold_available);
        Ok(())
    }
}
