//! Process settings from the environment, parsed once into a value that can only exist valid.
//!
//! Provider API keys are never read from the environment: every `*_API_KEY` is ignored here
//! and never passed to a child. A `.env` in the working directory is not read. An install passes
//! `CSGATEWAY_*` through the service environment (launchd `EnvironmentVariables`, systemd
//! `Environment=`), and a file in the working directory could set `CSGATEWAY_AUTH_REQUIRED=false`.

use crate::credentials::{Chain, Resolved, STORE_PROVIDERS};
use crate::log::{format_duration, parse_duration};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// Environment lookup: the process environment only, never a provider key.
#[derive(Debug, Clone, Default)]
pub struct Env;

impl Env {
    /// Process environment. A `.env` in the working directory is ignored.
    pub fn load() -> Self {
        Self
    }

    /// The raw value from the process environment, untrimmed; `None` when unset.
    #[allow(clippy::unused_self)]
    pub fn raw(&self, key: &str) -> Option<String> {
        if key.ends_with("_API_KEY") {
            return None;
        }
        std::env::var(key).ok()
    }

    /// The trimmed value, or the fallback when unset or blank.
    pub fn get(&self, key: &str, fallback: &str) -> String {
        match self.raw(key).map(|v| v.trim().to_string()) {
            Some(v) if !v.is_empty() => v,
            _ => fallback.to_string(),
        }
    }

    fn int(&self, key: &str, fallback: i64) -> i64 {
        self.get(key, "").parse().unwrap_or(fallback)
    }

    fn duration(&self, key: &str, fallback: Duration) -> Duration {
        let raw = self.get(key, "");
        if raw.is_empty() {
            return fallback;
        }
        parse_duration(&raw).unwrap_or_else(|| {
            println!(
                "invalid duration {key}={raw:?}, using default {}",
                format_duration(fallback)
            );
            fallback
        })
    }

    fn boolean(&self, key: &str, fallback: bool) -> bool {
        let raw = self.get(key, "").to_lowercase();
        if raw.is_empty() {
            return fallback;
        }
        matches!(raw.as_str(), "true" | "1" | "yes")
    }
}

/// Everything the gateway takes from its environment.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Lookup used for child environments (allowlisted names only).
    pub env: Env,
    /// `$HOME`, the state root's parent.
    pub home: Option<PathBuf>,
    /// TCP bind address (`CSAGENT_BIND`).
    pub bind: String,
    /// TCP port (`CSGATEWAY_PORT`).
    pub port: i64,
    /// Bearer token; empty means no auth.
    pub auth_token: String,
    /// Whether a token is required (`CSGATEWAY_AUTH_REQUIRED`).
    pub auth_required: bool,
    /// models.json (`CSAGENT_MODELS_CONFIG`).
    pub models_path: PathBuf,
    /// Inventory refresh period.
    pub refresh_interval: Duration,
    /// Ollama root (no `/v1`).
    pub ollama_endpoint: String,
    /// vLLM root (no `/v1`).
    pub vllm_endpoint: String,
    /// The credential store chain (empty: off), each provider resolved once per start.
    pub store: Resolved,
    /// Providers that resolve credentials from the store.
    pub store_providers: Vec<String>,
    /// Unix-socket listener path.
    pub socket: Option<PathBuf>,
    /// Discovery health file.
    pub health_path: Option<PathBuf>,
    /// Discovery freshness threshold.
    pub stale_after: Duration,
    /// Endpoints whose discovery is off.
    pub discovery_disabled: Vec<String>,
    /// Host overlay directory.
    pub providers_dir: Option<PathBuf>,
    /// Tenant policy file (single-node mode when unset).
    pub policy_path: Option<PathBuf>,
    /// Reported on /v1/health.
    pub role_aware_dispatch: bool,
    /// Concurrent HTTP connections (`CSGATEWAY_MAX_INFLIGHT`).
    pub max_inflight: usize,
    /// How long a new connection waits for an in-flight slot before `gateway.busy`.
    pub inflight_accept_timeout: Duration,
    /// How long to wait for a complete request head on a new connection.
    pub header_read_timeout: Duration,
}

