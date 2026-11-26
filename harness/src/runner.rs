//! One-cell loop; existing operations retain workspace, evaluation, publication and ledger ownership.
use crate::{
    deliverable::FileStamp,
    gateway::{
        CEILING_STOP, CallFailure, CallTrace, Gateway, Message, Partial, RawArguments, ToolCall,
    },
    process::{self, Capture, Control},
    spec::Spec,
    trace::{self, Clock, Span, Trace, Transcript},
};
use chrono::Utc;
mod receipt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Default, Deserialize)]
#[serde(default)]
struct Tool {
    #[serde(deserialize_with = "crate::spec::default_on_null")]
    description: String,
    #[serde(rename = "inputSchema", deserialize_with = "present")]
    input_schema: Option<Value>,
    schema: Value,
    #[serde(deserialize_with = "crate::spec::default_on_null")]
    bin: String,
    #[serde(deserialize_with = "crate::spec::default_on_null")]
    args: Vec<String>,
    #[serde(deserialize_with = "crate::spec::default_on_null")]
    image_out: bool,
    #[serde(deserialize_with = "crate::spec::default_on_null")]
    timeout_seconds: i64,
}

fn present<'de, D: serde::Deserializer<'de>>(value: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(value).map(Some)
}

#[derive(Default)]
pub struct ResultRow {
    pub verdict: String,
    pub stop: String,
    pub turns: i64,
    pub tool_calls: i64,
    pub cost: f64,
    pub estimated: f64,
    pub class: String,
    pub tokens: [Option<i64>; 2],
    pub replies: i64,
    pub pending: bool,
    pub execution: Option<(PathBuf, fs::File)>,
    pub error: Option<String>,
}

impl ResultRow {
    fn void(error: impl Into<String>) -> Self {
        Self {
            verdict: "void".into(),
            error: Some(error.into()),
            ..Self::default()
        }
    }

    fn count_usage(&mut self, usage: [Option<i64>; 2]) {
        if self.replies == 0 {
            self.tokens = usage;
        } else {
            for (total, reported) in self.tokens.iter_mut().zip(usage) {
                *total = total.zip(reported).map(|(sum, n)| sum.saturating_add(n));
            }
        }
        self.replies += 1;
    }
}

#[derive(Clone)]
struct Palette {
    tools: BTreeMap<String, Tool>,
    schemas: Vec<Value>,
    restriction: Option<&'static str>,
}

impl Palette {
    fn load(root: &Path, names: &[String]) -> Result<Self, String> {
        let mut tools = BTreeMap::new();
        let mut schemas = Vec::new();
        for name in names {
            let entry = if name == "run_check" {
                Tool {
                    description: "Run the frozen job check in the workspace and return its exit code and output tail.".into(),
                    schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
                    ..Tool::default()
                }
            } else {
                Self::load_tool(root, name)?
            };
            schemas.push(json!({
                "name": name,
                "description": entry.description,
                "schema": entry.input_schema.as_ref().unwrap_or(&entry.schema)
            }));
            tools.insert(name.clone(), entry);
        }
        Ok(Self {
            tools,
            schemas,
            restriction: None,
        })
    }

    fn load_tool(root: &Path, name: &str) -> Result<Tool, String> {
        let path = root.join(format!("tools/bin/{name}.json"));
        let descriptor_error = |detail| format!("tool {name:?}: {detail} — file pair in tools/bin");
        let bytes = fs::read(&path).map_err(|e| {
            let path = path.display();
            let error = process::io_error(&e);
            descriptor_error(format!("open {path}: {error}"))
        })?;
        serde_json::from_slice::<Option<Tool>>(&bytes)
            .map(Option::unwrap_or_default)
            .map_err(|e| descriptor_error(e.to_string()))
    }

    fn offer(&self, final_turn: bool) -> crate::gateway::ToolOffer<'_> {
        let schemas = (!self.schemas.is_empty()).then_some(self.schemas.as_slice());
        let final_write = final_turn && self.has_write_tools();
        let choice = match schemas {
            Some([tool]) if final_write => {
                Some(json!({"type": "function", "function": {"name": tool["name"]}}))
            }
            Some(_) if final_write => Some(json!("required")),
            _ => None,
        };
        crate::gateway::ToolOffer { schemas, choice }
    }

    fn has_write_tools(&self) -> bool {
        self.tools.keys().any(|name| is_write_tool(name))
    }

    fn restrict(&mut self, keep: fn(&str) -> bool, reason: &'static str) {
        self.restriction = Some(reason);
        self.tools.retain(|name, _| keep(name));
        self.schemas
            .retain(|schema| self.tools.contains_key(text(schema, "name")));
    }

    fn pause_reads(&self) -> Self {
        let mut paused = self.clone();
        let keep = |name: &str| !READ_TOOLS.split(' ').any(|read| read == name);
        paused.restrict(keep, "read tools are paused for one turn");
        paused
    }

    fn unknown_tool(&self, name: &str) -> String {
        let error = format!("tool {name:?} is not in this spec's palette");
        let available: Vec<_> = self.tools.keys().map(String::as_str).collect();
        let Some(closest) = available
            .iter()
            .min_by_key(|tool| edit_distance(name, tool))
        else {
            return format!("{error}\nAvailable tools: (none)");
        };
        let hint = if matches!(name, "list" | "list_dir") && self.tools.contains_key("fs_read") {
            "fs_read on a directory"
        } else {
            closest
        };
        format!(
            "{error}\nAvailable tools: {}\nDid you mean {hint}?",
            available.join(", ")
        )
    }
}

/// The tools `pause_reads` withholds for one turn.
const READ_TOOLS: &str =
    "fs_read fs_list repo_search search web_search web_fetch browser_fetch defuddle_extract";

fn is_write_tool(name: &str) -> bool {
    matches!(name, "fs_write" | "fs_edit")
}

fn edit_distance(name: &str, tool: &str) -> usize {
    let mut row: Vec<_> = (0..=tool.len()).collect();
    for (i, left) in name.bytes().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, right) in tool.bytes().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (diagonal + usize::from(left != right)).min(1 + above.min(row[j]));
            diagonal = above;
        }
    }
    row[tool.len()]
}

fn stream_event(dir: &Path, turn: i64, seq: u64, partial: &Partial<'_>) -> Result<(), String> {
    let (live, delta) = match partial {
        Partial::Delta(delta) => (true, *delta),
        _ => (false, ""),
    };
    let mut payload = json!({"turn": turn, "partial": live, "delta": delta});
    if matches!(partial, Partial::Discard) {
        payload["discard"] = json!(true);
    }
    let event = json!({"seq": seq, "kind": "model.output", "payload": payload});
    trace::append(
        dir,
        "events.jsonl",
        &trace::event_line(event).map_err(|e| e.to_string())?,
        false,
    )
}

