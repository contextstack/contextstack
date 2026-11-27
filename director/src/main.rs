//! `cs-director`: the standalone `ContextStack` Director (ADR 0012). It owns its own SQLite database and object store,
//! speaks node protocols 1 and 2 (`tests/director-spec/NODE-PROTOCOL.md`) and serves the Client API
//! (`tests/director-spec/CLIENT-API.md`) behind credentials that need no login (`access.rs`).
//!
//! ```text
//! cs-director serve [--data-dir D] [--owner-name NAME] [--listen ADDR]... [--tls-cert PEM --tls-key PEM] [--allow-origin O]...
//! cs-director node register NAME          prints {"node", "token", "installation"} once
//! cs-director node revoke NAME
//! cs-director job create --node NAME --bundle DIR [--task tasks/x] [--trials N] [--grant ID] [--meta k=v]...
//! cs-director ui-link [--listen ADDR]     prints a five-minute browser login URL
//! cs-director pair NAME                   prints a one-time pairing code for a device
//! cs-director credential list | credential revoke ID
//! cs-director installation                prints this Director's installation ID
//! ```
//!
//! Administrative commands work on the data directory directly, with the service up or down.

mod access;
mod action_approvals;
mod backlog;
mod backup_crypto;
mod budget;
mod capabilities;
mod chief_of_staff;
mod client_api;
mod context_windows;
mod control;
mod conversations;
mod decision_log;
mod enrollment_origin;
mod envelope;
mod events;
mod failure;
mod fleet;
mod gateway;
mod heartbeat;
mod jobs;
mod land;
mod lane;
mod lead;
mod lead_install;
mod lead_land;
mod lead_node;
mod live;
mod members;
mod models;
mod node_api;
mod peer_messages;
mod planning;
mod policy;
mod projections;
mod records;
mod repos;
mod reservations;
mod schedule_fields;
mod scopes;
mod server;
mod settings;
mod settings_write;
mod settle;
mod spec_edit;
mod spend;
mod sponsorship;
mod stamp;
mod store;
mod thread;
mod turns;
mod ui;
mod ui_auth;
mod usage;
mod util;
mod work;
mod work_trace;

use serde_json::{Value, json};
use settings::{Args, Settings};
use std::process::ExitCode;
use std::sync::Arc;
use store::Store;

const USAGE: &str = "usage: cs-director --schema-version | serve [--owner-name NAME] | node register NAME | node revoke NAME | \
job create --node NAME --bundle DIR | ui-link [--listen ADDR] | pair NAME | credential list | credential revoke ID | installation \
(all take --data-dir DIR)";

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match run(&argv) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cs-director: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Print one JSON line on stdout.
fn print(value: &Value) {
    println!("{value}");
}

