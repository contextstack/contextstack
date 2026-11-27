use std::{
    collections::BTreeMap,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Mutex as AsyncMutex, mpsc, watch},
    task::JoinSet,
    time::timeout,
};
use url::Url;

use crate::{
    Result,
    action::Claims,
    invalid,
    native::{self, Body, Message},
    registration::Settings,
};

const MAX_REQUEST: usize = 64 * 1024;
const MAX_TEXT: usize = 5 * 1024 * 1024;
const MAX_REPLY: usize = 8 * 1024 * 1024;
const MAX_CLIENTS_PER_SET: usize = 32;
const OPEN_TIMEOUT: Duration = Duration::from_secs(45);
const SOCKET_TIMEOUT: Duration = Duration::from_secs(75);
const TASKS_SET: &str = "google-tasks";
const TASKS_ORIGIN: &str = "https://tasks.google.com";
const GOOGLE_ACCOUNTS: &str = "https://accounts.google.com";

#[derive(Debug)]
pub(crate) struct Failure(pub(crate) &'static str, pub(crate) &'static str);

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.1)
    }
}

impl std::error::Error for Failure {}

fn navigation_blocked() -> Failure {
    Failure(
        "navigation_blocked",
        "navigation is outside the declared origins",
    )
}

fn safe_action_reason(message: Option<&str>) -> &'static str {
    [
        "expected exactly one form matching the target",
        "invalid form target",
        "origin changed",
        "form control overrides a form property",
        "expected one URL-encoded POST form",
        "unsupported form accept-charset; expected UTF-8",
        "form destination is outside the approved origin",
        "expected at most one submit button without submission overrides",
        "unsupported form control",
        "form payload is too large",
        "form summary is too large",
        "form digest mismatch",
        "approval expired or document location changed",
        "keyboard input reached the capture tab",
        "Could not start the managed tab",
        "The managed tab is no longer visible",
        "The managed tab changed during capture",
        "The form changed during capture",
        "The managed page opened a new tab or window",
    ]
    .into_iter()
    .find(|reason| Some(*reason) == message)
    .unwrap_or("browser action was refused")
}

#[derive(Deserialize)]
struct Policy {
    version: u64,
    sets: BTreeMap<String, Vec<String>>,
}

fn origin(url: &str) -> Option<String> {
    if !url.starts_with("https://") {
        return None;
    }
    let parsed = Url::parse(url).ok()?;
    Some(parsed.origin().ascii_serialization())
}

fn load_policy(path: &Path) -> Result<Policy> {
    let policy: Policy = serde_json::from_reader(File::open(path)?)?;
    if policy.version != 1 || policy.sets.is_empty() {
        return Err(invalid("expected browser policy version 1 with origin sets").into());
    }
    for (name, origins) in &policy.sets {
        if (name == TASKS_SET && origins.as_slice() != [TASKS_ORIGIN])
            || (name != TASKS_SET && origins.iter().any(|url| url == TASKS_ORIGIN))
            || name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            || origins.is_empty()
            || origins.iter().any(|url| origin(url).as_ref() != Some(url))
        {
            return Err(invalid(format!("invalid browser origin set {name:?}")).into());
        }
    }
    Ok(policy)
}

fn private_dir(path: &Path) -> Result<()> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o022 != 0
    {
        return Err(invalid(format!(
            "{} must be a user-owned directory without group or world write access",
            path.display()
        ))
        .into());
    }
    Ok(())
}

struct SocketPath(PathBuf);

impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn bind(path: PathBuf) -> Result<(UnixListener, SocketPath)> {
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            match std::os::unix::net::UnixStream::connect(&path) {
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    fs::remove_file(&path)?;
                }
                _ => {
                    return Err(
                        invalid(format!("socket {} may still be live", path.display())).into(),
                    );
                }
            }
        }
        Ok(_) => {
            return Err(invalid(format!("{} exists and is not a socket", path.display())).into());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(&path)?;
    let guard = SocketPath(path);
    fs::set_permissions(&guard.0, fs::Permissions::from_mode(0o600))?;
    Ok((listener, guard))
}

