//! Access without login: nodes, local credentials, pairing, and browser origins.
//!
//! - A **node** holds a node token (`csn_…`), issued once by `director-rs node register`.
//! - The **local credential** (`csl_…`) is created when the data directory is first opened and kept in
//!   `<data>/local-credential` (0600). It is bound to the bootstrap owner.
//! - A **device** (`csd_…`) pairs once: a member issues a one-time code on a trusted machine, and the
//!   device exchanges it at `POST /api/v1/pair`. Without a live code that endpoint creates nothing.
//!
//! Only SHA-256 hashes of tokens and codes are stored. Every credential can be revoked.
//! The transport rules (who may connect at all) and the browser-origin rule are here too.

use crate::store::{OrgScope, ScopedConnection, Store, write_private};
use crate::util::{self, now_utc, sha256_hex};
use rusqlite::{OptionalExtension, params};
use std::net::IpAddr;

/// How long an issued pairing code stays usable.
pub const PAIRING_TTL_SECONDS: i64 = 600;
/// Failed code exchanges after which every outstanding code is voided.
const PAIRING_MAX_FAILURES: i64 = 5;

/// What a client credential is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// This machine's own credential: the owner.
    Local,
    /// A paired device acting for its member.
    Device,
    /// A loopback browser session.
    Browser { session_id: String },
    /// A hosted lead bound to one conversation and its estate node.
    Lead,
}

/// An authenticated client.
#[derive(Debug, Clone)]
pub struct Client {
    /// The actor recorded on writes. Bootstrap browser sessions retain the legacy local actor.
    pub id: String,
    /// The credential kind and, for browsers, the session ID.
    pub kind: Kind,
    pub member: Member,
    /// The conversation a lead can read; absent for other credential kinds.
    pub conversation_id: Option<String>,
    /// The optional intent restricting a lead's work, plus its own submissions.
    pub intent_id: Option<String>,
    /// The estate node a lead can submit and inspect work on.
    pub node: Option<String>,
}

impl Client {
    pub fn member_id(&self) -> &str {
        &self.member.scope.member_id
    }

