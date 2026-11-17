//! In-process estate regressions (mirrors harness/tests/quality.py subsets).

mod estate_support;
use cs_harness::{cli, gateway, process};
use estate_support::{Lab, OK, drive, finish, mock_gateway_for_test, tool_reply, yaml};
use serde_json::Value;
use std::fs;

#[tokio::test]
async fn director_task_link_reaches_run_record() -> Result<(), String> {
    for intent in [None, Some("01ARZ3NDEKTSV4RRFFQ69G5FAV")] {
        let lab = Lab::build()?;
        let field = intent.map_or(String::new(), |id| format!("intent: {id}\n"));
        let spec = lab.write_spec(&yaml("{turns: 3, seconds: 30}", &field))?;
        let result = drive(&lab, &spec, vec![OK.into()]).await?;
        assert!(!result.pending, "{:?}", result.error);
        let terminal = lab.terminal()?;
        let ledger =
            fs::read_to_string(lab.root.join("ledger/ledger.jsonl")).map_err(|e| e.to_string())?;
        let row: Value = serde_json::from_str(ledger.trim()).map_err(|e| e.to_string())?;
        for record in [terminal, row] {
            assert_eq!(record.get("intent").and_then(Value::as_str), intent);
            assert!(record.get("exp").is_none());
        }
    }
    Ok(())
}

#[tokio::test]
async fn one_turn_run_finalizes_with_done_stop() -> Result<(), String> {
    let (row, terminal, _) = finish(
        &yaml("{turns: 3, tool_calls: 5, usd: 0, seconds: 30}", ""),
        vec![OK.into()],
    )
    .await?;
    assert_eq!(row.stop, "done");
    assert!(!row.pending, "run should finalize: {:?}", row.error);
    assert_eq!(row.verdict, "pass");
    assert_eq!(terminal["terminal_state"], "completed");
    assert!(terminal.get("intent").is_none());
    Ok(())
}

#[tokio::test]
async fn tool_turn_counts_calls_and_stops_on_budget() -> Result<(), String> {
    let lab = Lab::build()?;
    lab.tool_script("exit 0")?;
    let spec = lab.write_spec(&yaml("{turns: 3, tool_calls: 1, usd: 0, seconds: 30}", ""))?;
    let row = drive(&lab, &spec, vec![tool_reply("call", "{}"), OK.into()]).await?;
    assert_eq!(row.stop, "budget:tool_calls");
    assert_eq!(row.tool_calls, 1);
    Ok(())
}

#[tokio::test]
async fn gateway_error_becomes_void_run_with_error_stop() -> Result<(), String> {
    let err = r#"{"status":"error","error":{"class":"runtime","code":"gateway.upstream_error","message":"boom"}}"#;
    let (row, terminal, _) = finish(
        &yaml("{turns: 3, tool_calls: 5, usd: 0, seconds: 30}", ""),
        vec![err.into()],
    )
    .await?;
    assert_eq!(row.verdict, "void");
    assert!(row.stop.starts_with("error:"));
    assert_eq!(terminal["terminal_state"], "error");
    Ok(())
}

#[tokio::test]
async fn ceiling_denial_sets_usd_ceiling_stop() -> Result<(), String> {
    let denied = r#"{"status":"error","error":{"class":"fatal_session","code":"gateway.policy_denied","message":"no","budget":{"reason":"ceiling"}}}"#;
    let (row, _, _) = finish(
        &yaml("{turns: 3, tool_calls: 5, usd: 0, seconds: 30}", ""),
        vec![denied.into()],
    )
    .await?;
    assert_eq!(row.stop, gateway::CEILING_STOP);
    Ok(())
}

#[tokio::test]
async fn pre_tool_refusal_aborts_with_code_two() -> Result<(), String> {
    let lab = Lab::build()?;
    lab.hook("pre_tool", "echo vetoed; exit 2")?;
    let spec = lab.write_spec(&yaml(
        "{turns: 3, tool_calls: 5, usd: 0, seconds: 30}",
        "pre_tool: [pre_tool]\n",
    ))?;
    let row = drive(&lab, &spec, vec![tool_reply("nope", "{}"), OK.into()]).await?;
    assert_eq!(row.stop, "aborted");
    Ok(())
}

