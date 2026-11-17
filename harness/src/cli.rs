use crate::{gateway::Gateway, process, runner, spec};
use std::path::Path;
use tokio::signal::unix::{SignalKind, signal};

const USAGE: &str = "usage: run [-task dir] [-trial N] <spec.yaml>";

pub async fn execute_from(
    mut args: impl Iterator<Item = String>,
    repo: Option<&std::path::Path>,
) -> Result<u8, String> {
    if args.next().as_deref() != Some("run") {
        return Err(USAGE.into());
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
                "flag provided but not defined: {arg}\nUsage of run:\n  -task string\n    \ttask dir to run (required when the spec lists several)\n  -trial int\n    \ttrial number for the row and run dir (default $LAB_TRIAL, else 1)"
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
    let root = repo.map_or_else(std::env::current_dir, |path| Ok(path.to_path_buf()))
        .map_err(|e| e.to_string())?;
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
        trial = process::env("LAB_TRIAL")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
    }
    let gateway = Gateway::new(
        process::env("CSGATEWAY_ENDPOINT").unwrap_or_default(),
        sp.model.clone(),
        sp.id.clone(),
    )?;
    let control = process::Control::default();
    if repo.is_none() {
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
    let result = runner::run(&sp, &task, trial, &root, &gateway, control).await;
    if let Some(error) = result.error {
        eprintln!("lab: {task} — {error}");
    }
    println!(
        "  {task:<30} t{trial:<2} {:<4} stop={} turns={} tools={} ${:.4}",
        result.verdict, result.stop, result.turns, result.tool_calls, result.cost
    );
    Ok(if result.pending {
        3
    } else {
        u8::from(result.verdict != "pass")
    })
}
