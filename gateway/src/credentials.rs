//! Provider credentials: the local store chain, resolved once per provider per process start
//! (and again only on SIGHUP) for discovery and calls alike, and the operator CLI
//! (`csgateway-rs credentials ...`). A credential value is never logged and never an argument to a child.
//!
//! Resolution order: macOS Keychain (through `/usr/bin/security`, the same interface as Go), then the
//! file store `~/.contextstack/credentials/<provider>`. The first store holding the credential wins; a
//! store that refuses or fails stops the walk (fail closed).

use std::{
    collections::HashMap,
    fs,
    io::{BufRead, IsTerminal, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use tokio::sync::OnceCell;

/// The keyed cloud providers whose credentials live in the store, in reporting order.
pub const STORE_PROVIDERS: [&str; 8] = [
    "anthropic",
    "openai",
    "groq",
    "together",
    "lambda",
    "openrouter",
    "gemini",
    "typesafe",
];

/// A lookup outcome other than a credential.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum StoreError {
    /// No store holds it: the provider is unconfigured, which is not a failure.
    #[error("credential not configured")]
    NotFound,
    /// The store refused or failed; the message never carries a value.
    #[error("{0}")]
    Refused(String),
}

/// One credential store.
#[derive(Debug, Clone)]
pub enum Store {
    /// The macOS login Keychain, one generic-password item per provider (`<PROVIDER>_API_KEY`).
    Keychain {
        /// The `security` tool (fixtures may point elsewhere).
        command: PathBuf,
    },
    /// One file per provider in a private directory.
    File {
        /// The store directory (0700, owned by this user).
        dir: PathBuf,
    },
}

/// The resolution chain; empty when the store is off.
#[derive(Debug, Clone, Default)]
pub struct Chain(pub Vec<Store>);

impl Chain {
    /// Resolve `CSGATEWAY_CREDENTIALS`: "" is the file store `~/.contextstack/credentials`, "keychain"
    /// (macOS only) the login Keychain then that file store, "off" no store, else an absolute dir. The
    /// Keychain is opt-in: macOS re-prompts on its access list and partition checks.
    pub fn from_spec(spec: &str, home: Option<&Path>) -> Result<Self, String> {
        Self::chain_for(
            spec,
            home,
            Path::new("/usr/bin/security"),
            cfg!(target_os = "macos"),
        )
    }

    /// `from_spec` with the `security` tool and platform explicit, so a test can count its runs.
    fn chain_for(
        spec: &str,
        home: Option<&Path>,
        security: &Path,
        macos: bool,
    ) -> Result<Self, String> {
        match spec {
            "off" => Ok(Self(Vec::new())),
            "" | "keychain" => {
                let home = home.ok_or("credential store: $HOME is not defined")?;
                let file = Store::File {
                    dir: home.join(".contextstack").join("credentials"),
                };
                match (spec, macos) {
                    ("", _) => Ok(Self(vec![file])),
                    (_, true) => Ok(Self(vec![
                        Store::Keychain {
                            command: security.to_path_buf(),
                        },
                        file,
                    ])),
                    _ => Err(
                        "CSGATEWAY_CREDENTIALS=keychain needs macOS; leave it empty for the file store"
                            .into(),
                    ),
                }
            }
            p if Path::new(p).is_absolute() => Ok(Self(vec![Store::File {
                dir: clean(Path::new(p)),
            }])),
            _ => Err(format!(
                "CSGATEWAY_CREDENTIALS must be empty, \"keychain\", \"off\" or an absolute path, got {spec:?}"
            )),
        }
    }

    /// The stores in resolution order, for operator output.
    pub fn describe(&self) -> String {
        if self.0.is_empty() {
            return "off".into();
        }
        let names: Vec<String> = self
            .0
            .iter()
            .map(|s| match s {
                Store::File { dir } => format!("file {}", dir.display()),
                Store::Keychain { .. } => "keychain".into(),
            })
            .collect();
        names.join(", then ")
    }

    /// Whether any store is configured.
    pub fn is_on(&self) -> bool {
        !self.0.is_empty()
    }

    /// The credential and the store name that held it.
    pub async fn resolve(&self, provider: &str) -> Result<(String, &'static str), StoreError> {
        for store in &self.0 {
            match store.lookup(provider).await {
                Ok(key) => return Ok((key, store.name())),
                Err(StoreError::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        Err(StoreError::NotFound)
    }

    /// The credential alone.
    pub async fn lookup(&self, provider: &str) -> Result<String, StoreError> {
        self.resolve(provider).await.map(|(key, _)| key)
    }
}

type Outcome = Arc<OnceCell<Result<String, StoreError>>>;

/// A chain whose lookups run at most once per provider until [`Resolved::forget`], so discovery passes and
/// calls never re-run one. A Keychain item whose access list does not trust `security` opens a GUI prompt
/// per read; resolving once per start (or SIGHUP) keeps that to one. Found, not found and refused
/// outcomes are all kept.
#[derive(Clone, Default)]
pub struct Resolved {
    chain: Chain,
    got: Arc<Mutex<HashMap<String, Outcome>>>,
}

impl std::fmt::Debug for Resolved {
    /// The chain only: remembered outcomes hold credential values.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolved")
            .field("chain", &self.chain)
            .finish_non_exhaustive()
    }
}

impl Resolved {
    /// Lookups through `chain`, none run yet.
    pub fn new(chain: Chain) -> Self {
        Self {
            chain,
            got: Arc::default(),
        }
    }

    /// Whether any store is configured.
    pub fn is_on(&self) -> bool {
        self.chain.is_on()
    }

    /// [`Chain::lookup`], resolved once per provider; concurrent first callers share one lookup.
    pub async fn lookup(&self, provider: &str) -> Result<String, StoreError> {
        let cell = Arc::clone(
            self.got
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(provider.to_string())
                .or_default(),
        );
        cell.get_or_init(|| self.chain.lookup(provider))
            .await
            .clone()
    }

    /// Drop every remembered outcome so the next lookup reads the store. Only SIGHUP calls it.
    pub fn forget(&self) {
        self.got
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

fn valid_provider(provider: &str) -> bool {
    let mut chars = provider.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn service(provider: &str) -> String {
    format!("{}_API_KEY", provider.to_uppercase())
}

/// Lexically clean an absolute path (Go's `filepath.Clean` for the forms operators write).
pub fn clean(p: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for c in p.components() {
        match c {
            std::path::Component::Normal(n) => out.push(n),
            std::path::Component::ParentDir => {
                out.pop();
            }
            _ => {}
        }
    }
    out
}

impl Store {
    /// The store's reported name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Keychain { .. } => "keychain",
            Self::File { .. } => "file",
        }
    }

    /// Read one provider's credential.
    pub async fn lookup(&self, provider: &str) -> Result<String, StoreError> {
        if !valid_provider(provider) {
            return Err(StoreError::Refused("invalid credential provider".into()));
        }
        match self {
            Self::File { dir } => file_lookup(dir, provider),
            Self::Keychain { command } => {
                let out = security(
                    command,
                    None,
                    &["find-generic-password", "-s", &service(provider), "-w"],
                )
                .await;
                match out {
                    Ok(raw) => {
                        let key = String::from_utf8_lossy(&raw).trim().to_string();
                        if key.is_empty() || key.contains(['\r', '\n']) {
                            Err(StoreError::Refused(format!(
                                "keychain credential invalid for {provider}"
                            )))
                        } else {
                            Ok(key)
                        }
                    }
                    Err(Some(44)) => Err(StoreError::NotFound),
                    Err(_) => Err(StoreError::Refused(format!(
                        "keychain credential unavailable for {provider}"
                    ))),
                }
            }
        }
    }

    /// Add or replace one provider's credential.
    pub async fn set(&self, provider: &str, secret: &str) -> Result<(), String> {
        if !valid_provider(provider) {
            return Err("invalid credential provider".into());
        }
        match self {
            Self::File { dir } => file_set(dir, provider, secret),
            Self::Keychain { command } => {
                if secret.is_empty()
                    || !secret
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "._~+/=:-".contains(c))
                {
                    return Err(format!(
                        "credential for {provider} has characters the keychain store does not accept (whitespace or quotes)"
                    ));
                }
                let line = format!(
                    "add-generic-password -U -a csgateway -s {} -w {secret}\n",
                    service(provider)
                );
                if security(command, Some(line), &["-i"]).await.is_err() {
                    return Err(format!("keychain write failed for {provider}"));
                }
                // `security -i` exits 0 on a failed command: confirm by reading the item back.
                if self.lookup(provider).await.is_err() {
                    return Err(format!("keychain write not confirmed for {provider}"));
                }
                Ok(())
            }
        }
    }

    /// Remove a provider's credential (every Keychain item with its service name).
    pub async fn delete(&self, provider: &str) -> Result<(), String> {
        if !valid_provider(provider) {
            return Err("invalid credential provider".into());
        }
        match self {
            Self::File { dir } => match fs::remove_file(dir.join(provider)) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(_) => Err(format!("credential file for {provider} not removed")),
            },
            Self::Keychain { command } => {
                for _ in 0..8 {
                    match security(
                        command,
                        None,
                        &["delete-generic-password", "-s", &service(provider)],
                    )
                    .await
                    {
                        Ok(_) => {}
                        Err(Some(44)) => return Ok(()),
                        Err(_) => return Err(format!("keychain delete failed for {provider}")),
                    }
                }
                Err(format!(
                    "keychain delete failed for {provider}: too many items"
                ))
            }
        }
    }
}

/// Run `security` with no inherited environment; `Err(Some(code))` on a non-zero exit, `Err(None)` when it
/// could not run or timed out. Its output may hold a credential and is never wrapped into an error.
async fn security(
    command: &Path,
    stdin: Option<String>,
    args: &[&str],
) -> Result<Vec<u8>, Option<i32>> {
    let mut cmd = tokio::process::Command::new(command);
    cmd.args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|_| None)?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        use tokio::io::AsyncWriteExt;
        let _ = pipe.write_all(text.as_bytes()).await;
    }
    let out = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .map_err(|_| None)?
        .map_err(|_| None)?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(out.status.code())
    }
}