impl Settings {
    /// Read and validate. The error is the startup refusal's text.
    pub fn load(env: Env) -> Result<Self, String> {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from);
        let refresh_interval = env.duration(
            "CSAGENT_INVENTORY_REFRESH_INTERVAL",
            Duration::from_secs(60),
        );
        let agent_port = env.int("CSAGENT_PORT", 9100);
        if !(1..=65535).contains(&agent_port) {
            return Err(format!(
                "CSAGENT_PORT must be between 1 and 65535, got {agent_port}"
            ));
        }
        if refresh_interval.is_zero() {
            return Err("CSAGENT_INVENTORY_REFRESH_INTERVAL must be > 0".into());
        }
        let auth_token = match env.get("CSGATEWAY_AUTH_TOKEN", "") {
            t if t.is_empty() => env.get("CSAGENT_AUTH_TOKEN", ""),
            t => t,
        };
        let store = Chain::from_spec(&env.get("CSGATEWAY_CREDENTIALS", ""), home.as_deref())?;
        let store_providers = store_providers(&env, &store)?;
        let socket = socket(&env, home.as_deref())?;
        let health_path = state_path(
            &env,
            home.as_deref(),
            "CSGATEWAY_DISCOVERY_HEALTH",
            "discovery-health.json",
        )?;
        let stale_after = env.duration("CSGATEWAY_DISCOVERY_STALE_AFTER", Duration::from_mins(15));
        if stale_after <= refresh_interval {
            return Err(format!(
                "CSGATEWAY_DISCOVERY_STALE_AFTER ({}) must exceed the refresh interval ({})",
                format_duration(stale_after),
                format_duration(refresh_interval)
            ));
        }
        let providers_dir = state_path(
            &env,
            home.as_deref(),
            "CSGATEWAY_PROVIDERS_DIR",
            "providers.d",
        )?;
        let policy_path = match env.get("CSGATEWAY_POLICY", "") {
            p if p.is_empty() => None,
            p if Path::new(&p).is_absolute() => Some(PathBuf::from(p)),
            p => {
                return Err(format!(
                    "CSGATEWAY_POLICY must be an absolute path, got {p:?}"
                ));
            }
        };
        let discovery_disabled = env
            .get("CSGATEWAY_DISCOVERY_DISABLED", "")
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        let max_inflight = match env.int("CSGATEWAY_MAX_INFLIGHT", 256) {
            n if n < 1 => {
                return Err("CSGATEWAY_MAX_INFLIGHT must be >= 1".into());
            }
            n => usize::try_from(n)
                .map_err(|_| format!("CSGATEWAY_MAX_INFLIGHT out of range: {n}"))?,
        };
        let inflight_accept_timeout =
            env.duration("CSGATEWAY_INFLIGHT_ACCEPT_TIMEOUT", Duration::from_secs(5));
        if inflight_accept_timeout.is_zero() {
            return Err("CSGATEWAY_INFLIGHT_ACCEPT_TIMEOUT must be > 0".into());
        }
        let header_read_timeout =
            env.duration("CSGATEWAY_HEADER_READ_TIMEOUT", Duration::from_secs(10));
        if header_read_timeout.is_zero() {
            return Err("CSGATEWAY_HEADER_READ_TIMEOUT must be > 0".into());
        }
        Ok(Self {
            bind: env.get("CSAGENT_BIND", "127.0.0.1"),
            port: env.int("CSGATEWAY_PORT", 9200),
            auth_required: env.boolean("CSGATEWAY_AUTH_REQUIRED", true),
            models_path: PathBuf::from(env.get("CSAGENT_MODELS_CONFIG", "models.json")),
            ollama_endpoint: env.get("OLLAMA_ENDPOINT", "http://localhost:11434"),
            vllm_endpoint: env.get("VLLM_ENDPOINT", "http://127.0.0.1:8000"),
            role_aware_dispatch: env.boolean("CSAGENT_ROLE_AWARE_DISPATCH", false),
            auth_token,
            refresh_interval,
            store: Resolved::new(store),
            store_providers,
            socket,
            health_path,
            stale_after,
            discovery_disabled,
            providers_dir,
            policy_path,
            home,
            env,
            max_inflight,
            inflight_accept_timeout,
            header_read_timeout,
        })
    }

    /// `bind:port` for the TCP listener.
    pub fn listen_addr(&self) -> String {
        format!("{}:{}", self.bind, self.port)
    }

    /// Whether a provider resolves its credential from the store.
    pub fn store_backed(&self, provider: &str) -> bool {
        self.store_providers.iter().any(|p| p == provider)
    }
}

