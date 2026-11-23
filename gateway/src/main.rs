//! `csgateway-rs`: the `ContextStack` model gateway in Rust. The black-box contract suite
//! (`tests/gateway-spec/`) and `gateway/WIRE.md` are the contract.
//!
//! `csgateway-rs` serves; `csgateway-rs credentials <command>` is the operator credential CLI.

mod admission;
mod agentstream;
mod catalog;
mod clirun;
mod connection;
mod credentials;
mod discovery;
mod errors;
mod executables;
mod gateway;
mod inventory;
mod log;
mod pricing;
mod protocol;
mod server;
mod settings;
mod spend;
mod trace;
mod wire;
mod workorder;

use gateway::Gateway;
use hyper_util::server::graceful::GracefulShutdown;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

/// How often the host overlay and tenant policy are checked.
const CATALOG_POLL: Duration = Duration::from_secs(5);
/// How long shutdown waits for in-flight requests.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// `csgateway-rs --help`: printed without starting anything (no discovery, no provider calls).
const USAGE: &str = "usage: csgateway-rs                     serve (configured by CSGATEWAY_* environment variables)
       csgateway-rs credentials <command>  the operator credential CLI (`csgateway-rs credentials` lists the commands)
       csgateway-rs --help | --version";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("credentials") => {}
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some("-V" | "--version") => {
            println!("csgateway-rs {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some(other) => {
            eprintln!("csgateway-rs: unknown argument {other:?}\n{USAGE}");
            return ExitCode::from(2);
        }
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    if args.first().map(String::as_str) == Some("credentials") {
        return ExitCode::from(runtime.block_on(credentials::command(&args[1..])));
    }
    match runtime.block_on(serve()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::line(&e);
            ExitCode::FAILURE
        }
    }
}

/// Start, serve until SIGTERM or SIGINT, shut down cleanly. An error is the fatal log line.
async fn serve() -> Result<(), String> {
    let gw = compose().await?;
    if gw.settings.auth_required && gw.settings.auth_token.is_empty() {
        return Err("CSGATEWAY_AUTH_TOKEN is required (set CSGATEWAY_AUTH_REQUIRED=false to disable auth for dev/test)".into());
    }
    if !gw.settings.auth_required && gw.settings.auth_token.is_empty() {
        log::line("WARNING: gateway auth is disabled (CSGATEWAY_AUTH_REQUIRED=false)");
    }
    match gw.spend.ensure_owner_token() {
        Ok(Some(path)) => logf!("owner token created at {}", path.display()),
        Ok(None) => {}
        Err(e) => logf!("WARNING: no owner token, budget writes are refused: {e}"),
    }
    let shutdown = CancellationToken::new();
    let graceful = Arc::new(GracefulShutdown::new());
    let gate = Arc::new(connection::Gate::new(&gw.settings));
    let mut accepting = Vec::new();
    let socket_path = gw.settings.socket.clone();
    if let Some(path) = &socket_path {
        let listener = listen_unix(path)
            .await
            .map_err(|e| format!("socket: {e}"))?;
        logf!("also serving on unix:{}", path.display());
        accepting.push(tokio::spawn(connection::accept_unix(
            listener,
            Arc::clone(&gw),
            Arc::clone(&graceful),
            shutdown.clone(),
            Arc::clone(&gate),
        )));
    }
    let addr = gw.settings.listen_addr();
    logf!("starting on {addr}");
    let tcp = TcpListener::bind(&addr).await.map_err(|e| {
        format!(
            "server error: listen tcp {addr}: bind: {}",
            server::os_error(&e)
        )
    })?;
    accepting.push(tokio::spawn(connection::accept_tcp(
        tcp,
        Arc::clone(&gw),
        Arc::clone(&graceful),
        shutdown.clone(),
        Arc::clone(&gate),
    )));
    tokio::spawn(refresh_loop(Arc::clone(&gw), shutdown.clone()));
    tokio::spawn(catalog_loop(Arc::clone(&gw), shutdown.clone()));
    signals(&gw).await?;

    shutdown.cancel();
    for task in accepting {
        let _ = task.await;
    }
    gw.health.stop();
    if let Ok(g) = Arc::try_unwrap(graceful) {
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, g.shutdown()).await;
    }
    if let Some(p) = socket_path {
        let _ = std::fs::remove_file(p);
    }
    Ok(())
}

