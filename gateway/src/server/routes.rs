//! The read-only routes, inventory refresh and CLI work orders.

use super::{Body, Peer, error, json};
use crate::gateway::{Gateway, Snapshot};
use crate::inventory::ModelEntry;
use crate::log::rfc3339_local;
use http_body_util::{BodyExt, Limited};
use hyper::{Request, Response, body::Incoming};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

/// GET /v1/health.
pub async fn health(gw: &Arc<Gateway>) -> Response<Body> {
    let snap = gw.current();
    let inv = snap.inventory();
    let ollama = snap.candidate.entry("ollama").map_or_else(
        || gw.settings.ollama_endpoint.clone(),
        |e| crate::catalog::root(&e.endpoint),
    );
    let reachable = crate::discovery::ollama_reachable(&gw.http, &ollama).await;
    let wire = gw.wire.lock().map(|w| w.snapshot()).unwrap_or_default();
    json(
        200,
        &json!({
            "status": "healthy",
            "implementation": "rust",
            "version": "v0.1.0",
            "uptime_seconds": gw.started.elapsed().as_secs(),
            "models_available": inv.available().count(),
            "providers_active": inv.providers(),
            "ollama_reachable": reachable,
            "last_inventory_refresh": rfc3339_local(gw.last_refresh()),
            "role_aware_dispatch": gw.settings.role_aware_dispatch,
            "wire_cutover": wire,
            "discovery_stale": gw.health.stale_ids(chrono::Utc::now()),
            "catalog_revision": snap.revision,
        }),
    )
}

/// GET /v1/budgets: each pay-as-you-go source's budget state (off, active, exhausted). The sources are
/// the catalog's keyed endpoints, the providers of api-class models, and any configured budget.
pub async fn budgets(gw: &Arc<Gateway>) -> Response<Body> {
    let snap = gw.current();
    let inv = snap.inventory();
    let keyed = crate::catalog::embedded()
        .into_iter()
        .filter(|e| e.requires_key)
        .map(|e| e.name);
    let api: Vec<_> = inv
        .all()
        .iter()
        .filter(|e| !e.is_cli() && e.cost_class == crate::inventory::API)
        .map(|e| e.provider.clone())
        .collect();
    let guard = Arc::clone(&gw.spend);
    match tokio::task::spawn_blocking(move || guard.report(keyed.chain(api))).await {
        Ok(report) => json(200, &report),
        Err(e) => error(500, &e.to_string(), "budgets_read_failed"),
    }
}

/// The most a budgets document may be.
const MAX_BUDGETS_BODY: usize = 64 * 1024;

/// The owner credential on a budget write.
pub const OWNER_HEADER: &str = "x-contextstack-owner-token";

