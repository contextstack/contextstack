//! Production v1 request builders must match the frozen profiles.

use std::error::Error;
use std::path::Path;
use std::process::Command;

#[test]
fn cli_executor_matches_frozen_profiles() -> Result<(), Box<dyn Error>> {
    if !cfg!(target_os = "macos") {
        // The public cli-executor emitter explicitly refuses non-macOS platforms.
        return Ok(());
    }
    let sandbox = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = sandbox.parent().ok_or("sandbox crate has no parent")?;
    // Prefer the repository environment, then search the caller's PATH. In
    // particular, do not force macOS's older /usr/bin/python3.
    let local = root.join("cs/.venv/bin/python3");
    let python = if local.is_file() {
        local
    } else {
        let found = std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join("python3"))
                .find(|candidate| candidate.is_file())
        });
        let Some(path) = found else {
            eprintln!("SKIP e2_baseline: no cs/.venv/bin/python3 or python3 on inherited PATH");
            return Ok(());
        };
        path
    };
    let output = Command::new(&python)
        .arg(root.join("cs/tasks/regressions/e2-sbpl-baseline/reference.py"))
        .arg("--rust-report")
        .arg(env!("CARGO_BIN_EXE_cs-sandbox"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()?;
    assert!(
        output.status.success(),
        "Rust baseline rendering failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
