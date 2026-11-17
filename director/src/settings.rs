//! Command-line flags, each with an environment fallback, parsed once.
//!
//! | Flag | Environment | Default |
//! | --- | --- | --- |
//! | `--data-dir DIR` | `DIRECTOR_DATA_DIR` | `~/.contextstack/state/director` |
//! | `--listen ADDR` (repeatable) | `DIRECTOR_LISTEN` (comma-separated) | `127.0.0.1:8780` |
//! | `--tls-cert PEM`, `--tls-key PEM` | `DIRECTOR_TLS_CERT`, `DIRECTOR_TLS_KEY` | none: plain HTTP |
//! | `--allow-origin ORIGIN` (repeatable) | `DIRECTOR_ALLOWED_ORIGINS` (comma-separated) | none |
//! | `--stale-after SECONDS` | `DIRECTOR_STALE_AFTER` | 300 |
//! | `--context-window MODEL=TOKENS` (repeatable) | `DIRECTOR_CONTEXT_WINDOWS` (comma-separated) | the table in `thread.rs` |
//! | `--compact-at FRACTION` | `DIRECTOR_COMPACT_AT` | 0.6 |
//! | `--gateway URL` (loopback only) | `DIRECTOR_GATEWAY` | `http://127.0.0.1:9200` |
//! | `--gateway-owner-token FILE` | `DIRECTOR_GATEWAY_OWNER_TOKEN` | `~/.contextstack/config/gateway/owner-token` |
//! | `--reservation-stale-after SECONDS` | `DIRECTOR_RESERVATION_STALE_AFTER` | 3600 |
//! | `--reservation-timeout SECONDS` | `DIRECTOR_RESERVATION_TIMEOUT` | 86400 |

use std::collections::BTreeMap;
use std::path::PathBuf;

/// The default listen address: loopback only.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8780";

/// Parsed arguments: positionals and `--flag value` / `--flag=value` pairs (repeatable).
#[derive(Debug, Default)]
pub struct Args {
    /// Positional words, in order.
    pub words: Vec<String>,
    flags: BTreeMap<String, Vec<String>>,
}

/// Flags that take no value.
const SWITCHES: [&str; 1] = ["help"];

impl Args {
    /// Parse `args` (without the program name).
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut out = Self::default();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let Some(flag) = a.strip_prefix("--") else {
                out.words.push(a.clone());
                continue;
            };
            let (name, value) = match flag.split_once('=') {
                Some((n, v)) => (n.to_string(), v.to_string()),
                None if SWITCHES.contains(&flag) => (flag.to_string(), String::new()),
                None => (
                    flag.to_string(),
                    it.next()
                        .cloned()
                        .ok_or_else(|| format!("--{flag} needs a value"))?,
                ),
            };
            out.flags.entry(name).or_default().push(value);
        }
        Ok(out)
    }

    /// The last value of a flag, else the environment variable, else `None`.
    pub fn one(&self, flag: &str, env: &str) -> Option<String> {
        self.flags
            .get(flag)
            .and_then(|v| v.last().cloned())
            .or_else(|| std::env::var(env).ok().filter(|v| !v.trim().is_empty()))
    }

    /// Every value of a flag, else the comma-separated environment variable.
    pub fn many(&self, flag: &str, env: &str) -> Vec<String> {
        match self.flags.get(flag) {
            Some(v) => v.clone(),
            None => std::env::var(env)
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect(),
        }
    }

    /// Whether a switch was given.
    pub fn has(&self, flag: &str) -> bool {
        self.flags.contains_key(flag)
    }

    /// Flags given that are not in `known`.
    pub fn unknown(&self, known: &[&str]) -> Option<String> {
        self.flags
            .keys()
            .find(|k| !known.contains(&k.as_str()) && !SWITCHES.contains(&k.as_str()))
            .map(|k| format!("--{k}"))
    }

    /// The data directory.
    pub fn data_dir(&self) -> Result<PathBuf, String> {
        if let Some(d) = self.one("data-dir", "DIRECTOR_DATA_DIR") {
            return Ok(PathBuf::from(d));
        }
        let home = std::env::var("HOME").map_err(|_| "HOME is unset; pass --data-dir")?;
        Ok(PathBuf::from(home).join(".contextstack/state/director"))
    }
}