#[tokio::test]
async fn missing_hook_at_load_fails_before_gateway() -> Result<(), String> {
    let lab = Lab::build()?;
    let spec = lab.write_spec(&yaml(
        "{turns: 1, tool_calls: 0, usd: 0, seconds: 30}",
        "post_turn: [absent]\n",
    ))?;
    let row = drive(&lab, &spec, vec![OK.into()]).await?;
    assert_eq!(row.verdict, "void");
    assert!(row.stop.starts_with("error:"));
    Ok(())
}

async fn model_deadline() -> Result<(cs_harness::runner::ResultRow, String), String> {
    let lab = Lab::build()?;
    let spec = lab.write_spec(&yaml("{turns: 5, seconds: 3600}", ""))?;
    let received = std::sync::Arc::new(tokio::sync::Notify::new());
    let expire = async {
        received.notified().await;
        // Advance only after the gateway has read the model request: setup cannot satisfy this test.
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(3601)).await;
        tokio::time::resume();
    };
    let run = estate_support::drive_observed(
        &lab,
        &spec,
        vec!["__hang__".into()],
        Some(received.clone()),
    );
    let (row, ()) = tokio::join!(run, expire);
    let row = row?;
    let entry = fs::read_dir(lab.root.join("ledger/runs"))
        .map_err(|e| e.to_string())?
        .next()
        .ok_or("no run")?
        .map_err(|e| e.to_string())?;
    let messages =
        fs::read_to_string(entry.path().join("messages.jsonl")).map_err(|e| e.to_string())?;
    Ok((row, messages))
}

#[tokio::test]
async fn wall_clock_budget_stops_at_seconds() -> Result<(), String> {
    let (row, log) = model_deadline().await?;
    assert_eq!(row.stop, "budget:seconds");
    assert_eq!(row.turns, 1, "exactly one model call was attempted");
    assert!(
        !log.contains(r#""event":"turn""#),
        "no model reply completed"
    );
    Ok(())
}

#[tokio::test]
async fn usd_stops_on_charges_above_estimates() -> Result<(), String> {
    let first = r#"{"status":"ok","model":"fixture","message":{"content":"spend"},"tool_calls":[{"id":"1","name":"fx_tool","args":{}}],"cost":{"class":"api","est_usd":0.04,"charged_usd":0.07}}"#;
    let second = r#"{"status":"ok","model":"fixture","message":{"content":"more"},"tool_calls":[],"cost":{"class":"api","est_usd":0.02,"charged_usd":0.03}}"#;
    let lab = Lab::build()?;
    lab.tool_script("exit 0")?;
    let spec = lab.write_spec(&yaml(
        "{turns: 5, tool_calls: 5, usd: 0.10, seconds: 30}",
        "",
    ))?;
    let row = drive(&lab, &spec, vec![first.into(), second.into()]).await?;
    assert_eq!(row.stop, "budget:usd");
    assert!((row.cost - 0.10).abs() < f64::EPSILON);
    assert_eq!(row.turns, 2);
    assert_eq!(row.replies, 2);
    Ok(())
}

#[tokio::test]
async fn usd_budget_skips_tool_calls_after_reply() -> Result<(), String> {
    let reply = r#"{"status":"ok","model":"fixture","message":{"content":"call"},"tool_calls":[{"id":"1","name":"fx_tool","args":{}}],"cost":{"class":"api","est_usd":0.12,"charged_usd":0.12}}"#;
    let lab = Lab::build()?;
    lab.tool_script("touch tool-ran\n")?;
    let spec = lab.write_spec(&yaml(
        "{turns: 3, tool_calls: 5, usd: 0.10, seconds: 30}",
        "",
    ))?;
    let row = drive(&lab, &spec, vec![reply.into()]).await?;
    assert_eq!(row.stop, "budget:usd");
    assert_eq!(row.tool_calls, 0);
    let runs = lab.root.join("ledger/runs");
    let entry = fs::read_dir(&runs)
        .map_err(|e| e.to_string())?
        .next()
        .ok_or("no run".to_string())?
        .map_err(|e| e.to_string())?;
    let log = fs::read_to_string(entry.path().join("messages.jsonl")).map_err(|e| e.to_string())?;
    assert!(log.contains("tool_skipped"));
    assert!(!entry.path().join("workspace/tool-ran").exists());
    Ok(())
}

#[tokio::test]
async fn seconds_timeout_error_uses_budget_reason() -> Result<(), String> {
    let (row, log) = model_deadline().await?;
    assert_eq!(row.stop, "budget:seconds");
    let error = log
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|e| e.get("event") == Some(&Value::String("error".into())))
        .ok_or("no error event")?;
    assert_eq!(error["error"], "budget:seconds");
    Ok(())
}

