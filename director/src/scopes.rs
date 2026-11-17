//! Budgets as set on a project or conversation, and as shown: the room left under one (`usd − settled charges this
//! period − held reservations`, read from `job_budgets`), setting and removing one, and the `budget` fields of
//! project, conversation and job records. The lifecycle of a job's reservation is in `reservations.rs`.

use crate::budget::{self, Micros, Period, Room, Scope};
use crate::store::{Record, touch};
use crate::turns::{Res, db_err};
use crate::util::now_utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

/// The `job_budgets` column naming a scope kind.
pub(crate) fn column(kind: &str) -> &'static str {
    if kind == "project" {
        "project_id"
    } else {
        "conversation_id"
    }
}

/// A budget as set: `(usd, period, set_by, set_at)`.
type Set = (Micros, Period, String, String);

pub(crate) fn budget_of(db: &Connection, kind: &str, id: &str) -> rusqlite::Result<Option<Set>> {
    db.query_row(
        "SELECT usd_micros, period, set_by, set_at FROM budgets WHERE scope = ?1 AND scope_id = ?2",
        params![kind, id],
        |r| {
            let period: String = r.get(1)?;
            Ok((
                r.get(0)?,
                Period::named(&period).unwrap_or(Period::Total),
                r.get(2)?,
                r.get(3)?,
            ))
        },
    )
    .optional()
}

/// `(spent this period, held now)` under a scope.
pub(crate) fn tallies(
    db: &Connection,
    kind: &str,
    id: &str,
    period: Period,
) -> rusqlite::Result<(Micros, Micros)> {
    let col = column(kind);
    let since = period.window().map(|w| w.0);
    db.query_row(
        &format!(
            "SELECT COALESCE(SUM(CASE WHEN state = 'settled' AND (?2 IS NULL OR settled_at >= ?2)
                                      THEN charged_micros END), 0),
                    COALESCE(SUM(CASE WHEN state = 'held' THEN reserved_micros END), 0)
             FROM job_budgets WHERE {col} = ?1"
        ),
        params![id, since],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
}

pub(crate) fn room(db: &Connection, scope: &Scope) -> rusqlite::Result<Option<Room>> {
    let Some((usd, period, ..)) = budget_of(db, scope.kind, &scope.id)? else {
        return Ok(None);
    };
    let (spent, reserved) = tallies(db, scope.kind, &scope.id, period)?;
    Ok(Some(Room {
        scope: scope.clone(),
        budget: usd,
        spent,
        reserved,
    }))
}

/// Set a scope's budget, replacing any.
pub fn set(
    db: &Connection,
    kind: &'static str,
    id: &str,
    usd: Micros,
    period: Period,
    who: &str,
) -> Res<()> {
    db.execute(
        "INSERT INTO budgets (scope, scope_id, usd_micros, period, set_by, set_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(scope, scope_id) DO UPDATE SET usd_micros = excluded.usd_micros, period = excluded.period,
           set_by = excluded.set_by, set_at = excluded.set_at",
        params![kind, id, usd, period.name(), who, now_utc()],
    )
    .map_err(db_err)?;
    touch_kind(db, kind, id)
}

/// Remove a scope's budget: only its source budgets apply from now.
pub fn clear(db: &Connection, kind: &'static str, id: &str) -> Res<()> {
    db.execute(
        "DELETE FROM budgets WHERE scope = ?1 AND scope_id = ?2",
        params![kind, id],
    )
    .map_err(db_err)?;
    touch_kind(db, kind, id)
}

fn touch_kind(db: &Connection, kind: &str, id: &str) -> Res<()> {
    let record = if kind == "project" {
        Record::Project
    } else {
        Record::Conversation
    };
    touch(db, record, id).map_err(db_err).map(|_| ())
}