/// Refuse anything another user could read or swap.
fn private(path: &Path, meta: &fs::Metadata, dir: bool) -> Result<(), StoreError> {
    let shown = path.display();
    if meta.file_type().is_symlink() {
        return Err(StoreError::Refused(format!(
            "credential path {shown} is a symlink; refused"
        )));
    }
    if dir != meta.is_dir() || (!dir && !meta.is_file()) {
        return Err(StoreError::Refused(format!(
            "credential path {shown} has the wrong type; refused"
        )));
    }
    check_mode(
        path,
        meta.permissions().mode() & 0o7777,
        dir,
        cfg!(target_os = "linux"),
    )?;
    if meta.uid() != rustix::process::getuid().as_raw() {
        return Err(StoreError::Refused(format!(
            "credential path {shown} is owned by another user; refused"
        )));
    }
    Ok(())
}

fn check_mode(path: &Path, mode: u32, dir: bool, linux: bool) -> Result<(), StoreError> {
    let shown = path.display();
    let perm = mode & 0o777;
    if perm & 0o077 != 0 {
        return Err(StoreError::Refused(format!(
            "credential path {shown} is group/world-accessible (mode {perm:04o}); refused, chmod {}",
            if dir { "700" } else { "600" }
        )));
    }
    if linux {
        let want = if dir { 0o700 } else { 0o600 };
        let mode = mode & 0o7777;
        let required = if dir { 0o500 } else { 0o400 };
        if mode & 0o7000 != 0 || mode & required != required {
            return Err(StoreError::Refused(format!(
                "credential path {shown} has mode {mode:04o}; refused, chmod {want:03o}"
            )));
        }
    }
    Ok(())
}