#[derive(Serialize)]
struct Prompt<'a> {
    event: &'static str,
    turn: i64,
    from: usize,
    turn_guidance: &'a str,
    messages: &'a [Message],
}

#[derive(Serialize)]
struct Reply<'a> {
    event: &'static str,
    turn: i64,
    text: &'a str,
    tool_calls: &'a [ToolCall],
}

#[derive(Default, PartialEq)]
enum EditNudge {
    #[default]
    Pending,
    Shown,
    Escalated,
}

#[derive(Default)]
struct Guidance {
    check_ran: bool,
    check_passed: Option<bool>,
    edited: bool,
    text_only: u8,
    plan_requested: bool,
    nudge: EditNudge,
    calls_since_hint: u64,
}

const FINAL_TURN_WRITES: &str = "read tools are unavailable on the final turn";
const CONTINUE_CODING: &str = "You replied without using a tool. Continue: make your planned edit with fs_edit, then run_check.";

impl Guidance {
    fn record_call(&mut self, name: &str) {
        if is_write_tool(name) {
            self.calls_since_hint = 0;
            self.nudge = EditNudge::Pending;
        } else {
            self.calls_since_hint += 1;
        }
    }
}

#[derive(Default)]
struct RunMetrics {
    settlements: Vec<Value>,
    provider_reported_usd: f64,
    tool_errors: BTreeMap<String, u64>,
}

struct Cell<'a> {
    spec: &'a Spec,
    root: &'a Path,
    task: &'a str,
    trial: i64,
    dir: PathBuf,
    workspace: PathBuf,
    repo_view: Option<&'a Value>,
    stimulus: Value,
    model: String,
    row: ResultRow,
    seq: u64,
    messages: Transcript,
    trace: &'a Trace<'a>,
    metrics: RunMetrics,
    write_succeeded: bool,
    final_turn_retry: bool,
    guidance: Guidance,
    deliverables: Vec<(String, Option<FileStamp>)>,
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}
const ERROR_STOP: &str = "error: ";

fn seconds(value: i64) -> Duration {
    Duration::from_secs(value.unsigned_abs())
}

// The finalizer repeats terminal cost numbers with their original spelling.
fn wire_float(value: f64) -> Value {
    let number = json!(value);
    number
        .to_string()
        .strip_suffix(".0")
        .and_then(|n| n.parse::<i64>().ok())
        .map_or(number, |n| json!(n))
}

fn wall_deadline(seconds_budget: i64) -> (Option<tokio::time::Instant>, bool) {
    const EARLY_WALL: i64 = 86_400 * 366;
    let now = tokio::time::Instant::now();
    let wall = (seconds_budget > 0).then(|| {
        let d = seconds(seconds_budget);
        if d.as_nanos() > i64::MAX as u128 {
            now
        } else {
            now.checked_add(d).unwrap_or(now)
        }
    });
    (wall, seconds_budget > EARLY_WALL)
}

/// Trusted operations' watchdog; hooks are killed after the spec's `check_window` instead.
const WATCHDOG: Option<Duration> = Some(Duration::from_secs(120));

async fn op(root: &Path, name: &str, input: Value, control: &Control) -> (Value, Option<String>) {
    let bin = root.join("ops/bin").join(name);
    let output = process::command(
        &bin,
        &[],
        root,
        input.to_string().as_bytes(),
        &[],
        Some(control),
        WATCHDOG,
        Capture::Stdout,
    )
    .await;
    let output = match output {
        Ok(output) => output,
        Err(error) => return (Value::Null, Some(format!("{name}: {error}"))),
    };
    let raw = String::from_utf8_lossy(&output.bytes);
    let parsed = serde_json::from_str(raw.trim().lines().last().unwrap_or(""))
        .map_err(|e| e.to_string())
        .and_then(op_object);
    // A deadline or cancel is the run's stop, not a fault that finalization could retry.
    let error = output.error.map(|e| match control.reason() {
        Some(_) => format!("{name}: {e}"),
        None => format!("{name}: {e}; inspect the run and retry finalization"),
    });
    match parsed {
        Ok(value) => (value, error),
        Err(parse) => (Value::Null, error.or(Some(parse))),
    }
}

fn op_object(value: Value) -> Result<Value, String> {
    let kind = match value {
        Value::Null | Value::Object(_) => return Ok(value),
        Value::Array(_) => "array",
        Value::String(_) => "string",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
    };
    Err(format!(
        "json: cannot unmarshal {kind} into Go value of type map[string]interface {{}}"
    ))
}

async fn hook(
    cell: &Cell<'_>,
    name: &str,
    input: &[u8],
    tool: Option<&str>,
    control: &Control,
    parent: &str,
) -> Result<(String, i32), String> {
    let check = cell.dir.join("stimulus/check");
    let mut env = vec![("CS_CHECK".into(), check.to_string_lossy().into_owned())];
    env.extend(tool.map(|name| ("CS_TOOL".into(), name.into())));
    let capture = match name {
        "step-confirm" => Capture::Foreground,
        _ => Capture::Text,
    };
    let kind = tool.map_or("post_turn_hook", |_| "pre_tool_hook");
    let bin = cell.root.join("hooks/bin").join(name);
    let mut span = cell.trace.span(kind, name, parent);
    let result = process::command(
        &bin,
        &[],
        &cell.workspace,
        input,
        &env,
        Some(control),
        Some(seconds(cell.spec.check_window.unwrap_or(60))),
        capture,
    )
    .await;
    span.stop();
    let output = match result {
        Ok(output) => output,
        Err(error) => {
            span.close("error")?;
            return Err(format!("hook {name}: {error}"));
        }
    };
    let content = process::clip(&output.bytes);
    let pointer = cell.emit(json!({"event": "hook_output", "output": content}));
    span.close_with_content(trace::outcome(output.code == 0), "output", &pointer)?;
    pointer?;
    if let Some(reason) = control.reason() {
        return Err(reason.into());
    }
    if output.timed_out {
        return Err(format!(
            "hook {name}: watchdog expired; child killed; retry finalization"
        ));
    }
    if output.code == -1 {
        let error = output.error.as_deref().unwrap_or("process failed");
        return Err(format!("hook {name}: {error}"));
    }
    Ok((content, output.code))
}

async fn pre_tool(
    cell: &Cell<'_>,
    invocation: &ToolCall,
    control: &Control,
    parent: &str,
) -> Result<(String, i32), String> {
    #[derive(Serialize)]
    struct HookInput<'a> {
        args: &'a RawArguments,
        name: &'a str,
    }
    let input = serde_json::to_vec(&HookInput {
        args: &invocation.args,
        name: &invocation.name,
    })
    .map_err(|e| format!("pre_tool input: {e}"))?;
    for name in &cell.spec.pre_tool {
        let (out, code) = hook(cell, name, &input, Some(&invocation.name), control, parent).await?;
        if code != 0 {
            return Ok((format!("pre_tool {name} refused: {out}"), code));
        }
    }
    Ok((String::new(), 0))
}