fn run(raw: &[String]) -> Result<(), String> {
    if raw == ["--schema-version"] {
        println!("{}", store::SCHEMA_VERSION);
        return Ok(());
    }
    if let [command, mode] = raw
        && command == "backup-crypto"
    {
        return backup_crypto::run(mode).map_err(|error| error.to_string());
    }
    let args = Args::parse(raw)?;
    let words: Vec<&str> = args.words.iter().map(String::as_str).collect();
    if args.has("help") || words.is_empty() {
        println!("{USAGE}");
        return Ok(());
    }
    let known: &[&str] = match words.as_slice() {
        ["serve"] => &[
            "data-dir",
            "owner-name",
            "listen",
            "tls-cert",
            "tls-key",
            "allow-origin",
            "stale-after",
            "offer-grace",
            "browser-action-ttl",
            "context-window",
            "compact-at",
            "gateway",
            "gateway-owner-token",
            "reservation-stale-after",
            "reservation-timeout",
        ],
        ["ui-link"] => &["data-dir", "listen"],
        ["job", "create"] => &[
            "data-dir", "node", "bundle", "task", "trials", "grant", "meta",
        ],
        _ => &["data-dir"],
    };
    if let Some(flag) = args.unknown(known) {
        return Err(format!("unknown flag {flag}\n{USAGE}"));
    }
    if words == ["serve"] {
        return serve(&args);
    }
    let store = Store::open(&args.data_dir()?)?;
    let _mutation = store.lock_client_mutation().map_err(|e| e.to_string())?;
    access::ensure_local_credential(&store)?;
    let capability = policy::local_route(&words).ok_or(USAGE)?;
    let note_usage = matches!(words.as_slice(), ["pair", _] | ["credential", "revoke", _]);
    let actor = local_actor(&store, capability, note_usage)?;
    match words.as_slice() {
        ["node", "register", name] => {
            let token = access::register_node(&store, name)?;
            print(&json!({"node": name, "token": token, "installation": store.installation}));
        }
        ["node", "revoke", name] => {
            access::revoke_node(&store, name)?;
            print(&json!({"node": name, "revoked": true}));
        }
        ["job", "create"] => {
            print(&jobs::create(&store, &new_job(&args)?)?);
        }
        ["pair", name] => {
            let (code, expires_at) = access::issue_pairing(&store, name, &actor, None)?;
            print(&json!({"name": name, "code": code, "expires_at": expires_at}));
        }
        ["credential", "list"] => {
            print(&access::credentials(&store)?);
        }
        ["credential", "revoke", id] => {
            access::revoke_credential(&store, id, &actor)?;
            print(&json!({"id": id, "revoked": true}));
        }
        ["ui-link"] => {
            println!("{}", ui::link(&store, &args)?);
        }
        ["installation"] => println!("{}", store.installation),
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

fn local_actor(
    store: &Store,
    capability: policy::Capability,
    note_usage: bool,
) -> Result<String, String> {
    let token =
        std::fs::read_to_string(access::local_credential_path(store)).map_err(|e| e.to_string())?;
    access::authenticate(store, token.trim(), note_usage)?
        .filter(|client| {
            client.kind == access::Kind::Local
                && policy::allows(
                    client,
                    capability,
                    policy::Target::Org(&client.member.scope.org_id),
                )
        })
        .map(|client| client.id)
        .ok_or_else(|| "local credential is unavailable".to_string())
}

fn new_job(args: &Args) -> Result<jobs::NewJob, String> {
    let need = |flag: &str| {
        args.one(flag, "")
            .ok_or_else(|| format!("job create needs --{flag}"))
    };
    let trials = match args.one("trials", "") {
        None => 1,
        Some(t) => t
            .parse()
            .ok()
            .filter(|n| *n > 0)
            .ok_or("--trials wants a positive integer")?,
    };
    let mut meta = serde_json::Map::new();
    for pair in args.many("meta", "") {
        let (k, v) = pair.split_once('=').ok_or("--meta wants key=value")?;
        meta.insert(k.to_string(), Value::String(v.to_string()));
    }
    Ok(jobs::NewJob {
        node: need("node")?,
        bundle: need("bundle")?.into(),
        task: args.one("task", ""),
        trials,
        grant: args.one("grant", ""),
        meta,
    })
}

fn serve(args: &Args) -> Result<(), String> {
    let settings = Settings::from_args(args)?;
    let name = args.one("owner-name", "DIRECTOR_OWNER_NAME");
    let name = name.as_deref().map(str::trim);
    let store = if let Some(name) = name {
        if !members::valid_display_name(name) {
            return Err("--owner-name needs a display name of at most 100 UTF-8 bytes without control characters".into());
        }
        Store::open_with_owner(&settings.data_dir, Some(name))?
    } else {
        Store::open(&settings.data_dir)?
    };
    access::ensure_local_credential(&store)?;
    logf!("data directory {}", settings.data_dir.display());
    // Applied cancels that left a job open with no run (before the Director finished those): end them now.
    let orphans = reservations::sweep_orphan_applied_cancels(&store)?;
    if orphans > 0 {
        logf!("orphan applied cancels at start: {orphans} job(s) finished");
    }
    // Reservations are rows: settle what finished while the service was down, then offer what now fits.
    let (settled, offered) = reservations::reconcile(&store)?;
    if settled + offered > 0 {
        logf!("budgets at start: {settled} job(s) settled, {offered} waiting job(s) offered");
    }
    let expired_approvals =
        action_approvals::expire_ended_jobs(&store.db()).map_err(|e| e.to_string())?;
    if expired_approvals > 0 {
        logf!("action approvals at start: {expired_approvals} approval(s) expired (job ended)");
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    let hub = events::Hub::new(&store)?;
    if expired_approvals > 0 {
        hub.publish(&store);
    }
    records::set_stale_after(settings.stale_after_seconds);
    records::set_offer_grace(settings.offer_grace_seconds);
    thread::configure(settings.compaction.clone());
    runtime.block_on(server::serve(Arc::new(server::State {
        store,
        settings,
        hub,
    })))
}

#[cfg(test)]
mod review_regressions;
