use std::io::{Read, Write};

use serde::Deserialize;
use serde_json::Value;

use crate::{Result, invalid};

const FROM_CHROME: usize = 64 * 1024 * 1024;
const TO_CHROME: usize = 1024 * 1024;

#[derive(Deserialize)]
pub(crate) struct Message {
    pub(crate) id: u64,
    #[serde(flatten)]
    pub(crate) body: Body,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Body {
    Navigation {
        event: u64,
        url: String,
        #[serde(default)]
        main_frame: bool,
        parent_origin: Option<String>,
    },
    Result {
        final_url: String,
        status: Option<u16>,
        title: String,
        text: String,
    },
    ActionResult {
        status: Option<u16>,
    },
    ActionSnapshot {
        digest: String,
    },
    ProposalSnapshot {
        digest: String,
        page_url: String,
        values_summary: Value,
        image: String,
    },
    Error {
        #[serde(default)]
        error_kind: Option<String>,
        message: Option<String>,
    },
    #[serde(alias = "logged_out")]
    SignInRequired,
    Status {
        origins: Vec<String>,
    },
}

pub(crate) fn read<T: serde::de::DeserializeOwned>(mut input: impl Read) -> Result<Option<T>> {
    let mut header = [0; 4];
    if input.read(&mut header[..1])? == 0 {
        return Ok(None);
    }
    input.read_exact(&mut header[1..])?;
    let size = u32::from_le_bytes(header) as usize;
    if size > FROM_CHROME {
        return Err(invalid("native frame exceeds Chrome-to-host size limit").into());
    }
    let mut bytes = vec![0; size];
    input.read_exact(&mut bytes)?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

pub(crate) fn write(mut output: impl Write, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > TO_CHROME {
        return Err(invalid("native frame exceeds host-to-Chrome size limit").into());
    }
    output.write_all(&u32::try_from(bytes.len())?.to_le_bytes())?;
    output.write_all(&bytes)?;
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{FROM_CHROME, TO_CHROME, read, write};

    #[test]
    fn round_trip_and_oversize() -> crate::Result<()> {
        let mut wire = Vec::new();
        let message = json!({"text":"café"});
        write(&mut wire, &message)?;
        assert_eq!(&wire[..4], &[16, 0, 0, 0]);
        assert_eq!(read::<Value>(&wire[..])?, Some(message));
        let size = u32::try_from(FROM_CHROME + 1)?.to_le_bytes();
        assert!(read::<Value>(&size[..]).is_err());
        assert!(write(&mut wire, &json!("x".repeat(TO_CHROME))).is_err());
        Ok(())
    }
}
