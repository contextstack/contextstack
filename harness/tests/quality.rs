//! Run the Rust binary against the real Lab finalizer in temporary estates.

#[test]
fn runtime_quality_regressions() -> std::io::Result<()> {
    let status = std::process::Command::new("python3")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/quality.py"))
        .arg(env!("CARGO_BIN_EXE_cs-harness"))
        .status()?;
    assert!(status.success(), "runtime quality fixtures exited {status}");
    Ok(())
}
