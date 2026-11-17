//! A conversation's context (CLIENT-API.md §11): what earlier turns go into the next turn's job, when
//! the thread is compacted, and where a reply comes from. Pure: no database, no clock.
//!
//! **How context reaches a job.** A turn's job is an ordinary bundle: the conversation's spec files, plus three
//! files the Director writes. The spec names them, so the runner reads them the way it reads any spec:
//!
//! | Path | What | How the runner reads it |
//! | --- | --- | --- |
//! | `conversation/context.md` | the thread so far: a summary of an older portion, then earlier turns whole | listed in the spec's `context`, so it joins the system prompt (`system.txt`) |
//! | `tasks/turn/goal.md` | the person's new message, as written | the task's goal: the user message |
//! | `tasks/turn/check` | `exit 0`: a turn has no check; its reply is its result | the task's check |
//!
//! **Where a reply comes from.** The run's evidence: the text of the last `turn` event with text in its
//! `messages.jsonl` (the runner's record of each model reply). A run without one has no reply.
//!
//! **Compaction.** Tokens are estimated as one per four bytes. A turn sends the whole thread while the context file
//! plus the message stays within the threshold: `window × compact_at − instructions − 500 × tools`, where `window`
//! is the model's context size (a setting, else the table below, else 32,768) and `compact_at` is 0.6, leaving the
//! rest for tool definitions, the reply and the estimate's error. Past it, the turn sends the newest summary plus the
//! turns after it whole. When that is still too big, a new summary is made first, by an ordinary job, of everything
//! but the most recent turns that fit in half the threshold.

use serde_json::Value;
use std::fmt::Write;
use std::sync::{PoisonError, RwLock};

/// The shipped hosted-lead spec id (`cs/lead/spec.yaml`, `cs lead create`).
pub const HOSTED_LEAD_SPEC_ID: &str = "hosted-lead";

/// Models a hosted lead can run on: the lead host always runs the Claude CLI with the lead session's MCP wired
/// (`cs/scripts/cs_lead.py` `LEAD_MODELS`). The restricted planner, judge and patch templates disable MCP or its
/// tools, so they cannot lead even though their ids share the `claude-code` prefix.
pub const LEAD_MODELS: [&str; 3] = ["claude-code-sonnet", "claude-code", "claude-code-opus"];

/// Whether a gateway model id can host a lead turn.
pub fn lead_model_ok(id: &str) -> bool {
    LEAD_MODELS.contains(&id)
}

/// Detail for refusals when a hosted-lead names an unsupported model.
pub fn lead_models_detail() -> Value {
    serde_json::json!({"models": LEAD_MODELS})
}

/// The generated context file; a conversation's spec lists it in `context`.
pub const CONTEXT_PATH: &str = "conversation/context.md";
/// The generated task; a conversation's spec lists it in `tasks`.
pub const TASK: &str = "tasks/turn";
/// The task's goal: the message.
pub const GOAL_PATH: &str = "tasks/turn/goal.md";
/// The task's check.
pub const CHECK_PATH: &str = "tasks/turn/check";
/// A turn's check: there is nothing to check.
pub const CHECK: &[u8] =
    b"#!/bin/sh\n# A conversation turn has no check: its reply is its result.\nexit 0\n";

/// The share of the window the thread and the instructions may fill before compaction.
pub const COMPACT_AT: f64 = 0.6;
/// Tokens set aside for each tool the spec lists (its definition).
pub const TOOL_TOKENS: i64 = 500;
/// The smallest threshold, however large the instructions.
const MIN_THRESHOLD: i64 = 256;

/// The compaction settings `serve` was given.
#[derive(Debug, Clone)]
pub struct Settings {
    /// `--context-window MODEL=TOKENS`: exact model names first.
    pub windows: Vec<(String, i64)>,
    /// `--compact-at FRACTION`.
    pub compact_at: f64,
}

static SETTINGS: RwLock<Option<Settings>> = RwLock::new(None);

/// Use these settings from now on.
pub fn configure(settings: Settings) {
    *SETTINGS.write().unwrap_or_else(PoisonError::into_inner) = Some(settings);
}

fn settings() -> Settings {
    SETTINGS
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .unwrap_or(Settings {
            windows: Vec::new(),
            compact_at: COMPACT_AT,
        })
}