/// The composition root: settings, host configuration, executables, discovery health, admission, the
/// CLI executor, then the gateway and its first snapshot. Refusals match the contract suite's order and wording.
async fn compose() -> Result<Arc<Gateway>, String> {
    let settings =
        settings::Settings::load(settings::Env::load()).map_err(|e| format!("config: {e}"))?;
    // Load the deployment config first so a broken models.json reports as `config:`.
    let mut host =
        admission::Config::load(&settings.models_path).map_err(|e| format!("config: {e}"))?;
    let catalog_opts = gateway::catalog_options(&settings);
    let cand = catalog::Candidate::load(&catalog_opts).map_err(|e| format!("catalog: {e}"))?;
    host.merge_overlay(&cand);
    let path = std::env::var("PATH").unwrap_or_default();
    let exe = executables::Resolver::new(
        &host.executables.clone().unwrap_or_default(),
        &path,
        settings.home.as_deref(),
    )
    .map_err(|e| format!("config: {e}"))?;
    let known = gateway::known_ids(&settings)?;
    let health = discovery::health::Health::new(
        settings.health_path.clone(),
        settings.stale_after,
        &settings.discovery_disabled,
        &known,
    )
    .map_err(|e| format!("config: {e}"))?;
    let manager = admission::Manager::new(&host).map_err(|e| format!("gateway: {e}"))?;
    let executor = if host.cli_executor.is_empty() {
        None
    } else {
        let python = exe
            .look_path("python3")
            .map_err(|e| format!("gateway: CLI executor interpreter: {e}"))?;
        let ex =
            workorder::Executor::new(&host.cli_executor, python, &settings, &exe.search_path())
                .map_err(|e| format!("gateway: {e}"))?;
        Some(ex)
    };
    let gw = Gateway::new(settings, manager, exe, executor, health)
        .await
        .map_err(|e| format!("gateway: {e}"))?;
    let snap = gw.current();
    let shown = |p: &Option<PathBuf>| {
        p.as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    };
    logf!(
        "catalog {}: overlay {} {:?}, policy {:?}",
        snap.revision,
        shown(&gw.settings.providers_dir),
        snap.candidate.files,
        shown(&gw.settings.policy_path)
    );
    Ok(Arc::new(gw))
}

/// Wait for SIGTERM or SIGINT; SIGHUP re-reads credentials and reloads the catalog, or refreshes the
/// inventory when unchanged.
async fn signals(gw: &Arc<Gateway>) -> Result<(), String> {
    let mut hup = signal(SignalKind::hangup()).map_err(|e| e.to_string())?;
    let mut term = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
    let mut int = signal(SignalKind::interrupt()).map_err(|e| e.to_string())?;
    loop {
        tokio::select! {
            _ = hup.recv() => {
                log::line("SIGHUP received, reloading catalog");
                gw.settings.store.forget(); // the one explicit credential reload
                let gw = Arc::clone(gw);
                tokio::spawn(async move {
                    if !matches!(gw.reload().await, Ok(true)) {
                        match gw.refresh().await {
                            Ok(_) => log::line("inventory refreshed via signal"),
                            Err(e) => logf!("triggered inventory refresh failed: {e}"),
                        }
                    }
                });
            }
            _ = term.recv() => { log::line("signal terminated received, shutting down"); return Ok(()); }
            _ = int.recv() => { log::line("signal interrupt received, shutting down"); return Ok(()); }
        }
    }
}

/// Periodic inventory refresh.
async fn refresh_loop(gw: Arc<Gateway>, stop: CancellationToken) {
    let every = gw.settings.refresh_interval;
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if let Err(e) = gw.refresh().await {
                    logf!("periodic inventory refresh failed: {e}");
                }
            }
            () = stop.cancelled() => return,
        }
    }
}

/// Poll the host layers.
async fn catalog_loop(gw: Arc<Gateway>, stop: CancellationToken) {
    let mut tick =
        tokio::time::interval_at(tokio::time::Instant::now() + CATALOG_POLL, CATALOG_POLL);
    loop {
        tokio::select! {
            _ = tick.tick() => { let _ = gw.reload().await; }
            () = stop.cancelled() => return,
        }
    }
}

/// Bind `path` with mode 0600, replacing a stale socket a dead gateway left. Refuses a live socket, a
/// path that is not a socket, and a directory another user could write. The process umask is never
/// touched: the socket is bound in this uid's private directory and narrowed before the first accept.
async fn listen_unix(path: &Path) -> Result<UnixListener, String> {
    if !path.is_absolute() {
        return Err(format!(
            "socket path {:?} must be absolute",
            path.display().to_string()
        ));
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("/"));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| server::os_error(&e))?;
    let meta = std::fs::metadata(dir).map_err(|e| server::os_error(&e))?;
    let shown = dir.display();
    if !meta.is_dir() {
        return Err(format!("socket directory {shown} is not a directory"));
    }
    if meta.permissions().mode() & 0o002 != 0 {
        return Err(format!(
            "socket directory {shown} is world-writable; use a private directory such as ~/.contextstack/gateway"
        ));
    }
    let uid = meta.uid();
    if uid != rustix::process::geteuid().as_raw() && uid != 0 {
        return Err(format!(
            "socket directory {shown} is owned by uid {uid}, not this user"
        ));
    }
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(server::os_error(&e)),
        Ok(m) if !m.file_type().is_socket() => {
            return Err(format!(
                "{} exists and is not a socket; refusing to remove it",
                path.display()
            ));
        }
        Ok(_) => {
            if tokio::time::timeout(Duration::from_secs(1), UnixStream::connect(path))
                .await
                .is_ok_and(|r| r.is_ok())
            {
                return Err(format!("{} is in use by a running gateway", path.display()));
            }
            std::fs::remove_file(path).map_err(|e| server::os_error(&e))?;
        }
    }
    let listener = UnixListener::bind(path).map_err(|e| server::os_error(&e))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| server::os_error(&e))?;
    Ok(listener)
}