/// A job's `budget` field: its reservation, or null for a job made before budgets.
pub fn job_view(db: &Connection, job_id: &str) -> rusqlite::Result<Value> {
    let money = |m: Option<Micros>| m.map_or(Value::Null, budget::usd);
    db.query_row(
        "SELECT state, project_id, conversation_id, ceiling_micros, trials, reserved_micros, charged_micros,
                charge_basis, reason, detail, reserved_at, settled_at, settled_by, corrected_at
         FROM job_budgets WHERE job_id = ?1",
        params![job_id],
        |r| {
            let state: String = r.get(0)?;
            let reserved: Micros = r.get(5)?;
            let charged: Option<Micros> = r.get(6)?;
            let trials: u32 = r.get(4)?;
            let held = reserved > 0;
            Ok(json!({"state": state, "project_id": r.get::<_, Option<String>>(1)?,
                      "conversation_id": r.get::<_, Option<String>>(2)?,
                      "ceiling_usd": money(r.get(3)?),
                      "reserved_usd": budget::usd(reserved),
                      "per_run_usd": if held { budget::usd(budget::per_run(reserved, trials).max(1)) } else { Value::Null },
                      "charged_usd": money(charged),
                      "released_usd": charged.map_or(Value::Null, |c| budget::usd(budget::released(reserved, c))),
                      "charge_basis": r.get::<_, Option<String>>(7)?, "reason": r.get::<_, Option<String>>(8)?,
                      "detail": r.get::<_, Option<String>>(9)?, "reserved_at": r.get::<_, Option<String>>(10)?,
                      "settled_at": r.get::<_, Option<String>>(11)?,
                      "settled_by": r.get::<_, Option<String>>(12)?,
                      "corrected_at": r.get::<_, Option<String>>(13)?}))
        },
    )
    .optional()
    .map(Option::unwrap_or_default)
}

/// A project's or conversation's `budget` field. "No budget set" (`none`) is not "budget used up" (`exhausted`).
pub fn scope_view(db: &Connection, kind: &'static str, id: &str) -> rusqlite::Result<Value> {
    let col = column(kind);
    let mut stmt = db.prepare_cached(&format!(
        "SELECT job_id FROM job_budgets WHERE {col} = ?1 AND state = 'waiting' ORDER BY rowid"
    ))?;
    let waiting: Vec<String> = stmt
        .query_map(params![id], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let Some((usd, period, set_by, set_at)) = budget_of(db, kind, id)? else {
        let (spent, reserved) = tallies(db, kind, id, Period::Total)?;
        return Ok(
            json!({"state": "none", "shown": "no budget set", "usd": null, "period": null,
                         "spent_usd": budget::usd(spent), "reserved_usd": budget::usd(reserved),
                         "remaining_usd": null, "resets_at": null, "waiting_job_ids": waiting,
                         "set_by": null, "set_at": null}),
        );
    };
    let scope = Scope {
        kind,
        id: id.into(),
    };
    let r = room(db, &scope)?.unwrap_or(Room {
        scope,
        budget: usd,
        spent: 0,
        reserved: 0,
    });
    let money = budget::dollars;
    let (state, shown) = if r.exhausted() {
        ("exhausted", "budget used up".to_string())
    } else if r.remaining() == 0 {
        (
            "reserved",
            "all that remains is reserved for running work".to_string(),
        )
    } else {
        let per = match period {
            Period::Total => String::new(),
            p => format!(" this {}", p.name()),
        };
        (
            "active",
            format!("{} of {} left{per}", money(r.remaining()), money(usd)),
        )
    };
    let resets = period.window().map(|w| w.1);
    Ok(
        json!({"state": state, "shown": shown, "usd": budget::usd(usd), "period": period.name(),
              "spent_usd": budget::usd(r.spent), "reserved_usd": budget::usd(r.reserved),
              "remaining_usd": budget::usd(r.remaining()), "resets_at": resets, "waiting_job_ids": waiting,
              "set_by": set_by, "set_at": set_at}),
    )
}