async fn tool(
    cell: &Cell<'_>,
    invocation: &ToolCall,
    entry: &Tool,
    control: &Control,
    span: &mut Span<'_>,
) -> (String, bool, Option<Value>) {
    let name = &invocation.name;
    let is_check = name == "run_check";
    let bin = if is_check {
        cell.dir.join("stimulus/check")
    } else if entry.bin.is_empty() {
        cell.root.join("tools/bin").join(name)
    } else {
        cell.root.parent().unwrap_or(cell.root).join(&entry.bin)
    };
    let prefix = process::env("CS_TOOL_PREFIX").unwrap_or_default();
    let bin = bin.to_string_lossy();
    let mut words: Vec<&str> = prefix
        .split_whitespace()
        .chain([bin.as_ref()])
        .chain(entry.args.iter().map(String::as_str))
        .collect();
    let timeout_secs = if is_check {
        cell.spec.check_window.unwrap_or(60)
    } else {
        entry.timeout_seconds.max(60)
    };
    let mut budget = vec![("CS_TOOL_TIMEOUT_SECONDS".into(), timeout_secs.to_string())];
    let (binding, run_dir, view);
    let lane_binding = process::env("CS_HARNESS_SANDBOX");
    if let Some(toolchain) = process::env("CS_HARNESS_TOOLCHAIN_ENV")
        .and_then(|value| serde_json::from_str::<BTreeMap<String, String>>(&value).ok())
    {
        budget.extend(toolchain);
    }
    if name == "request_action" {
        run_dir = cell.dir.to_string_lossy();
        view = cell.repo_view.map(Value::to_string).unwrap_or_default();
        words = vec![bin.as_ref(), &run_dir, &cell.model, &view];
    } else if let Some(bound) = cell.repo_view.map(Value::to_string).or(lane_binding) {
        // Read-only views and writable lanes alike must reach cs-sandbox; never run unbound.
        let Some(separator) = prefix
            .split_whitespace()
            .position(|word| word == "--")
            .filter(|&i| i > 0)
        else {
            return ("repo views require cs-sandbox".into(), true, None);
        };
        binding = bound;
        words.splice(separator..separator, ["--repo-view", &binding]);
        if let Some(view) = cell.repo_view {
            budget.push(("CS_REPO_VIEW".into(), text(view, "path").to_owned()));
        }
    }
    let capture = if is_check {
        Capture::Tail
    } else if entry.image_out {
        Capture::Image
    } else {
        Capture::Text
    };
    let output = process::command(
        Path::new(words[0]),
        &words[1..],
        &cell.workspace,
        if is_check {
            &[]
        } else {
            invocation.args.get().as_bytes()
        },
        &budget,
        Some(control),
        Some(seconds(timeout_secs)),
        capture,
    )
    .await;
    span.stop();
    if is_check {
        return check_observation(output, timeout_secs);
    }
    tool_observation(name, entry.image_out, output, span)
}

fn tool_observation(
    name: &str,
    image_out: bool,
    output: Result<process::Output, String>,
    span: &mut Span<'_>,
) -> (String, bool, Option<Value>) {
    let (bytes, content, error) = match output {
        Ok(output) => {
            let content = process::clip(&output.bytes);
            if name == "request_action" {
                span.data["action_approval"] =
                    serde_json::from_str(content.lines().next().unwrap_or_default())
                        .unwrap_or_default();
            }
            (output.bytes, content, output.error)
        }
        Err(error) => (Vec::new(), String::new(), Some(error)),
    };
    if let Some(error) = error {
        return (format!("{content}\nexit: {error}"), true, None);
    }
    if !image_out {
        return (content, false, None);
    }
    let observation: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let media_type = text(&observation["image"], "media_type");
    let base64 = text(&observation["image"], "base64");
    if (!observation["text"].is_null() && !observation["text"].is_string())
        || media_type.is_empty()
        || base64.is_empty()
    {
        let error = format!("{name}: image_out expects {{text, image{{media_type, base64}}}}");
        return (error, true, None);
    }
    let image = json!({"type": "image", "media_type": media_type, "base64": base64});
    let caption = process::clip(text(&observation, "text").as_bytes());
    (caption, false, Some(image))
}

fn check_observation(
    output: Result<process::Output, String>,
    timeout_secs: i64,
) -> (String, bool, Option<Value>) {
    let output = match output {
        Ok(output) => output,
        Err(error) => return (format!("Cannot run the frozen check: {error}"), true, None),
    };
    let status = if output.timed_out {
        format!("Check timed out after {timeout_secs}s.")
    } else {
        format!("Check exit code: {}.", output.code)
    };
    let tail = String::from_utf8_lossy(&output.bytes);
    (
        format!("{status} Output tail:\n{tail}"),
        output.timed_out || output.code != 0,
        None,
    )
}