/// `CSGATEWAY_KEYCHAIN_PROVIDERS` narrows which providers use the store (default: all of `STORE_PROVIDERS`).
fn store_providers(env: &Env, store: &Chain) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    if !store.is_on() {
        return Ok(out);
    }
    for name in env
        .get("CSGATEWAY_KEYCHAIN_PROVIDERS", &STORE_PROVIDERS.join(","))
        .split(',')
    {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        if !STORE_PROVIDERS.contains(&name) {
            return Err(format!("unknown keychain provider {name:?}"));
        }
        out.push(name.to_string());
    }
    Ok(out)
}

/// `CSGATEWAY_SOCKET`: empty is off, `default` the state root's socket, else an absolute path.
fn socket(env: &Env, home: Option<&Path>) -> Result<Option<PathBuf>, String> {
    match env.get("CSGATEWAY_SOCKET", "").as_str() {
        "" => Ok(None),
        "default" => Ok(Some(
            state_root(home, "CSGATEWAY_SOCKET=default")?.join("csgateway.sock"),
        )),
        p if Path::new(p).is_absolute() => Ok(Some(crate::credentials::clean(Path::new(p)))),
        p => Err(format!(
            "CSGATEWAY_SOCKET must be an absolute path or \"default\", got {p:?}"
        )),
    }
}

fn state_root(home: Option<&Path>, what: &str) -> Result<PathBuf, String> {
    home.map(|h| h.join(".contextstack").join("gateway"))
        .ok_or_else(|| format!("{what}: $HOME is not defined"))
}

/// Empty is `~/.contextstack/gateway/<name>`, `off` disables, else an absolute path.
fn state_path(
    env: &Env,
    home: Option<&Path>,
    key: &str,
    name: &str,
) -> Result<Option<PathBuf>, String> {
    match env.get(key, "").as_str() {
        "off" => Ok(None),
        "" => Ok(Some(
            state_root(home, &format!("{key} default"))?.join(name),
        )),
        p if Path::new(p).is_absolute() => Ok(Some(crate::credentials::clean(Path::new(p)))),
        p => Err(format!(
            "{key} must be an absolute path or \"off\", got {p:?}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn cwd_dotenv_cannot_disable_auth() -> Result<(), Box<dyn std::error::Error>> {
        // Re-exec with the temp directory as the process directory. A `.env` there is the file a
        // dotenv loader would read. Spawning sets that directory; changing it in this process is
        // refused by some confined runners, and the repo directory refuses a `.env` write.
        if std::env::var("CSGATEWAY_DOTENV_CHILD").is_ok() {
            let env = Env::load();
            assert!(
                env.raw("CSGATEWAY_DOTENV_PROBE").is_none(),
                "a .env in the working directory was read"
            );
            let settings = Settings::load(env)?;
            assert!(
                settings.auth_required,
                "CSGATEWAY_AUTH_REQUIRED=false in the working directory .env must be ignored"
            );
            std::fs::write("dotenv-checked", "1")?;
            return Ok(());
        }
        let dir = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
        std::fs::write(
            dir.path().join(".env"),
            "CSGATEWAY_AUTH_REQUIRED=false\nCSGATEWAY_DOTENV_PROBE=from-dotenv\n",
        )
        .map_err(|e| format!("write .env: {e}"))?;
        let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let out = Command::new(exe)
            .args([
                "settings::tests::cwd_dotenv_cannot_disable_auth",
                "--exact",
                "--test-threads=1",
            ])
            .current_dir(dir.path())
            .env_clear()
            .env("HOME", home)
            .env("CSGATEWAY_DOTENV_CHILD", "1")
            .output()
            .map_err(|e| format!("spawn: {e}"))?;
        if !out.status.success() || !dir.path().join("dotenv-checked").is_file() {
            return Err(format!(
                "child status {}\nstdout:\n{}\nstderr:\n{}",
                out.status,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
            .into());
        }
        Ok(())
    }
}