/// What `serve` needs.
#[derive(Debug, Clone)]
pub struct Settings {
    /// The data directory.
    pub data_dir: PathBuf,
    /// Addresses to listen on.
    pub listen: Vec<String>,
    /// TLS certificate chain and key, PEM.
    pub tls: Option<(PathBuf, PathBuf)>,
    /// Browser origins allowed to call.
    pub allowed_origins: Vec<String>,
    /// A node not heard from for this long is shown as stale.
    pub stale_after_seconds: i64,
    /// Conversation compaction: context windows and the share of a window a thread may fill.
    pub compaction: crate::thread::Settings,
    /// The local model gateway, for source budgets.
    pub gateway: crate::gateway::Gateway,
    /// When a held reservation closes though its job has not ended.
    pub reservations: crate::reservations::Bounds,
}

/// A positive number of seconds from a flag, else `default`.
fn seconds(args: &Args, flag: &str, env: &str, default: i64) -> Result<i64, String> {
    match args.one(flag, env) {
        None => Ok(default),
        Some(s) => s
            .parse::<i64>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| format!("--{flag} {s:?}: want a positive number of seconds")),
    }
}

impl Settings {
    /// Settings for `serve` from its arguments.
    pub fn from_args(args: &Args) -> Result<Self, String> {
        let mut listen = args.many("listen", "DIRECTOR_LISTEN");
        if listen.is_empty() {
            listen.push(DEFAULT_LISTEN.into());
        }
        let tls = match (
            args.one("tls-cert", "DIRECTOR_TLS_CERT"),
            args.one("tls-key", "DIRECTOR_TLS_KEY"),
        ) {
            (Some(c), Some(k)) => Some((PathBuf::from(c), PathBuf::from(k))),
            (None, None) => None,
            _ => return Err("--tls-cert and --tls-key go together".into()),
        };
        let stale_after_seconds = seconds(args, "stale-after", "DIRECTOR_STALE_AFTER", 300)?;
        let bounds = crate::reservations::Bounds::default();
        let reservations = crate::reservations::Bounds {
            node_stale: seconds(
                args,
                "reservation-stale-after",
                "DIRECTOR_RESERVATION_STALE_AFTER",
                bounds.node_stale,
            )?,
            timeout: seconds(
                args,
                "reservation-timeout",
                "DIRECTOR_RESERVATION_TIMEOUT",
                bounds.timeout,
            )?,
        };
        let mut windows = Vec::new();
        for pair in args.many("context-window", "DIRECTOR_CONTEXT_WINDOWS") {
            let parsed = pair
                .split_once('=')
                .and_then(|(m, t)| Some((m.to_string(), t.parse::<i64>().ok().filter(|n| *n > 0)?)))
                .filter(|(m, _)| !m.is_empty());
            windows.push(
                parsed.ok_or_else(|| format!("--context-window {pair:?}: want MODEL=TOKENS"))?,
            );
        }
        let compact_at = match args.one("compact-at", "DIRECTOR_COMPACT_AT") {
            None => crate::thread::COMPACT_AT,
            Some(f) => f
                .parse::<f64>()
                .ok()
                .filter(|x| *x > 0.0 && *x < 1.0)
                .ok_or_else(|| format!("--compact-at {f:?}: want a fraction between 0 and 1"))?,
        };
        let token_file = match args.one("gateway-owner-token", "DIRECTOR_GATEWAY_OWNER_TOKEN") {
            Some(f) => PathBuf::from(f),
            None => PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join(".contextstack/config/gateway/owner-token"),
        };
        let gateway = crate::gateway::Gateway::new(
            &args
                .one("gateway", "DIRECTOR_GATEWAY")
                .unwrap_or_else(|| crate::gateway::DEFAULT_URL.into()),
            token_file,
        )?;
        Ok(Self {
            gateway,
            reservations,
            compaction: crate::thread::Settings {
                windows,
                compact_at,
            },
            data_dir: args.data_dir()?,
            listen,
            tls,
            allowed_origins: args.many("allow-origin", "DIRECTOR_ALLOWED_ORIGINS"),
            stale_after_seconds,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Res = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn flags_and_words() -> Res {
        let argv: Vec<String> = [
            "node",
            "register",
            "node-a",
            "--data-dir",
            "/d",
            "--listen=a:1",
            "--listen",
            "b:2",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        let a = Args::parse(&argv)?;
        assert_eq!(a.words, ["node", "register", "node-a"]);
        assert_eq!(a.data_dir()?, PathBuf::from("/d"));
        assert_eq!(a.many("listen", "DIRECTOR_TEST_UNSET"), ["a:1", "b:2"]);
        assert_eq!(a.unknown(&["data-dir"]), Some("--listen".into()));
        assert!(Args::parse(&["--data-dir".to_string()]).is_err());
        let s = Settings::from_args(&Args::parse(&[
            "--data-dir=/d".to_string(),
            "--tls-cert=x".to_string(),
        ])?);
        assert!(s.is_err(), "cert without key");
        Ok(())
    }
}
