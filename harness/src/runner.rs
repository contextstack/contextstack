//! One-cell loop; existing operations retain workspace, evaluation, publication and ledger ownership.
use crate::{
    gateway::{CEILING_STOP, Gateway, Message, Partial, RawArguments, ToolCall},
    process::{self, Capture, Control},
    spec::Spec,
};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Default, Deserialize)]
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
    pub error: Option<String>,
}

impl ResultRow {
    fn count_usage(&mut self, usage: [Option<i64>; 2]) {
        for (total, reported) in self.tokens.iter_mut().zip(usage) {
            *total = if self.replies == 0 {
                reported
            } else {
                total.zip(reported).map(|(sum, n)| sum.saturating_add(n))
            };
        }
        self.replies += 1;
    }
}

struct Palette {
    tools: BTreeMap<String, Tool>,
    schemas: Option<Vec<Value>>,
}

#[derive(Default)]
struct Events {
    seq: u64,
}

impl Events {
    fn write(&mut self, dir: &Path, turn: i64, partial: &Partial<'_>) -> Result<(), String> {
        let (flag, delta) = match partial {
            Partial::Delta(delta) => (true, *delta),
            Partial::Done | Partial::Discard => (false, ""),
        };
        let mut payload = json!({"turn": turn, "partial": flag, "delta": delta});
        if matches!(partial, Partial::Discard) {
            payload["discard"] = json!(true);
        }
        self.seq += 1;
        let event = json!({"seq": self.seq, "kind": "model.output", "payload": payload});
        append(&dir.join("events.jsonl"), event)
    }
}

fn append(path: &Path, event: impl Serialize) -> Result<(), String> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let line = event_line(event).map_err(|e| format!("{name}: {e}"))?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| {
            let offset = file.metadata()?.len();
            file.write_all(line.as_bytes()).inspect_err(|_| {
                let _ = file.set_len(offset);
            })
        })
        .map_err(|e| format!("{name}: {e}"))
}

struct Cell<'a> {
    spec: &'a Spec,
    root: &'a Path,
    task: &'a str,
    trial: i64,
    dir: PathBuf,
    workspace: PathBuf,
    stimulus: Value,
    model: String,
    row: ResultRow,
    events: Events,
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}
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

// The finalizer uses splitlines(), which also splits on Unicode separators.
fn event_line(event: impl Serialize) -> Result<String, serde_json::Error> {
    let mut line = serde_json::to_string(&event)?;
    line.retain(|c| c != '\r' && c != '\n');
    line = line
        .replace('\u{85}', r"\u0085")
        .replace('\u{2028}', r"\u2028")
        .replace('\u{2029}', r"\u2029");
    line.push('\n');
    Ok(line)
}

async fn op(
    root: &Path,
    name: &str,
    input: Value,
    control: Option<&Control>,
) -> (Value, Option<String>) {
    match process::command(
        &root.join("ops/bin").join(name),
        &[],
        root,
        input.to_string().as_bytes(),
        &[],
        control,
        Some(Duration::from_secs(120)),
        Capture::Stdout,
    )
    .await
    {
        Ok(output) => {
            let raw = String::from_utf8_lossy(&output.bytes);
            let parsed = serde_json::from_str::<Value>(raw.trim().lines().last().unwrap_or(""))
                .map_err(|e| e.to_string()).and_then(|value| {
                    if value.is_object() || value.is_null() { Ok(value) } else {
                        let kind = match value { Value::Array(_) => "array", Value::String(_) => "string", Value::Bool(_) => "bool", _ => "number" };
                        Err(format!("json: cannot unmarshal {kind} into Go value of type map[string]interface {{}}"))
                    }
                });
            let error = output
                .error
                .map(|e| format!("{name}: {e}; inspect the run and retry finalization"))
                .or_else(|| parsed.as_ref().err().map(ToString::to_string));
            (parsed.unwrap_or(Value::Null), error)
        }
        Err(error) => (Value::Null, Some(format!("{name}: {error}"))),
    }
}

