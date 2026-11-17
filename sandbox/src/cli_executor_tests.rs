//! Native v1 edge cases beyond the frozen template cross product.
use super::*;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};

fn request(home: &Path) -> Result<ExecutorRequest, String> {
    serde_json::from_value(serde_json::json!({
        "version": 1, "variant": "native", "home": home,
        "workspace": home.join("workspace"), "tmpdir": home.join("private"),
        "lookup_path": [home.join("bin")]
    }))
    .map_err(|e| e.to_string())
}

#[test]
fn native_interpreters_follow_child_path_and_do_not_grant_payloads() -> Result<(), String> {
    let temp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let home = crate::sbpl::resolve(temp.path()).map_err(|e| e.message().to_owned())?;
    let bin = home.join("bin");
    let version = home.join("runtime/versions/22/bin");
    fs::create_dir_all(&bin).map_err(|e| e.to_string())?;
    fs::create_dir_all(&version).map_err(|e| e.to_string())?;
    let node = version.join("node");
    fs::write(&node, b"node").map_err(|e| e.to_string())?;
    fs::set_permissions(&node, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    symlink(&node, bin.join("node")).map_err(|e| e.to_string())?;
    let launcher = bin.join("launcher");
    fs::write(&launcher, b"#!/usr/bin/env -S 'node' --flag\n").map_err(|e| e.to_string())?;
    let normalized = bin.join("missing/../launcher");
    assert_eq!(
        executable_paths(&normalized, Some(&[]))
            .map_err(|e| e.message().to_owned())?
            .0,
        launcher
    );
    let mut req = request(&home)?;
    req.executable = Some(launcher.clone());
    let rules = executable_home_rules(&home, Some(&launcher), false, &req)
        .map_err(|e| e.message().to_owned())?;
    let declared = executable_home_rules(&home, Some(Path::new("node")), true, &req)
        .map_err(|e| e.message().to_owned())?;
    assert!(rules.ends_with(&declared));
    assert!(
        declared
            .iter()
            .any(|r| r.contains(&format!("(literal \"{}\")", node.display())))
    );
    assert!(declared.iter().all(|r| !r.contains("(subpath")));
    // Python inspects an outside-home launcher too.
    let outside = tempfile::tempdir().map_err(|e| e.to_string())?;
    let external = outside.path().join("launcher");
    fs::write(&external, b"#!/usr/bin/env node\n").map_err(|e| e.to_string())?;
    assert_eq!(
        executable_home_rules(&home, Some(&external), false, &req)
            .map_err(|e| e.message().to_owned())?,
        declared
    );
    // Empty lookup_path must never fall back to the parent process's PATH.
    req.lookup_path.clear();
    assert_eq!(
        executable_home_rules(&home, Some(&external), false, &req)
            .map_err(|e| e.message().to_owned())?,
        Vec::<String>::new()
    );
    Ok(())
}

#[test]
fn native_missing_login_targets_keep_guards_and_directory_targets_do_not() -> Result<(), String> {
    let mut req = request(Path::new("/owner"))?;
    req.login_targets = vec![
        LoginTarget {
            path: "/owner/missing/auth".into(),
            allow_directory: false,
        },
        LoginTarget {
            path: "/owner/oauth".into(),
            allow_directory: true,
        },
    ];
    req.read_links = vec!["/owner/toolchain/config".into()];
    let rules = login_swap_rules(&req).map_err(|e| e.message().to_owned())?;
    assert_eq!(
        &rules[..2],
        [
            "(deny file-link file-write-create file-write-unlink (literal \"/owner/missing/auth\"))",
            "(deny file-link file-write-create file-write-unlink (literal \"/owner/missing\"))"
        ]
    );
    assert!(
        rules
            .iter()
            .all(|r| !r.contains("oauth") && !r.contains("toolchain"))
    );
    Ok(())
}

#[test]
fn native_login_validation_and_path_quoting_match_python() -> Result<(), String> {
    let entry = |raw: &str| LoginWriteEntry {
        literal: Some(raw.to_owned()),
        regex: None,
        subpath: None,
    };
    assert!(login_write_rules(&[entry("~/name..ok/auth")], Path::new("/owner"), true).is_ok());
    for raw in ["~/auth", "~/../auth", "~/a/../auth", "~/a/"] {
        assert!(
            login_write_rules(&[entry(raw)], Path::new("/owner"), true).is_err(),
            "{raw}"
        );
    }
    for control in ['\0', '\n', '\r', '\t', '\u{7f}'] {
        let bad = PathBuf::from(format!("/owner/bad{control}path"));
        assert!(protected_rules(&bad, true).is_err());
        assert!(estate_read_rules(&bad, &[], true).is_err());
        let mut req = request(Path::new("/owner"))?;
        req.login_targets.push(LoginTarget {
            path: bad,
            allow_directory: false,
        });
        assert!(login_swap_rules(&req).is_err());
    }
    let legacy = protected_rules(Path::new("/owner/protected"), false)
        .map_err(|e| e.message().to_owned())?;
    let native =
        protected_rules(Path::new("/owner/protected"), true).map_err(|e| e.message().to_owned())?;
    assert_eq!(legacy.len(), native.len() + 1);
    assert_eq!(
        legacy[1],
        "(deny file-write-unlink (literal \"/owner/protected\"))"
    );
    assert_eq!(native[1], "(deny file-write-unlink (literal \"/owner\"))");
    Ok(())
}

#[test]
fn native_cargo_defaults_are_suppressed_without_removing_other_caches() -> Result<(), String> {
    let home = Path::new("/owner");
    let rules = crate::seatbelt::home_read_rules_configured(home, &[], &[], true, true)
        .map_err(|e| e.message().to_owned())?;
    assert!(
        rules
            .iter()
            .all(|r| !r.contains(".cargo") && !r.contains(".rustup"))
    );
    assert!(rules.iter().any(|r| r.contains(".npm")));
    assert!(rules.iter().any(|r| r.contains("go/pkg/mod")));
    Ok(())
}

#[test]
fn versioned_fields_are_accepted_only_for_v1() -> Result<(), String> {
    let mut req = request(Path::new("/owner"))?;
    req.interpreters = vec!["node".into()];
    req.login_targets.push(LoginTarget {
        path: "/owner/missing".into(),
        allow_directory: false,
    });
    req.toolchains = Some(Toolchains {
        cargo_configured: true,
        ..Default::default()
    });
    assert!(req.validate().is_ok());
    req.version = None;
    assert!(req.validate().is_err());
    req.version = Some(1);
    req.login_targets[0].path = "relative".into();
    assert!(req.validate().is_err());
    Ok(())
}

#[test]
fn shebang_parser_matches_python_quotes_escapes_and_noncomments() {
    assert_eq!(
        shebang_words("env -S 'node name' --flag #literal"),
        Some(
            vec!["env", "-S", "node name", "--flag", "#literal"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        )
    );
    assert_eq!(
        shebang_words(r#"env "node\$name" a\ b ''"#),
        Some(
            vec!["env", r"node\$name", "a b", ""]
                .into_iter()
                .map(str::to_owned)
                .collect()
        )
    );
    assert_eq!(shebang_words("env 'unterminated"), None);
    assert_eq!(shebang_words("env node\\"), None);
}

#[test]
fn legacy_literal_controls_remain_json_escaped() -> Result<(), Failure> {
    assert_eq!(
        path_quote(Path::new("/owner/a\nb"), false)?,
        r#""/owner/a\nb""#
    );
    assert!(path_quote(Path::new("/owner/a\nb"), true).is_err());
    Ok(())
}
