use std::{
    fs::{File, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, TimeDelta, Utc};
use ring::{
    hmac,
    rand::{SecureRandom, SystemRandom},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{Result, invalid};

// The Director keeps the review copy and caps proposals at seven days. The
// host can discard its staging copy once even the latest decision has aged
// another seven days.
const SCREENSHOT_RETENTION: std::time::Duration = std::time::Duration::from_hours(14 * 24);

pub(crate) fn prune_screenshots(
    directory: &Path,
    now: std::time::SystemTime,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let filename = entry.file_name();
        let name = filename.as_encoded_bytes();
        if name.len() != 64 || !name.iter().all(u8::is_ascii_hexdigit) {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.is_file()
            && now
                .duration_since(metadata.modified()?)
                .is_ok_and(|age| age >= SCREENSHOT_RETENTION)
        {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

pub(crate) struct Claims {
    key: hmac::Key,
    used: PathBuf,
}

impl Claims {
    pub(crate) fn authorize_capture(&self, request: &Value) -> Result<()> {
        let bad = || invalid("browser capture requires host authorization");
        let mut request = request.clone();
        let proof = request
            .as_object_mut()
            .and_then(|fields| fields.remove("host_proof"))
            .and_then(|proof| hex::decode(proof.as_str()?).ok())
            .ok_or_else(bad)?;
        let wire = json!(["contextstack-browser-capture-request-v1", request]).to_string();
        hmac::verify(&self.key, wire.as_bytes(), &proof).map_err(|_| bad())?;
        Ok(())
    }

    pub(crate) fn capture(&self, mut data: Value, image: &str, directory: &Path) -> Result<Value> {
        let image = image
            .strip_prefix("data:image/jpeg;base64,")
            .filter(|image| image.len() <= 4 * 1024 * 1024)
            .ok_or_else(|| invalid("invalid or oversized browser screenshot"))?;
        let bytes = STANDARD.decode(image)?;
        if !bytes.starts_with(&[0xff, 0xd8, 0xff]) {
            return Err(invalid("invalid browser screenshot").into());
        }
        let sha = hex::encode(Sha256::digest(&bytes));
        let mut nonce = [0; 12];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| invalid("could not generate screenshot nonce"))?;
        let temp = directory.join(hex::encode(nonce));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        let saved = (|| -> std::io::Result<()> {
            use std::io::Write;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&temp, directory.join(&sha))
        })();
        if saved.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        saved?;
        data["screenshot_sha"] = json!(sha);
        data["captured_at"] = json!(Utc::now().to_rfc3339());
        let proof = hmac::sign(
            &self.key,
            json!(["contextstack-browser-capture-v1", data])
                .to_string()
                .as_bytes(),
        );
        Ok(json!({"ok":true,"capture":data,"capture_proof":hex::encode(proof.as_ref())}))
    }

    pub(crate) fn load(secret: &Path, used: PathBuf) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
            .open(secret)?;
        let meta = file.metadata()?;
        if !meta.is_file()
            || meta.uid() != rustix::process::getuid().as_raw()
            || meta.mode() & 0o077 != 0
        {
            return Err(invalid("browser action key must be a private owner file").into());
        }
        let mut token = String::new();
        file.take(4097).read_to_string(&mut token)?;
        let key = hex::decode(token.trim())
            .ok()
            .filter(|key| key.len() == 32)
            .ok_or_else(|| invalid("invalid browser action key"))?;
        Ok(Self {
            key: hmac::Key::new(hmac::HMAC_SHA256, &key),
            used,
        })
    }

    pub(crate) fn consume(&self, approval: &Value, origin: &str) -> Result<Value> {
        let bad = || invalid("browser action requires a verified claim");
        let id = approval["id"].as_str().ok_or_else(bad)?;
        let date = |name| {
            approval[name]
                .as_str()
                .and_then(|date| DateTime::parse_from_rfc3339(date).ok())
                .ok_or_else(bad)
        };
        let decided = date("decided_at")?;
        let claimed = date("claimed_at")?;
        let now = Utc::now();
        if id.len() != 26
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
            || approval["kind"] != "browser_action"
            || approval["origin"] != origin
            || approval["status"] != "approved"
            || approval["outcome"]["status"] != "unknown"
            || !matches!(approval["action"].as_str(), Some("submit" | "post" | "buy"))
            || claimed < decided
            || now.signed_duration_since(decided) >= TimeDelta::minutes(5)
        {
            return Err(bad().into());
        }
        let fields = json!([
            "contextstack-browser-action-v1",
            id,
            origin,
            approval["action"],
            approval["target"],
            approval["field_values_digest"],
            approval["screenshot_sha"],
            approval["decided_at"],
            approval["claimed_at"]
        ]);
        let proof =
            hex::decode(approval["claim_proof"].as_str().ok_or_else(bad)?).map_err(|_| bad())?;
        hmac::verify(&self.key, fields.to_string().as_bytes(), &proof).map_err(|_| bad())?;
        if claimed > now + TimeDelta::seconds(30) {
            return Err(crate::broker::Failure(
                "clock_skew",
                "Director claim time is ahead of this node",
            )
            .into());
        }
        for entry in std::fs::read_dir(&self.used)?.flatten() {
            if entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age.as_secs() > 600)
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.used.join(id))
            .map_err(|_| {
                invalid("browser action claim has already been consumed or could not be saved")
            })?
            .sync_all()?;
        File::open(&self.used)?.sync_all()?;
        let mut action = approval.clone();
        action
            .as_object_mut()
            .ok_or_else(bad)?
            .remove("claim_proof");
        Ok(action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prunes_old_screenshot_copies_and_preserves_recent_files() -> Result<()> {
        let folder = tempfile::tempdir()?;
        let old = folder.path().join("a".repeat(64));
        let recent = folder.path().join("b".repeat(64));
        let unrelated = folder.path().join("readme");
        let link = folder.path().join("c".repeat(64));
        let now = std::time::SystemTime::now();
        for (path, modified) in [
            (
                &old,
                now - SCREENSHOT_RETENTION - std::time::Duration::from_secs(1),
            ),
            (&recent, now),
            (&unrelated, now - SCREENSHOT_RETENTION),
        ] {
            File::create(path)?.set_modified(modified)?;
        }
        std::os::unix::fs::symlink(&unrelated, &link)?;
        prune_screenshots(folder.path(), now)?;
        assert!(!old.exists());
        assert!(recent.exists() && unrelated.exists() && link.is_symlink());
        Ok(())
    }

    #[test]
    fn proof_is_bound_to_the_request_and_consumed_once() -> Result<()> {
        let folder = tempfile::tempdir()?;
        let key = [23; 32];
        let claims = Claims {
            key: hmac::Key::new(hmac::HMAC_SHA256, &key),
            used: folder.path().to_owned(),
        };
        let id = "01ABCDEFGHIJKL0123456789AB";
        let origin = "https://example.test";
        let stamp = (Utc::now() + TimeDelta::seconds(2)).to_rfc3339();
        let digest = "a".repeat(64);
        let screenshot = "b".repeat(64);
        let wire = json!([
            "contextstack-browser-action-v1",
            id,
            origin,
            "submit",
            "#order",
            digest,
            screenshot,
            stamp,
            stamp
        ]);
        let proof = hmac::sign(&claims.key, wire.to_string().as_bytes());
        let mut row = json!({
            "id":id, "kind":"browser_action", "status":"approved", "outcome":{"status":"unknown"},
            "origin":origin, "action":"submit", "target":"#order", "field_values_digest":digest,
            "screenshot_sha":screenshot, "decided_at":stamp, "claimed_at":stamp,
        });
        assert!(claims.consume(&row, origin).is_err());
        row["claim_proof"] = json!(hex::encode(proof.as_ref()));
        let mut changed = row.clone();
        changed["target"] = json!("#other");
        assert!(claims.consume(&changed, origin).is_err());
        assert!(claims.consume(&row, "https://other.test").is_err());
        assert!(claims.consume(&row, origin)?.get("claim_proof").is_none());
        assert!(claims.consume(&row, origin).is_err());
        Ok(())
    }
}