async fn hook(
    cell: &Cell<'_>,
    name: &str,
    input: &[u8],
    tool: Option<&str>,
    control: &Control,
) -> Result<(String, i32), String> {
    let check = cell.dir.join("stimulus/check");
    let mut env = vec![("LAB_CHECK".into(), check.to_string_lossy().into_owned())];
    env.extend(tool.map(|name| ("LAB_TOOL".into(), name.into())));
    let capture = if name == "step-confirm" {
        Capture::Foreground
    } else {
        Capture::Text
    };
    match process::command(
        &cell.root.join("hooks/bin").join(name),
        &[],
        &cell.workspace,
        input,
        &env,
        Some(control),
        Some(Duration::from_secs(120)),
        capture,
    )
    .await
    {
        Ok(output) if output.code == -1 && !control.cancel.is_cancelled() => Err(format!(
            "hook {name}: watchdog expired; child killed; retry finalization"
        )),
        Ok(output) => Ok((process::clip(&output.bytes), output.code)),
        Err(error) => Err(format!("hook {name}: {error}")),
    }
}

async fn pre_tool(
    cell: &Cell<'_>,
    invocation: &ToolCall,
    control: &Control,
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
        let (out, code) = hook(cell, name, &input, Some(&invocation.name), control).await?;
        if code != 0 {
            return Ok((format!("pre_tool {name} refused: {out}"), code));
        }
    }
    Ok((String::new(), 0))
}