#[tokio::test]
async fn trusted_operations_and_hooks_have_watchdogs() -> Result<(), String> {
    for name in ["capture-stimulus", "finalize-run", "pre_tool", "post_turn"] {
        let lab = Lab::build()?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let script = format!(
            "#!/usr/bin/python3\nimport socket, time\ns = socket.socket()\ns.connect(('127.0.0.1', {port}))\ntime.sleep(600)\n"
        );
        let kind = if name.ends_with("_turn") || name == "pre_tool" {
            "hooks"
        } else {
            "ops"
        };
        let path = lab.root.join(kind).join("bin").join(name);
        fs::write(&path, script).map_err(|e| e.to_string())?;
        fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .map_err(|e| e.to_string())?;
        let hooks = if kind == "hooks" {
            format!("{name}: [{name}]\n")
        } else {
            String::new()
        };
        let spec = lab.write_spec(&yaml("{turns: 3, seconds: 0}", &hooks))?;
        let expire = async {
            let (mut connection, _) = listener.accept().await.map_err(|e| e.to_string())?;
            tokio::time::pause();
            tokio::time::advance(std::time::Duration::from_secs(121)).await;
            tokio::time::resume();
            assert_eq!(
                tokio::io::AsyncReadExt::read(&mut connection, &mut [0])
                    .await
                    .map_err(|e| e.to_string())?,
                0,
                "watchdog must kill the child"
            );
            Ok::<_, String>(())
        };
        let (row, expired) = tokio::join!(
            drive(&lab, &spec, vec![tool_reply("call", "{}"), OK.into()]),
            expire
        );
        expired?;
        let row = row?;
        assert!(row.pending, "{name} must leave finalization pending");
        assert!(
            row.error.as_ref().is_some_and(|e| e.contains(name)),
            "{:?}",
            row.error
        );
        let run = fs::read_dir(lab.root.join("ledger/runs"))
            .map_err(|e| e.to_string())?
            .next()
            .ok_or("no run")?
            .map_err(|e| e.to_string())?;
        assert!(
            !run.path().join("finalized.json").exists(),
            "{name} finalized despite watchdog"
        );
    }
    Ok(())
}

#[tokio::test]
async fn cli_end_to_end_run_exits_from_finalize_verdict() -> Result<(), String> {
    let lab = Lab::build()?;
    lab.spec()?;
    let (endpoint, server) = mock_gateway_for_test(vec![OK.into()]).await?;
    let _env = process::ScopedEnv::set([
        ("CSGATEWAY_ENDPOINT".to_string(), endpoint),
        (
            "LAB_CODE_ROOT".to_string(),
            lab.root.to_string_lossy().into_owned(),
        ),
        (
            "LAB_STATE_ROOT".to_string(),
            lab.root.join("ledger").to_string_lossy().into_owned(),
        ),
        (
            "LAB_CONFIG_ROOT".to_string(),
            lab.root.to_string_lossy().into_owned(),
        ),
    ]);
    let code = cli::execute_from(
        ["run".into(), "specs/fx.yaml".into()].into_iter(),
        Some(&lab.root),
    )
    .await?;
    server.abort();
    assert_eq!(code, 0);
    Ok(())
}

#[tokio::test]
async fn trial_flag_and_task_flag_are_parsed() -> Result<(), String> {
    let lab = Lab::build()?;
    lab.spec()?;
    let (endpoint, server) = mock_gateway_for_test(vec![OK.into()]).await?;
    let _env = process::ScopedEnv::set([("CSGATEWAY_ENDPOINT".to_string(), endpoint)]);
    let code = cli::execute_from(
        [
            "run".into(),
            "-task".into(),
            "tasks/fx".into(),
            "-trial".into(),
            "2".into(),
            "specs/fx.yaml".into(),
        ]
        .into_iter(),
        Some(&lab.root),
    )
    .await?;
    server.abort();
    assert_eq!(code, 0);
    Ok(())
}
