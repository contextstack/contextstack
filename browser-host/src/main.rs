//! Chrome native messaging host for the `ContextStack` browser broker contract.
use std::{env, error::Error, io, process::ExitCode};

#[cfg(unix)]
mod action;
#[cfg(unix)]
mod broker;
#[cfg(unix)]
mod native;
mod registration;

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

fn main() -> ExitCode {
    let args: Vec<_> = env::args().skip(1).collect();
    let result = if args.first().is_some_and(|arg| arg == "manifest") {
        registration::generate(&args[1..])
    } else {
        serve(&args)
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("browser-host: {error}");
            ExitCode::FAILURE
        }
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(unix)]
fn serve(args: &[String]) -> Result<()> {
    let state =
        std::path::PathBuf::from(env::var_os("HOME").ok_or("HOME is unset")?).join(".contextstack");
    let mut startup_step = "settings";
    let result = serve_configured(args, &state, &mut startup_step);
    if result.is_err() {
        let _ = broker::startup_failure(&state.join("browser"), startup_step);
    }
    result
}

#[cfg(unix)]
fn serve_configured(
    args: &[String],
    state: &std::path::Path,
    startup_step: &mut &'static str,
) -> Result<()> {
    let mut settings: registration::Settings =
        serde_json::from_reader(std::fs::File::open(state.join("browser/native-host.json"))?)?;
    let expected = registration::extension_origin(&settings.extension_id)?;
    *startup_step = "caller";
    if args.first() != Some(&expected) {
        return Err(invalid("unpinned extension caller").into());
    }
    *startup_step = "policy";
    let installed = state.join("current/cs/policy/browser-native.json");
    let path = settings.policy.take().unwrap_or(installed);
    let path = path.canonicalize()?;
    if !settings.development
        && !["browser", "config", "releases"]
            .iter()
            .filter_map(|name| state.join(name).canonicalize().ok())
            .any(|root| path.starts_with(root))
    {
        return Err(invalid(
            "browser policy must be in the installed release or owner configuration",
        )
        .into());
    }
    settings.policy = Some(path);
    broker::serve(&state.join("browser"), &settings, startup_step)
}

#[cfg(not(unix))]
fn serve(_args: &[String]) -> Result<()> {
    Err(invalid("the browser broker requires Unix sockets on a Unix host").into())
}
