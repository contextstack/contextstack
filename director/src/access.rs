//! Access without login: nodes, local credentials, pairing, and browser origins.
//!
//! - A **node** holds a node token (`csn_…`), issued once by `director-rs node register`.
//! - The **local credential** (`csl_…`) is created when the data directory is first opened and kept in
//!   `<data>/local-credential` (0600). Clients on this machine read it; holding it makes a client the
//!   owner (pairing, revoking).
//! - A **device** (`csd_…`) pairs once: the owner issues a one-time code on a trusted machine, and the
//!   device exchanges it at `POST /api/v1/pair`. Without a live code that endpoint creates nothing.
//!
//! Only SHA-256 hashes of tokens and codes are stored. Every credential can be revoked.
//! The transport rules (who may connect at all) and the browser-origin rule are here too.

use crate::store::{Store, write_private};
use crate::util::{self, now_utc, sha256_hex};
use rusqlite::{OptionalExtension, params};
use std::net::IpAddr;

/// How long an issued pairing code stays usable.
pub const PAIRING_TTL_SECONDS: i64 = 600;
/// Failed code exchanges after which every outstanding code is voided.
const PAIRING_MAX_FAILURES: i64 = 5;

/// What a client credential is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// This machine's own credential: the owner.
    Local,
    /// A paired device: reads and writes work; not pairing or revoking.
    Device,
    /// A hosted lead bound to one conversation and its estate node.
    Lead,
}

/// An authenticated client.
#[derive(Debug, Clone)]
pub struct Client {
    /// The credential's id (`cred_…`).
    pub id: String,
    /// Local, device or hosted lead.
    pub kind: Kind,
    /// The conversation a lead can read; absent for other credential kinds.
    pub conversation_id: Option<String>,
    /// The optional intent restricting a lead's work, plus its own submissions.
    pub intent_id: Option<String>,
    /// The estate node a lead can submit and inspect work on.
    pub node: Option<String>,
}

/// The file local clients read their credential from.
pub fn local_credential_path(store: &Store) -> std::path::PathBuf {
    store.dir.join("local-credential")
}

