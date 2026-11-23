use crate::process;
use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
    time::Instant,
};

pub struct Clock(pub chrono::DateTime<Utc>, Instant);

impl Clock {
    pub fn start() -> Self {
        Self(Utc::now(), Instant::now())
    }
    pub fn elapsed_from(&self, start: &Self) -> f64 {
        self.1.duration_since(start.1).as_secs_f64()
    }
}

#[derive(Default)]
pub struct Transcript(Cell<u64>);

impl Transcript {
    pub fn append(&self, dir: &Path, event: impl Serialize) -> Result<Value, String> {
        let line = event_line(event).map_err(|e| format!("messages.jsonl: {e}"))?;
        append(dir, "messages.jsonl", &line, false)?;
        self.0.set(self.0.get() + 1);
        let hash = Sha256::digest(line.trim_end_matches('\n').as_bytes());
        Ok(json!({"file": "messages.jsonl", "line": self.0.get(), "sha256": hex::encode(hash)}))
    }
}

// The finalizer splits on Unicode line separators, including inside JSON strings.
pub fn event_line(event: impl Serialize) -> Result<String, serde_json::Error> {
    Ok(serde_json::to_string(&event)?
        .replace(['\r', '\n'], "")
        .replace('\u{85}', r"\u0085")
        .replace('\u{2028}', r"\u2028")
        .replace('\u{2029}', r"\u2029")
        + "\n")
}

pub fn append(dir: &Path, name: &str, line: &str, durable: bool) -> Result<(), String> {
    (|| {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(name))?;
        let offset = file.metadata()?.len();
        let result = file.write_all(line.as_bytes()).and_then(|()| {
            if !durable {
                return Ok(());
            }
            file.sync_all()?;
            if offset == 0 {
                File::open(dir)?.sync_all()?;
                File::open(dir.parent().unwrap_or(dir))?.sync_all()?;
            }
            Ok(())
        });
        result.inspect_err(|_| {
            let _ = file.set_len(offset);
        })
    })()
    .map_err(|e| format!("{name}: {e}"))
}

pub struct Trace<'a> {
    dir: &'a Path,
    pub job: Option<String>,
    pub run: String,
    pub(crate) machine: String,
    next: Cell<u64>,
}

impl<'d> Trace<'d> {
    pub fn new(dir: &'d Path) -> Self {
        Self {
            dir,
            job: process::env("CS_JOB_ID").filter(|id| !id.is_empty()),
            run: dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            machine: rustix::system::uname()
                .nodename()
                .to_string_lossy()
                .into_owned(),
            next: Cell::new(0),
        }
    }

    // The trusted executor may already have recorded the installed estate's machine ID.
    pub fn use_observed_machine(&mut self, machine: &str) {
        machine.clone_into(&mut self.machine);
    }

    pub fn span<'a>(&'a self, kind: &'static str, name: &str, parent: &'a str) -> Span<'a> {
        self.next.set(self.next.get() + 1);
        Span {
            trace: self,
            id: format!("{}-{}", kind, self.next.get()),
            kind,
            name: name.to_owned(),
            parent,
            clock: Clock::start(),
            ended: None,
            data: json!({}),
        }
    }
}

pub struct Span<'a> {
    trace: &'a Trace<'a>,
    pub id: String,
    kind: &'static str,
    name: String,
    parent: &'a str,
    pub clock: Clock,
    ended: Option<Clock>,
    pub data: Value,
}

impl Span<'_> {
    pub fn close_with_content(
        mut self,
        outcome: &str,
        role: &str,
        pointer: &Result<Value, String>,
    ) -> Result<(), String> {
        if let Ok(pointer) = pointer {
            self.data["content"][role] = pointer.clone();
        }
        self.close(outcome)
    }

    pub fn stop(&mut self) {
        self.ended = Some(Clock::start());
    }

    pub fn close(mut self, outcome: &str) -> Result<(), String> {
        let end = self.ended.unwrap_or_else(Clock::start);
        let stamp = |time: Clock| time.0.to_rfc3339_opts(SecondsFormat::Micros, true);
        let duration = end.elapsed_from(&self.clock) * 1000.0;
        self.data["machine"] = json!(self.trace.machine);
        let line = json!({
            "trace_id": self.trace.run, "span_id": self.id, "kind": self.kind, "name": self.name,
            "parent_id": (!self.parent.is_empty()).then_some(self.parent),
            "start": stamp(self.clock), "end": stamp(end), "duration_ms": duration,
            "attributes": self.data, "outcome": outcome,
        });
        append(self.trace.dir, "trace.jsonl", &format!("{line}\n"), true)
    }
}

pub fn outcome(ok: bool) -> &'static str {
    if ok { "completed" } else { "error" }
}