async fn tool(
    cell: &Cell<'_>,
    name: &str,
    entry: &Tool,
    args: &[u8],
    control: &Control,
) -> (String, bool, Option<Value>) {
    let bin = if entry.bin.is_empty() {
        cell.root.join("tools/bin").join(name)
    } else {
        cell.root.parent().unwrap_or(cell.root).join(&entry.bin)
    };
    let prefix = process::env("CS_TOOL_PREFIX").unwrap_or_default();
    let bin = bin.to_string_lossy();
    let words: Vec<&str> = prefix
        .split_whitespace()
        .chain([bin.as_ref()])
        .chain(entry.args.iter().map(String::as_str))
        .collect();
    let timeout_secs = entry.timeout_seconds.max(60);
    let deadline = seconds(timeout_secs);
    let budget = [("LAB_TOOL_TIMEOUT_SECONDS".into(), timeout_secs.to_string())];
    let output = process::command(
        Path::new(words[0]),
        &words[1..],
        &cell.workspace,
        args,
        &budget,
        Some(control),
        Some(deadline),
        if entry.image_out {
            Capture::Image
        } else {
            Capture::Text
        },
    )
    .await;
    let output = match output {
        Ok(output) => output,
        Err(error) => return (format!("\nexit: {error}"), true, None),
    };
    let content = process::clip(&output.bytes);
    if let Some(error) = output.error {
        return (format!("{content}\nexit: {error}"), true, None);
    }
    if !entry.image_out {
        return (content, false, None);
    }
    let observation: Value = serde_json::from_slice(&output.bytes).unwrap_or(Value::Null);
    let image = &observation["image"];
    let media_type = text(image, "media_type");
    let base64 = text(image, "base64");
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

impl Cell<'_> {
    fn emit(&self, event: impl Serialize) -> Result<(), String> {
        append(&self.dir.join("messages.jsonl"), event)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Keep frozen input preparation and its wire fields in execution order."
    )]
    async fn prepare(
        &mut self,
        endpoint: &str,
        control: &Control,
    ) -> Result<(Palette, Vec<Message>), String> {
        let sp = self.spec;
        fs::create_dir_all(&self.workspace).map_err(|e| e.to_string())?;
        let mut tools = BTreeMap::new();
        let mut schemas = None;
        for name in &sp.tools {
            let path = self.root.join("tools/bin").join(format!("{name}.json"));
            let bytes = fs::read(&path).map_err(|e| {
                format!(
                    "tool {name:?}: open {}: {} — file pair in tools/bin",
                    path.display(),
                    process::io_error(&e)
                )
            })?;
            let entry = serde_json::from_slice::<Option<Tool>>(&bytes)
                .map_err(|e| format!("tool {name:?}: {e} — file pair in tools/bin"))?
                .unwrap_or_default();
            schemas.get_or_insert_with(Vec::new).push(json!({
                "name": name,
                "description": entry.description,
                "schema": entry.input_schema.as_ref().unwrap_or(&entry.schema)
            }));
            tools.insert(name.clone(), entry);
        }
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
                Some(control),
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
                "run_id": self.dir.file_name().unwrap_or_default().to_string_lossy(),
                "task": self.task,
                "spec": sp.path,
                "spec_sha": sp.sha,
                "trial": self.trial,
                "adapter": "runner",
                "workspace": self.workspace,
                "prompts": prompts,
                "tools": sp.tools,
                "hooks": hooks,
                // capture-stimulus hashes this binary and the wrapper into the frozen identity, and
                // records which gateway serves this endpoint.
                "gateway_endpoint": endpoint,
                "runner_implementation": "rust",
                "runner_path": process::env("CS_RUNNER_PATH").unwrap_or_else(|| std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default())
            }),
            Some(control),
        )
        .await;
        self.stimulus = stimulus;
        if let Some(error) = error {
            return Err(error);
        }
        let read_frozen = |name| {
            fs::read_to_string(self.dir.join("stimulus").join(name))
                .map_err(|e| format!("frozen stimulus unreadable: {e}"))
        };
        let goal = read_frozen("goal.md")?;
        let system = read_frozen("system.txt")?;
        let messages = vec![
            Message::System { content: system },
            Message::User { content: goal },
        ];
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
        Ok((Palette { tools, schemas }, messages))
    }

    async fn post_turn(&self, control: &Control) -> Result<String, String> {
        let mut feedback = String::new();
        for name in &self.spec.post_turn {
            if control.reason().is_some() {
                break;
            }
            let (output, code) = hook(self, name, &[], None, control).await?;
            if code != 0 {
                if output.is_empty() {
                    feedback.push_str("post_turn ");
                    feedback.push_str(name);
                    feedback.push_str(" failed");
                } else {
                    feedback.push_str(&output);
                }
            }
        }
        Ok(feedback)
    }

    fn record_tool(
        &self,
        messages: &mut Vec<Message>,
        call: &ToolCall,
        turn: i64,
        content: &str,
        is_error: bool,
        image: Option<Value>,
    ) -> Result<(), String> {
        #[derive(Serialize)]
        struct ToolEvent<'a> {
            // The finalizer slices the original argument token from the first field.
            args: &'a RawArguments,
            event: &'static str,
            turn: i64,
            name: &'a str,
            error: bool,
            content: &'a str,
        }
        let mark = if is_error { " ERROR" } else { "" };
        let header = format!("[{} {}]{mark}\n{content}", call.name, call.args.get());
        let body = match image {
            Some(image) => json!([{"type": "text", "text": header}, image]),
            None => Value::String(header),
        };
        messages.push(Message::Tool {
            content: body,
            tool_call_id: call.id.clone(),
            is_error,
        });
        self.emit(ToolEvent {
            args: &call.args,
            event: "tool",
            turn,
            name: &call.name,
            error: is_error,
            content,
        })
    }

    async fn execute(
        &mut self,
        gateway: &Gateway,
        control: &mut Control,
    ) -> Result<String, String> {
        const EARLY_WALL: i64 = 86_400 * 366;
        let sp = self.spec;
        let now = tokio::time::Instant::now();
        let wall = (sp.budget.seconds > 0).then(|| {
            let d = seconds(sp.budget.seconds);
            if d.as_nanos() > i64::MAX as u128 {
                now
            } else {
                now.checked_add(d).unwrap_or(now)
            }
        });
        if sp.budget.seconds > 0 && sp.budget.seconds <= EARLY_WALL {
            control.deadline = wall;
        }
        let (palette, mut messages) = self.prepare(&gateway.endpoint, control).await?;
        if sp.budget.seconds > EARLY_WALL {
            control.deadline = wall;
        }
        gateway.probe(control).await;
        for turn in 1..=sp.budget.turns {
            self.row.turns = turn;
            let ceiling = self.ceiling();
            let (dir, events) = (&self.dir, &mut self.events);
            let mut sink = |partial: Partial<'_>| events.write(dir, turn, &partial);
            let schemas = palette.schemas.as_deref();
            let response = match gateway
                .call(&messages, schemas, ceiling, control, &mut sink)
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    let refused = error
                        .strip_prefix(CEILING_STOP)
                        .and_then(|e| e.strip_prefix(": "));
                    let error = refused.unwrap_or(&error);
                    let message = match control.reason() {
                        Some("budget:seconds") => "budget:seconds",
                        Some(reason) if reason != "interrupted" => reason,
                        _ => error,
                    };
                    self.emit(json!({"event": "error", "turn": turn, "error": message}))?;
                    if refused.is_some() {
                        return Ok(CEILING_STOP.into());
                    }
                    return Ok(control
                        .reason()
                        .map_or_else(|| format!("error: {error}"), str::to_owned));
                }
            };
            if !response.model.is_empty() {
                self.model = response.model;
            }
            self.row.estimated += response.est_usd;
            self.row.cost += response.charged_usd;
            self.row.class = response.cost_class;
            self.row.count_usage(response.usage);
            self.emit(json!({"event": "turn", "turn": turn, "text": response.text}))?;
            let mut observations = Vec::new();
            if sp.budget.usd > 0.0 && self.usd_spent() >= sp.budget.usd {
                for call in &response.tool_calls {
                    self.emit(json!({
                        "event": "tool_skipped",
                        "turn": turn,
                        "name": call.name,
                        "reason": "budget:usd",
                    }))?;
                }
                return Ok("budget:usd".into());
            }
            for call in &response.tool_calls {
                if let Some(stop) = self
                    .run_tool(&palette, &mut observations, call, turn, control)
                    .await?
                {
                    return Ok(stop.into());
                }
            }
            let used_tools = !response.tool_calls.is_empty();
            messages.push(Message::Assistant {
                content: response.text,
                tool_calls: response.tool_calls,
            });
            messages.extend(observations);
            let feedback = self.post_turn(control).await?;
            let keep = !feedback.is_empty();
            if keep {
                self.emit(json!({"event": "post_turn", "turn": turn, "output": feedback}))?;
                messages.push(Message::User { content: feedback });
            }
            if let Some(reason) = control.reason() {
                return Ok(reason.into());
            }
            if !keep && (!used_tools || !sp.post_turn.is_empty()) {
                return Ok("done".into());
            }
        }
        Ok("budget:turns".into())
    }

    fn usd_spent(&self) -> f64 {
        self.row.cost.max(self.row.estimated)
    }

    fn ceiling(&self) -> Option<f64> {
        let usd = self.spec.budget.usd;
        (usd > 0.0).then(|| (usd - self.usd_spent()).max(0.0))
    }

    async fn run_tool(
        &mut self,
        palette: &Palette,
        messages: &mut Vec<Message>,
        call: &ToolCall,
        turn: i64,
        control: &Control,
    ) -> Result<Option<&'static str>, String> {
        let (gate, code) = pre_tool(self, call, control).await?;
        if let Some(reason) = control.reason() {
            return Ok(Some(reason));
        }
        if code == 2 {
            self.emit(json!({"event": "pre_tool", "turn": turn, "output": gate}))?;
            return Ok(Some("aborted"));
        }
        let (content, is_error, image) = if code != 0 {
            (gate, true, None)
        } else if let Some(entry) = palette.tools.get(&call.name) {
            tool(self, &call.name, entry, call.args.get().as_bytes(), control).await
        } else {
            let error = format!("tool {:?} is not in this spec's palette", call.name);
            (error, true, None)
        };
        self.row.tool_calls += 1;
        self.record_tool(messages, call, turn, &content, is_error, image)?;
        Ok(control.reason().or_else(|| {
            let cap = self.spec.budget.tool_calls;
            (cap > 0 && self.row.tool_calls >= cap).then_some("budget:tool_calls")
        }))
    }
}

