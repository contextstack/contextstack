//! Exercise the land-check renderer through its command-line interface.

use std::process::{Command, Output};

fn render(path: &str) -> std::io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_cs-sandbox"))
        .args(["--profile", "land-check", "--request", path])
        .output()
}

#[cfg(target_os = "macos")]
#[test]
fn renders_land_check() -> std::io::Result<()> {
    let output = render(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/land-check.json"
    ))?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        include_str!("fixtures/land-check.sb")
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
fn refuses_overlapping_write_grants() -> std::io::Result<()> {
    let fixture = include_str!("fixtures/land-check.json");
    for (from, to) in [
        (
            "/private/tmp/land-run/target",
            "/Users/check-owner/main/target",
        ),
        (
            "/private/tmp/land-run/cache",
            "/private/tmp/land-run/../outside",
        ),
        ("/private/tmp/land-run/target", "/Users/check-owner/.rustup"),
        ("/Users/check-owner/land", "/Users/check-owner"),
        ("/private/var/folders/zz/land/T", "/private/var/folders"),
    ] {
        use std::io::Write;
        let mut request = tempfile::NamedTempFile::new()?;
        request.write_all(fixture.replace(from, to).as_bytes())?;
        let output = render(&request.path().to_string_lossy())?;
        assert_eq!(output.status.code(), Some(101), "{from} -> {to}");
        assert_eq!(output.stdout, [] as [u8; 0]);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
fn refuses_the_main_targets_resolved_location() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let main = directory.path().join("main");
    let target = directory.path().join("shared-target");
    std::fs::create_dir(&main)?;
    std::fs::create_dir(&target)?;
    std::os::unix::fs::symlink(&target, main.join("target"))?;
    let mut request: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/land-check.json"))?;
    request["main_checkout"] = serde_json::to_value(main)?;
    request["cargo_target_dir"] = serde_json::to_value(target)?;
    let path = directory.path().join("request.json");
    std::fs::write(&path, serde_json::to_vec(&request)?)?;
    let output = render(&path.to_string_lossy())?;
    assert_eq!(output.status.code(), Some(101));
    assert!(String::from_utf8_lossy(&output.stderr).contains("overlaps a protected path"));
    Ok(())
}

#[cfg(not(target_os = "macos"))]
#[test]
fn reports_unsupported_before_loading_request() -> std::io::Result<()> {
    let output = render("missing-request.json")?;
    assert_eq!(output.status.code(), Some(101));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported"));
    Ok(())
}