struct Channel {
    incoming: mpsc::Receiver<Message>,
    id: u64,
}

struct Broker {
    channel: AsyncMutex<Channel>,
    log: Mutex<File>,
    action_key_file: Option<PathBuf>,
    used_claims: PathBuf,
}

#[derive(Default)]
struct Navigations {
    allowed: Vec<String>,
    blocked: Vec<String>,
}

impl Navigations {
    fn check(&mut self, url: &str, candidate: Option<String>, origins: &[String]) -> Result<()> {
        if candidate.is_some_and(|candidate| origins.contains(&candidate)) {
            return Ok(());
        }
        self.blocked.push(url.to_owned());
        Err(navigation_blocked().into())
    }
}

impl Channel {
    async fn open(
        &mut self,
        set: &str,
        url: &str,
        origins: &[String],
        navigations: &mut Navigations,
        action: Option<(&str, &Value)>,
        confirmed: &mut bool,
    ) -> Result<Value> {
        self.id += 1;
        let id = self.id;
        send_open(id, url, origins, action)?;
        loop {
            let message = self.incoming.recv().await.ok_or(Failure(
                "extension_missing_or_disabled",
                "browser extension disconnected",
            ))?;
            if message.id < id {
                continue;
            }
            if message.id != id {
                return Err(invalid("unexpected extension request ID").into());
            }
            match message.body {
                Body::Navigation {
                    event,
                    url,
                    main_frame,
                    parent_origin,
                } => {
                    let sign_in = set == TASKS_SET
                        && main_frame
                        && origin(&url).as_deref() == Some(GOOGLE_ACCOUNTS);
                    let candidate = navigation_candidate(&url, main_frame, parent_origin);
                    if let Some(reply) =
                        answer_navigation(id, event, url, candidate, sign_in, origins, navigations)?
                    {
                        return Ok(reply);
                    }
                }
                Body::Result {
                    final_url,
                    status,
                    title,
                    text,
                } if action.is_none() => {
                    navigations.check(&final_url, origin(&final_url), origins)?;
                    return page_evidence(set, url, &final_url, status, &title, &text, navigations);
                }
                Body::ProposalSnapshot {
                    digest,
                    page_url,
                    values_summary,
                    image,
                } if matches!(action, Some(("capture_action", _))) => {
                    navigations.check(&page_url, origin(&page_url), origins)?;
                    return Ok(json!({"digest":digest,"page_url":page_url,
                        "values_summary":values_summary,"image":image}));
                }
                Body::ActionSnapshot { digest }
                    if !*confirmed && matches!(action, Some(("inspect_action", _))) =>
                {
                    confirm_action(id, &digest, action, confirmed)?;
                }
                Body::ActionResult { status } if *confirmed => {
                    return Ok(action_outcome(status));
                }
                Body::Result { .. }
                | Body::ActionResult { .. }
                | Body::ActionSnapshot { .. }
                | Body::ProposalSnapshot { .. } => {
                    return Err(invalid("unexpected extension result").into());
                }
                Body::Error {
                    error_kind,
                    message,
                } => {
                    return Err(extension_failure(
                        error_kind.as_deref(),
                        message.as_deref(),
                        action.is_some(),
                        *confirmed,
                    )
                    .into());
                }
                Body::SignInRequired => return Ok(sign_in_required()),
                Body::Status { .. } => {}
            }
        }
    }
}

fn send_open(id: u64, url: &str, origins: &[String], action: Option<(&str, &Value)>) -> Result<()> {
    let mut request = json!({"type":"open","id":id,"url":url,"origins":origins});
    if let Some((kind, approval)) = action {
        request["type"] = json!(kind);
        request["approval"] = approval.clone();
    }
    native::write(io::stdout().lock(), &request)
}