fn file_lookup(dir: &Path, provider: &str) -> Result<String, StoreError> {
    match fs::symlink_metadata(dir) {
        Ok(meta) => private(dir, &meta, true)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(StoreError::NotFound),
        Err(e) => return Err(StoreError::Refused(e.to_string())),
    }
    let path = dir.join(provider);
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(StoreError::NotFound),
        Err(_) => {
            return Err(StoreError::Refused(format!(
                "credential file for {provider} unreadable"
            )));
        }
    };
    private(&path, &meta, false)?;
    let raw = fs::read_to_string(&path)
        .map_err(|_| StoreError::Refused(format!("credential file for {provider} unreadable")))?;
    let key = raw.trim_end_matches(['\r', '\n']);
    if key.is_empty() || key.contains(['\r', '\n']) {
        return Err(StoreError::Refused(format!(
            "credential file for {provider} is empty or not one line"
        )));
    }
    Ok(key.to_string())
}

fn file_set(dir: &Path, provider: &str, secret: &str) -> Result<(), String> {
    if secret.is_empty() || secret.contains(['\r', '\n']) {
        return Err(format!(
            "credential for {provider} is empty or not one line"
        ));
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("credential directory: {e}"))?;
    let meta = fs::symlink_metadata(dir).map_err(|e| e.to_string())?;
    private(dir, &meta, true).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!(".set-{}", std::process::id()));
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| {
            f.write_all(format!("{secret}\n").as_bytes())?;
            f.sync_all()
        });
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
        return Err(format!("credential file for {provider} not written"));
    }
    fs::rename(&tmp, dir.join(provider)).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })
}

