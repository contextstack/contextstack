//! `cs-director`: the standalone `ContextStack` Director (ADR 0012). It owns its own SQLite database and object store,
//! speaks node protocols 1 and 2 (`tests/director-spec/NODE-PROTOCOL.md`) and serves the Client API
//! (`tests/director-spec/CLIENT-API.md`) behind credentials that need no login (`access.rs`).
//!
//! ```text
//! cs-director serve [--data-dir D] [--listen ADDR]... [--tls-cert PEM --tls-key PEM] [--allow-origin O]...
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
mod budget;
mod chief_of_staff;
mod client_api;
mod context_windows;
mod control;
mod conversations;
mod envelope;
mod events;
mod failure;
mod fleet;
mod gateway;
mod jobs;
mod land;
mod lane;
mod lead;
mod lead_node;
mod live;
mod models;
mod node_api;
mod records;
mod repos;
mod reservations;
mod scopes;
mod server;
mod settings;
mod settle;
mod spec_edit;
mod spend;
mod stamp;
mod store;
mod thread;
mod turns;
mod ui;
mod ui_auth;
mod usage;
mod util;
mod work;

use serde_json::{Value, json};
use settings::{Args, Settings};
use std::process::ExitCode;
use std::sync::Arc;
use store::Store;

const USAGE: &str = "usage: cs-director serve | node register NAME | node revoke NAME | \
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
    let args = Args::parse(raw)?;
    let words: Vec<&str> = args.words.iter().map(String::as_str).collect();
    if args.has("help") || words.is_empty() {
        println!("{USAGE}");
        return Ok(());
    }
    let known: &[&str] = match words.as_slice() {
        ["serve"] => &[
            "data-dir",
            "listen",
            "tls-cert",
            "tls-key",
            "allow-origin",
            "stale-after",
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
    access::ensure_local_credential(&store)?;
    match words.as_slice() {
        ["node", "register", name] => {
            let token = access::register_node(&store, name)?;
            print(&json!({"node": name, "token": token, "installation": store.installation}));
        }
        ["node", "revoke", name] => {
            access::revoke_node(&store, name)?;
            print(&json!({"node": name, "revoked": true}));
        }
        ["job", "create"] => print(&jobs::create(&store, &new_job(&args)?)?),
        ["pair", name] => {
            let (code, expires_at) = access::issue_pairing(&store, name)?;
            print(&json!({"name": name, "code": code, "expires_at": expires_at}));
        }
        ["credential", "list"] => print(&json!({"credentials": access::credentials(&store)?})),
        ["credential", "revoke", id] => {
            access::revoke_credential(&store, id)?;
            print(&json!({"id": id, "revoked": true}));
        }
        ["ui-link"] => println!("{}", ui::link(&store, &args)?),
        ["installation"] => println!("{}", store.installation),
        _ => return Err(USAGE.into()),
    }
    Ok(())
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
    let store = Store::open(&settings.data_dir)?;
    access::ensure_local_credential(&store)?;
    logf!("data directory {}", settings.data_dir.display());
    // Reservations are rows: settle what finished while the service was down, then offer what now fits.
    let (settled, offered) = reservations::reconcile(&store)?;
    if settled + offered > 0 {
        logf!("budgets at start: {settled} job(s) settled, {offered} waiting job(s) offered");
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    let hub = events::Hub::new(&store)?;
    records::set_stale_after(settings.stale_after_seconds);
    thread::configure(settings.compaction.clone());
    runtime.block_on(server::serve(Arc::new(server::State {
        store,
        settings,
        hub,
    })))
}