/// The origin a navigation is judged by: blank and srcdoc subframes inherit their parent's.
fn navigation_candidate(
    url: &str,
    main_frame: bool,
    parent_origin: Option<String>,
) -> Option<String> {
    if main_frame {
        origin(url)
    } else if matches!(url, "about:blank" | "about:srcdoc") {
        parent_origin
    } else {
        origin(url.strip_prefix("blob:").unwrap_or(url))
    }
}

/// Records and answers one navigation. A reply ends the request because sign-in is required.
fn answer_navigation(
    id: u64,
    event: u64,
    url: String,
    candidate: Option<String>,
    sign_in: bool,
    origins: &[String],
    navigations: &mut Navigations,
) -> Result<Option<Value>> {
    let decision = navigations.check(&url, candidate, origins);
    if decision.is_ok() {
        navigations.allowed.push(url);
    }
    native::write(
        io::stdout().lock(),
        &json!({
            "type":"navigation_decision","id":id,"event":event,"allow":decision.is_ok()
        }),
    )?;
    if sign_in {
        return Ok(Some(sign_in_required()));
    }
    decision?;
    Ok(None)
}

/// The reply for a page read. The caller must already have checked the final URL's origin.
fn page_evidence(
    set: &str,
    requested_url: &str,
    final_url: &str,
    status: Option<u16>,
    title: &str,
    text: &str,
    navigations: &Navigations,
) -> Result<Value> {
    if text.len() > MAX_TEXT {
        return Err(invalid(format!("page text exceeds {MAX_TEXT} bytes")).into());
    }
    Ok(json!({
        "ok":true, "set":set, "requested_url":requested_url, "final_url":final_url,
        "status":status, "title":title, "bytes":text.len(),
        "sha256":hex::encode(Sha256::digest(text.as_bytes())), "text":text,
        "navigations":navigations.allowed, "blocked":navigations.blocked,
    }))
}

/// Lets the extension execute only if the inspected form still matches the approved digest.
fn confirm_action(
    id: u64,
    digest: &str,
    action: Option<(&str, &Value)>,
    confirmed: &mut bool,
) -> Result<()> {
    if action.is_none_or(|(_, row)| row["field_values_digest"] != digest) {
        native::write(
            io::stdout().lock(),
            &json!({"type":"cancel_action","id":id}),
        )?;
        return Err(Failure("action_refused", "form digest mismatch").into());
    }
    *confirmed = true;
    native::write(
        io::stdout().lock(),
        &json!({"type":"execute_confirmed","id":id}),
    )
}

fn action_outcome(status: Option<u16>) -> Value {
    let outcome = if status.is_some() {
        "submitted"
    } else {
        "submission_unknown"
    };
    json!({"ok":true,"status":status,"outcome":outcome})
}

fn extension_failure(
    error_kind: Option<&str>,
    message: Option<&str>,
    acting: bool,
    confirmed: bool,
) -> Failure {
    match error_kind {
        Some("origin_not_permitted") => {
            Failure("origin_not_permitted", "Chrome has not granted site access")
        }
        Some("navigation_blocked") => navigation_blocked(),
        Some("page_timeout") => Failure("page_timeout", "browser request timed out"),
        Some("action_refused_before_send") if confirmed => {
            Failure("action_refused", safe_action_reason(message))
        }
        Some("action_refused") if acting && !confirmed => {
            Failure("action_refused", safe_action_reason(message))
        }
        _ => Failure(
            "unknown",
            "extension could not complete the browser request",
        ),
    }
}

fn sign_in_required() -> Value {
    json!({"ok":false, "error_kind":"logged_out"})
}

/// The binding for a screenshot capture, built only from validated request fields.
fn capture_binding(request: &Value, set: &str, origin: &str) -> Result<Value> {
    let field = |name, limit| {
        request[name]
            .as_str()
            .filter(|value| {
                !value.trim().is_empty()
                    && value.len() <= limit
                    && !value.chars().any(char::is_control)
            })
            .ok_or_else(|| invalid("invalid browser capture binding"))
    };
    let action = field("action", 6)?;
    if !matches!(action, "submit" | "post" | "buy") {
        return Err(invalid("invalid browser capture action").into());
    }
    Ok(json!({"origin":origin,"origin_set":set,"action":action,
        "target":field("target", 2048)?,"job_id":field("job_id", 128)?,
        "run_id":field("run_id", 127)?}))
}

