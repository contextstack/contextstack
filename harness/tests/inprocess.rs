//! In-process estate regressions (mirrors harness/tests/quality.py subsets).

mod estate_support;
use cs_harness::{cli, gateway, process};
use estate_support::{ContextStack, OK, drive, finish, mock_gateway_for_test, tool_reply, yaml};
use serde_json::Value;
use std::fs;

#[tokio::test]
async fn director_task_link_reaches_run_record() -> Result<(), String> {
    for intent in [None, Some("01ARZ3NDEKTSV4RRFFQ69G5FAV")] {
        let cs = ContextStack::build()?;
        let field = intent.map_or(String::new(), |id| format!("intent: {id}\n"));
        let spec = cs.write_spec(&yaml("{turns: 3, seconds: 30}", &field))?;
        let result = drive(&cs, &spec, vec![OK.into()]).await?;
        assert!(!result.pending, "{:?}", result.error);
        let terminal = cs.terminal()?;
        let ledger =
            fs::read_to_string(cs.root.join("ledger/ledger.jsonl")).map_err(|e| e.to_string())?;
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
    let cs = ContextStack::build()?;
    cs.tool_script("exit 0")?;
    let spec = cs.write_spec(&yaml("{turns: 3, tool_calls: 1, usd: 0, seconds: 30}", ""))?;
    let row = drive(&cs, &spec, vec![tool_reply("call", "{}"), OK.into()]).await?;
    assert_eq!(row.stop, "budget:tool_calls");
    assert_eq!(row.tool_calls, 1);
    Ok(())
}

fn run_events(terminal: &Value) -> Result<Vec<Value>, String> {
    let dir = terminal["run_dir"]
        .as_str()
        .ok_or("missing run directory")?;
    let log = fs::read_to_string(std::path::Path::new(dir).join("messages.jsonl"))
        .map_err(|e| e.to_string())?;
    log.lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())
}

