use std::path::PathBuf;

use serde_json::json;

use crate::{Result, invalid};

const NAME: &str = "com.contextstack.browser";

#[cfg(unix)]
#[derive(serde::Deserialize)]
pub(crate) struct Settings {
    pub(crate) extension_id: String,
    pub(crate) policy: Option<PathBuf>,
    #[serde(default)]
    pub(crate) development: bool,
    pub(crate) action_key_file: Option<PathBuf>,
}

pub(crate) fn extension_origin(id: &str) -> Result<String> {
    if id.len() != 32 || !id.bytes().all(|byte| (b'a'..=b'p').contains(&byte)) {
        return Err(invalid("extension ID must contain 32 letters in a–p").into());
    }
    Ok(format!("chrome-extension://{id}/"))
}

pub(crate) fn generate(args: &[String]) -> Result<()> {
    let [os, id, executable] = args else {
        return Err(invalid("usage: browser-host manifest <macos|linux|windows> <extension-id> <absolute binary path>").into());
    };
    let destination = match os.as_str() {
        "macos" => {
            format!("~/Library/Application Support/Google/Chrome/NativeMessagingHosts/{NAME}.json")
        }
        "linux" => format!("~/.config/google-chrome/NativeMessagingHosts/{NAME}.json"),
        "windows" => format!(
            "Set the default REG_SZ value of HKCU\\Software\\Google\\Chrome\\NativeMessagingHosts\\{NAME} to the absolute path of the saved manifest JSON."
        ),
        _ => return Err(invalid("unknown manifest platform").into()),
    };
    let absolute = if os == "windows" {
        let bytes = executable.as_bytes();
        executable.starts_with("\\\\")
            || (bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && matches!(bytes[2], b'\\' | b'/'))
    } else {
        PathBuf::from(executable).has_root() && executable.starts_with('/')
    };
    if !absolute {
        return Err(invalid("binary path must be absolute for the target OS").into());
    }
    let manifest = json!({
        "name": NAME,
        "description": "ContextStack browser native messaging host",
        "path": executable,
        "type": "stdio",
        "allowed_origins": [extension_origin(id)?],
    });
    eprintln!("{destination}");
    println!("{}", serde_json::to_string_pretty(&manifest)?);
    Ok(())
}