async fn request(stream: &mut UnixStream) -> Result<Value> {
    let mut line = Vec::new();
    timeout(
        SOCKET_TIMEOUT,
        BufReader::new(stream.take((MAX_REQUEST + 1) as u64)).read_until(b'\n', &mut line),
    )
    .await??;
    if line.len() > MAX_REQUEST {
        return Err(invalid(format!("request exceeds {MAX_REQUEST} bytes")).into());
    }
    Ok(serde_json::from_slice(&line)?)
}

impl Broker {
    /// Verifies an action request. An execution consumes its claim; a capture returns the key
    /// that will sign the screenshot.
    fn authorize(
        &self,
        request: &Value,
        set: &str,
        url: &str,
    ) -> Result<(Option<Claims>, (&'static str, Value))> {
        let origin = origin(url).ok_or_else(|| invalid("invalid action origin"))?;
        let key = self
            .action_key_file
            .as_ref()
            .filter(|_| set != TASKS_SET)
            .ok_or_else(|| invalid("browser action requires a verified claim"))?;
        let key = Claims::load(key, self.used_claims.clone())?;
        if request["verb"].as_str() == Some("execute_action") {
            let row = key.consume(&request["approval"], &origin)?;
            return Ok((None, ("inspect_action", row)));
        }
        key.authorize_capture(request).map_err(|_| {
            Failure(
                "action_refused",
                "browser capture requires host authorization",
            )
        })?;
        let binding = capture_binding(request, set, &origin)?;
        Ok((Some(key), ("capture_action", binding)))
    }

    /// Runs one extension exchange. After execution is confirmed, any failure other than an
    /// explicit refusal means the submission outcome is unknown.
    async fn exchange(
        &self,
        set: &str,
        url: &str,
        origins: &[String],
        navigations: &mut Navigations,
        action: Option<(&str, &Value)>,
    ) -> Result<Value> {
        let mut confirmed = false;
        let reply = timeout(OPEN_TIMEOUT, async {
            self.channel
                .lock()
                .await
                .open(set, url, origins, navigations, action, &mut confirmed)
                .await
        })
        .await;
        match reply {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(error))
                if !confirmed
                    || error
                        .downcast_ref::<Failure>()
                        .is_some_and(|failure| failure.0 == "action_refused") =>
            {
                Err(error)
            }
            Err(error) if !confirmed => Err(error.into()),
            _ => Ok(json!({"ok":true,"status":null,"outcome":"submission_unknown"})),
        }
    }

    /// Signs the capture of an extension snapshot against its authorized binding.
    fn sign_capture(&self, claims: &Claims, mut data: Value, snapshot: &Value) -> Result<Value> {
        data["field_values_digest"] = snapshot["digest"].clone();
        data["page_url"] = snapshot["page_url"].clone();
        data["values_summary"] = snapshot["values_summary"].clone();
        let screenshots = self.used_claims.with_file_name("action-screenshots");
        claims.capture(
            data,
            snapshot["image"].as_str().unwrap_or_default(),
            &screenshots,
        )
    }

