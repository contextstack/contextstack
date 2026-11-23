use crate::{cli_work, gateway::Gateway, process, runner, spec};
use std::{fs::File, path::Path, sync::OnceLock};
use tokio::signal::unix::{SignalKind, signal};

static EXECUTION_LOCK: OnceLock<File> = OnceLock::new();

#[derive(serde::Deserialize)]
struct MachineOrder {
    order: serde_json::Value,
    token: String,
}

const USAGE: &str = "usage: run [-task dir] [-trial N] <spec.yaml> | cli-run < work-order.json";

pub async fn execute_from(
    mut args: impl Iterator<Item = String>,
    repo: Option<&std::path::Path>,
) -> Result<u8, String> {
    match args.next().as_deref() {
        Some("run") => {}
        Some("cli-run") => return execute_cli(args, repo).await,
        _ => return Err(USAGE.into()),
    }
    let mut task = String::new();
    let mut trial = 0_i64;
    let spec_path = loop {
        let arg = args.next().ok_or(USAGE)?;
        if !arg.starts_with('-') {
            break arg;
        }
        if !matches!(arg.as_str(), "-task" | "-trial") {
            return Err(format!(
                "flag provided but not defined: {arg}\nUsage of run:\n  -task string\n    \ttask dir to run (required when the spec lists several)\n  -trial int\n    \ttrial number for the row and run dir (default $CS_TRIAL, else 1)"
            ));
        }
        let value = args
            .next()
            .ok_or_else(|| format!("flag needs an argument: {arg}"))?;
        if arg == "-task" {
            task = value;
        } else {
            trial = value
                .parse()
                .map_err(|_| format!("invalid value {value:?} for flag -trial: parse error"))?;
        }
    };
    if args.next().is_some() {
        return Err(USAGE.into());
    }
    if repo.is_none() && !Path::new("INVARIANTS.md").exists() {
        return Err("run from the repository root (INVARIANTS.md not found)".into());
    }
    let root = root(repo)?;
    let spec_path = if repo.is_some() && !Path::new(&spec_path).is_absolute() {
        root.join(&spec_path).to_string_lossy().into_owned()
    } else {
        spec_path
    };
    let sp = spec::load(&spec_path)?;
    if task.is_empty() {
        if sp.tasks.len() != 1 {
            return Err(format!(
                "{spec_path} lists {} tasks: pass -task <dir>, or run the whole spec with ops/bin/run-spec",
                sp.tasks.len()
            ));
        }
        task.clone_from(&sp.tasks[0]);
    }
    if trial <= 0 {
        trial = process::env("CS_TRIAL")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
    }
    let endpoint = process::env("CSGATEWAY_ENDPOINT").unwrap_or_default();
    let gateway = Gateway::new(endpoint, sp.model.clone())?;
    let control = control(repo.is_none())?;
    let result = runner::run(&sp, &task, trial, &root, &gateway, control).await;
    if let Some(error) = result.error {
        eprintln!("cs: {task} — {error}");
    }
    if let Some((dir, lock)) = result.execution {
        retain_execution_lock(repo, lock)?;
        println!("{}", serde_json::json!({"run_dir": dir}));
        return Ok(0);
    }
    Ok(if result.pending { 3 } else { 1 })
}

async fn execute_cli(
    mut args: impl Iterator<Item = String>,
    repo: Option<&Path>,
) -> Result<u8, String> {
    let machine = match (args.next().as_deref(), args.next()) {
        (None, None) => false,
        (Some("--machine"), None) => true,
        _ => return Err(USAGE.into()),
    };
    let control = control(repo.is_none())?;
    let input = std::io::stdin().lock();
    let (order, token) = if machine {
        let request: MachineOrder = serde_json::from_reader(input).map_err(|e| e.to_string())?;
        (request.order, Some(request.token))
    } else {
        (
            serde_json::from_reader(input).map_err(|e| e.to_string())?,
            None,
        )
    };
    let root = root(repo)?;
    let result = cli_work::run(order, &root, &control, token, |text| {
        if !machine {
            eprint!("{text}");
        }
    })
    .await?;
    let output = serde_json::to_string(&result).map_err(|e| e.to_string())?;
    retain_execution_lock(repo, result.lock)?;
    println!("{output}");
    Ok(0)
}

fn root(repo: Option<&Path>) -> Result<std::path::PathBuf, String> {
    repo.map_or_else(std::env::current_dir, |path| Ok(path.to_path_buf()))
        .map_err(|e| e.to_string())
}

fn retain_execution_lock(repo: Option<&Path>, lock: File) -> Result<(), String> {
    if repo.is_none() {
        EXECUTION_LOCK
            .set(lock)
            .map_err(|_| "execution lock already installed")?;
    }
    Ok(())
}

fn control(install_signals: bool) -> Result<process::Control, String> {
    let control = process::Control::default();
    if install_signals {
        let mut interrupts = signal(SignalKind::interrupt()).map_err(|e| e.to_string())?;
        let mut terminations = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
        let signals = control.clone();
        tokio::spawn(async move {
            signals.cancel_with(tokio::select! {
                _ = interrupts.recv() => "SIGINT",
                _ = terminations.recv() => "SIGTERM",
            });
        });
    }
    Ok(control)
}