const USAGE: &str = "usage: csgateway credentials <command>
  list                     providers and whether each is configured (never values)
  set <provider>           read one credential from stdin (hidden on a terminal) into the first store
  delete <provider>        remove a provider's credential from every store
  import-env [provider...] move <PROVIDER>_API_KEY from this command's environment into the store
store: CSGATEWAY_CREDENTIALS (\"\" the file store ~/.contextstack/credentials, \"keychain\" the macOS
       login Keychain then that file store, \"off\", or an absolute directory)
providers: anthropic openai groq together lambda openrouter gemini";

/// The operator CLI. Values come from stdin or this process's environment and are never printed.
pub async fn command(args: &[String]) -> u8 {
    let mut err = std::io::stderr();
    let usage = |err: &mut std::io::Stderr| {
        let _ = writeln!(err, "{USAGE}");
        2
    };
    if args.is_empty() {
        return usage(&mut err);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let spec = std::env::var("CSGATEWAY_CREDENTIALS").unwrap_or_default();
    let chain = match Chain::from_spec(&spec, home.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(err, "credentials: {e}");
            return 1;
        }
    };
    let one =
        (args.len() == 2 && STORE_PROVIDERS.contains(&args[1].as_str())).then(|| args[1].as_str());
    let result = match (args[0].as_str(), chain.0.first(), one) {
        ("list", _, _) => return list(&chain, &mut std::io::stdout()).await,
        (_, None, _) => Err("the store is off (CSGATEWAY_CREDENTIALS=off)".to_string()),
        ("set", Some(primary), Some(p)) => set(primary, p).await,
        ("delete", Some(_), Some(p)) => delete(&chain, p).await,
        ("import-env", Some(primary), _) => return import_env(primary, &args[1..]).await,
        _ => return usage(&mut err),
    };
    match result {
        Ok(line) => {
            println!("{line}");
            0
        }
        Err(e) => {
            let _ = writeln!(err, "credentials: {e}");
            1
        }
    }
}

async fn set(primary: &Store, provider: &str) -> Result<String, String> {
    let secret = read_secret(provider, &mut std::io::stderr())?;
    primary.set(provider, &secret).await?;
    Ok(format!(
        "stored {provider} in {}; a running gateway picks it up on SIGHUP or restart",
        primary.name()
    ))
}

async fn delete(chain: &Chain, provider: &str) -> Result<String, String> {
    for store in &chain.0 {
        store.delete(provider).await?;
    }
    Ok(format!("deleted {provider}"))
}

/// Move `<PROVIDER>_API_KEY` values from this command's environment into the store; prints names only.
async fn import_env(primary: &Store, names: &[String]) -> u8 {
    let names: Vec<&str> = if names.is_empty() {
        STORE_PROVIDERS.to_vec()
    } else {
        names.iter().map(String::as_str).collect()
    };
    let mut failures = 0;
    for p in names {
        if !STORE_PROVIDERS.contains(&p) {
            eprintln!("credentials: unknown provider {p:?}");
            failures += 1;
            continue;
        }
        let variable = service(p);
        let value = std::env::var(&variable)
            .unwrap_or_default()
            .trim()
            .to_string();
        if value.is_empty() {
            println!("skipped  {p} ({variable} not set)");
            continue;
        }
        match primary.set(p, &value).await {
            Ok(()) => println!("imported {p} from {variable} into {}", primary.name()),
            Err(e) => {
                eprintln!("credentials: {e}");
                failures += 1;
            }
        }
    }
    u8::from(failures > 0)
}

async fn list(chain: &Chain, out: &mut impl Write) -> u8 {
    let _ = writeln!(out, "store: {}", chain.describe());
    for p in STORE_PROVIDERS {
        let line = match chain.resolve(p).await {
            Ok((_, store)) => format!("{p:<11} configured ({store})"),
            Err(StoreError::NotFound) => format!("{p:<11} not configured"),
            Err(e) => format!("{p:<11} refused: {e}"),
        };
        let _ = writeln!(out, "{line}");
    }
    0
}

/// One line from stdin; on a terminal, prompt on stderr with echo off.
fn read_secret(provider: &str, err: &mut impl Write) -> Result<String, String> {
    let stdin = std::io::stdin();
    let tty = stdin.is_terminal();
    if tty {
        let _ = write!(err, "credential for {provider} (input hidden): ");
        let hidden = std::process::Command::new("/bin/stty")
            .arg("-echo")
            .stdin(Stdio::inherit())
            .status()
            .is_ok_and(|s| s.success());
        if !hidden {
            return Err("cannot hide terminal input; pipe the credential on stdin instead".into());
        }
    }
    let mut line = String::new();
    let read = stdin.lock().take_line(&mut line);
    if tty {
        let _ = std::process::Command::new("/bin/stty")
            .arg("echo")
            .stdin(Stdio::inherit())
            .status();
        let _ = writeln!(err);
    }
    read.map_err(|()| "reading credential from stdin failed".to_string())?;
    let secret = line.trim().to_string();
    if secret.is_empty() {
        return Err("no credential on stdin".into());
    }
    Ok(secret)
}

trait TakeLine {
    fn take_line(&mut self, line: &mut String) -> Result<(), ()>;
}

impl<R: BufRead> TakeLine for R {
    /// Read at most 64 KiB up to a newline.
    fn take_line(&mut self, line: &mut String) -> Result<(), ()> {
        use std::io::Read;
        let mut buf = Vec::new();
        self.take(64 << 10)
            .read_until(b'\n', &mut buf)
            .map_err(|_| ())?;
        *line = String::from_utf8_lossy(&buf).into_owned();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_credential_modes() {
        let path = Path::new("fixture");
        for (dir, accepted, refused) in [
            (
                true,
                [0o700, 0o500],
                &[0o750, 0o300, 0o600, 0o4700, 0o2700, 0o1700][..],
            ),
            (
                false,
                [0o600, 0o400],
                &[0o640, 0o604, 0o644, 0o200, 0o4600, 0o2600, 0o1600][..],
            ),
        ] {
            for mode in accepted {
                assert!(check_mode(path, mode, dir, true).is_ok(), "mode {mode:04o}");
            }
            let fix = if dir { "chmod 700" } else { "chmod 600" };
            for &mode in refused {
                assert!(
                    matches!(check_mode(path, mode, dir, true), Err(StoreError::Refused(message)) if message.contains(fix)),
                    "mode {mode:04o}"
                );
            }
        }
    }

    /// A fake `security` that counts its runs: each provider's lookup runs once (found or missing),
    /// concurrent first callers share it, and only `forget` reads again.
    #[tokio::test]
    async fn resolved_looks_each_provider_up_once() {
        use std::os::unix::fs::PermissionsExt;
        let Ok(dir) = tempfile::tempdir() else { return };
        let count = dir.path().join("count");
        let fake = dir.path().join("security");
        let script = format!(
            "#!/bin/sh\necho x >> {}\n[ \"$3\" = OPENAI_API_KEY ] && {{ echo sk-kc-FAKE; exit 0; }}\nexit 44\n",
            count.display()
        );
        assert!(fs::write(&fake, script).is_ok());
        assert!(fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).is_ok());
        let runs = || fs::read_to_string(&count).map_or(0, |s| s.lines().count());
        let once = Resolved::new(Chain(vec![Store::Keychain {
            command: fake.clone(),
        }]));
        let (a, b) = tokio::join!(once.lookup("openai"), once.lookup("openai"));
        assert_eq!(
            (a.as_deref().ok(), b.as_deref().ok()),
            (Some("sk-kc-FAKE"), Some("sk-kc-FAKE"))
        );
        for _ in 0..3 {
            assert_eq!(once.lookup("groq").await, Err(StoreError::NotFound));
            assert!(once.lookup("openai").await.is_ok());
        }
        assert_eq!(runs(), 2);
        assert!(!format!("{once:?}").contains("sk-kc-FAKE"));
        once.forget();
        assert!(once.lookup("openai").await.is_ok());
        assert_eq!(runs(), 3);
    }

    #[tokio::test]
    async fn file_store_refuses_open_modes_and_reads_private_ones() {
        let Ok(dir) = tempfile::tempdir() else { return };
        let store = Store::File {
            dir: dir.path().join("s"),
        };
        assert_eq!(store.lookup("openai").await, Err(StoreError::NotFound));
        assert!(store.set("openai", "sk-x").await.is_ok());
        assert_eq!(store.lookup("openai").await.ok().as_deref(), Some("sk-x"));
        let _ = fs::set_permissions(dir.path().join("s"), fs::Permissions::from_mode(0o750));
        assert!(
            matches!(store.lookup("openai").await, Err(StoreError::Refused(m)) if m.contains("group/world-accessible"))
        );
    }

    /// The Keychain store through a fake `security` with the same interface: `find-generic-password -s
    /// <SERVICE> -w` prints the secret or exits 44 (errSecItemNotFound); `-i` reads commands on stdin.
    #[tokio::test]
    async fn keychain_through_the_security_interface() {
        use std::os::unix::fs::PermissionsExt;
        let Ok(dir) = tempfile::tempdir() else { return };
        let log = dir.path().join("stdin.log");
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n -i) cat >> {log}; exit 0;;\n find-generic-password)\n  [ \"$3\" = OPENAI_API_KEY ] && {{ echo sk-kc-FAKE; exit 0; }}\n  [ \"$3\" = GROQ_API_KEY ] && exit 1\n  exit 44;;\nesac\nexit 2\n",
            log = log.display()
        );
        let fake = dir.path().join("security");
        assert!(fs::write(&fake, script).is_ok());
        assert!(fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).is_ok());
        let kc = Store::Keychain { command: fake };
        assert_eq!(
            kc.lookup("openai").await.ok().as_deref(),
            Some("sk-kc-FAKE")
        );
        assert_eq!(kc.lookup("anthropic").await, Err(StoreError::NotFound));
        assert_eq!(
            kc.lookup("groq").await,
            Err(StoreError::Refused(
                "keychain credential unavailable for groq".into()
            ))
        );
        assert!(kc.set("openai", "sk-kc-FAKE").await.is_ok());
        let sent = fs::read_to_string(&log).unwrap_or_default();
        assert_eq!(
            sent, "add-generic-password -U -a csgateway -s OPENAI_API_KEY -w sk-kc-FAKE\n",
            "secret only on stdin"
        );
        assert!(kc.set("openai", "has space").await.is_err());
        let chain = Chain(vec![
            kc,
            Store::File {
                dir: dir.path().join("absent"),
            },
        ]);
        assert_eq!(
            chain.resolve("openai").await.map(|(_, s)| s),
            Ok("keychain")
        );
        assert!(
            matches!(chain.resolve("groq").await, Err(StoreError::Refused(_))),
            "a refusing store stops the walk"
        );
    }

    #[test]
    fn spec_forms() {
        assert!(Chain::from_spec("off", None).is_ok_and(|c| !c.is_on()));
        assert!(Chain::from_spec("rel", None).is_err());
        assert!(Chain::from_spec("/abs/dir/", None).is_ok_and(|c| c.is_on()));
        let home = Path::new("/home/fixture");
        assert!(
            Chain::from_spec("", Some(home))
                .is_ok_and(|c| c.describe() == "file /home/fixture/.contextstack/credentials")
        );
        assert!(
            Chain::chain_for("keychain", Some(home), Path::new("/x"), false)
                .is_err_and(|e| e.contains("needs macOS"))
        );
    }

    /// A fake `security` that counts its runs: the default store never runs it, for found, missing and
    /// refused lookups alike; the "keychain" opt-in does.
    #[tokio::test]
    async fn default_store_never_runs_security() {
        use std::os::unix::fs::PermissionsExt;
        let Ok(dir) = tempfile::tempdir() else { return };
        let count = dir.path().join("count");
        let fake = dir.path().join("security");
        let script = format!("#!/bin/sh\necho x >> {}\nexit 44\n", count.display());
        assert!(fs::write(&fake, script).is_ok());
        assert!(fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).is_ok());
        let runs = || fs::read_to_string(&count).map_or(0, |s| s.lines().count());
        let home = dir.path().join("home");
        let Ok(def) = Chain::chain_for("", Some(&home), &fake, true) else {
            panic!("default chain")
        };
        assert_eq!(def.lookup("openai").await, Err(StoreError::NotFound));
        assert!(def.0[0].set("openai", "sk-fixture-FAKE").await.is_ok());
        assert_eq!(
            def.lookup("openai").await.ok().as_deref(),
            Some("sk-fixture-FAKE")
        );
        assert_eq!(def.lookup("groq").await, Err(StoreError::NotFound));
        let store_dir = home.join(".contextstack").join("credentials");
        assert!(fs::set_permissions(&store_dir, fs::Permissions::from_mode(0o750)).is_ok());
        assert!(matches!(
            def.lookup("openai").await,
            Err(StoreError::Refused(_))
        ));
        assert_eq!(runs(), 0);
        let Ok(opt) = Chain::chain_for("keychain", Some(&home), &fake, true) else {
            panic!("keychain chain")
        };
        assert_eq!(
            opt.0.iter().map(Store::name).collect::<Vec<_>>(),
            ["keychain", "file"]
        );
        let _ = opt.lookup("groq").await;
        assert_eq!(runs(), 1);
    }
}