    async fn perform(
        &self,
        request: &Value,
        set: &str,
        origins: &[String],
        navigations: &mut Navigations,
    ) -> Result<Value> {
        let verb = request["verb"].as_str();
        if matches!(verb, Some("submit" | "post" | "buy")) {
            return Err(invalid("browser action requires a verified claim").into());
        }
        if !matches!(verb, Some("open" | "execute_action" | "capture_action")) {
            return Err(invalid("unknown verb; the only verb is open").into());
        }
        let url = request["url"]
            .as_str()
            .ok_or("open requires a URL string")?;
        if verb == Some("open") && set != TASKS_SET {
            return Err(
                invalid("native page reads require the scheduled reader's origin set").into(),
            );
        }
        navigations.check(url, origin(url), origins)?;
        let (claims, approval) = if matches!(verb, Some("execute_action" | "capture_action")) {
            let (claims, approval) = self.authorize(request, set, url)?;
            (claims, Some(approval))
        } else {
            (None, None)
        };
        let action_origins;
        let origins = if let Some((_, row)) = &approval {
            action_origins = [row["origin"].as_str().unwrap_or_default().to_owned()];
            &action_origins
        } else {
            origins
        };
        let action = approval.as_ref().map(|(kind, row)| (*kind, row));
        let reply = self
            .exchange(set, url, origins, navigations, action)
            .await?;
        let reply = if let (Some(claims), Some((_, data))) = (claims, approval) {
            self.sign_capture(&claims, data, &reply)?
        } else {
            reply
        };
        if serde_json::to_vec(&reply)?.len() >= MAX_REPLY {
            return Err(invalid("broker reply exceeds socket size limit").into());
        }
        Ok(reply)
    }

    async fn handle(&self, mut stream: UnixStream, set: &str, origins: &[String]) -> Result<()> {
        let started = Instant::now();
        let parsed = request(&mut stream).await;
        let verb = parsed.as_ref().ok().and_then(|value| value.get("verb"));
        let mut navigations = Navigations::default();
        let result = match &parsed {
            Ok(request) => self.perform(request, set, origins, &mut navigations).await,
            Err(error) => Err(invalid(format!("bad request: {error}")).into()),
        };
        let reply = result.unwrap_or_else(|error| {
            let failure = if let Some(failure) = error.downcast_ref::<Failure>() {
                Failure(failure.0, failure.1)
            } else if error.is::<tokio::time::error::Elapsed>() {
                Failure("page_timeout", "browser request timed out")
            } else if matches!(
                verb.and_then(Value::as_str),
                Some("execute_action" | "capture_action" | "submit" | "post" | "buy")
            ) {
                Failure("action_refused", "browser action requires a verified claim")
            } else {
                Failure("unknown", "browser request failed")
            };
            let mut error = json!({"ok":false, "error_kind":failure.0, "error":failure.1});
            if set != TASKS_SET && failure.0 == "navigation_blocked" {
                error["navigations"] = json!(navigations.allowed);
                error["blocked"] = json!(navigations.blocked);
            }
            error
        });
        let mut row = json!({
            "ts":chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "set":set, "verb":verb.and_then(Value::as_str), "ms":started.elapsed().as_millis(),
        });
        for key in ["ok", "status", "error_kind", "outcome"] {
            if let Some(value) = reply.get(key) {
                row[key] = value.clone();
            }
        }
        let mut log = serde_json::to_vec(&row)?;
        log.push(b'\n');
        self.log
            .lock()
            .map_err(|_| invalid("broker log lock poisoned"))?
            .write_all(&log)?;
        let mut wire = serde_json::to_vec(&reply)?;
        wire.push(b'\n');
        timeout(SOCKET_TIMEOUT, stream.write_all(&wire)).await??;
        Ok(())
    }
}

async fn listen(
    listener: UnixListener,
    _path: SocketPath,
    broker: Arc<Broker>,
    name: String,
    origins: Vec<String>,
    mut disconnected: watch::Receiver<bool>,
) -> Result<()> {
    let name: Arc<str> = name.into();
    let origins: Arc<[String]> = origins.into();
    let mut clients = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = disconnected.changed() => break,
            Some(completed) = clients.join_next() => completed?,
            accepted = listener.accept(), if clients.len() < MAX_CLIENTS_PER_SET => {
                let (stream, _) = accepted?;
                let (broker, name, origins) = (broker.clone(), name.clone(), origins.clone());
                clients.spawn(async move {
                    if let Err(error) = broker.handle(stream, &name, &origins).await {
                        eprintln!("browser-host: {error}");
                    }
                });
            }
        }
    }
    while let Some(completed) = clients.join_next().await {
        completed?;
    }
    Ok(())
}