/// PUT /v1/budgets: replace the limit file with the body (a whole limits.json document) and answer
/// the new state report. Owner only, always: the owner token (`config/gateway/owner-token`, created
/// at first start) in `X-ContextStack-Owner-Token`, from a local peer (loopback TCP or the unix
/// socket). The shared bearer token and the tokenless socket are never enough. Invalid writes nothing.
pub async fn put_budgets(gw: &Arc<Gateway>, peer: Peer, req: Request<Incoming>) -> Response<Body> {
    let local = match peer {
        Peer::Unix => true,
        Peer::Tcp(ip) => ip.is_loopback(),
    };
    let given = req
        .headers()
        .get(OWNER_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !local || !gw.spend.is_owner(given) {
        let refusal = error(
            403,
            "setting budgets needs the owner token (X-ContextStack-Owner-Token, from config/gateway/owner-token) from a local peer",
            "owner_only",
        );
        return super::refuse(req, refusal).await;
    }
    let body = match Limited::new(req.into_body(), MAX_BUDGETS_BODY)
        .collect()
        .await
    {
        Ok(b) => b.to_bytes(),
        Err(_) => {
            return error(
                413,
                "budgets document over 64 KiB or unreadable",
                "invalid_budgets",
            );
        }
    };
    let guard = Arc::clone(&gw.spend);
    let result = tokio::task::spawn_blocking(move || guard.replace(&body)).await;
    match result.unwrap_or_else(|e| Err(crate::spend::SetError::Failed(e.to_string()))) {
        Ok(()) => {
            crate::log::row(&json!({"event": "budgets_set"}));
            budgets(gw).await
        }
        Err(crate::spend::SetError::Invalid(e)) => {
            error(400, &format!("invalid budgets: {e}"), "invalid_budgets")
        }
        Err(crate::spend::SetError::Failed(e)) => error(
            500,
            &format!("budgets not written: {e}"),
            "budgets_write_failed",
        ),
    }
}

fn binary(e: &ModelEntry) -> &str {
    if e.cli_binary.is_empty() {
        &e.name
    } else {
        &e.cli_binary
    }
}

/// Whether `model.call` with `stream: true` can serve this model: its endpoint's protocol ∩ declaration
/// ∩ policy. A CLI entry never streams (model.call refuses it). A caller picks unary from this, at $0.
fn streams(snap: &Snapshot, e: &ModelEntry) -> bool {
    !e.is_cli()
        && snap
            .candidate
            .entry(&e.provider)
            .is_some_and(|c| snap.candidate.effective(c, chrono::Utc::now()).streaming)
}

/// GET /v1/capabilities.
pub fn capabilities(gw: &Arc<Gateway>) -> Response<Body> {
    let snap = gw.current();
    let inv = snap.inventory();
    let api: Vec<Value> = inv
        .available()
        .filter(|m| !m.is_cli())
        .map(|m| json!({"name": m.name, "provider": m.provider, "tier": m.tier, "capability": m.capability, "available": true, "streaming": streams(&snap, m)}))
        .collect();
    let cli: Vec<Value> = inv
        .cli(0)
        .map(|e| json!({"name": e.name, "kind": "cli", "binary": binary(e), "tier": e.tier, "capability": e.capability, "available": true}))
        .collect();
    json(
        200,
        &json!({
            "gateway_status": "online",
            "last_refresh_at": rfc3339_local(gw.last_refresh()),
            "providers_active": inv.providers(),
            "supports_local_only": inv.available().any(|m| m.provider == "ollama"),
            "api_models": api,
            "cli_executors": cli,
            "models_available": inv.available().count(),
            "degraded_reasons": [],
        }),
    )
}

fn tier_filter(q: &BTreeMap<String, String>) -> i64 {
    q.get("tier").and_then(|t| t.parse().ok()).unwrap_or(0)
}

/// GET /v1/models.
pub fn models(gw: &Arc<Gateway>, q: &BTreeMap<String, String>) -> Response<Body> {
    let snap = gw.current();
    let inv = snap.inventory();
    let tier = tier_filter(q);
    let capability = q.get("capability").cloned().unwrap_or_default();
    let available_only = q.get("available_only").map(String::as_str) != Some("false");
    let rows: Vec<Value> = inv
        .all()
        .iter()
        .filter(|e| {
            (tier <= 0 || e.tier == tier)
                && (capability.is_empty() || e.capability == capability)
                && (!available_only || e.available)
        })
        .map(|e| {
            let mut r = json!({
                "name": e.name, "provider": e.provider, "tier": e.tier, "capability": e.capability,
                "available": e.available, "preferred": e.preferred, "streaming": streams(&snap, e),
            });
            if e.candidate {
                r["candidate"] = json!(true);
            }
            if !e.candidate_profiles.is_empty() {
                r["candidate_profiles"] = json!(e.candidate_profiles);
            }
            if !e.executor.is_empty() {
                r["executor"] = json!(e.executor);
            }
            if e.is_cli() && !e.cli_binary.is_empty() {
                r["cli_binary"] = json!(e.cli_binary);
            }
            if !e.cost_class.is_empty() {
                r["cost_class"] = json!(e.cost_class);
            }
            r
        })
        .collect();
    let (estate, capacity) = gw.admission.capacity();
    json(
        200,
        &json!({
            "models": rows,
            "last_refresh": rfc3339_local(gw.last_refresh()),
            "providers_active": inv.providers(),
            "estate": estate,
            "deployment_capacity": capacity,
        }),
    )
}

/// GET /v1/models/cli.
pub fn models_cli(gw: &Arc<Gateway>, q: &BTreeMap<String, String>) -> Response<Body> {
    let inv = gw.current().inventory();
    let rows: Vec<Value> = inv
        .cli(tier_filter(q))
        .map(|e| json!({"name": e.name, "binary": binary(e), "tier": e.tier, "capability": e.capability, "available": e.available}))
        .collect();
    json(200, &json!({"executors": rows}))
}

/// POST /v1/inventory/refresh.
pub async fn refresh(gw: &Arc<Gateway>) -> Response<Body> {
    match gw.refresh().await {
        Ok(r) => {
            let changes: Vec<Value> = r
                .changes
                .iter()
                .map(|(a, m, p)| json!({"action": a, "model": m, "provider": p}))
                .collect();
            json(
                200,
                &json!({
                    "models_before": r.before, "models_after": r.after, "changes": changes,
                    "refreshed_at": rfc3339_local(gw.last_refresh()),
                }),
            )
        }
        Err(e) => error(500, &format!("inventory refresh failed: {e}"), ""),
    }
}