fn state_roots(root: &Path) -> (PathBuf, PathBuf) {
    let read = |name| {
        process::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let state = read("LAB_STATE_ROOT").unwrap_or_else(|| root.join("ledger"));
    let workspaces = read("LAB_WORKSPACES_ROOT").unwrap_or_else(|| state.join("runs"));
    (state, workspaces)
}

// Reserve atomically: wall clocks can repeat between simultaneous processes.
fn reserve_run(
    runs: &Path,
    sp: &Spec,
    task: &str,
    trial: i64,
    mut stamp: chrono::DateTime<Utc>,
) -> Result<PathBuf, String> {
    fs::create_dir_all(runs).map_err(|e| e.to_string())?;
    let task = Path::new(task)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    loop {
        let time = stamp.format("%Y%m%d-%H%M%S.%9f");
        let id = format!("{time}-{}-{task}-t{trial}", sp.id);
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

/// Execute one cell and hand durable terminal state to the unchanged finalizer, including on failure.
#[allow(clippy::too_many_lines)]
pub async fn run(
    sp: &Spec,
    task: &str,
    trial: i64,
    root: &Path,
    gateway: &Gateway,
    mut control: Control,
) -> ResultRow {
    let started = Utc::now();
    let (state, workspaces) = state_roots(root);
    let dir = match reserve_run(&state.join("runs"), sp, task, trial, started) {
        Ok(dir) => dir,
        Err(error) => {
            return ResultRow {
                verdict: "void".into(),
                error: Some(error),
                ..ResultRow::default()
            };
        }
    };
    let mut cell = Cell {
        spec: sp,
        root,
        task,
        trial,
        workspace: workspaces
            .join(dir.file_name().unwrap_or_default())
            .join("workspace"),
        dir,
        stimulus: Value::Null,
        model: sp.model.clone(),
        row: ResultRow::default(),
        events: Events::default(),
    };
    let stop = cell
        .execute(gateway, &mut control)
        .await
        .unwrap_or_else(|error| {
            let stop = format!("error: {error}");
            cell.row.error = Some(error);
            stop
        });
    cell.row.stop = if control.cancel.is_cancelled() {
        "interrupted".into()
    } else {
        stop
    };
    if cell.row.stop == "interrupted" || cell.row.stop.starts_with("error:") {
        cell.row.verdict = "void".into();
    }
    let terminal_state = match cell.row.stop.as_str() {
        _ if control.cancel.is_cancelled() => "cancelled",
        stop if stop.starts_with("error:") => "error",
        "budget:seconds" => "timeout",
        _ => "completed",
    };
    let ended = Utc::now();
    let id = cell.dir.file_name().unwrap_or_default().to_string_lossy();
    let mut state = json!({
        "run_id": id,
        "runner_implementation": "rust",
        "runner_build": cell.stimulus["runner_build"],
        "run_dir": cell.dir,
        "transcript": format!("ledger/runs/{id}"),
        "workspace": cell.workspace,
        "stimulus_dir": cell.dir.join("stimulus"),
        "spec": sp.id,
        "spec_sha": sp.sha,
        "spec_path": sp.path,
        "task": task,
        "trial": trial,
        "task_sha": text(&cell.stimulus, "task_sha"),
        "env_sha": text(&cell.stimulus, "env_sha"),
        "stimulus_sha": text(&cell.stimulus, "stimulus_sha"),
        "model": cell.model,
        "stop": cell.row.stop,
        "verdict": cell.row.verdict,
        "turns": cell.row.turns,
        "tool_calls": cell.row.tool_calls,
        "cost_usd": wire_float(cell.row.cost),
        "est_usd": wire_float(cell.row.estimated),
        "est_basis": "rate",
        "cost_class": cell.row.class,
        "input_tokens": cell.row.tokens[0],
        "output_tokens": cell.row.tokens[1],
        "terminal_state": terminal_state,
        "cancel_reason": (terminal_state == "cancelled").then(|| control.cause.get().map_or("cancelled", |c| c)),
        "started": started.to_rfc3339_opts(SecondsFormat::Nanos, true),
        "ended": ended.to_rfc3339_opts(SecondsFormat::Nanos, true),
        "seconds": wire_float((ended-started).to_std().unwrap_or_default().as_secs_f64()),
        "check_window": sp.check_window.max(60)
    });
    if let Some(intent) = &sp.intent {
        state["intent"] = json!(intent);
    }
    let temp = cell.dir.join("terminal.json.tmp");
    let write = fs::create_dir_all(&cell.dir)
        .and_then(|()| fs::write(&temp, state.to_string()))
        .and_then(|()| fs::rename(temp, cell.dir.join("terminal.json")));
    let trusted_hung = cell.row.error.as_deref().is_some_and(|e| {
        ["capture-stimulus:", "setup-workspace:", "hook "]
            .iter()
            .any(|p| e.starts_with(p))
            && (e.contains("deadline") || e.contains("signal:") || e.contains("watchdog"))
    });
    let (finalized, error) = if trusted_hung {
        (Value::Null, None)
    } else {
        op(root, "finalize-run", json!({"run_dir": cell.dir}), None).await
    };
    if !text(&finalized, "verdict").is_empty() {
        cell.row.verdict = text(&finalized, "verdict").into();
    }
    if !text(&finalized, "stop").is_empty() {
        cell.row.stop = text(&finalized, "stop").into();
    }
    cell.row.pending = trusted_hung
        || write.is_err()
        || error.is_some()
        || text(&finalized, "state") != "finalized";
    if cell.row.pending {
        eprintln!("lab: finalization pending: {}", cell.dir.display());
    }
    // The run's own failure (a refused capture-stimulus, say) is the cause; a finalize error that follows it
    // must not replace it in the row or in the line the person reads.
    if let Err(error) = write {
        cell.row.error.get_or_insert_with(|| error.to_string());
    } else if let Some(err) = error {
        cell.row.error.get_or_insert(err);
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
            stimulus: Value::Null,
            model: String::new(),
            row: ResultRow::default(),
            events: Events::default(),
        };
        let control = Control::default();
        assert!(hook(&cell, "missing", &[], None, &control).await.is_err());
        assert!(cell.post_turn(&control).await.is_err());
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
                stimulus: Value::Null,
                model: String::new(),
                row: ResultRow {
                    cost,
                    estimated,
                    ..ResultRow::default()
                },
                events: Events::default(),
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
        let first = reserve_run(scratch.path(), &spec, "task", 1, stamp)?;
        let second = reserve_run(scratch.path(), &spec, "task", 1, stamp)?;
        assert_ne!(first, second);
        assert!(first.is_dir() && second.is_dir());
        Ok(())
    }
    #[test]
    fn event_line_strips_unicode_line_separators() -> Result<(), serde_json::Error> {
        let line = event_line(json!({"text": "a\u{2028}b"}))?;
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
    fn state_roots_honor_lab_overlay_values() -> Result<(), String> {
        let scratch = tempfile::tempdir().map_err(|e| e.to_string())?;
        let _env = process::ScopedEnv::set([
            (
                "LAB_STATE_ROOT".to_string(),
                "/tmp/custom-state".to_string(),
            ),
            (
                "LAB_WORKSPACES_ROOT".to_string(),
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
        append(&path, json!({"ok": true}))?;
        let before = fs::read(&path).map_err(|e| e.to_string())?;
        let bad = scratch.path().join("blocked");
        fs::write(&bad, b"x").map_err(|e| e.to_string())?;
        let _ = append(&bad, json!({"bad": true}));
        let after = fs::read(&path).map_err(|e| e.to_string())?;
        assert_eq!(before, after);
        Ok(())
    }
}