    pub fn reload_member(&self, db: &ScopedConnection) -> rusqlite::Result<Option<Self>> {
        Ok(Member::load(db, self.member_id().to_owned())?
            .filter(Member::can_authenticate)
            .map(|member| Self {
                member,
                ..self.clone()
            }))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Viewer,
    Member,
    Admin,
    Owner,
}

impl Role {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "owner" => Some(Self::Owner),
            "admin" => Some(Self::Admin),
            "member" => Some(Self::Member),
            "viewer" => Some(Self::Viewer),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Member {
    pub scope: OrgScope,
    pub display_name: String,
    pub active: bool,
}

impl Member {
    pub fn load(db: &ScopedConnection, id: String) -> rusqlite::Result<Option<Self>> {
        let row = db.query_row(
            "SELECT display_name, role, status FROM members WHERE +org_id = :org_id AND id = ?1",
            [&id],
            |r| Ok((r.get(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
        ).optional()?;
        let Some((display_name, role, status)) = row else {
            return Ok(None);
        };
        let Some(role) = Role::parse(&role) else {
            return Ok(None);
        };
        Ok(Some(Self {
            scope: OrgScope {
                org_id: db.scope().org_id.clone(),
                member_id: id,
                role: Some(role),
            },
            display_name,
            active: status == "active",
        }))
    }

    pub fn can_authenticate(&self) -> bool {
        self.active
    }

    #[cfg(test)]
    pub fn fixture() -> Self {
        Self {
            scope: OrgScope {
                org_id: String::new(),
                member_id: String::new(),
                role: Some(Role::Owner),
            },
            display_name: "Owner".into(),
            active: true,
        }
    }
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
                "SELECT id FROM credentials WHERE +org_id = :org_id AND member_id = :member_id
                 AND token_hash = ?1 AND kind = 'local' AND revoked_at IS NULL",
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
    let db = store.db();
    let id = insert_credential(&db, &db.scope().member_id, "local", "local", &token)?;
    drop(db);
    write_private(&path, format!("{token}\n").as_bytes())?;
    crate::logf!("local credential {id} written to {} (0600)", path.display());
    Ok(())
}

fn insert_credential(
    db: &ScopedConnection,
    member_id: &str,
    kind: &str,
    name: &str,
    token: &str,
) -> Result<String, String> {
    let id = format!("cred_{}", util::ulid()?.to_lowercase());
    db.execute(
        "INSERT INTO credentials (org_id, member_id, id, kind, name, token_hash, created_at)
                 VALUES (:org_id, ?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            member_id,
            id,
            kind,
            name,
            sha256_hex(token.as_bytes()),
            now_utc()
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(id)
}

pub fn mint_device(
    db: &ScopedConnection,
    member: &str,
    name: &str,
) -> Result<(String, String), String> {
    let token = util::secret("csd_")?;
    let id = insert_credential(db, member, "device", name, &token)?;
    Ok((id, token))
}

/// The bearer token in an `Authorization` header value: 1–512 non-space characters.
pub fn bearer(header: Option<&str>) -> Option<&str> {
    let token = header?.strip_prefix("Bearer ")?;
    (!token.is_empty() && token.len() <= 512 && !token.chars().any(char::is_whitespace))
        .then_some(token)
}

/// The live client credential a token names, noting its use.
pub fn client(store: &Store, token: &str) -> Result<Option<Client>, String> {
    authenticate(store, token, true)
}

pub fn authenticate(
    store: &Store,
    token: &str,
    note_usage: bool,
) -> Result<Option<Client>, String> {
    let db = store.db();
    let found = db
        .query_row(
            "SELECT id, kind, conversation_id, intent_id, node, member_id FROM credentials WHERE +org_id = :org_id AND token_hash = ?1 AND revoked_at IS NULL",
            params![sha256_hex(token.as_bytes())],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?,
                     r.get::<_, Option<String>>(2)?, r.get::<_, Option<String>>(3)?,
                     r.get::<_, Option<String>>(4)?, r.get::<_, String>(5)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let Some((id, kind, conversation_id, intent_id, node, member_id)) = found else {
        return Ok(None);
    };
    let kind = match kind.as_str() {
        "local" if member_id == db.scope().member_id => Kind::Local,
        "device" => Kind::Device,
        "lead" if conversation_id.is_some() && node.is_some() => Kind::Lead,
        _ => return Ok(None),
    };
    let Some(member) = Member::load(&db, member_id)
        .map_err(|e| e.to_string())?
        .filter(Member::can_authenticate)
    else {
        return Ok(None);
    };
    if note_usage {
        db.execute(
            "UPDATE credentials SET last_used_at = ?1 WHERE +org_id = :org_id AND id = ?2
             AND revoked_at IS NULL",
            params![now_utc(), id],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(Some(Client {
        id,
        kind,
        member,
        conversation_id,
        intent_id,
        node,
    }))
}

/// Mint a hosted lead's token once, persisting only its hash and validated bindings.
/// The caller must enforce the route's sponsorship and target policy.
pub fn mint_lead(
    store: &Store,
    name: &str,
    conversation_id: &str,
    intent_id: Option<&str>,
    actor: &str,
) -> Result<serde_json::Value, String> {
    let mut db = store.db();
    let tx = crate::store::write_tx(&mut db).map_err(|e| e.to_string())?;
    let credential = mint_lead_in(&tx, name, conversation_id, intent_id, actor)?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(credential)
}

pub(crate) fn mint_lead_in(
    tx: &ScopedConnection,
    name: &str,
    conversation_id: &str,
    intent_id: Option<&str>,
    actor: &str,
) -> Result<serde_json::Value, String> {
    if !util::plain_name(name, 100) {
        return Err("the lead name must match [A-Za-z0-9][A-Za-z0-9._-]{0,99}".into());
    }
    let (node, member_id): (String, String) = tx
        .query_row(
            "SELECT node, COALESCE(sponsor_member_id, responsible_member_id, :member_id)
             FROM conversations WHERE +org_id = :org_id AND conversation_id = ?1",
            params![conversation_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?
        .ok_or("no such conversation")?;
    if !crate::policy::eligible_sponsor(tx, &member_id).map_err(|e| e.to_string())? {
        return Err("the conversation sponsor cannot delegate work".into());
    }
    if let Some(intent) = intent_id {
        let valid = tx
            .query_row(
                "SELECT 1 FROM intents WHERE +org_id = :org_id AND intent_id = ?1 AND node = ?2",
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
    tx.execute(
        "INSERT INTO credentials (org_id, member_id, id, kind, name, token_hash, created_at, conversation_id, intent_id, node, created_by)
         VALUES (:org_id, ?9, ?1, 'lead', ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![id, name, sha256_hex(token.as_bytes()), now_utc(), conversation_id, intent_id, node, actor, member_id],
    ).map_err(|e| e.to_string())?;
    Ok(
        serde_json::json!({"credential_id": id, "kind": "lead", "name": name, "token": token,
                          "conversation_id": conversation_id, "intent_id": intent_id, "node": node}),
    )
}

/// Machine authority is independent of the member who registered the node.
pub struct NodePrincipal {
    pub id: i64,
    pub name: String,
    pub registered_by: Member,
}

/// The live node a token names, recording that it was heard from.
pub fn node(store: &Store, token: &str) -> Result<Option<NodePrincipal>, String> {
    let db = store.db();
    let found: Option<(i64, String, String)> = db
        .query_row(
            "SELECT id, name, COALESCE(registered_by, :member_id) FROM nodes WHERE +org_id = :org_id AND token_hash = ?1 AND revoked_at IS NULL",
            params![sha256_hex(token.as_bytes())],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let Some((id, name, member_id)) = found else {
        return Ok(None);
    };
    let Some(registered_by) = Member::load(&db, member_id).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    db.execute(
        "UPDATE nodes SET last_seen_at = ?1 WHERE +org_id = :org_id AND (id = ?2)",
        params![now_utc(), id],
    )
    .map_err(|e| e.to_string())?;
    Ok(Some(NodePrincipal {
        id,
        name,
        registered_by,
    }))
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
            "INSERT OR IGNORE INTO nodes (org_id, registered_by, name, token_hash, created_at) VALUES (:org_id, :member_id, ?1, ?2, ?3)",
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
            "UPDATE nodes SET revoked_at = ?1 WHERE +org_id = :org_id AND (name = ?2 AND revoked_at IS NULL)",
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
pub fn revoke_credential(store: &Store, id: &str, actor: &str) -> Result<(), String> {
    let kind = {
        let mut db = store.db();
        let tx = crate::store::write_tx(&mut db).map_err(|e| e.to_string())?;
        let now = now_utc();
        let kind: String = tx.query_row(
            "UPDATE credentials SET revoked_at = ?1, revoked_by = ?3 WHERE +org_id = :org_id AND (id = ?2 AND revoked_at IS NULL
             ) RETURNING kind",
            params![now, id, actor],
            |row| row.get(0),
        ).optional().map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no live credential {id}"))?;
        if kind == "device" {
            void_device_pairings(&tx, id, &now).map_err(|e| e.to_string())?;
        }
        // The running Director also publishes changes written by the host CLI.
        tx.execute(
            "INSERT INTO changes (org_id, kind, record_id, at) VALUES (:org_id, 'credential_revoked', ?1, ?2)",
            params![id, now],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        kind
    };
    if kind == "local" {
        ensure_local_credential(store)?;
    }
    Ok(())
}

/// The pairing row records the issuing credential. `pairing_issuers` records its member.
pub fn void_device_pairings(db: &ScopedConnection, id: &str, now: &str) -> rusqlite::Result<()> {
    db.execute(
        "UPDATE pairings SET expires_at=?2 WHERE +org_id=:org_id AND used_at IS NULL
         AND issued_by=?1 AND expires_at>?2",
        params![id, now],
    )?;
    Ok(())
}

/// Every client credential, for the owner: no hashes.
pub fn credentials(store: &Store) -> Result<serde_json::Value, String> {
    credentials_for(store, None)
}

pub fn credentials_for(
    store: &Store,
    client: Option<&Client>,
) -> Result<serde_json::Value, String> {
    let member = client.map(Client::member_id);
    let actor = client.map(|c| c.id.as_str());
    let role = client
        .and_then(|c| c.member.scope.role)
        .unwrap_or(Role::Owner);
    let db = store.db();
    let mut stmt = db
        .prepare("SELECT c.id, kind, name, created_at, last_used_at, revoked_at, conversation_id, intent_id, node, created_by, revoked_by,c.member_id
          FROM (SELECT rowid, * FROM credentials WHERE +org_id = :org_id) c
          JOIN (SELECT id, role FROM members WHERE +org_id = :org_id) m ON m.id=c.member_id
          WHERE (?1 OR
           (c.kind='device' AND c.member_id=?3) OR
           (?2 AND m.role IN ('member','viewer') AND c.kind IN ('device','lead')) OR
           (c.kind='lead' AND (c.created_by=?4 OR EXISTS (
             SELECT 1 FROM credentials a WHERE +org_id = :org_id AND a.id=c.created_by AND a.member_id=?3))))
          ORDER BY c.rowid")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(
            params![role == Role::Owner, role == Role::Admin, member, actor],
            |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?,
                    "name": r.get::<_, String>(2)?, "created_at": r.get::<_, String>(3)?,
                    "last_used_at": r.get::<_, Option<String>>(4)?,
                    "revoked_at": r.get::<_, Option<String>>(5)?,
                    "conversation_id": r.get::<_, Option<String>>(6)?,
                    "intent_id": r.get::<_, Option<String>>(7)?,
                    "node": r.get::<_, Option<String>>(8)?,
                    "created_by": r.get::<_, Option<String>>(9)?,
                    "revoked_by": r.get::<_, Option<String>>(10)?,
                    "member_id": r.get::<_, String>(11)?,
                }))
            },
        )
        .map_err(|e| e.to_string())?;
    let credentials = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| e.to_string())?;
    let mut stmt = db
        .prepare(
            "SELECT name, issued_by, created_at, expires_at, used_at, credential_id,p.member_id
          FROM (SELECT rowid, * FROM pairings WHERE +org_id = :org_id) p
          JOIN (SELECT id, role FROM members WHERE +org_id = :org_id) m ON m.id=p.member_id
          WHERE (?1 OR p.member_id=?3 OR (?2 AND m.role IN ('member','viewer')))
          ORDER BY p.rowid",
        )
        .map_err(|e| e.to_string())?;
    let pairings = stmt
        .query_map(
            params![role == Role::Owner, role == Role::Admin, member],
            |r| {
                Ok(serde_json::json!({
                    "name": r.get::<_, String>(0)?,
                    "issued_by": r.get::<_, Option<String>>(1)?,
                    "issued_at": r.get::<_, String>(2)?,
                    "expires_at": r.get::<_, String>(3)?,
                    "used_at": r.get::<_, Option<String>>(4)?,
                    "credential_id": r.get::<_, Option<String>>(5)?,
                    "member_id": r.get::<_, String>(6)?,
                }))
            },
        )
        .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({"credentials": credentials, "pairings": pairings}))
}

/// Issue a self-pairing from the host or from an already resolved member.
pub fn issue_pairing(
    store: &Store,
    name: &str,
    actor: &str,
    member: Option<&Member>,
) -> Result<(String, String), String> {
    let mut db = store.db();
    let tx = crate::store::write_tx(&mut db).map_err(|e| e.to_string())?;
    let issuer = member.map_or(tx.scope().member_id.as_str(), |m| {
        m.scope.member_id.as_str()
    });
    let result = insert_pairing(
        &tx,
        name,
        actor,
        issuer,
        issuer,
        member.is_none_or(|m| m.scope.role == Some(Role::Owner)),
    )?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(result)
}

pub fn insert_pairing(
    db: &ScopedConnection,
    name: &str,
    actor: &str,
    issuer: &str,
    target: &str,
    reset: bool,
) -> Result<(String, String), String> {
    if !util::plain_name(name, 100) {
        return Err("the device name must match [A-Za-z0-9][A-Za-z0-9._-]{0,99}".into());
    }
    let code = util::pairing_code()?;
    let hash = sha256_hex(util::canonical_code(&code).as_bytes());
    let expires = util::utc_in(PAIRING_TTL_SECONDS);
    db.execute(
        "INSERT INTO pairings (org_id,member_id,code_hash,name,created_at,expires_at,issued_by)
         VALUES (:org_id,?1,?2,?3,?4,?5,?6)",
        params![target, hash, name, now_utc(), expires, actor],
    )
    .map_err(|e| e.to_string())?;
    db.execute(
        "INSERT INTO pairing_issuers (org_id,code_hash,issuer_member_id) VALUES (:org_id,?1,?2)",
        params![hash, issuer],
    )
    .map_err(|e| e.to_string())?;
    if reset {
        db.execute(
            "DELETE FROM org_settings WHERE +org_id = :org_id AND (key = 'pairing_failures' OR key GLOB 'pairing_failures:*')",
            params![],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok((code, expires))
}

/// Exchange a pairing code for a device token: `Ok(Some((credential_id, name, token)))`, or `Ok(None)`
/// when the code is unknown, used or expired. Too many failures void every outstanding code.
#[cfg(test)]
pub fn exchange_pairing(
    store: &Store,
    code: &str,
) -> Result<Option<(String, String, String)>, String> {
    Ok(
        exchange_device(store, code, std::net::Ipv4Addr::LOCALHOST.into())?
            .map(|(id, name, token, _)| (id, name, token)),
    )
}

pub fn exchange_device(
    store: &Store,
    code: &str,
    source: std::net::IpAddr,
) -> Result<Option<(String, String, String, Member)>, String> {
    let bucket = format!("pairing_failures:{}", source.to_canonical());
    let hash = sha256_hex(util::canonical_code(code).as_bytes());
    let now = now_utc();
    let mut db = store.db();
    let tx = crate::store::write_tx(&mut db).map_err(|e| e.to_string())?;
    let claimed: Option<(String, String)> = tx
            .query_row(
                "UPDATE pairings SET used_at = ?1 WHERE +org_id = :org_id AND (code_hash = ?2 AND used_at IS NULL AND expires_at > ?1)
                 RETURNING name, member_id",
                params![now, hash],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
    let Some((name, member_id)) = claimed else {
        tx.execute(
            "DELETE FROM org_settings WHERE +org_id=:org_id AND key GLOB 'pairing_failures:*'
             AND json_extract(value,'$.expires')<=?1",
            [&now],
        )
        .map_err(|e| e.to_string())?;
        let failures: i64 = tx.query_row(
            "INSERT INTO org_settings (org_id,key,value) VALUES (:org_id,?1,json_object('expires',?2,'count',1))
             ON CONFLICT(org_id,key) DO UPDATE SET value=json_set(value,'$.count',json_extract(value,'$.count')+1)
             WHERE +org_settings.org_id=:org_id RETURNING json_extract(value,'$.count')",
            params![bucket, util::utc_in(PAIRING_TTL_SECONDS)], |r| r.get(0),
        ).map_err(|e| e.to_string())?;
        if failures >= PAIRING_MAX_FAILURES {
            tx.execute(
                    "UPDATE pairings SET expires_at = ?1 WHERE +org_id = :org_id AND (used_at IS NULL AND expires_at > ?1)",
                    params![now],
                )
                .map_err(|e| e.to_string())?;
            crate::logf!("pairing: {failures} failed codes; every outstanding code voided");
        }
        tx.commit().map_err(|e| e.to_string())?;
        return Ok(None);
    };
    let Some(member) = Member::load(&tx, member_id.clone())
        .map_err(|e| e.to_string())?
        .filter(Member::can_authenticate)
    else {
        return Ok(None);
    };
    let issuer: Option<String> = tx
        .query_row(
            "SELECT issuer_member_id FROM pairing_issuers WHERE +org_id=:org_id AND code_hash=?1",
            [&hash],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    // Existing codes predate issuer bindings and were issued as self-pairings.
    let issuer = issuer.as_deref().unwrap_or(&member_id);
    let authority = crate::members::active_role(&tx, issuer).map_err(|e| e.to_string())?;
    if !authority.is_some_and(|role| {
        issuer == member_id
            || member
                .scope
                .role
                .is_some_and(|target| crate::members::manages(role, target))
    }) {
        return Ok(None);
    }
    let (id, token) = mint_device(&tx, &member_id, &name)?;
    tx.execute(
        "UPDATE pairings SET credential_id = ?1 WHERE +org_id = :org_id AND (code_hash = ?2)",
        params![id, hash],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM org_settings WHERE +org_id=:org_id AND key=?1",
        [bucket],
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(Some((id, name, token, member)))
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
            "INSERT INTO conversations (org_id, conversation_id, node, spec, spec_version, model,
             instructions_tokens, tools, created_by, created_at)
             VALUES (:org_id, 'conv_test', 'estate', 'lead', '1', 'test', 10, 1, 'owner', 'now')",
            params![],
        )?;
        for (id, node) in [("intent_here", "estate"), ("intent_other", "other")] {
            store.db().execute(
                "INSERT INTO intents (org_id, intent_id, node, spec, task, trials, meta, bundle_sha256,
                 created_by, created_at) VALUES (:org_id, ?1, ?2, 's', 't', 1, '{}', 'sha', 'owner', 'now')",
                params![id, node],
            )?;
        }
        assert!(mint_lead(&store, "lead", "missing", None, "cred_test").is_err());
        assert!(mint_lead(&store, "lead", "conv_test", Some("missing"), "cred_test").is_err());
        assert!(
            mint_lead(
                &store,
                "lead",
                "conv_test",
                Some("intent_other"),
                "cred_test"
            )
            .is_err()
        );
        assert!(mint_lead(&store, "bad name", "conv_test", None, "cred_test").is_err());
        let minted = mint_lead(
            &store,
            "lead",
            "conv_test",
            Some("intent_here"),
            "cred_test",
        )?;
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
        revoke_credential(&store, &lead.id, "cred_test")?;
        assert!(client(&store, token)?.is_none());
        let unscoped = mint_lead(&store, "lead2", "conv_test", None, "cred_test")?;
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
                "INSERT INTO credentials (org_id, member_id, id, kind, name, token_hash, created_at)
                 VALUES (:org_id, :member_id, ?1, ?1, 'test', ?2, 'now')",
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
        let (code, _) = issue_pairing(&store, "phone", "cred_test", None)?;
        let (id, name, token) = exchange_pairing(&store, &code.to_lowercase())?.ok_or("paired")?;
        assert_eq!(name, "phone");
        assert!(token.starts_with("csd_"));
        assert!(exchange_pairing(&store, &code)?.is_none(), "used once");
        assert_eq!(client(&store, &token)?.map(|c| c.kind), Some(Kind::Device));
        revoke_credential(&store, &id, "cred_test")?;
        assert!(client(&store, &token)?.is_none(), "revoked");

        let (code, _) = issue_pairing(&store, "laptop", "cred_test", None)?;
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
    fn pairing_failures_expire_and_success_resets_the_source() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = Store::open(&tmp.path().join("data"))?;
        let member = store.db().scope().member_id.clone();
        let other: std::net::IpAddr = "192.0.2.2".parse()?;
        let code = |name| insert_pairing(&store.db(), name, "owner", &member, &member, false);
        for _ in 1..PAIRING_MAX_FAILURES {
            assert!(exchange_device(&store, "wrong", other)?.is_none());
        }
        store.db().execute(
            "UPDATE org_settings SET value=json_set(value,'$.expires','2000-01-01')
             WHERE +org_id=:org_id AND key='pairing_failures:192.0.2.2'",
            rusqlite::params![],
        )?;
        let (first, _) = code("first")?;
        assert!(exchange_device(&store, "wrong", other)?.is_none());
        assert!(exchange_device(&store, &first, other)?.is_some());
        for _ in 1..PAIRING_MAX_FAILURES {
            assert!(exchange_device(&store, "wrong", other)?.is_none());
        }
        let (second, _) = code("second")?;
        assert!(exchange_pairing(&store, "wrong")?.is_none());
        assert!(exchange_device(&store, &second, other)?.is_some());
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
        revoke_credential(&store, &c.id, "cred_test")?;
        assert!(client(&store, &first)?.is_none());
        let second = std::fs::read_to_string(&path)?.trim().to_string();
        assert_ne!(first, second);
        assert!(client(&store, &second)?.is_some());
        Ok(())
    }
}