/// Estimated tokens: one per four bytes, rounded up.
pub fn tokens(text: &str) -> i64 {
    i64::try_from(text.len().div_ceil(4)).unwrap_or(i64::MAX)
}

/// A model's context size and where it came from: `setting`, `table` or `default`.
pub fn window(model: &str) -> (i64, &'static str) {
    if let Some((_, w)) = settings().windows.iter().find(|(m, _)| m == model) {
        return (*w, "setting");
    }
    let resolved = crate::context_windows::resolve(model);
    crate::context_windows::window_for_lookup(resolved.lookup, model, resolved.template_window)
}

/// What a conversation may send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// The model's context size.
    pub window: i64,
    /// Where `window` came from.
    pub source: &'static str,
    /// The most the context file plus the message may be.
    pub threshold: i64,
    /// The most the turns kept whole after a new summary may be.
    pub keep: i64,
}

/// The budget for a model, given its instructions' size and how many tools the spec lists.
pub fn budget(model: &str, instructions: i64, tools: i64) -> Budget {
    let (window, source) = window(model);
    let share = settings().compact_at;
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    let room = (window as f64 * share) as i64;
    let threshold = (room - instructions - TOOL_TOKENS * tools).max(MIN_THRESHOLD);
    Budget {
        window,
        source,
        threshold,
        keep: threshold / 2,
    }
}

/// What a conversation needs to know about its spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecFacts {
    /// `model`.
    pub model: String,
    /// The prompt files: `system` and `context`, without the generated one.
    pub prompts: Vec<String>,
    /// How many tools it lists.
    pub tools: i64,
}