/// Make sure a live local credential exists and its file holds it: create both on first start, and
/// issue a fresh one when the file is missing or names a revoked credential.
pub fn ensure_local_credential(store: &Store) -> Result<(), String> {
    let path = local_credential_path(store);
    if let Ok(text) = std::fs::read_to_string(&path) {
        let live: Option<String> = store
            .db()
            .query_row(
                "SELECT id FROM credentials WHERE token_hash = ?1 AND kind = 'local' AND revoked_at IS NULL",
                params![sha256_hex(text.trim().as_bytes())],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if live.is_some() {
            return Ok(());
        }
    }
    let token = util::secret("csl_")?;
    let id = insert_credential(store, Kind::Local, "local", &token)?;
    write_private(&path, format!("{token}\n").as_bytes())?;
    crate::logf!("local credential {id} written to {} (0600)", path.display());
    Ok(())
}

fn insert_credential(store: &Store, kind: Kind, name: &str, token: &str) -> Result<String, String> {
    let id = format!("cred_{}", util::ulid()?.to_lowercase());
    let kind = match kind {
        Kind::Local => "local",
        Kind::Device => "device",
        Kind::Lead => return Err("lead credentials require a conversation binding".into()),
    };
    store
        .db()
        .execute(
            "INSERT INTO credentials (id, kind, name, token_hash, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, kind, name, sha256_hex(token.as_bytes()), now_utc()],
        )
        .map_err(|e| e.to_string())?;
    Ok(id)
}

/// The bearer token in an `Authorization` header value: 1–512 non-space characters.
pub fn bearer(header: Option<&str>) -> Option<&str> {
    let token = header?.strip_prefix("Bearer ")?;
    (!token.is_empty() && token.len() <= 512 && !token.chars().any(char::is_whitespace))
        .then_some(token)
}

/// The live client credential a token names, noting its use.
pub fn client(store: &Store, token: &str) -> Result<Option<Client>, String> {
    let db = store.db();
    let found = db
        .query_row(
            "SELECT id, kind, conversation_id, intent_id, node FROM credentials WHERE token_hash = ?1 AND revoked_at IS NULL",
            params![sha256_hex(token.as_bytes())],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?,
                     r.get::<_, Option<String>>(2)?, r.get::<_, Option<String>>(3)?,
                     r.get::<_, Option<String>>(4)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let Some((id, kind, conversation_id, intent_id, node)) = found else {
        return Ok(None);
    };
    let kind = match kind.as_str() {
        "local" => Kind::Local,
        "device" => Kind::Device,
        "lead" if conversation_id.is_some() && node.is_some() => Kind::Lead,
        _ => return Ok(None),
    };
    db.execute(
        "UPDATE credentials SET last_used_at = ?1 WHERE id = ?2",
        params![now_utc(), id],
    )
    .map_err(|e| e.to_string())?;
    Ok(Some(Client {
        id,
        kind,
        conversation_id,
        intent_id,
        node,
    }))
}

/// Mint a hosted lead's token once, persisting only its hash and validated bindings.
/// The caller must enforce the owner-only Client API boundary.
pub fn mint_lead(
    store: &Store,
    name: &str,
    conversation_id: &str,
    intent_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    if !util::plain_name(name, 100) {
        return Err("the lead name must match [A-Za-z0-9][A-Za-z0-9._-]{0,99}".into());
    }
    let db = store.db();
    let node: String = db
        .query_row(
            "SELECT node FROM conversations WHERE conversation_id = ?1",
            params![conversation_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?
        .ok_or("no such conversation")?;
    if let Some(intent) = intent_id {
        let valid = db
            .query_row(
                "SELECT 1 FROM intents WHERE intent_id = ?1 AND node = ?2",
                params![intent, node],
                |_| Ok(()),
            )
            .optional()
            .map_err(|e| e.to_string())?
            .is_some();
        if !valid {
            return Err("the intent must exist on the conversation's node".into());
        }
    }
    let token = util::secret("cslead_")?;
    let id = format!("cred_{}", util::ulid()?.to_lowercase());
    db.execute(
        "INSERT INTO credentials (id, kind, name, token_hash, created_at, conversation_id, intent_id, node)
         VALUES (?1, 'lead', ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, name, sha256_hex(token.as_bytes()), now_utc(), conversation_id, intent_id, node],
    ).map_err(|e| e.to_string())?;
    Ok(
        serde_json::json!({"credential_id": id, "kind": "lead", "name": name, "token": token,
                          "conversation_id": conversation_id, "intent_id": intent_id, "node": node}),
    )
}

/// The live node a token names (its row id and name), recording that it was heard from.
pub fn node(store: &Store, token: &str) -> Result<Option<(i64, String)>, String> {
    let db = store.db();
    let found: Option<(i64, String)> = db
        .query_row(
            "SELECT id, name FROM nodes WHERE token_hash = ?1 AND revoked_at IS NULL",
            params![sha256_hex(token.as_bytes())],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    if let Some((id, _)) = &found {
        db.execute(
            "UPDATE nodes SET last_seen_at = ?1 WHERE id = ?2",
            params![now_utc(), id],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(found)
}

/// Register a node: its token, returned once, and only the hash kept.
pub fn register_node(store: &Store, name: &str) -> Result<String, String> {
    if !util::plain_name(name, 100) {
        return Err("the node name must match [A-Za-z0-9][A-Za-z0-9._-]{0,99}".into());
    }
    let token = util::secret("csn_")?;
    let inserted = store
        .db()
        .execute(
            "INSERT OR IGNORE INTO nodes (name, token_hash, created_at) VALUES (?1, ?2, ?3)",
            params![name, sha256_hex(token.as_bytes()), now_utc()],
        )
        .map_err(|e| e.to_string())?;
    if inserted == 0 {
        return Err(format!("a node named {name} already exists"));
    }
    crate::lead_node::note_local_node(&store.db(), name).map_err(|e| e.to_string())?;
    crate::store::touch(&store.db(), crate::store::Record::Node, name)
        .map_err(|e| e.to_string())?;
    Ok(token)
}

/// Revoke a node's token. Its jobs and runs stay on record.
pub fn revoke_node(store: &Store, name: &str) -> Result<(), String> {
    let changed = store
        .db()
        .execute(
            "UPDATE nodes SET revoked_at = ?1 WHERE name = ?2 AND revoked_at IS NULL",
            params![now_utc(), name],
        )
        .map_err(|e| e.to_string())?;
    if changed == 0 {
        return Err(format!("no live node named {name}"));
    }
    crate::store::touch(&store.db(), crate::store::Record::Node, name)
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Revoke a client credential. Revoking the local one issues a fresh local credential at once.
pub fn revoke_credential(store: &Store, id: &str) -> Result<(), String> {
    let changed = store
        .db()
        .execute(
            "UPDATE credentials SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
            params![now_utc(), id],
        )
        .map_err(|e| e.to_string())?;
    if changed == 0 {
        return Err(format!("no live credential {id}"));
    }
    ensure_local_credential(store)
}

/// Every client credential, for the owner: no hashes.
pub fn credentials(store: &Store) -> Result<Vec<serde_json::Value>, String> {
    let db = store.db();
    let mut stmt = db
        .prepare("SELECT id, kind, name, created_at, last_used_at, revoked_at, conversation_id, intent_id, node FROM credentials ORDER BY rowid")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?,
                "name": r.get::<_, String>(2)?, "created_at": r.get::<_, String>(3)?,
                "last_used_at": r.get::<_, Option<String>>(4)?,
                "revoked_at": r.get::<_, Option<String>>(5)?,
                "conversation_id": r.get::<_, Option<String>>(6)?,
                "intent_id": r.get::<_, Option<String>>(7)?,
                "node": r.get::<_, Option<String>>(8)?,
            }))
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<_, _>>().map_err(|e| e.to_string())
}

/// Issue a one-time pairing code for a device the owner names. Returns `(code, expires_at)`.
pub fn issue_pairing(store: &Store, name: &str) -> Result<(String, String), String> {
    if !util::plain_name(name, 100) {
        return Err("the device name must match [A-Za-z0-9][A-Za-z0-9._-]{0,99}".into());
    }
    let code = util::pairing_code()?;
    let expires = util::utc_in(PAIRING_TTL_SECONDS);
    let db = store.db();
    db.execute(
        "INSERT INTO pairings (code_hash, name, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
        params![
            sha256_hex(util::canonical_code(&code).as_bytes()),
            name,
            now_utc(),
            expires
        ],
    )
    .and_then(|_| db.execute("DELETE FROM meta WHERE key = 'pairing_failures'", []))
    .map_err(|e| e.to_string())?;
    Ok((code, expires))
}

/// Exchange a pairing code for a device token: `Ok(Some((credential_id, name, token)))`, or `Ok(None)`
/// when the code is unknown, used or expired. Too many failures void every outstanding code.
pub fn exchange_pairing(
    store: &Store,
    code: &str,
) -> Result<Option<(String, String, String)>, String> {
    let hash = sha256_hex(util::canonical_code(code).as_bytes());
    let now = now_utc();
    let name: Option<String> = {
        let db = store.db();
        let claimed = db
            .execute(
                "UPDATE pairings SET used_at = ?1 WHERE code_hash = ?2 AND used_at IS NULL AND expires_at > ?1",
                params![now, hash],
            )
            .map_err(|e| e.to_string())?;
        if claimed == 0 {
            let failures: i64 = db
                .query_row(
                    "INSERT INTO meta (key, value) VALUES ('pairing_failures', '1')
                     ON CONFLICT(key) DO UPDATE SET value = CAST(value AS INTEGER) + 1 RETURNING value",
                    [],
                    |r| r.get::<_, String>(0),
                )
                .map_err(|e| e.to_string())?
                .parse()
                .unwrap_or(PAIRING_MAX_FAILURES);
            if failures >= PAIRING_MAX_FAILURES {
                db.execute(
                    "UPDATE pairings SET expires_at = ?1 WHERE used_at IS NULL AND expires_at > ?1",
                    params![now],
                )
                .map_err(|e| e.to_string())?;
                crate::logf!("pairing: {failures} failed codes; every outstanding code voided");
            }
            return Ok(None);
        }
        db.query_row(
            "SELECT name FROM pairings WHERE code_hash = ?1",
            params![hash],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?
    };
    let Some(name) = name else { return Ok(None) };
    let token = util::secret("csd_")?;
    let id = insert_credential(store, Kind::Device, &name, &token)?;
    store
        .db()
        .execute(
            "UPDATE pairings SET credential_id = ?1 WHERE code_hash = ?2",
            params![id, hash],
        )
        .map_err(|e| e.to_string())?;
    Ok(Some((id, name, token)))
}

/// Whether a connection from `ip` may be served at all. Loopback always; another machine only over
/// Tailscale (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`), whose own encryption carries it, or when TLS is
/// configured. Plain HTTP from another machine is refused, credentials or not.
pub fn admit(ip: IpAddr, tls: bool) -> bool {
    let ip = ip.to_canonical();
    tls || ip.is_loopback()
        || match ip {
            IpAddr::V4(v4) => {
                let [a, b, ..] = v4.octets();
                a == 100 && (64..128).contains(&b)
            }
            IpAddr::V6(v6) => {
                let s = v6.segments();
                s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
            }
        }
}

/// Whether a request came from a browser page this Director does not recognise. A browser names the
/// page's origin in `Origin` (and its relation in `Sec-Fetch-Site`); programs send neither. A request
/// naming an origin outside `allowed` is refused, whatever credential it carries.
pub fn foreign_origin(origin: Option<&str>, fetch_site: Option<&str>, allowed: &[String]) -> bool {
    let known = origin.is_some_and(|o| allowed.iter().any(|a| a == o));
    if known {
        return false;
    }
    origin.is_some() || fetch_site.is_some_and(|s| !matches!(s, "same-origin" | "none"))
}

#[cfg(test)]
mod tests {
    use super::*;

    type Res = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn lead_binding_is_validated_persistent_and_revocable() -> Res {
        let tmp = tempfile::tempdir()?;
        let dir = tmp.path().join("data");
        let store = Store::open(&dir)?;
        store.db().execute(
            "INSERT INTO conversations (conversation_id, node, spec, spec_version, model,
             instructions_tokens, tools, created_by, created_at)
             VALUES ('conv_test', 'estate', 'lead', '1', 'test', 10, 1, 'owner', 'now')",
            [],
        )?;
        for (id, node) in [("intent_here", "estate"), ("intent_other", "other")] {
            store.db().execute(
                "INSERT INTO intents (intent_id, node, spec, task, trials, meta, bundle_sha256,
                 created_by, created_at) VALUES (?1, ?2, 's', 't', 1, '{}', 'sha', 'owner', 'now')",
                params![id, node],
            )?;
        }
        assert!(mint_lead(&store, "lead", "missing", None).is_err());
        assert!(mint_lead(&store, "lead", "conv_test", Some("missing")).is_err());
        assert!(mint_lead(&store, "lead", "conv_test", Some("intent_other")).is_err());
        assert!(mint_lead(&store, "bad name", "conv_test", None).is_err());
        let minted = mint_lead(&store, "lead", "conv_test", Some("intent_here"))?;
        let token = minted["token"].as_str().ok_or("token")?;
        assert!(token.starts_with("cslead_"));
        let hash: String = store.db().query_row(
            "SELECT token_hash FROM credentials WHERE id = ?1",
            params![minted["credential_id"].as_str()],
            |r| r.get(0),
        )?;
        assert_eq!(hash, sha256_hex(token.as_bytes()));
        drop(store);
        let store = Store::open(&dir)?;
        let lead = client(&store, token)?.ok_or("lead authenticated")?;
        assert_eq!(lead.kind, Kind::Lead);
        assert_eq!(lead.conversation_id.as_deref(), Some("conv_test"));
        assert_eq!(lead.intent_id.as_deref(), Some("intent_here"));
        assert_eq!(lead.node.as_deref(), Some("estate"));
        revoke_credential(&store, &lead.id)?;
        assert!(client(&store, token)?.is_none());
        let unscoped = mint_lead(&store, "lead2", "conv_test", None)?;
        let unscoped = client(&store, unscoped["token"].as_str().ok_or("token")?)?
            .ok_or("unscoped lead authenticated")?;
        assert_eq!(unscoped.intent_id, None);
        Ok(())
    }

    #[test]
    fn unknown_kinds_and_unbound_leads_do_not_authenticate() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = Store::open(&tmp.path().join("data"))?;
        for kind in ["future", "lead"] {
            store.db().execute(
                "INSERT INTO credentials (id, kind, name, token_hash, created_at)
                 VALUES (?1, ?1, 'test', ?2, 'now')",
                params![kind, sha256_hex(kind.as_bytes())],
            )?;
            assert!(client(&store, kind)?.is_none());
        }
        Ok(())
    }

    #[test]
    fn transport_rule() -> Res {
        // Synthetic protocol boundary cases, independent of any installed machine.
        for (ip, plain, tls) in [
            ("127.0.0.1", true, true),
            ("::1", true, true),
            ("::ffff:127.0.0.1", true, true),
            (
                &std::net::Ipv4Addr::new(100, 64, 0, 1).to_string(),
                true,
                true,
            ),
            (
                &std::net::Ipv4Addr::new(100, 127, 255, 254).to_string(),
                true,
                true,
            ),
            ("100.128.0.1", false, true),
            ("100.63.255.255", false, true),
            (
                &std::net::Ipv6Addr::new(0xfd7a, 0x115c, 0xa1e0, 0, 0, 0, 0, 1).to_string(),
                true,
                true,
            ),
            ("192.0.2.20", false, true),
            ("8.8.8.8", false, true),
        ] {
            let ip: IpAddr = ip.parse()?;
            assert_eq!(admit(ip, false), plain, "{ip} plain");
            assert_eq!(admit(ip, true), tls, "{ip} tls");
        }
        Ok(())
    }

    #[test]
    fn origin_rule() {
        let allowed = vec!["http://127.0.0.1:8740".to_string()];
        assert!(!foreign_origin(None, None, &allowed), "a program");
        assert!(foreign_origin(Some("https://evil.example"), None, &allowed));
        assert!(foreign_origin(Some("null"), None, &allowed));
        assert!(!foreign_origin(
            Some("http://127.0.0.1:8740"),
            Some("cross-site"),
            &allowed
        ));
        assert!(foreign_origin(None, Some("cross-site"), &allowed));
        assert!(!foreign_origin(None, Some("none"), &allowed));
    }

    #[test]
    fn bearer_shape() {
        assert_eq!(bearer(Some("Bearer abc")), Some("abc"));
        assert_eq!(bearer(Some("Bearer ")), None);
        assert_eq!(bearer(Some("Bearer a b")), None);
        assert_eq!(bearer(Some("Basic abc")), None);
        assert_eq!(bearer(None), None);
    }

    #[test]
    fn pairing_is_one_time_and_guarded() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = Store::open(&tmp.path().join("data"))?;
        assert!(
            exchange_pairing(&store, "AAAAA-AAAAA")?.is_none(),
            "no code, nothing"
        );
        let (code, _) = issue_pairing(&store, "phone")?;
        let (id, name, token) = exchange_pairing(&store, &code.to_lowercase())?.ok_or("paired")?;
        assert_eq!(name, "phone");
        assert!(token.starts_with("csd_"));
        assert!(exchange_pairing(&store, &code)?.is_none(), "used once");
        assert_eq!(client(&store, &token)?.map(|c| c.kind), Some(Kind::Device));
        revoke_credential(&store, &id)?;
        assert!(client(&store, &token)?.is_none(), "revoked");

        let (code, _) = issue_pairing(&store, "laptop")?;
        for _ in 0..PAIRING_MAX_FAILURES {
            assert!(exchange_pairing(&store, "ZZZZZ-ZZZZZ")?.is_none());
        }
        assert!(
            exchange_pairing(&store, &code)?.is_none(),
            "voided by guessing"
        );
        Ok(())
    }

    #[test]
    fn local_credential_rotates_when_revoked() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = Store::open(&tmp.path().join("data"))?;
        ensure_local_credential(&store)?;
        let path = local_credential_path(&store);
        let first = std::fs::read_to_string(&path)?.trim().to_string();
        ensure_local_credential(&store)?;
        assert_eq!(
            std::fs::read_to_string(&path)?.trim(),
            first,
            "kept across starts"
        );
        let c = client(&store, &first)?.ok_or("live")?;
        assert_eq!(c.kind, Kind::Local);
        revoke_credential(&store, &c.id)?;
        assert!(client(&store, &first)?.is_none());
        let second = std::fs::read_to_string(&path)?.trim().to_string();
        assert_ne!(first, second);
        assert!(client(&store, &second)?.is_some());
        Ok(())
    }
}
