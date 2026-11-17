//! Budgets in dollars of actual pay-as-you-go spending: money, periods, the reservation rule and the
//! spec's ceiling. Nothing here touches the database; `reservations.rs` applies it.
//!
//! Money is kept as whole micro-dollars (`Micros`), so sums and differences are exact.
//!
//! **The reservation rule.** A job under one or more budgets (its project's, its conversation's) is given
//! `min(ceiling, room)`, where `room` is the least that remains of those budgets (`usd − spent this period −
//! reserved for running jobs`) and `ceiling` is what its spec states (`budget.usd × trials`; when the spec states
//! none, the whole room). While `room` is zero the job waits. The amount reserved is written into the job's spec
//! as `budget.usd` (per run: the amount ÷ trials, rounded down), so the node's runner and the gateway hold the
//! run to it. When the run reports its actual charge the reservation settles and the rest is released.

use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone, Utc};
use serde_json::{Value, json};

/// Whole micro-dollars.
pub type Micros = i64;

/// Dollars to micro-dollars: finite and non-negative, rounded to the nearest micro-dollar.
pub fn micros(usd: f64) -> Option<Micros> {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "bounded to 1e12 dollars before the cast"
    )]
    (usd.is_finite() && (0.0..1e12).contains(&usd)).then(|| (usd * 1e6).round() as Micros)
}

/// Micro-dollars as dollars, for JSON.
pub fn usd(m: Micros) -> Value {
    #[allow(clippy::cast_precision_loss, reason = "money is far below 2^53 micros")]
    let dollars = m as f64 / 1e6;
    json!(dollars)
}

/// Micro-dollars as text a person reads: `$1.25`. Whole cents show as cents. Anything finer shows more digits, so
/// small amounts stay honest: under $1, three significant figures (`$0.0184`, `$0.00154`); from $1, four decimals
/// (`$4.9999`). Extra digits are cut, never rounded up, so an amount left is never shown as the whole budget once
/// anything was spent.
pub fn dollars(m: Micros) -> String {
    let m = m.max(0);
    let (whole, frac) = (m / 1_000_000, m % 1_000_000);
    let places = if frac % 10_000 == 0 {
        2
    } else if whole > 0 {
        4
    } else {
        // Leading zeros after the point, then three significant figures.
        let zeros = 6 - frac.to_string().len();
        (zeros + 3).min(6)
    };
    let digits = format!("{frac:06}");
    let shown = digits[..places].trim_end_matches('0');
    let shown = format!("{shown:0<2}");
    format!("${whole}.{shown}")
}

/// A dollar amount from JSON: a number or a decimal string, non-negative.
pub fn amount(v: &Value) -> Option<Micros> {
    match v {
        Value::Number(n) => n.as_f64().and_then(micros),
        Value::String(s) => s.trim().parse::<f64>().ok().and_then(micros),
        _ => None,
    }
}

/// How long a budget's spending counts. Days and months are local calendar days and months on the Director's host,
/// the gateway's convention for source budgets (WIRE.md, budgets lines), so a project's day and a source's day are
/// the same day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    /// Everything ever charged under it.
    Total,
    /// The current local day.
    Day,
    /// The current local month.
    Month,
}

impl Period {
    /// A period by its wire name.
    pub fn named(s: &str) -> Option<Self> {
        match s {
            "total" => Some(Self::Total),
            "day" => Some(Self::Day),
            "month" => Some(Self::Month),
            _ => None,
        }
    }

    /// Its wire name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Total => "total",
            Self::Day => "day",
            Self::Month => "month",
        }
    }

    /// `(start, end)` of the period holding this moment on the Director's host, as UTC stamps
    /// (`YYYY-MM-DDTHH:MM:SSZ`, the form every stored time takes); `None` for `total`.
    pub fn window(self) -> Option<(String, String)> {
        self.window_at(&Local::now())
    }

    /// `(start, end)` of the period holding `now`, in `now`'s time zone; `None` for `total`.
    pub fn window_at<Tz: TimeZone>(self, now: &DateTime<Tz>) -> Option<(String, String)> {
        let unit = match self {
            Self::Total => return None,
            Self::Day => "day",
            Self::Month => "month",
        };
        span(unit, now.date_naive(), &now.timezone())
    }
}