/// The `id:` field of a spec yaml, when present.
pub fn spec_id(yaml: &[u8]) -> Option<String> {
    let doc: serde_yaml::Value = serde_yaml::from_slice(yaml).ok()?;
    doc.get("id")
        .and_then(serde_yaml::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Whether a spec names the hosted-lead id.
pub fn is_lead_spec(yaml: &[u8]) -> bool {
    spec_id(yaml).as_deref() == Some(HOSTED_LEAD_SPEC_ID)
}

/// Read a conversation's spec. It must list the generated context file and task, so its turns can carry them.
pub fn read_spec(yaml: &[u8]) -> Result<SpecFacts, String> {
    let doc: serde_yaml::Value =
        serde_yaml::from_slice(yaml).map_err(|e| format!("the spec is not YAML: {e}"))?;
    let text = |k: &str| doc.get(k).and_then(serde_yaml::Value::as_str);
    let list = |k: &str| -> Vec<String> {
        doc.get(k)
            .and_then(serde_yaml::Value::as_sequence)
            .map(|s| {
                s.iter()
                    .filter_map(serde_yaml::Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    };
    let model = text("model")
        .filter(|m| !m.is_empty())
        .ok_or("the spec names no model")?;
    let context = list("context");
    if !context.iter().any(|c| c == CONTEXT_PATH) {
        return Err(format!(
            "a conversation's spec lists {CONTEXT_PATH} in its context"
        ));
    }
    if !list("tasks").iter().any(|t| t == TASK) {
        return Err(format!("a conversation's spec lists {TASK} in its tasks"));
    }
    let prompts = text("system")
        .filter(|s| !s.is_empty())
        .map(String::from)
        .into_iter()
        .chain(context.into_iter().filter(|c| c != CONTEXT_PATH))
        .collect();
    Ok(SpecFacts {
        model: model.to_string(),
        prompts,
        tools: i64::try_from(list("tools").len()).unwrap_or(0),
    })
}

/// A turn that is over, as the thread holds it.
#[derive(Debug, Clone)]
pub struct Past {
    /// Its number in the conversation, from 1.
    pub seq: i64,
    /// The person's message.
    pub message: String,
    /// The reply, when there was one.
    pub reply: Option<String>,
    /// `done`, `error` or `stopped`.
    pub state: String,
}

/// A summary that is done.
#[derive(Debug, Clone)]
pub struct Summary {
    /// Its id.
    pub id: String,
    /// It covers turns 1 to this.
    pub through: i64,
    /// What it says.
    pub text: String,
}

/// The context file: an optional summary, then turns whole.
pub fn render(summary: Option<&Summary>, turns: &[Past]) -> String {
    let mut out = String::from("# Conversation so far\n\n");
    if summary.is_none() && turns.is_empty() {
        out.push_str("This is the first message of the conversation.\n");
        return out;
    }
    out.push_str(
        "The thread of this conversation before the newest message, kept by the Director. The newest message is \
         the task.\n",
    );
    if let Some(s) = summary {
        let _ = write!(
            out,
            "\n## Summary of turns 1-{}\n\n{}\n",
            s.through,
            s.text.trim_end()
        );
    }
    for t in turns {
        let _ = write!(
            out,
            "\n## Turn {}: the person\n\n{}\n",
            t.seq,
            t.message.trim_end()
        );
        let reply = match (&t.reply, t.state.as_str()) {
            (Some(r), _) => r.trim_end().to_string(),
            (None, "stopped") => "_(no reply: the turn was stopped)_".into(),
            (None, _) => "_(no reply: the turn ended in error)_".into(),
        };
        let _ = write!(out, "\n## Turn {}: the reply\n\n{reply}\n", t.seq);
    }
    out
}

/// The goal of a summary job covering turns 1 to `through`.
pub fn summary_goal(through: i64) -> String {
    format!(
        "Summarise turns 1-{through} of the conversation above, so it can continue without them. Keep what was \
         decided, facts and figures, names, open questions and anything the person asked to be remembered. Answer \
         with the summary only.\n"
    )
}

/// What a turn sends, decided when it becomes eligible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// Send the summary named (if any) and every turn after it whole.
    Send {
        /// The summary sent, if any.
        summary: Option<String>,
        /// Whether this is over the threshold because no smaller form exists (nothing is dropped).
        over: bool,
    },
    /// Make a summary of turns 1 to `through` first, from `based_on` (if any) and the turns after it.
    Summarise {
        /// The last turn it covers.
        through: i64,
        /// The summary it builds on.
        based_on: Option<String>,
    },
}

fn after(turns: &[Past], through: i64) -> &[Past] {
    let from = turns.partition_point(|t| t.seq <= through);
    &turns[from..]
}

/// Decide what a turn sends. `turns` are the turns before it, in order; `best` the newest summary that is done;
/// `failed` the portion a summary was last tried for and failed (so it is not tried again on its own).
pub fn choose(
    turns: &[Past],
    message: &str,
    best: Option<&Summary>,
    budget: &Budget,
    failed: Option<i64>,
) -> Choice {
    let size = |s: Option<&Summary>, t: &[Past]| tokens(&render(s, t)) + tokens(message);
    if size(None, turns) <= budget.threshold {
        return Choice::Send {
            summary: None,
            over: false,
        };
    }
    let since = best.map_or(turns, |s| after(turns, s.through));
    if best.is_some() && size(best, since) <= budget.threshold {
        return Choice::Send {
            summary: best.map(|s| s.id.clone()),
            over: false,
        };
    }
    // Keep the newest turns that fit in half the threshold; summarise the rest.
    let mut kept = turns.len();
    while kept > 0 && tokens(&render(None, &turns[kept - 1..])) <= budget.keep {
        kept -= 1;
    }
    let through = match kept.checked_sub(1).map(|i| turns[i].seq) {
        Some(t) if best.is_none_or(|s| s.through < t) && failed != Some(t) => t,
        _ => {
            return Choice::Send {
                summary: best.map(|s| s.id.clone()),
                over: true,
            };
        }
    };
    Choice::Summarise {
        through,
        based_on: best.map(|s| s.id.clone()),
    }
}

/// A run's reply: the last model reply with text in its `messages.jsonl`.
pub fn reply_of(messages: &[u8]) -> Option<String> {
    String::from_utf8_lossy(messages)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| e.get("event").and_then(Value::as_str) == Some("turn"))
        .filter_map(|e| e.get("text").and_then(Value::as_str).map(String::from))
        .rfind(|t| !t.trim().is_empty())
}

#[cfg(test)]
mod lead_model_tests {
    #[test]
    fn rust_and_python_lead_models_match() {
        let Some(repo) = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent() else {
            panic!("manifest parent missing");
        };
        let Ok(out) = std::process::Command::new("python3")
            .arg("-c")
            .arg("import sys; sys.path.insert(0, 'cs/scripts'); import cs_lead; print(' '.join(cs_lead.LEAD_MODELS))")
            .current_dir(repo)
            .output() else {
            panic!("python3 cs_lead failed to run");
        };
        assert!(out.status.success());
        let Ok(stdout) = String::from_utf8(out.stdout) else {
            panic!("python stdout not utf-8");
        };
        assert_eq!(stdout.trim(), super::LEAD_MODELS.join(" "));
        assert!(!super::lead_model_ok("claude-code-opus-judge"));
        assert!(!super::lead_model_ok("claude-code-sonnet-planner"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn past(seq: i64, words: usize) -> Past {
        Past {
            seq,
            message: format!("question {seq} {}", "x".repeat(words)),
            reply: Some(format!("answer {seq} {}", "y".repeat(words))),
            state: "done".into(),
        }
    }

    fn small(threshold: i64) -> Budget {
        Budget {
            window: 0,
            source: "test",
            threshold,
            keep: threshold / 2,
        }
    }

    #[test]
    fn spec_must_name_the_generated_files() {
        let good = b"id: c\nmodel: claude-x\nsystem: prompts/s.md\ncontext: [prompts/c.md, conversation/context.md]\n\
                     tools: [fs_read, shell]\ntasks: [tasks/turn]\n";
        let facts = read_spec(good).map_err(|e| e.clone());
        assert_eq!(
            facts,
            Ok(SpecFacts {
                model: "claude-x".into(),
                prompts: vec!["prompts/s.md".into(), "prompts/c.md".into()],
                tools: 2
            })
        );
        assert!(read_spec(b"model: m\ncontext: []\ntasks: [tasks/turn]\n").is_err());
        assert!(
            read_spec(b"model: m\ncontext: [conversation/context.md]\ntasks: [tasks/t]\n").is_err()
        );
        assert!(read_spec(b"context: [conversation/context.md]\ntasks: [tasks/turn]\n").is_err());
        assert!(read_spec(b": : :").is_err());
    }

    #[test]
    fn windows_come_from_settings_then_the_table() {
        assert_eq!(window("claude-sonnet-4-5"), (200_000, "table"));
        assert_eq!(
            window("somebody-else"),
            (crate::context_windows::DEFAULT_WINDOW, "default")
        );
        let b = budget("local-coder", 1000, 2);
        assert_eq!(b.threshold, 19_660 - 1000 - 1000);
        assert_eq!(b.keep, b.threshold / 2);
        assert_eq!(budget("tiny", 1_000_000, 0).threshold, MIN_THRESHOLD);
    }

    #[test]
    fn context_windows_for_listed_models_and_aliases() {
        const CASES: &[(&str, i64)] = &[
            ("claude-opus-5-5", 200_000),
            ("claude-sonnet-5-5", 200_000),
            ("claude-haiku-5-5", 200_000),
            ("gpt-6.1-sol", 272_000),
            ("gpt-5.6-sol", 272_000),
            ("gpt-5.6-sol-high", 272_000),
            ("gpt-5.3-codex-high", 200_000),
            ("gpt-5.3-codex-spark", 200_000),
            ("grok-4.7", 131_072),
            ("grok-4.7-high", 131_072),
            ("composer-2.5", 200_000),
            ("kimi-k3", 128_000),
            ("kimi-k3.1-pro", 256_000),
            ("glm-5.3", 128_000),
            ("deepseek-v4-pro", 128_000),
            ("qwen3.7-plus", 128_000),
            ("cursor-sol", 272_000),
            ("cursor-codex", 200_000),
            ("cursor-composer", 200_000),
            ("cursor-grok", 131_072),
            ("cursor-sonnet", 200_000),
            ("cursor-agent", 128_000),
            ("pi", 128_000),
            ("goose", 128_000),
            ("grok", 131_072),
            ("opencode-go", 128_000),
            ("antigravity", 1_000_000),
            ("antigravity-hydrogen", 1_000_000),
            ("claude-code", 200_000),
            ("claude-code-sonnet", 200_000),
            ("claude-code-opus", 200_000),
            ("codex", 200_000),
            ("gemini-cli", 1_000_000),
            ("sonnet", 200_000),
            ("opus", 200_000),
        ];
        for &(name, want) in CASES {
            let (got, source) = window(name);
            assert_eq!(got, want, "window for {name}");
            assert_eq!(source, "table", "source for {name}");
        }
    }

    #[test]
    fn settings_override_template_resolution() {
        configure(Settings {
            windows: vec![("cursor-composer".into(), 50_000)],
            compact_at: COMPACT_AT,
        });
        assert_eq!(window("cursor-composer"), (50_000, "setting"));
        configure(Settings {
            windows: Vec::new(),
            compact_at: COMPACT_AT,
        });
    }

    #[test]
    fn a_25k_token_thread_on_a_large_model_is_not_summarised() {
        let payload = "word ".repeat(25_000);
        let turns = vec![Past {
            seq: 1,
            message: payload,
            reply: Some("short reply".into()),
            state: "done".into(),
        }];
        let b = budget("claude-opus-5-5", 0, 0);
        let whole = tokens(&render(None, &turns)) + tokens("follow-up");
        assert!(
            whole <= b.threshold,
            "25k tokens ({whole}) should be under threshold {}",
            b.threshold
        );
        assert_eq!(
            choose(&turns, "follow-up", None, &b, None),
            Choice::Send {
                summary: None,
                over: false
            }
        );
        let b = budget("cursor-sol", 0, 0);
        assert!(
            whole <= b.threshold,
            "cursor-sol should resolve to 272k Sol family"
        );
    }

    #[test]
    fn render_marks_turns_without_replies() {
        assert!(render(None, &[]).contains("first message"));
        let mut t = past(1, 0);
        t.reply = None;
        t.state = "stopped".into();
        let s = Summary {
            id: "s".into(),
            through: 3,
            text: "earlier".into(),
        };
        let text = render(Some(&s), &[t]);
        assert!(text.contains("## Summary of turns 1-3\n\nearlier"));
        assert!(text.contains("## Turn 1: the person") && text.contains("the turn was stopped"));
    }

    #[test]
    fn whole_thread_until_the_threshold() {
        let turns: Vec<Past> = (1..=4).map(|s| past(s, 40)).collect();
        let whole = tokens(&render(None, &turns)) + tokens("next");
        assert_eq!(
            choose(&turns, "next", None, &small(whole), None),
            Choice::Send {
                summary: None,
                over: false
            }
        );
        let Choice::Summarise { through, based_on } =
            choose(&turns, "next", None, &small(whole - 1), None)
        else {
            panic!("over the threshold: summarise")
        };
        assert!((1..4).contains(&through) && based_on.is_none());
    }

    #[test]
    fn a_summary_is_used_then_built_on() {
        let turns: Vec<Past> = (1..=6).map(|s| past(s, 40)).collect();
        let s = Summary {
            id: "sum1".into(),
            through: 3,
            text: "short".into(),
        };
        let fits = tokens(&render(Some(&s), &turns[3..])) + tokens("m");
        assert_eq!(
            choose(&turns, "m", Some(&s), &small(fits), None),
            Choice::Send {
                summary: Some("sum1".into()),
                over: false
            }
        );
        match choose(&turns, "m", Some(&s), &small(fits - 1), None) {
            Choice::Summarise { through, based_on } => {
                assert!(through > 3);
                assert_eq!(based_on.as_deref(), Some("sum1"));
            }
            other @ Choice::Send { .. } => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_failed_portion_is_sent_whole_not_dropped() {
        let turns: Vec<Past> = (1..=4).map(|s| past(s, 40)).collect();
        let b = small(10);
        let Choice::Summarise { through, .. } = choose(&turns, "m", None, &b, None) else {
            panic!("summarise")
        };
        assert_eq!(
            choose(&turns, "m", None, &b, Some(through)),
            Choice::Send {
                summary: None,
                over: true
            }
        );
    }

    #[test]
    fn reply_is_the_last_turn_text() {
        let m = b"{\"event\":\"header\"}\n{\"event\":\"turn\",\"turn\":1,\"text\":\"first\"}\n\
                  {\"event\":\"tool\"}\n{\"event\":\"turn\",\"turn\":2,\"text\":\"final\"}\n\
                  {\"event\":\"turn\",\"turn\":3,\"text\":\"  \"}\n{torn";
        assert_eq!(reply_of(m).as_deref(), Some("final"));
        assert_eq!(reply_of(b"{\"event\":\"header\"}\n"), None);
    }
}