impl Cell<'_> {
    fn emit(&self, event: impl Serialize) -> Result<Value, String> {
        self.messages.append(&self.dir, event)
    }

    async fn prepare(
        &mut self,
        endpoint: &str,
        control: &Control,
    ) -> Result<(Palette, Vec<Message>), String> {
        let sp = self.spec;
        fs::create_dir_all(&self.workspace).map_err(|e| e.to_string())?;
        let mut names = sp.tools.clone();
        let repo_view = self.repo_view.is_some();
        if repo_view && !names.iter().any(|name| name == "repo_search") {
            names.push("repo_search".into());
        }
        let palette = Palette::load(self.root, &names)?;
        for (kind, names) in [("post_turn", &sp.post_turn), ("pre_tool", &sp.pre_tool)] {
            for name in names {
                if !self.root.join("hooks/bin").join(name).exists() {
                    return Err(format!("{kind} {name:?}: not in hooks/bin"));
                }
            }
        }
        let setup = Path::new(self.task).join("setup");
        if setup.is_dir()
            && let (_, Some(error)) = op(
                self.root,
                "setup-workspace",
                json!({"setup": setup, "workspace": self.workspace, "task": self.task}),
                control,
            )
            .await
        {
            return Err(error);
        }
        let prompts: Vec<_> = std::iter::once(&sp.system).chain(&sp.context).collect();
        let hooks: Vec<_> = sp.post_turn.iter().chain(&sp.pre_tool).collect();
        let (stimulus, error) = op(
            self.root,
            "capture-stimulus",
            json!({
                "run_dir": self.dir,
                "run_id": self.trace.run,
                "task": self.task,
                "spec": sp.path,
                "spec_sha": sp.sha,
                "trial": self.trial,
                "adapter": "runner",
                "workspace": self.workspace,
                "prompts": prompts,
                "tools": names,
                "hooks": hooks,
                // capture-stimulus hashes this binary and the wrapper into the frozen identity, and
                // records which gateway serves this endpoint.
                "gateway_endpoint": endpoint,
                "runner_implementation": "rust",
                "runner_path": process::env("CS_RUNNER_PATH").unwrap_or_else(|| std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default())
            }),
            control,
        )
        .await;
        self.stimulus = stimulus;
        if let Some(error) = error {
            return Err(error);
        }
        let messages = self.frozen_messages()?;
        let mut budget = json!(sp.budget);
        budget["usd"] = wire_float(sp.budget.usd);
        self.emit(json!({
            "event": "header",
            "spec": sp.id,
            "task": self.task,
            "trial": self.trial,
            "model": sp.model,
            "budget": budget
        }))?;
        Ok((palette, messages))
    }

    fn frozen_messages(&mut self) -> Result<Vec<Message>, String> {
        let read_frozen = |name| {
            fs::read_to_string(self.dir.join("stimulus").join(name))
                .map_err(|e| format!("frozen stimulus unreadable: {e}"))
        };
        let mut goal = read_frozen("goal.md")?;
        let check = read_frozen("check").unwrap_or_default();
        self.deliverables = crate::deliverable::named_files(&goal, &check)
            .into_iter()
            .map(|path| {
                let before = crate::deliverable::stamp(&self.workspace, path);
                (path.to_owned(), before)
            })
            .collect();
        if self.dir.join("stimulus/context/decision-log.md").is_file() {
            let markdown = read_frozen("context/decision-log.md")?;
            goal.push_str("\n\n---\n\nJob context (`context/decision-log.md`):\n\n");
            goal.push_str(&markdown);
        }
        if let Some(view) = self.repo_view {
            goal = format!(
                "Read the repository snapshot at {}. It is read-only. Use repo_search to find files or text, and use absolute snapshot paths with fs_read or exec. Write your answer in the current workspace.\n\n{goal}",
                text(view, "path")
            );
        }
        let system = read_frozen("system.txt")?;
        Ok(vec![
            Message::System { content: system },
            Message::User { content: goal },
        ])
    }

    async fn post_turn(&self, control: &Control, parent: &str) -> Result<String, String> {
        let mut feedback = String::new();
        for name in &self.spec.post_turn {
            if control.reason().is_some() {
                break;
            }
            let (output, code) = hook(self, name, &[], None, control, parent).await?;
            if code != 0 {
                let text = if output.is_empty() {
                    format!("post_turn {name} failed")
                } else {
                    output
                };
                feedback.push_str(&text);
            }
        }
        Ok(feedback)
    }

    fn record_tool(
        &self,
        messages: &mut Vec<Message>,
        call: &ToolCall,
        result: (String, bool, Option<Value>),
    ) -> Result<Value, String> {
        #[derive(Serialize)]
        struct ToolEvent<'a> {
            // The finalizer slices the original argument token from the first field.
            args: &'a RawArguments,
            event: &'static str,
            turn: i64,
            name: &'a str,
            error: bool,
            content: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            image: Option<&'a Value>,
        }
        let (content, is_error, image) = result;
        let pointer = self.emit(ToolEvent {
            args: &call.args,
            event: "tool",
            turn: self.row.turns,
            name: &call.name,
            error: is_error,
            content: &content,
            image: image.as_ref(),
        });
        let mark = if is_error { " ERROR" } else { "" };
        let header = format!("[{} {}]{mark}\n{content}", call.name, call.args.get());
        let body = match image {
            Some(image) => json!([{"type": "text", "text": header}, image]),
            None => json!(header),
        };
        messages.push(Message::Tool {
            content: body,
            tool_call_id: call.id.clone(),
            is_error,
        });
        pointer
    }

    fn call_failure(&mut self, failure: CallFailure, control: &Control) -> Result<String, String> {
        if !failure.cost_class.is_empty() {
            self.row.class = failure.cost_class;
        }
        let refused = failure
            .message
            .strip_prefix(CEILING_STOP)
            .and_then(|rest| rest.strip_prefix(": "));
        let error = refused.unwrap_or(&failure.message);
        let stop = control.reason();
        let message = stop.filter(|s| *s != "interrupted").unwrap_or(error);
        self.emit(json!({"event": "error", "turn": self.row.turns, "error": message}))?;
        Ok(if refused.is_some() {
            CEILING_STOP.into()
        } else {
            stop.map_or_else(|| format!("{ERROR_STOP}{error}"), str::to_owned)
        })
    }

    fn guidance(
        &mut self,
        palette: &Palette,
        workspace: &Path,
        turn: i64,
        remaining: i64,
    ) -> (String, bool) {
        const EDIT_NUDGE_INTERVAL: u64 = 12;
        const PRODUCE: &str = "Produce the deliverable requested in the task goal now, then stop.";
        let named = !self.deliverables.is_empty();
        let writable = palette.has_write_tools();
        let has_check = palette.tools.contains_key("run_check");
        let coding = !named && writable;
        let finish_coding = !named && (self.spec.lane.is_some() || has_check);
        let mut content = match remaining {
            0 if writable => "Read tools are unavailable. You must now write the deliverable using what you already have.".into(),
            0 => PRODUCE.into(),
            _ => format!("Turns remaining (including this turn): {remaining}."),
        };
        if remaining == 1 && finish_coding {
            content
                .push_str("\nFinish the coding task and summarize the changes and verification.");
        } else if (1..=2).contains(&remaining) && !finish_coding {
            content.push('\n');
            content.push_str(PRODUCE);
        }
        if turn == 1 || remaining <= 1 {
            content = format!(
                "{content}\nYour workspace (write deliverables here, relative paths resolve here): {}",
                workspace.display()
            );
        }
        let state = &mut self.guidance;
        if state.text_only > 0 {
            content = format!("{content}\n{CONTINUE_CODING}");
        }
        // The Director freezes a passing placeholder when the lead supplies no check.
        if turn == 1 && coding && has_check
            && fs::read_to_string(self.dir.join("stimulus/check")).is_ok_and(|check| {
                !check.trim().is_empty()
                    && check != "#!/bin/sh\n# no check: the lead gave none, so every run passes\nexit 0\n"
            })
        {
            content.push_str("\nStart with run_check to see the failure.");
        } else if remaining == 1 && finish_coding && has_check && !state.check_ran {
            content.push_str("\nUse run_check if you still need to verify the change.");
        }
        let plan = coding && !state.plan_requested && (state.check_ran || turn == 6);
        if plan {
            state.plan_requested = true;
            content.push_str("\nState your plan in a few lines: which files must change and why, and the first edit you will make.");
        }
        let mut pause = false;
        if coding && state.calls_since_hint >= EDIT_NUDGE_INTERVAL {
            state.calls_since_hint = 0;
            if state.nudge == EditNudge::Shown {
                state.nudge = EditNudge::Escalated;
                pause = true;
                content.push_str("\nRead tools are paused for one turn: make your first planned edit now, then run_check.");
            } else {
                if state.nudge == EditNudge::Pending {
                    state.nudge = EditNudge::Shown;
                }
                content.push_str("\nYou've read a lot. State your hypothesis in one sentence, make the smallest edit that tests it, then run_check.");
            }
        }
        (content, pause)
    }

    async fn execute(
        &mut self,
        gateway: &Gateway,
        control: &mut Control,
    ) -> Result<String, String> {
        let sp = self.spec;
        let (wall, late_wall) = wall_deadline(sp.budget.seconds);
        if !late_wall {
            control.deadline = wall.or(control.deadline);
        }
        let (mut palette, mut messages) = self.prepare(&gateway.endpoint, control).await?;
        if late_wall {
            control.deadline = wall;
        }
        gateway.probe(control).await;
        let workspace = std::path::absolute(&self.workspace).map_err(|e| e.to_string())?;
        let has_deliverable = !self.deliverables.is_empty();
        let coding = !has_deliverable && palette.has_write_tools();
        let has_check = palette.tools.contains_key("run_check") || !sp.post_turn.is_empty();
        let mut from = 0;
        for turn in (1..=sp.budget.turns).chain(
            sp.budget
                .turns
                .checked_add(1)
                .filter(|_| has_deliverable && sp.budget.turns > 0),
        ) {
            if let Some(reason) = control.reason() {
                return Ok(reason.into());
            }
            let remaining = sp.budget.turns - (turn - 1);
            if remaining == 1 && has_deliverable && palette.has_write_tools() {
                palette.restrict(is_write_tool, FINAL_TURN_WRITES);
            }
            self.final_turn_retry = remaining == 0;
            let (content, pause) = self.guidance(&palette, &workspace, turn, remaining);
            let paused = pause.then(|| palette.pause_reads());
            let prompt = self.emit(Prompt {
                event: "prompt",
                turn,
                from,
                turn_guidance: &content,
                messages: &messages[from..],
            })?;
            from = messages.len();
            let Message::System { content: system } = &messages[0] else {
                return Err("missing system message".into());
            };
            let content = format!("{system}\n\n{content}");
            let original = std::mem::replace(&mut messages[0], Message::System { content });
            let mut span = self.trace.span("turn", "turn", "run");
            span.data = json!({"turn": turn, "content": {"prompt": prompt}});
            self.row.turns = turn;
            let active = paused.as_ref().unwrap_or(&palette);
            let result = self.turn(gateway, control, active, &mut messages, &mut span);
            let result = result.await;
            messages[0] = original;
            span.close(trace::outcome(result.is_ok()))?;
            let stop = result?;
            let used_tools = matches!(
                messages.get(from),
                Some(Message::Assistant { tool_calls, .. }) if !tool_calls.is_empty()
            );
            if matches!(stop.as_deref(), None | Some("done")) {
                if has_deliverable && remaining <= 1 {
                    if self.deliverable_finished(stop.is_some()) {
                        return Ok("done".into());
                    }
                    match remaining {
                        1 if used_tools => continue,
                        1 if stop.is_some() => return Ok("done".into()),
                        _ => break,
                    }
                }
                if coding && self.continue_coding(used_tools, has_check, remaining)? {
                    continue;
                }
                if self.guidance.text_only == 3 {
                    return Ok("done".into());
                }
            }
            if let Some(stop) = stop {
                return Ok(stop);
            }
        }
        Ok("budget:turns".into())
    }

    fn continue_coding(
        &mut self,
        used_tools: bool,
        has_check: bool,
        remaining: i64,
    ) -> Result<bool, String> {
        let state = &mut self.guidance;
        if used_tools || (state.edited && (!has_check || state.check_passed == Some(true))) {
            state.text_only = 0;
            return Ok(false);
        }
        state.text_only += 1;
        let retry = state.text_only < 3 && remaining > 1;
        if retry {
            self.emit(
                json!({"event": "guidance", "turn": self.row.turns, "content": CONTINUE_CODING}),
            )?;
        }
        Ok(retry)
    }

    fn deliverable_finished(&self, stopped: bool) -> bool {
        (stopped && !self.spec.post_turn.is_empty())
            || self.write_succeeded
            || self.deliverables.iter().any(|(path, before)| {
                crate::deliverable::stamp(&self.workspace, path)
                    .is_some_and(|after| Some(after) != *before)
            })
    }

    async fn turn(
        &mut self,
        gateway: &Gateway,
        control: &Control,
        palette: &Palette,
        messages: &mut Vec<Message>,
        turn_span: &mut Span<'_>,
    ) -> Result<Option<String>, String> {
        let parent = &turn_span.id;
        let turn = self.row.turns;
        let prompt = &turn_span.data["content"]["prompt"];
        let call = CallTrace::new(&self.trace.run, self.trace.job.as_deref(), turn);
        let mut span = self.trace.span("model_call", "model_call", parent);
        // The gateway can derive its exchange parent directly from the wire request ID.
        span.id.clone_from(&call.id);
        let ceiling = self.ceiling();
        let (dir, seq) = (&self.dir, &mut self.seq);
        let mut sink = |partial: Partial<'_>| {
            *seq += 1;
            stream_event(dir, turn, *seq, &partial)
        };
        let final_turn = turn >= self.spec.budget.turns && !self.deliverables.is_empty();
        let offer = palette.offer(final_turn);
        let response = gateway.call(messages, offer, ceiling, control, &mut sink, &call);
        let response = response.await;
        span.stop();
        let sends = call.sends();
        let (charged, reported) = match &response {
            Ok(reply) => (reply.charged_usd, reply.provider_reported_usd),
            Err(error) => (error.charged_usd, error.provider_reported_usd),
        };
        self.row.cost += charged;
        self.metrics.provider_reported_usd += reported.unwrap_or_default();
        if charged != 0.0 {
            self.metrics
                .settlements
                .push(json!({"request_id": call.id, "send_index": sends.len()}));
        }
        span.data = json!({"request_id": call.id, "sends": sends, "content": {"prompt": prompt}});
        if response.is_err() && charged != 0.0 {
            // A call that ended without completing settled partially (tracing CONTRACT rule 3).
            span.data["charged_usd"] = json!(charged);
            span.data["partial"] = json!(true);
        }
        let (response, reply_pointer) = match response {
            Ok(reply) => {
                let pointer = self.emit(Reply {
                    event: "turn",
                    turn,
                    text: &reply.text,
                    tool_calls: &reply.tool_calls,
                });
                span.close_with_content("completed", "reply", &pointer)?;
                (reply, pointer?)
            }
            Err(failure) => {
                span.close("error")?;
                return self.call_failure(failure, control).map(Some);
            }
        };
        if !response.model.is_empty() {
            self.model = response.model;
        }
        self.row.estimated += response.est_usd;
        self.row.class = response.cost_class;
        self.row.count_usage(response.usage);
        let mut observations = Vec::new();
        if let Some(reason) = self
            .run_tools(
                palette,
                &mut observations,
                &response.tool_calls,
                control,
                parent,
                &reply_pointer,
            )
            .await?
        {
            return Ok(Some(reason.into()));
        }
        let used_tools = !response.tool_calls.is_empty();
        messages.push(Message::assistant(response.text, response.tool_calls));
        messages.extend(observations);
        let feedback = self.post_turn(control, parent).await;
        if let Some(reason) = control.reason() {
            return Ok(Some(reason.into()));
        }
        let feedback = feedback?;
        let has_feedback = !feedback.is_empty();
        if !self.spec.post_turn.is_empty() {
            self.guidance.check_passed = Some(!has_feedback);
        }
        if has_feedback {
            self.emit(json!({"event": "post_turn", "turn": turn, "output": feedback}))?;
            messages.push(Message::User { content: feedback });
        }
        Ok(control
            .reason()
            .or_else(|| {
                (!has_feedback && (!used_tools || !self.spec.post_turn.is_empty()))
                    .then_some("done")
            })
            .map(str::to_owned))
    }

    fn ceiling(&self) -> Option<f64> {
        let usd = self.spec.budget.usd;
        let spent = self.row.cost.max(self.row.estimated);
        (usd > 0.0).then(|| (usd - spent).max(0.0))
    }

    async fn run_tools(
        &mut self,
        palette: &Palette,
        observations: &mut Vec<Message>,
        calls: &[ToolCall],
        control: &Control,
        parent: &str,
        reply_pointer: &Value,
    ) -> Result<Option<&'static str>, String> {
        let mut stop = Ok((self.ceiling() == Some(0.0)).then_some("budget:usd"));
        for call in calls {
            let name = call.name.as_str();
            let name = if name.is_empty() { "tool" } else { name };
            let mut tool_span = self.trace.span("tool_call", name, parent);
            tool_span.data["content"]["arguments"] = reply_pointer.clone();
            if matches!(stop, Ok(None)) {
                stop = self
                    .run_tool(palette, observations, call, control, tool_span)
                    .await;
                continue;
            }
            let outcome = match stop {
                Ok(Some(reason)) if reason.starts_with("budget:") => "skipped_for_budget",
                _ => "cancelled",
            };
            tool_span.close(outcome)?;
            if matches!(stop, Ok(Some("budget:usd"))) {
                self.emit(
                    json!({"event": "tool_skipped", "turn": self.row.turns, "name": call.name, "reason": "budget:usd"}),
                )?;
            }
        }
        stop
    }

    async fn run_tool(
        &mut self,
        palette: &Palette,
        messages: &mut Vec<Message>,
        call: &ToolCall,
        control: &Control,
        mut span: Span<'_>,
    ) -> Result<Option<&'static str>, String> {
        let turn = self.row.turns;
        let entry = palette.tools.get(&call.name);
        let final_turn = turn >= self.spec.budget.turns && !self.deliverables.is_empty();
        let withheld = palette
            .restriction
            .filter(|_| entry.is_none() && self.spec.tools.contains(&call.name));
        let gate =
            if call.invalid_arguments || withheld.is_some() || (final_turn && entry.is_none()) {
                Ok((String::new(), 0))
            } else {
                pre_tool(self, call, control, &span.id).await
            };
        if let Some(reason) = control.reason() {
            span.close("cancelled")?;
            return Ok(Some(reason));
        }
        let (gate, code) = match gate {
            Ok(gate) => gate,
            Err(error) => {
                span.close("aborted_by_hook")?;
                return Err(error);
            }
        };
        if code == 2 {
            let pointer = self.emit(json!({"event": "pre_tool", "turn": turn, "output": gate}));
            span.close_with_content("aborted_by_hook", "result", &pointer)?;
            return pointer.map(|_| Some("aborted"));
        }
        let (outcome, result, process) = if call.invalid_arguments {
            (
                "invalid_arguments",
                ("Your tool call arguments were not valid JSON. For large files use fs_edit with a smaller change.".into(), true, None),
                None,
            )
        } else if code != 0 {
            ("refused_by_hook", (gate, true, None), None)
        } else if let Some(entry) = entry {
            self.guidance.check_ran |= call.name == "run_check";
            let mut child = self.trace.span("tool_process", &call.name, &span.id);
            let result = tool(self, call, entry, control, &mut child).await;
            ("executed", result, Some(child))
        } else {
            let error = match withheld {
                Some(reason) => format!("tool {:?} was withheld because {reason}", call.name),
                None => palette.unknown_tool(&call.name),
            };
            span.data["error"] = json!(error);
            ("unknown_tool", (error, true, None), None)
        };
        self.row.tool_calls += 1;
        self.guidance.record_call(&call.name);
        let (_, is_error, _) = result;
        if call.name == "run_check" && process.is_some() {
            self.guidance.check_passed = Some(!is_error);
        }
        if is_error {
            *self
                .metrics
                .tool_errors
                .entry(call.name.clone())
                .or_default() += 1;
        } else if is_write_tool(&call.name) {
            self.guidance.edited = true;
            self.write_succeeded |= final_turn;
        }
        let result = self.record_tool(messages, call, result);
        if let Some(child) = process {
            child.close_with_content(trace::outcome(!is_error), "output", &result)?;
        }
        span.close_with_content(outcome, "result", &result)?;
        result?;
        let cap = self.spec.budget.tool_calls;
        Ok(control
            .reason()
            .or((cap > 0 && self.row.tool_calls >= cap).then_some("budget:tool_calls")))
    }
}