/// `(start, end)` of the local `day` or `month` holding `date` in `tz`, as UTC stamps; the end is exclusive. A
/// boundary is local midnight; where a clock change skips midnight, the first moment of that day.
pub fn span<Tz: TimeZone>(unit: &str, date: NaiveDate, tz: &Tz) -> Option<(String, String)> {
    let (start, end) = match unit {
        "day" => (date, date.succ_opt()?),
        "month" => {
            let first = date.with_day(1)?;
            (first, first.checked_add_months(chrono::Months::new(1))?)
        }
        _ => return None,
    };
    Some((midnight(start, tz)?, midnight(end, tz)?))
}

/// The first moment of `date` in `tz`, as a UTC stamp.
fn midnight<Tz: TimeZone>(date: NaiveDate, tz: &Tz) -> Option<String> {
    let at = |h| {
        date.and_hms_opt(h, 0, 0)
            .and_then(|t| tz.from_local_datetime(&t).earliest())
    };
    let first = at(0).or_else(|| at(1))?;
    Some(
        first
            .with_timezone(&Utc)
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string(),
    )
}

/// Which budget: a project's or a conversation's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// `project` or `conversation`.
    pub kind: &'static str,
    /// Its id.
    pub id: String,
}

/// One budget as it stands.
#[derive(Debug, Clone)]
pub struct Room {
    /// Whose budget.
    pub scope: Scope,
    /// The budget.
    pub budget: Micros,
    /// Charged under it this period.
    pub spent: Micros,
    /// Set aside for jobs that have not settled.
    pub reserved: Micros,
}

impl Room {
    /// What may still be reserved; never below zero.
    pub fn remaining(&self) -> Micros {
        (self.budget - self.spent - self.reserved).max(0)
    }

    /// Spent in full: "budget used up", not merely spoken for by running work.
    pub fn exhausted(&self) -> bool {
        self.spent >= self.budget
    }
}

/// Why a job waits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    /// `budget_exhausted` (spent in full) or `budget_reserved` (what remains is set aside for running jobs).
    pub reason: &'static str,
    /// The budget that holds it.
    pub scope: Scope,
    /// Words a client can show.
    pub detail: String,
}

/// What the rule gives a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// No budget applies.
    Free,
    /// Set aside this much; the job is offered.
    Reserve(Micros),
    /// Nothing remains; the job is created and held.
    Wait(Waiting),
}

/// The reservation rule (module docs): `ceiling` is `budget.usd × trials` from the spec, `None` when it states none.
pub fn place(ceiling: Option<Micros>, rooms: &[Room]) -> Place {
    let Some(tight) = rooms.iter().min_by_key(|r| r.remaining()) else {
        return Place::Free;
    };
    if tight.remaining() > 0 {
        let room = tight.remaining();
        return Place::Reserve(ceiling.map_or(room, |c| c.min(room)));
    }
    // Prefer to name a budget that is used up over one that is only spoken for.
    let holder = rooms
        .iter()
        .find(|r| r.remaining() == 0 && r.exhausted())
        .unwrap_or(tight);
    let money = dollars;
    let (reason, detail) = if holder.exhausted() {
        (
            "budget_exhausted",
            format!(
                "budget used up: the {} has spent {} of its {} budget",
                holder.scope.kind,
                money(holder.spent),
                money(holder.budget)
            ),
        )
    } else {
        (
            "budget_reserved",
            format!(
                "waiting for running work: all {} left of the {}'s budget is reserved for jobs not yet settled",
                money(holder.budget - holder.spent),
                holder.scope.kind
            ),
        )
    };
    Place::Wait(Waiting {
        reason,
        scope: holder.scope.clone(),
        detail,
    })
}

/// What each run of a job may spend: the reservation split evenly over its trials, rounded down.
pub fn per_run(reserved: Micros, trials: u32) -> Micros {
    reserved / Micros::from(trials.max(1))
}

/// What settling releases: the reservation less the charge, never below zero. A charge above the reservation
/// counts in full as spent; it releases nothing.
pub fn released(reserved: Micros, charged: Micros) -> Micros {
    (reserved - charged).max(0)
}