fn fixture_tools(cs: &ContextStack, names: &[&str]) -> Result<(), String> {
    for name in names {
        for suffix in ["", ".json"] {
            fs::copy(
                cs.root.join(format!("tools/bin/fx_tool{suffix}")),
                cs.root.join(format!("tools/bin/{name}{suffix}")),
            )
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn scripted_reply(text: &str, names: impl IntoIterator<Item = &'static str>) -> String {
    let calls = names
        .into_iter()
        .enumerate()
        .map(|(id, name)| serde_json::json!({"id": id.to_string(), "name": name, "args": {}}))
        .collect::<Vec<_>>();
    serde_json::json!({
        "status": "ok",
        "model": "fixture",
        "message": {"content": text},
        "tool_calls": calls,
        "cost": {"class": "free_local", "est_usd": 0, "charged_usd": 0}
    })
    .to_string()
}

fn guidance_turns(events: &[Value], prefix: &str) -> Vec<u64> {
    events
        .iter()
        .filter(|event| event["event"] == "prompt")
        .filter(|event| {
            event["turn_guidance"]
                .as_str()
                .unwrap_or_default()
                .lines()
                .any(|line| line.starts_with(prefix))
        })
        .filter_map(|event| event["turn"].as_u64())
        .collect()
}

#[tokio::test]
async fn coding_plan_follows_first_check_or_appears_on_turn_six() -> Result<(), String> {
    for (writes, goal, check_turn, plan_turn) in [
        (true, "Fix the bug.", 2, Some(3)),
        (true, "Fix the bug.", 7, Some(6)),
        (false, "Investigate the bug.", 2, None),
        (true, "Write answer.md.", 2, None),
    ] {
        let cs = ContextStack::build()?;
        cs.tool_script("exit 0")?;
        fixture_tools(&cs, &["fs_write"])?;
        fs::write(cs.root.join("tasks/fx/goal.md"), goal).map_err(|e| e.to_string())?;
        let tools = if writes {
            "fx_tool, run_check, fs_write"
        } else {
            "fx_tool, run_check"
        };
        let spec = cs.write_spec(
            &yaml("{turns: 11, seconds: 30}", "").replace("[fx_tool]", &format!("[{tools}]")),
        )?;
        let plan = "Update parser and caller to agree on errors. Edit the parser first.";
        let mut replies = (1..=8)
            .map(|turn| {
                if Some(turn) == plan_turn {
                    scripted_reply(plan, [])
                } else {
                    let name = if turn == check_turn || turn == 7 {
                        "run_check"
                    } else {
                        "fx_tool"
                    };
                    scripted_reply("observe", [name])
                }
            })
            .collect::<Vec<_>>();
        let expected_turns = if plan_turn.is_some() {
            replies.push(scripted_reply("fix the bug", ["fs_write"]));
            10
        } else {
            9
        };
        replies.push(OK.into());
        let row = drive(&cs, &spec, replies).await?;
        assert_eq!((row.stop.as_str(), row.turns), ("done", expected_turns));
        let events = run_events(&cs.terminal()?)?;
        assert_eq!(
            guidance_turns(&events, "State your plan in a few lines:"),
            plan_turn.into_iter().collect::<Vec<_>>()
        );
        if let Some(turn) = plan_turn {
            assert!(events.iter().any(|event| {
                event["event"] == "turn" && event["turn"] == turn && event["text"] == plan
            }));
        }
    }
    Ok(())
}

#[tokio::test]
async fn coding_nudge_pauses_reads_for_one_turn_unless_a_write_resets_it() -> Result<(), String> {
    for write in [None, Some("fs_write"), Some("fs_edit")] {
        let cs = ContextStack::build()?;
        cs.tool_script("exit 0")?;
        let reads = ["fs_read", "repo_search", "fs_list"];
        let kept = ["exec", "fs_write", "fs_edit", "run_check"];
        fixture_tools(
            &cs,
            &[
                "fs_read",
                "repo_search",
                "fs_list",
                "exec",
                "fs_write",
                "fs_edit",
            ],
        )?;
        let spec = cs.write_spec(&yaml("{turns: 12, seconds: 30}", "").replace(
            "[fx_tool]",
            "[fs_read, repo_search, fs_list, exec, fs_write, fs_edit, run_check]",
        ))?;
        let replies = vec![
            scripted_reply("one large batch", ["fs_read"; 24]),
            scripted_reply("after the nudge", ["fs_read"; 11].into_iter().chain(write)),
            scripted_reply("one more read", ["fs_read"]),
            scripted_reply("probe palette", reads.into_iter().chain(kept)),
            scripted_reply("probe restored reads", reads),
            OK.into(),
        ];
        let row = drive(&cs, &spec, replies).await?;
        assert_eq!(row.stop, "done");
        let events = run_events(&cs.terminal()?)?;
        assert_eq!(guidance_turns(&events, "You've read a lot."), [2]);
        let expected: &[u64] = if write.is_none() { &[4] } else { &[] };
        assert_eq!(
            guidance_turns(&events, "Read tools are paused for one turn:"),
            expected
        );
        for turn in [3, 4, 5] {
            for event in events
                .iter()
                .filter(|event| event["event"] == "tool" && event["turn"] == turn)
            {
                let withheld =
                    write.is_none() && turn == 4 && reads.iter().any(|name| event["name"] == *name);
                assert_eq!(
                    event["content"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("was withheld"),
                    withheld,
                    "{event}"
                );
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn early_check_guidance_requires_a_coding_run_with_a_check() -> Result<(), String> {
    for (tools, goal, has_check, expected) in [
        ("fs_write, run_check", "Fix the bug.", true, true),
        ("fs_write, run_check", "Fix the bug.", false, false),
        ("fs_write", "Fix the bug.", true, false),
        ("run_check", "Investigate the bug.", true, false),
        ("fs_write, run_check", "Write answer.md.", true, false),
    ] {
        let cs = ContextStack::build()?;
        cs.tool_script("exit 0")?;
        fixture_tools(&cs, &["fs_write"])?;
        if !has_check {
            fs::write(
                cs.root.join("tasks/fx/check"),
                "#!/bin/sh\n# no check: the lead gave none, so every run passes\nexit 0\n",
            )
            .map_err(|e| e.to_string())?;
        }
        fs::write(cs.root.join("tasks/fx/goal.md"), goal).map_err(|e| e.to_string())?;
        let spec = cs.write_spec(
            &yaml("{turns: 4, seconds: 30}", "")
                .replace("[fx_tool]", &format!("[fx_tool, {tools}]")),
        )?;
        let mut replies = vec![tool_reply("observe", "{}")];
        if goal == "Fix the bug." {
            let check = tools.contains("run_check").then_some("run_check");
            replies.push(scripted_reply(
                "fix the bug",
                ["fs_write"].into_iter().chain(check),
            ));
        }
        replies.push(OK.into());
        let row = drive(&cs, &spec, replies).await?;
        assert_eq!(row.stop, "done", "{tools}, {goal}, has_check={has_check}");
        let events = run_events(&cs.terminal()?)?;
        let prompts = events.iter().filter(|event| event["event"] == "prompt");
        let reminders: Vec<_> = prompts
            .filter(|event| {
                event["turn_guidance"]
                    .as_str()
                    .unwrap_or_default()
                    .lines()
                    .any(|line| line == "Start with run_check to see the failure.")
            })
            .map(|event| event["turn"].as_u64())
            .collect();
        assert_eq!(
            reminders,
            if expected { &[Some(1)][..] } else { &[] },
            "{tools}, {goal}, has_check={has_check}",
        );
    }
    Ok(())
}

#[tokio::test]
async fn workspace_guidance_appears_on_first_final_and_extra_turns() -> Result<(), String> {
    let cs = ContextStack::build()?;
    cs.tool_script("exit 0")?;
    fs::write(cs.root.join("tasks/fx/goal.md"), "Write answer.md.").map_err(|e| e.to_string())?;
    let spec = cs.write_spec(&yaml("{turns: 3, seconds: 30}", ""))?;
    let replies = (1..=4)
        .map(|turn| tool_reply(&turn.to_string(), "{}"))
        .collect();
    let row = drive(&cs, &spec, replies).await?;
    assert_eq!(row.stop, "budget:turns");
    let terminal = cs.terminal()?;
    let workspace = terminal["workspace"].as_str().ok_or("missing workspace")?;
    assert!(std::path::Path::new(workspace).is_absolute());
    let workspace_line = format!(
        "Your workspace (write deliverables here, relative paths resolve here): {workspace}"
    );
    let events = run_events(&terminal)?;
    let prompts: Vec<_> = events
        .iter()
        .filter(|event| event["event"] == "prompt")
        .collect();
    assert_eq!(prompts.len(), 4);
    for (prompt, (turn, expected_count)) in
        prompts.into_iter().zip([(1, 1), (2, 0), (3, 1), (4, 1)])
    {
        assert_eq!(prompt["turn"], turn);
        let guidance = prompt["turn_guidance"]
            .as_str()
            .ok_or("missing turn guidance")?;
        let workspace_lines: Vec<_> = guidance
            .lines()
            .filter(|line| line.starts_with("Your workspace"))
            .collect();
        assert_eq!(
            workspace_lines,
            vec![workspace_line.as_str(); expected_count]
        );
    }
    Ok(())
}

#[tokio::test]
async fn final_turn_retry_is_write_only_and_runs_at_most_once() -> Result<(), String> {
    let write = tool_reply("write", r#"{"path":"answer.md","content":"answer"}"#)
        .replace("fx_tool", "fs_write");
    let failed_write = tool_reply(
        "failed",
        r#"{"path":"/snapshot/answer.md","content":"answer"}"#,
    )
    .replace("fx_tool", "fs_write");
    let read = tool_reply("read", r#"{"path":"."}"#).replace("fx_tool", "fs_read");
    let mut refused: Value = serde_json::from_str(&read).map_err(|e| e.to_string())?;
    refused["tool_calls"]
        .as_array_mut()
        .ok_or("missing tool calls")?
        .push(serde_json::json!({"id":"2", "name":"fx_tool", "args":{"query":"fact"}}));
    let refused = refused.to_string();
    for (replies, turns, stop, written, refused_calls, failed_writes) in [
        (vec![refused.clone(), write.clone()], 2, "done", true, 2, 0),
        (vec![refused, read], 2, "budget:turns", false, 3, 0),
        (
            vec![failed_write.clone(), write.clone()],
            2,
            "done",
            true,
            0,
            1,
        ),
        (
            vec![failed_write, OK.into()],
            2,
            "budget:turns",
            false,
            0,
            1,
        ),
        (vec![OK.into(), write.clone()], 1, "done", false, 0, 0),
        (vec![write], 1, "done", true, 0, 0),
    ] {
        let cs = ContextStack::build()?;
        cs.tool_script(
            r#"case "$(cat)" in
                */snapshot/*) echo "path escapes workspace: /snapshot/answer.md" >&2; exit 1 ;;
            esac
            echo answer > answer.md"#,
        )?;
        fixture_tools(&cs, &["fs_read", "fs_write"])?;
        let task = cs.root.join("tasks/fx");
        fs::write(task.join("goal.md"), "Write answer.md.").map_err(|e| e.to_string())?;
        fs::write(task.join("check"), "#!/bin/sh\ntest -f answer.md\n")
            .map_err(|e| e.to_string())?;
        let spec = cs.write_spec(
            &yaml("{turns: 1, seconds: 30}", "")
                .replace("tools: [fx_tool]", "tools: [fs_read, fs_write, fx_tool]"),
        )?;
        let row = drive(&cs, &spec, replies).await?;
        assert_eq!((row.turns, row.stop.as_str()), (turns, stop));
        let terminal = cs.terminal()?;
        let metrics = &terminal["harness_trace"]["identity"]["terminal"];
        assert_eq!(metrics["deliverables"]["answer.md"], written);
        assert_eq!(
            metrics.get("final_turn_retry").and_then(Value::as_bool),
            (turns == 2).then_some(true)
        );
        assert_eq!(
            metrics["tool_errors"]["fs_write"].as_u64().unwrap_or(0),
            failed_writes
        );
        if turns == 2 {
            let events = run_events(&terminal)?;
            let guidance = events
                .iter()
                .find(|event| event["event"] == "prompt" && event["turn"] == 2)
                .and_then(|event| event["turn_guidance"].as_str())
                .ok_or("missing extra turn guidance")?;
            assert_eq!(
                guidance.lines().next(),
                Some(
                    "Read tools are unavailable. You must now write the deliverable using what you already have."
                )
            );
            let refusals: Vec<_> = events
                .iter()
                .filter(|event| {
                    event["event"] == "tool"
                        && event["error"] == true
                        && event["name"] != "fs_write"
                })
                .collect();
            assert_eq!(refusals.len(), refused_calls);
            for event in refusals {
                let content = event["content"].as_str().ok_or("missing refusal content")?;
                let first = content.lines().next().unwrap_or_default();
                assert!(
                    first.contains(
                        "was withheld because read tools are unavailable on the final turn"
                    ),
                    "unexpected refusal: {content}"
                );
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn unknown_directory_tool_lists_available_names_and_suggests_fs_read() -> Result<(), String> {
    let cs = ContextStack::build()?;
    for suffix in ["", ".json"] {
        fs::copy(
            cs.root.join(format!("tools/bin/fx_tool{suffix}")),
            cs.root.join(format!("tools/bin/fs_read{suffix}")),
        )
        .map_err(|e| e.to_string())?;
    }
    let spec = cs.write_spec(
        &yaml("{turns: 3, seconds: 30}", "")
            .replace("tools: [fx_tool]", "tools: [fx_tool, fs_read]"),
    )?;
    let row = drive(
        &cs,
        &spec,
        vec![
            tool_reply("list files", "{}").replace("fx_tool", "list_dir"),
            OK.into(),
        ],
    )
    .await?;
    assert_eq!(row.stop, "done");
    let terminal = cs.terminal()?;
    let events = run_events(&terminal)?;
    let error = events
        .iter()
        .find(|event| event["event"] == "tool")
        .ok_or("missing tool event")?;
    assert_eq!(
        error["content"],
        "tool \"list_dir\" is not in this spec's palette\nAvailable tools: fs_read, fx_tool\nDid you mean fs_read on a directory?"
    );
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
async fn partial_stream_error_charge_reaches_result_and_terminal() -> Result<(), String> {
    let stream = concat!(
        "data: {\"kind\":\"delta\",\"content\":\"partial\"}\n\n",
        "data: {\"kind\":\"error\",\"cost\":{\"currency\":\"USD\",\"amount\":\"0.004200\",",
        "\"class\":\"api\",\"est_usd\":0.0042,\"charged_usd\":0.0042},",
        "\"error\":{\"class\":\"runtime_fault\",\"code\":\"gateway.transport_failed\",",
        "\"message\":\"stream failed\"}}\n\n"
    );
    let (row, terminal, _) = finish(
        &yaml("{turns: 3, tool_calls: 5, usd: 1, seconds: 30}", ""),
        vec![stream.into()],
    )
    .await?;
    assert_eq!(
        (row.cost, terminal["cost_usd"].as_f64()),
        (0.0042, Some(0.0042)),
        "ResultRow.cost and terminal.json cost_usd"
    );
    Ok(())
}

/// A deadline mid-call still counts the charge the gateway settles, on the call span and the terminal.
#[tokio::test]
async fn deadline_keeps_the_charge_the_gateway_settles() -> Result<(), String> {
    let late = r#"__late__{"status":"error","cost":{"class":"api","charged_usd":0.0042},"error":{"class":"runtime","code":"gateway.deadline","message":"late"}}"#;
    let cs = ContextStack::build()?;
    let spec = cs.write_spec(&yaml("{turns: 3, tool_calls: 5, usd: 1, seconds: 4}", ""))?;
    let row = drive(&cs, &spec, vec![late.into()]).await?;
    assert_eq!((row.stop.as_str(), row.cost), ("budget:seconds", 0.0042));
    let run = fs::read_dir(cs.root.join("ledger/runs"))
        .map_err(|e| e.to_string())?
        .next()
        .ok_or("no run")?
        .map_err(|e| e.to_string())?;
    let trace = fs::read_to_string(run.path().join("trace.jsonl")).map_err(|e| e.to_string())?;
    let spans = trace
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let call = spans
        .iter()
        .find(|span| span["kind"] == "model_call")
        .ok_or("no model_call span")?;
    assert_eq!(call["attributes"]["charged_usd"], 0.0042, "{call}");
    assert_eq!(call["attributes"]["partial"], true, "{call}");
    let terminal = spans
        .iter()
        .find_map(|span| span["attributes"]["terminal"].as_object())
        .ok_or("no terminal")?;
    assert_eq!(terminal["charged_usd"], 0.0042);
    assert_eq!(
        terminal["accounted_settlements"].as_array().map(Vec::len),
        Some(1)
    );
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
    let cs = ContextStack::build()?;
    cs.hook("pre_tool", "echo vetoed; exit 2")?;
    let spec = cs.write_spec(&yaml(
        "{turns: 3, tool_calls: 5, usd: 0, seconds: 30}",
        "pre_tool: [pre_tool]\n",
    ))?;
    let row = drive(&cs, &spec, vec![tool_reply("nope", "{}"), OK.into()]).await?;
    assert_eq!(row.stop, "aborted");
    Ok(())
}

#[tokio::test]
async fn missing_hook_at_load_fails_before_gateway() -> Result<(), String> {
    let cs = ContextStack::build()?;
    let spec = cs.write_spec(&yaml(
        "{turns: 1, tool_calls: 0, usd: 0, seconds: 30}",
        "post_turn: [absent]\n",
    ))?;
    let row = drive(&cs, &spec, vec![OK.into()]).await?;
    assert_eq!(row.verdict, "void");
    assert!(row.stop.starts_with("error:"));
    let terminal = cs.terminal()?;
    let run_id = terminal["run_id"].as_str().ok_or("missing run ID")?;
    let trace = fs::read_to_string(cs.root.join("ledger/runs").join(run_id).join("trace.jsonl"))
        .map_err(|e| e.to_string())?;
    let spans = trace
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let finalization = spans
        .iter()
        .find(|span| span["kind"] == "finalization")
        .ok_or("missing finalization span")?;
    assert_eq!(
        finalization["attributes"].get("evidence_id"),
        Some(&Value::Null)
    );
    Ok(())
}

async fn model_deadline(
    cs: &ContextStack,
) -> Result<(cs_harness::runner::ResultRow, String), String> {
    let spec = cs.write_spec(&yaml("{turns: 5, seconds: 3600}", ""))?;
    let received = std::sync::Arc::new(tokio::sync::Notify::new());
    let expire = async {
        received.notified().await;
        // Advance only after the gateway has read the model request: setup cannot satisfy this test.
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(3601)).await;
        tokio::time::resume();
    };
    let run =
        estate_support::drive_observed(cs, &spec, vec!["__hang__".into()], Some(received.clone()));
    let (row, ()) = tokio::join!(run, expire);
    let row = row?;
    let entry = fs::read_dir(cs.root.join("ledger/runs"))
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
    let (row, log) = model_deadline(&ContextStack::build()?).await?;
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
    let cs = ContextStack::build()?;
    cs.tool_script("exit 0")?;
    let spec = cs.write_spec(&yaml(
        "{turns: 5, tool_calls: 5, usd: 0.10, seconds: 30}",
        "",
    ))?;
    let row = drive(&cs, &spec, vec![first.into(), second.into()]).await?;
    assert_eq!(row.stop, "budget:usd");
    assert!((row.cost - 0.10).abs() < f64::EPSILON);
    assert_eq!(row.turns, 2);
    assert_eq!(row.replies, 2);
    Ok(())
}

#[tokio::test]
async fn usd_budget_skips_tool_calls_after_reply() -> Result<(), String> {
    let reply = r#"{"status":"ok","model":"fixture","message":{"content":"call"},"tool_calls":[{"id":"1","name":"fx_tool","args":{}}],"cost":{"class":"api","est_usd":0.12,"charged_usd":0.12}}"#;
    let cs = ContextStack::build()?;
    cs.tool_script("touch tool-ran\n")?;
    let spec = cs.write_spec(&yaml(
        "{turns: 3, tool_calls: 5, usd: 0.10, seconds: 30}",
        "",
    ))?;
    let row = drive(&cs, &spec, vec![reply.into()]).await?;
    assert_eq!(row.stop, "budget:usd");
    assert_eq!(row.tool_calls, 0);
    let runs = cs.root.join("ledger/runs");
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
    let (row, log) = model_deadline(&ContextStack::build()?).await?;
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
    for name in ["capture-stimulus", "pre_tool", "post_turn"] {
        let cs = ContextStack::build()?;
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
        let path = cs.root.join(kind).join("bin").join(name);
        fs::write(&path, script).map_err(|e| e.to_string())?;
        fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .map_err(|e| e.to_string())?;
        let hooks = if kind == "hooks" {
            format!("{name}: [{name}]\n")
        } else {
            String::new()
        };
        let spec = cs.write_spec(&yaml("{turns: 3, seconds: 0}", &hooks))?;
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
            drive(&cs, &spec, vec![tool_reply("call", "{}"), OK.into()]),
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
        let run = fs::read_dir(cs.root.join("ledger/runs"))
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

/// A deadline that expires in setup or capture-stimulus is the run's timeout, not a void error.
#[tokio::test]
async fn a_deadline_during_setup_is_a_timeout() -> Result<(), String> {
    for name in ["setup-workspace", "capture-stimulus"] {
        let cs = ContextStack::build()?;
        let path = cs.root.join("ops/bin").join(name);
        fs::write(&path, "#!/bin/sh\nexec sleep 600\n").map_err(|e| e.to_string())?;
        fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .map_err(|e| e.to_string())?;
        if name == "setup-workspace" {
            fs::create_dir_all(cs.root.join("tasks/fx/setup")).map_err(|e| e.to_string())?;
        }
        let spec = cs.write_spec(&yaml("{turns: 3, seconds: 1}", ""))?;
        let task = cs.root.join("tasks/fx");
        let task = task.to_str().ok_or("non-UTF8 path")?;
        let row = estate_support::drive_task(&cs, &spec, task, vec![OK.into()], None)
            .await
            .map_err(|e| format!("drive {name}: {e}"))?;
        assert_eq!(row.stop, "budget:seconds", "{name}: {:?}", row.error);
        assert_ne!(row.verdict, "void", "{name}");
        let error = row.error.unwrap_or_default();
        assert!(
            error.contains(name) && !error.contains("retry finalization"),
            "{error}"
        );
        let run = fs::read_dir(cs.root.join("ledger/runs"))
            .map_err(|e| e.to_string())?
            .next()
            .ok_or("no run")?
            .map_err(|e| e.to_string())?;
        let receipt = fs::read(run.path().join("execution-receipt.json"))
            .map_err(|e| format!("{name}: receipt: {e}"))?;
        let receipt: Value = serde_json::from_slice(&receipt).map_err(|e| e.to_string())?;
        assert_eq!(
            receipt["row"]["terminal_state"], "timeout",
            "{name}: {receipt}"
        );
    }
    Ok(())
}

/// Hooks die after the spec's `check_window` (SCHEMA Hooks; default 60), not the ops watchdog.
#[tokio::test]
async fn hooks_are_killed_after_the_check_window() -> Result<(), String> {
    for (window, alive_after, killed_after) in [("", 0, 61), ("check_window: 300\n", 121, 301)] {
        let cs = ContextStack::build()?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let script = format!(
            "#!/usr/bin/python3\nimport socket, time\ns = socket.socket()\ns.connect(('127.0.0.1', {port}))\ntime.sleep(600)\n"
        );
        let path = cs.root.join("hooks/bin/post_turn");
        fs::write(&path, script).map_err(|e| e.to_string())?;
        fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .map_err(|e| e.to_string())?;
        let middle = format!("post_turn: [post_turn]\npre_tool: []\n{window}");
        let spec = cs.write_spec(&yaml("{turns: 3, seconds: 0}", &middle))?;
        let expire = async {
            let (mut connection, _) = listener.accept().await.map_err(|e| e.to_string())?;
            let mut byte = [0];
            if alive_after > 0 {
                tokio::time::pause();
                tokio::time::advance(std::time::Duration::from_secs(alive_after)).await;
                tokio::time::resume();
                let read = tokio::io::AsyncReadExt::read(&mut connection, &mut byte);
                let waited = tokio::time::timeout(std::time::Duration::from_millis(500), read);
                assert!(waited.await.is_err(), "hook killed before its check_window");
            }
            tokio::time::pause();
            tokio::time::advance(std::time::Duration::from_secs(killed_after - alive_after)).await;
            tokio::time::resume();
            let read = tokio::io::AsyncReadExt::read(&mut connection, &mut byte);
            let read = tokio::time::timeout(std::time::Duration::from_secs(10), read)
                .await
                .map_err(|_| format!("hook outlived check_window {window:?}"))?;
            assert_eq!(
                read.map_err(|e| e.to_string())?,
                0,
                "watchdog must kill the hook"
            );
            Ok::<_, String>(())
        };
        let (row, expired) = tokio::join!(
            drive(&cs, &spec, vec![tool_reply("call", "{}"), OK.into()]),
            expire
        );
        expired?;
        let row = row?;
        assert!(
            row.error.as_ref().is_some_and(|e| e.contains("post_turn")),
            "{:?}",
            row.error
        );
    }
    Ok(())
}

#[tokio::test]
async fn cli_end_to_end_run_writes_execution_receipt() -> Result<(), String> {
    let cs = ContextStack::build()?;
    cs.spec()?;
    let (endpoint, server) = mock_gateway_for_test(vec![OK.into()]).await?;
    let _env = process::ScopedEnv::set([
        ("CS_JOB_ID".to_string(), "dispatch-test".into()),
        ("CSGATEWAY_ENDPOINT".to_string(), endpoint),
        (
            "CS_CODE_ROOT".to_string(),
            cs.root.to_string_lossy().into_owned(),
        ),
        (
            "CS_STATE_ROOT".to_string(),
            cs.root.join("ledger").to_string_lossy().into_owned(),
        ),
        (
            "CS_CONFIG_ROOT".to_string(),
            cs.root.to_string_lossy().into_owned(),
        ),
    ]);
    let code = cli::execute_from(
        ["run".into(), "specs/fx.yaml".into()].into_iter(),
        Some(&cs.root),
    )
    .await?;
    server.abort();
    assert_eq!(code, 0);
    let run = fs::read_dir(cs.root.join("ledger/runs"))
        .map_err(|e| e.to_string())?
        .next()
        .ok_or("no run")?
        .map_err(|e| e.to_string())?
        .path();
    assert!(!run.join("terminal.json").exists());
    assert!(!run.join("check.json").exists());
    let bytes = fs::read(run.join("execution-receipt.json")).map_err(|e| e.to_string())?;
    let receipt: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let identity = &receipt["trace"]["harness_trace"]["identity"];
    assert_eq!(receipt["job_id"], "dispatch-test");
    assert_eq!(identity["job_id"], "dispatch-test");
    assert_eq!(identity["task"], "tasks/fx");
    assert_eq!(identity["trial"], 1);
    assert_eq!(identity["run_id"], receipt["run_id"]);
    Ok(())
}

#[tokio::test]
async fn trial_flag_and_task_flag_are_parsed() -> Result<(), String> {
    let cs = ContextStack::build()?;
    cs.spec()?;
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
        Some(&cs.root),
    )
    .await?;
    server.abort();
    assert_eq!(code, 0);
    Ok(())
}

#[tokio::test]
async fn harness_traces_reconcile_with_the_contract() -> Result<(), Box<dyn std::error::Error>> {
    use serde_json::json;

    for case in [
        "plain", "slow", "unknown", "refused", "timeout", "fallback", "stream",
    ] {
        let cs = ContextStack::build()?;
        let row = if case == "timeout" {
            model_deadline(&cs).await?.0
        } else {
            let hooks = if case == "slow" {
                cs.hook("before", "echo before")?;
                cs.hook("after", "echo after")?;
                "pre_tool: [before]\npost_turn: [after]\n"
            } else {
                ""
            };
            let budget = match case {
                "unknown" => "{turns: 1, tool_calls: 1, seconds: 30}",
                _ => "{turns: 2, seconds: 30}",
            };
            let spec = cs.write_spec(&yaml(budget, hooks))?;
            let replies = match case {
                "slow" => {
                    cs.tool_script("sleep 0.2; echo tool-output")?;
                    vec![tool_reply("call", "{\"id\":1}")]
                }
                "unknown" => vec![tool_reply("call", "{}").replace("fx_tool", "absent")],
                "refused" => vec![],
                "fallback" => vec![
                    "data: {\"kind\":\"error\",\"error\":{\"code\":\"gateway.streaming_unsupported\"}}\n\n"
                        .into(),
                    OK.into(),
                ],
                "stream" => vec![concat!(
                    "data: {\"kind\":\"delta\",\"content\":\"hello\"}\n\n",
                    "data: {\"kind\":\"stop\",\"cost\":{\"class\":\"free_local\",\"est_usd\":0,\"charged_usd\":0}}\n\n"
                ).into()],
                _ => vec![OK.into()],
            };
            drive(&cs, &spec, replies).await?
        };
        assert!(!row.pending, "{case}: {:?}", row.error);
        let dir = fs::read_dir(cs.root.join("ledger/runs"))?
            .next()
            .ok_or("missing run directory")??
            .path();
        let trace_id = dir.file_name().ok_or("missing run ID")?.to_string_lossy();
        let trace_file = dir.join("trace.jsonl");
        let spans = fs::read_to_string(&trace_file)?
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<Result<Vec<_>, _>>()?;
        let get = |kind: &str| {
            spans
                .iter()
                .find(|span| span["kind"] == kind)
                .unwrap_or(&Value::Null)
        };
        let root = get("run");
        assert_eq!(root["outcome"], "finalized");
        assert_eq!(root["trace_id"], trace_id.as_ref());
        assert_eq!(root["attributes"]["terminal"]["turns"], 1);
        assert_eq!(
            root["attributes"]["terminal"]["tool_calls"],
            i64::from(matches!(case, "slow" | "unknown"))
        );
        let model = &get("model_call")["attributes"];
        assert!(model["content"]["prompt"].is_object());
        let request_id = model["request_id"].as_str().ok_or("missing request ID")?;
        let send = match case {
            "refused" => "connection_failed",
            "timeout" => "unknown",
            _ => "delivered",
        };
        let mut expected_sends = vec![json!({"send_index": 1, "outcome": send})];
        if case == "fallback" {
            expected_sends.push(json!({"send_index": 2, "outcome": send}));
        }
        assert_eq!(model["sends"], json!(expected_sends));
        if send == "delivered" {
            assert!(model["content"]["reply"].is_object());
        }
        assert_tool_spans(case, get)?;

        check_harness_spans(
            case,
            &dir,
            &trace_id,
            request_id,
            send,
            expected_sends.len(),
        )?;
    }
    Ok(())
}

fn assert_tool_spans<'a>(
    case: &str,
    get: impl Fn(&str) -> &'a Value,
) -> Result<(), Box<dyn std::error::Error>> {
    if case == "slow" {
        let process = get("tool_process");
        assert_eq!(get("tool_call")["outcome"], "executed");
        for kind in ["pre_tool_hook", "post_turn_hook"] {
            assert_eq!(get(kind)["outcome"], "completed");
            assert!(get(kind)["attributes"]["content"]["output"].is_object());
        }
        assert!(!process.to_string().contains("tool-output"));
        assert!(process["attributes"]["content"]["output"].is_object());
        let ms = process["duration_ms"]
            .as_f64()
            .ok_or("missing tool duration")?;
        assert!(ms >= 180.0);
        assert!(
            get("turn")["duration_ms"]
                .as_f64()
                .ok_or("missing turn duration")?
                >= ms
        );
    }
    if case == "unknown" {
        assert_eq!(get("tool_call")["outcome"], "unknown_tool");
        assert!(
            get("tool_call")["attributes"]["error"]
                .as_str()
                .is_some_and(|s| s.contains("absent"))
        );
        assert!(get("tool_process").is_null());
    }
    Ok(())
}

fn check_harness_spans(
    case: &str,
    dir: &std::path::Path,
    trace_id: &str,
    request_id: &str,
    send: &str,
    send_count: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    use serde_json::json;
    use std::process::Command;
    let trace_file = dir.join("trace.jsonl");
    let checker = concat!(env!("CARGO_MANIFEST_DIR"), "/../tracing/check-trace");
    let directories = dir.join("directories.json");
    fs::write(
        &directories,
        json!({"run_directories": {trace_id: dir}}).to_string(),
    )?;
    let output = Command::new("python3")
        .arg(checker)
        .args([trace_file, directories])
        .output()?;
    let actual: Value = serde_json::from_slice(&output.stdout).map_err(|e| {
        format!(
            "{case}: checker: {e}: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })?;
    let (result, missing, findings) = match send {
        "connection_failed" => ("complete", vec![], vec![]),
        "unknown" => (
            "complete_with_findings",
            vec![],
            vec![json!({
                "kind": "delivery_unconfirmed", "trace_id": trace_id, "request_id": request_id,
                "send_index": 1, "gateway_found": false
            })],
        ),
        _ => (
            "incomplete",
            (1..=send_count)
                .map(|index| format!("2: {trace_id}/request {request_id} send {index} exchange"))
                .collect(),
            vec![],
        ),
    };
    assert_eq!(
        output.status.code(),
        Some(i32::from(result == "incomplete")),
        "{case}: {actual}"
    );
    assert_eq!(
        actual,
        json!({"result": result, "missing": missing, "findings": findings}),
        "{case}"
    );
    Ok(())
}