pub(crate) fn state_roots(root: &Path) -> (PathBuf, PathBuf) {
    let read = |name| {
        process::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let state = read("CS_STATE_ROOT").unwrap_or_else(|| root.join("ledger"));
    let workspaces = read("CS_WORKSPACES_ROOT").unwrap_or_else(|| state.join("runs"));
    (state, workspaces)
}

// Reserve atomically: wall clocks can repeat between simultaneous processes.
pub(crate) fn reserve_run(
    runs: &Path,
    label: &str,
    task: &str,
    trial: i64,
    mut stamp: chrono::DateTime<Utc>,
) -> Result<PathBuf, String> {
    fs::create_dir_all(runs).map_err(|e| e.to_string())?;
    let task = Path::new(task).file_name().unwrap_or_default();
    let task = task.to_string_lossy();
    loop {
        let time = stamp.format("%Y%m%d-%H%M%S.%9f");
        let id = format!("{time}-{label}-{task}-t{trial}");
        let dir = runs.join(id);
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                stamp += chrono::TimeDelta::nanoseconds(1);
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

/// Execute one cell and persist its receipt, including on failure.
pub async fn run(
    sp: &Spec,
    task: &str,
    trial: i64,
    root: &Path,
    gateway: &Gateway,
    mut control: Control,
) -> ResultRow {
    let clock = Clock::start();
    let lane: Option<Value> =
        process::env("CS_HARNESS_LANE").and_then(|value| serde_json::from_str(&value).ok());
    let bound = match (&sp.repo, &sp.lane, &lane) {
        (None, None, None) => !sp.repo_read_only,
        (Some(repo), name, Some(binding)) => {
            binding["repo"].as_str() == Some(repo)
                && binding["lane"].as_str() == name.as_deref()
                && Path::new(text(binding, "path")).is_absolute()
                && binding["read_only"].as_bool().unwrap_or(false) == sp.repo_read_only
                && name.is_none() == sp.repo_read_only
        }
        _ => false,
    };
    if !bound {
        return ResultRow::void(
            "repo requires a workspace or read-only view bound by the machine launcher",
        );
    }
    let (state, workspaces) = state_roots(root);
    let dir = match reserve_run(&state.join("runs"), &sp.id, task, trial, clock.0) {
        Ok(dir) => dir,
        Err(error) => return ResultRow::void(error),
    };
    let trace = Trace::new(&dir);
    let (repo_view, lane) = if sp.repo_read_only {
        (lane, None)
    } else {
        (None, lane)
    };
    let run_name = dir.file_name().unwrap_or_default();
    let fallback = || workspaces.join(run_name).join("workspace");
    let workspace = lane.as_ref().and_then(|lane| lane["path"].as_str());
    let workspace = workspace.map_or_else(fallback, PathBuf::from);
    let mut cell = Cell {
        spec: sp,
        root,
        task,
        trial,
        workspace,
        repo_view: repo_view.as_ref(),
        dir: dir.clone(),
        stimulus: Value::Null,
        model: sp.model.clone(),
        row: ResultRow::default(),
        seq: 0,
        messages: Transcript::default(),
        trace: &trace,
        metrics: RunMetrics::default(),
        write_succeeded: false,
        final_turn_retry: false,
        guidance: Guidance::default(),
        deliverables: Vec::new(),
    };
    cell.row.stop = cell
        .execute(gateway, &mut control)
        .await
        .unwrap_or_else(|error| {
            // A deadline that expires in setup or capture-stimulus is a timeout, not an error.
            let stop = match control.reason() {
                Some(deadline @ "budget:seconds") => deadline.into(),
                _ => format!("{ERROR_STOP}{error}"),
            };
            cell.row.error = Some(error);
            stop
        });
    let terminal_state = if control.cancel.is_cancelled() {
        cell.row.stop = "interrupted".into();
        "cancelled"
    } else if cell.row.stop.starts_with(ERROR_STOP.trim_end()) {
        "error"
    } else if cell.row.stop == "budget:seconds" {
        "timeout"
    } else {
        "completed"
    };
    if matches!(terminal_state, "cancelled" | "error") {
        cell.row.verdict = "void".into();
    }
    let receipt = cell.receipt(&clock, terminal_state, &control, lane);
    match receipt::write_receipt(&cell.dir, &receipt) {
        Ok(lock) => cell.row.execution = Some((cell.dir, lock)),
        Err(error) => {
            cell.row.pending = true;
            cell.row.error.get_or_insert_with(|| error.to_string());
        }
    }
    cell.row
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unlaunchable_hook_returns_an_actionable_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let scratch = tempfile::tempdir()?;
        let spec = Spec {
            post_turn: vec!["missing".into()],
            ..Spec::default()
        };
        let cell = Cell {
            spec: &spec,
            root: scratch.path(),
            task: "task",
            trial: 1,
            dir: scratch.path().into(),
            workspace: scratch.path().into(),
            repo_view: None,
            stimulus: Value::Null,
            model: String::new(),
            row: ResultRow::default(),
            seq: 0,
            messages: Transcript::default(),
            trace: &Trace::new(scratch.path()),
            metrics: RunMetrics::default(),
            write_succeeded: false,
            final_turn_retry: false,
            guidance: Guidance::default(),
            deliverables: Vec::new(),
        };
        let control = Control::default();
        assert!(
            hook(&cell, "missing", &[], None, &control, "test")
                .await
                .is_err()
        );
        assert!(cell.post_turn(&control, "test").await.is_err());
        Ok(())
    }

    /// A sandbox binding without a `--` in `CS_TOOL_PREFIX` refuses the tool; with one it reaches cs-sandbox.
    #[tokio::test]
    async fn sandbox_bindings_fail_closed_without_a_prefix_separator()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;
        let scratch = tempfile::tempdir()?;
        let root = scratch.path();
        fs::create_dir_all(root.join("tools/bin"))?;
        let script = |path: &Path, body: &str| -> std::io::Result<()> {
            fs::write(path, format!("#!/bin/sh\n{body}\n"))?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        };
        let (ran, argv) = (root.join("ran"), root.join("argv"));
        script(
            &root.join("tools/bin/probe"),
            &format!("touch {}", ran.display()),
        )?;
        let wrap = root.join("wrap");
        script(
            &wrap,
            &format!("echo \"$@\" > {}; shift 3; exec \"$@\"", argv.display()),
        )?;
        let spec = Spec::default();
        let trace = Trace::new(root);
        let view = json!({"path": "/repo"});
        let entry: Tool = serde_json::from_value(json!({"schema": {}}))?;
        let invocation: ToolCall = serde_json::from_value(json!({"name": "probe"}))?;
        for (read_only, prefix, refusal) in [
            (false, "nice -n 0", Some("repo views require cs-sandbox")),
            (true, "nice -n 0", Some("repo views require cs-sandbox")),
            (false, &*format!("{} --", wrap.display()), None),
        ] {
            let _ = fs::remove_file(&ran);
            let mut vars = vec![("CS_TOOL_PREFIX".to_string(), prefix.to_string())];
            if !read_only {
                vars.push(("CS_HARNESS_SANDBOX".into(), "lane-binding".into()));
            }
            let _env = process::ScopedEnv::set(vars);
            let cell = Cell {
                spec: &spec,
                root,
                task: "task",
                trial: 1,
                dir: root.into(),
                workspace: root.into(),
                repo_view: read_only.then_some(&view),
                stimulus: Value::Null,
                model: String::new(),
                row: ResultRow::default(),
                seq: 0,
                messages: Transcript::default(),
                trace: &trace,
                metrics: RunMetrics::default(),
                write_succeeded: false,
                final_turn_retry: false,
                guidance: Guidance::default(),
                deliverables: Vec::new(),
            };
            let mut span = trace.span("tool_call", "probe", "test");
            let (output, is_error, _) =
                tool(&cell, &invocation, &entry, &Control::default(), &mut span).await;
            if let Some(refusal) = refusal {
                assert!(is_error && output.contains(refusal), "{prefix}: {output}");
                assert!(!ran.exists(), "{prefix}: the tool ran without its binding");
            } else {
                assert!(!is_error, "{output}");
                assert!(fs::read_to_string(&argv)?.starts_with("--repo-view lane-binding -- "));
                assert!(ran.exists());
            }
        }
        Ok(())
    }

    #[test]
    fn ceiling_is_the_budget_less_charges_and_none_when_unlimited() {
        let scratch = std::path::Path::new("/nonexistent");
        let mut spec = Spec::default();
        let ceiling = |spec: &Spec, cost: f64, estimated: f64| {
            let cell = Cell {
                spec,
                root: scratch,
                task: "task",
                trial: 1,
                dir: scratch.into(),
                workspace: scratch.into(),
                repo_view: None,
                stimulus: Value::Null,
                model: String::new(),
                row: ResultRow {
                    cost,
                    estimated,
                    ..ResultRow::default()
                },
                seq: 0,
                messages: Transcript::default(),
                trace: &Trace::new(scratch),
                metrics: RunMetrics::default(),
                write_succeeded: false,
                final_turn_retry: false,
                guidance: Guidance::default(),
                deliverables: Vec::new(),
            };
            cell.ceiling()
        };
        assert_eq!(ceiling(&spec, 0.0, 0.0), None, "usd 0 is unlimited");
        spec.budget.usd = 0.5;
        assert_eq!(ceiling(&spec, 0.0, 0.0), Some(0.5));
        assert_eq!(
            ceiling(&spec, 0.125, 0.0),
            Some(0.375),
            "charges shrink the ceiling"
        );
        assert_eq!(ceiling(&spec, 0.75, 0.0), Some(0.0), "never negative");
        assert_eq!(
            ceiling(&spec, 0.125, 0.25),
            Some(0.25),
            "higher estimates shrink the ceiling"
        );
    }

    #[test]
    fn token_totals_are_null_unless_every_reply_reports_them() {
        let mut row = ResultRow::default();
        assert_eq!(row.tokens, [None, None], "no reply: not reported, not zero");
        row.count_usage([Some(10), Some(5)]);
        row.count_usage([Some(7), None]);
        assert_eq!(row.tokens, [Some(17), None]);
        row.count_usage([Some(1), Some(2)]);
        assert_eq!(
            row.tokens,
            [Some(18), None],
            "a missing total stays missing"
        );
        let mut first_missing = ResultRow::default();
        first_missing.count_usage([None, Some(0)]);
        first_missing.count_usage([Some(3), Some(4)]);
        assert_eq!(first_missing.tokens, [None, Some(4)]);
        assert_eq!(first_missing.replies, 2);
    }

    #[test]
    fn descriptors_prefer_input_schema_and_name_an_exe() -> Result<(), serde_json::Error> {
        let wire = |raw: &str| -> Result<Value, serde_json::Error> {
            let tool = serde_json::from_str::<Tool>(raw)?;
            Ok(tool.input_schema.unwrap_or(tool.schema))
        };
        assert_eq!(
            wire(r#"{"inputSchema":{"a":1},"schema":{"b":2}}"#)?,
            json!({"a": 1})
        );
        assert_eq!(wire(r#"{"schema":{"b":2}}"#)?, json!({"b": 2}));
        assert_eq!(
            wire(r#"{"inputSchema":null,"schema":{"b":2}}"#)?,
            Value::Null
        );
        assert_eq!(wire("{}")?, Value::Null);
        let exe: Tool = serde_json::from_str(
            r#"{"runtime":"exe","bin":"target/release/cs-tools","args":["fs_read","two words"]}"#,
        )?;
        assert_eq!(exe.bin, "target/release/cs-tools");
        assert_eq!(exe.args, ["fs_read", "two words"]);
        let nulls: Tool = serde_json::from_str(r#"{"bin":null,"args":null}"#)?;
        assert!(nulls.bin.is_empty() && nulls.args.is_empty());
        Ok(())
    }

    #[test]
    fn nullable_tool_descriptors_keep_go_defaults() -> Result<(), serde_json::Error> {
        for raw in [
            "null",
            r#"{"description":null,"schema":null,"image_out":null,"timeout_seconds":null}"#,
        ] {
            let tool = serde_json::from_str::<Option<Tool>>(raw)?.unwrap_or_default();
            assert!(tool.description.is_empty() && tool.schema.is_null());
            assert!(!tool.image_out);
            assert_eq!(tool.timeout_seconds, 0);
        }
        Ok(())
    }

    #[test]
    fn identical_timestamps_reserve_distinct_run_directories()
    -> Result<(), Box<dyn std::error::Error>> {
        let scratch = tempfile::tempdir()?;
        let spec_path = scratch.path().join("spec.yaml");
        fs::write(
            &spec_path,
            "id: parallel\nmodel: mock\ntasks: [task]\ntrials: 1\nbudget: {turns: 1}\nhypothesis: fixture parity\n",
        )?;
        let spec = crate::spec::load(spec_path.to_str().ok_or("non-UTF8 path")?)?;
        let stamp = Utc::now();
        let first = reserve_run(scratch.path(), &spec.id, "task", 1, stamp)?;
        let second = reserve_run(scratch.path(), &spec.id, "task", 1, stamp)?;
        assert_ne!(first, second);
        assert!(first.is_dir() && second.is_dir());
        Ok(())
    }
    #[test]
    fn event_line_strips_unicode_line_separators() -> Result<(), serde_json::Error> {
        let line = crate::trace::event_line(json!({"text": "a\u{2028}b"}))?;
        assert!(line.contains(r"\u2028"));
        assert!(!line.contains('\u{2028}'));
        Ok(())
    }

    #[test]
    fn wire_float_matches_go_integer_spellings() {
        assert_eq!(wire_float(1.0), json!(1));
        assert_eq!(wire_float(1.5), json!(1.5));
    }

    #[test]
    fn state_roots_honor_overlay_values() -> Result<(), String> {
        let scratch = tempfile::tempdir().map_err(|e| e.to_string())?;
        let _env = process::ScopedEnv::set([
            ("CS_STATE_ROOT".to_string(), "/tmp/custom-state".to_string()),
            (
                "CS_WORKSPACES_ROOT".to_string(),
                "/tmp/custom-workspaces".to_string(),
            ),
        ]);
        let (state, workspaces) = state_roots(scratch.path());
        assert_eq!(state, Path::new("/tmp/custom-state"));
        assert_eq!(workspaces, Path::new("/tmp/custom-workspaces"));
        Ok(())
    }

    #[test]
    fn append_rolls_back_on_failed_writes() -> Result<(), String> {
        let scratch = tempfile::tempdir().map_err(|e| e.to_string())?;
        let path = scratch.path().join("events.jsonl");
        trace::append(scratch.path(), "events.jsonl", "{}\n", false)?;
        let before = fs::read(&path).map_err(|e| e.to_string())?;
        let bad = scratch.path().join("blocked");
        fs::write(&bad, b"x").map_err(|e| e.to_string())?;
        let _ = trace::append(scratch.path(), "blocked/events.jsonl", "{}\n", false);
        let after = fs::read(&path).map_err(|e| e.to_string())?;
        assert_eq!(before, after);
        Ok(())
    }
}