/// The spec's ceiling for a whole job: `budget.usd × trials`; `None` when it states no positive amount.
pub fn ceiling(spec: &[u8], trials: u32) -> Result<Option<Micros>, String> {
    let doc: serde_yaml::Value =
        serde_yaml::from_slice(spec).map_err(|e| format!("the spec is not YAML: {e}"))?;
    if !doc.is_mapping() {
        return Err("the spec is not a YAML mapping".into());
    }
    let usd = doc
        .get("budget")
        .and_then(|b| b.get("usd"))
        .and_then(serde_yaml::Value::as_f64)
        .unwrap_or(0.0);
    Ok(micros(usd)
        .filter(|m| *m > 0)
        .map(|m| m.saturating_mul(Micros::from(trials.max(1)))))
}

/// The spec with `budget.usd` set to `per_run`, edited in place in the budget's own style (`spec_edit.rs`): every
/// other byte stays as the client wrote it.
pub fn with_usd(spec: &[u8], per_run: Micros) -> Result<Vec<u8>, String> {
    crate::spec_edit::set_budget_usd(spec, per_run)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn room(kind: &'static str, budget: f64, spent: f64, reserved: f64) -> Room {
        let m = |x| micros(x).unwrap_or_default();
        Room {
            scope: Scope {
                kind,
                id: kind.into(),
            },
            budget: m(budget),
            spent: m(spent),
            reserved: m(reserved),
        }
    }

    #[test]
    fn money_is_exact() {
        assert_eq!(micros(0.1), Some(100_000));
        assert_eq!(micros(-1.0), None);
        assert_eq!(micros(f64::NAN), None);
        assert_eq!(amount(&json!("2.50")), Some(2_500_000));
        assert_eq!(amount(&json!(3)), Some(3_000_000));
        assert_eq!(amount(&json!(true)), None);
        assert_eq!(usd(1_250_000), json!(1.25));
    }

    #[test]
    fn small_amounts_are_shown_honestly() {
        assert_eq!(dollars(0), "$0.00");
        assert_eq!(dollars(1_250_000), "$1.25");
        assert_eq!(dollars(2_500_000), "$2.50");
        assert_eq!(dollars(20_000), "$0.02");
        // $0.02 with $0.001542 spent: not "$0.02 of $0.02 left".
        assert_eq!(dollars(18_458), "$0.0184");
        assert_eq!(dollars(1_542), "$0.00154");
        assert_eq!(dollars(1_700), "$0.0017");
        assert_eq!(dollars(198_300), "$0.198");
        assert_eq!(dollars(42), "$0.000042");
        assert_eq!(dollars(1), "$0.000001");
        assert_eq!(dollars(4_999_999), "$4.9999");
        assert_eq!(dollars(12_345_678), "$12.3456");
        assert_eq!(dollars(12_340_000), "$12.34");
        assert_eq!(dollars(-5), "$0.00");
    }

    #[test]
    fn no_budget_is_free() {
        assert_eq!(place(Some(5), &[]), Place::Free);
    }

    #[test]
    fn the_ceiling_is_reserved_capped_by_what_remains() {
        let p = [room("project", 5.0, 1.0, 1.0)];
        assert_eq!(place(micros(2.0), &p), Place::Reserve(2_000_000));
        assert_eq!(place(micros(9.0), &p), Place::Reserve(3_000_000), "capped");
        assert_eq!(
            place(None, &p),
            Place::Reserve(3_000_000),
            "no ceiling: the room"
        );
    }

    #[test]
    fn the_tightest_budget_decides() {
        let p = [
            room("project", 5.0, 0.0, 0.0),
            room("conversation", 1.0, 0.25, 0.0),
        ];
        assert_eq!(place(micros(2.0), &p), Place::Reserve(750_000));
    }

    #[test]
    fn two_jobs_with_room_for_one() {
        // A $5 project; each job's spec states $5. The first takes it all, the second waits for it.
        let mut p = [room("project", 5.0, 0.0, 0.0)];
        let Place::Reserve(first) = place(micros(5.0), &p) else {
            panic!("the first job starts")
        };
        p[0].reserved += first;
        let Place::Wait(w) = place(micros(5.0), &p) else {
            panic!("the second waits")
        };
        assert_eq!(w.reason, "budget_reserved");
        assert!(w.detail.contains("reserved"), "{}", w.detail);
        // The first settles at $1.20: $3.80 is released and the second starts with it.
        let charged = micros(1.2).unwrap_or_default();
        assert_eq!(released(first, charged), 3_800_000);
        p[0].reserved -= first;
        p[0].spent += charged;
        assert_eq!(place(micros(5.0), &p), Place::Reserve(3_800_000));
    }

    #[test]
    fn exhausted_is_not_reserved() {
        let used = [room("project", 5.0, 5.0, 0.0)];
        let Place::Wait(w) = place(None, &used) else {
            panic!("waits")
        };
        assert_eq!(w.reason, "budget_exhausted");
        assert!(w.detail.starts_with("budget used up"), "{}", w.detail);
        let over = [room("project", 5.0, 6.0, 0.0)];
        assert!(matches!(place(None, &over), Place::Wait(w) if w.reason == "budget_exhausted"));
        // Of two full budgets, the used-up one is named.
        let both = [
            room("conversation", 1.0, 0.0, 1.0),
            room("project", 2.0, 2.0, 0.0),
        ];
        assert!(
            matches!(place(None, &both), Place::Wait(w) if w.scope.kind == "project" && w.reason == "budget_exhausted")
        );
    }

    #[test]
    fn settling_releases_the_difference() {
        assert_eq!(released(2_000_000, 0), 2_000_000, "a refusal costs $0");
        assert_eq!(released(2_000_000, 500_000), 1_500_000);
        assert_eq!(released(2_000_000, 3_000_000), 0, "never negative");
        assert_eq!(per_run(1_000_000, 3), 333_333);
        assert_eq!(per_run(7, 0), 7);
    }

    #[test]
    fn periods() {
        let at = Utc.with_ymd_and_hms(2026, 12, 31, 23, 0, 0).single();
        let at = at.unwrap_or_default();
        assert_eq!(Period::Total.window_at(&at), None);
        assert_eq!(
            Period::Day.window_at(&at),
            Some(("2026-12-31T00:00:00Z".into(), "2027-01-01T00:00:00Z".into()))
        );
        assert_eq!(
            Period::Month.window_at(&at),
            Some(("2026-12-01T00:00:00Z".into(), "2027-01-01T00:00:00Z".into()))
        );
        assert_eq!(Period::named("week"), None);
    }

    #[test]
    fn periods_are_local_days_and_months() -> Result<(), &'static str> {
        // 23:00 on 31 December in New York (UTC−5) is 04:00 on 1 January in UTC: still December's day and month.
        let ny = chrono::FixedOffset::west_opt(5 * 3600).ok_or("offset")?;
        let at = ny
            .with_ymd_and_hms(2026, 12, 31, 23, 0, 0)
            .single()
            .ok_or("time")?;
        assert_eq!(
            Period::Day.window_at(&at),
            Some(("2026-12-31T05:00:00Z".into(), "2027-01-01T05:00:00Z".into()))
        );
        assert_eq!(
            Period::Month.window_at(&at),
            Some(("2026-12-01T05:00:00Z".into(), "2027-01-01T05:00:00Z".into()))
        );
        // East of UTC the local day starts the evening before, in UTC.
        let tokyo = chrono::FixedOffset::east_opt(9 * 3600).ok_or("offset")?;
        let d = NaiveDate::from_ymd_opt(2026, 11, 10).ok_or("date")?;
        assert_eq!(
            span("day", d, &tokyo),
            Some(("2026-11-09T15:00:00Z".into(), "2026-11-10T15:00:00Z".into()))
        );
        assert_eq!(span("week", d, &tokyo), None);
        Ok(())
    }

    #[test]
    fn the_spec_carries_the_reservation() -> Result<(), String> {
        let spec = b"id: x\nmodel: m\nbudget: {turns: 3, usd: 2}\ntrials: 1\n";
        assert_eq!(ceiling(spec, 2)?, Some(4_000_000));
        assert_eq!(ceiling(b"id: x\nbudget: {usd: 0}\n", 1)?, None);
        assert_eq!(ceiling(b"id: x\n", 1)?, None);
        assert!(ceiling(b"- a list\n", 1).is_err());
        let out = with_usd(spec, 1_250_000)?;
        let doc: serde_yaml::Value = serde_yaml::from_slice(&out).map_err(|e| e.to_string())?;
        assert_eq!(doc["budget"]["usd"].as_f64(), Some(1.25));
        assert_eq!(doc["budget"]["turns"].as_i64(), Some(3), "other keys kept");
        assert_eq!(doc["id"].as_str(), Some("x"));
        let added = with_usd(b"id: y\n", 500_000)?;
        assert_eq!(ceiling(&added, 1)?, Some(500_000));
        Ok(())
    }
}