pub(crate) fn startup_failure(root: &Path, stage: &str) -> Result<()> {
    private_dir(root.parent().ok_or("missing state directory")?)?;
    private_dir(root)?;
    let logs = root.join("log");
    private_dir(&logs)?;
    let temp = logs.join(format!(".host-start.{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let status = json!({"ts":chrono::Utc::now().timestamp(),"error":stage});
    let result =
        writeln!(file, "{status}").and_then(|()| fs::rename(&temp, logs.join("host-start.json")));
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    Ok(result?)
}

#[tokio::main(flavor = "current_thread")]
pub(crate) async fn serve(
    root: &Path,
    settings: &Settings,
    stage: &mut &'static str,
) -> Result<()> {
    let policy = load_policy(settings.policy.as_deref().ok_or("missing browser policy")?)?;
    *stage = "socket";
    // Chrome launches a dedicated host process. Keep new socket paths private at bind time.
    rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o077));
    private_dir(root.parent().ok_or("missing state directory")?)?;
    private_dir(root)?;
    let sockets = root.join("native-sockets");
    let logs = root.join("log");
    private_dir(&sockets)?;
    private_dir(&logs)?;
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(root.join("native-host.lock"))?;
    *stage = "lock";
    lock.try_lock()?;
    *stage = "socket";
    let used_claims = root.join("used-action-claims");
    if settings.action_key_file.is_some() {
        private_dir(&used_claims)?;
        let screenshots = root.join("action-screenshots");
        private_dir(&screenshots)?;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_hours(24));
            loop {
                interval.tick().await;
                let _ =
                    crate::action::prune_screenshots(&screenshots, std::time::SystemTime::now());
            }
        });
    }
    let log = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(logs.join("broker.jsonl"))?;

    let (sender, incoming) = mpsc::channel(1);
    let (closed, disconnected) = watch::channel(false);
    let broker = Arc::new(Broker {
        channel: AsyncMutex::new(Channel { incoming, id: 0 }),
        log: Mutex::new(log),
        action_key_file: settings.action_key_file.clone(),
        used_claims,
    });
    let mut listeners = JoinSet::new();
    for (name, origins) in policy.sets {
        let (listener, path) = bind(sockets.join(format!("{name}.sock")))?;
        listeners.spawn(listen(
            listener,
            path,
            broker.clone(),
            name,
            origins,
            disconnected.clone(),
        ));
    }
    *stage = "runtime";
    let status_path = root.join("extension-status.json");
    std::thread::spawn(move || {
        let mut input = io::stdin().lock();
        loop {
            match native::read(&mut input) {
                Ok(Some(Message {
                    id: 0,
                    body: Body::Status { origins },
                })) => {
                    let temp = status_path.with_extension("tmp");
                    let status = json!({"origins": origins});
                    if fs::write(&temp, status.to_string()).is_ok()
                        && fs::rename(temp, &status_path).is_ok()
                    {
                        let _ = fs::remove_file(logs.join("host-start.json"));
                    }
                }
                Ok(Some(message)) => {
                    if sender.blocking_send(message).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    eprintln!("browser-host: could not read a valid native message");
                    break;
                }
            }
        }
        drop(sender);
        let _ = closed.send(true);
    });
    while let Some(result) = listeners.join_next().await {
        result??;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Navigations;

    #[test]
    fn origin_checks_use_the_parsed_https_authority() -> crate::Result<()> {
        let mut navigations = Navigations::default();
        let origins = ["https://example.com".to_owned()];
        let url = "https://EXAMPLE.COM:443/a";
        navigations.check(url, super::origin(url), &origins)?;
        let off_set = [
            "https://example.com@other.example/",
            "https://example.com:444/",
            "http://example.com/",
        ];
        for url in off_set {
            assert!(
                navigations
                    .check(url, super::origin(url), &origins)
                    .is_err()
            );
        }
        assert_eq!(navigations.blocked, off_set);
        Ok(())
    }
}
