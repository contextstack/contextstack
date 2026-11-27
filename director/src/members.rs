//! Invitations and member lifecycle.
use crate::access::{self, Client, Kind, Role};
use crate::server::{Call, Peer, Reply, State, json};
use crate::store::{ScopedConnection, write_tx};
use crate::util::{now_utc, secret, sha256_hex, utc_in};
use crate::work::{self, No};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

type Result<T> = std::result::Result<T, No>;

fn reject(status: u16, code: &'static str, detail: &str) -> No {
    No(status, code, detail.into(), Value::Null)
}

fn missing() -> No {
    reject(404, "not_found", "no such record")
}

fn invalid() -> No {
    reject(422, "invalid_request", "invalid member or invite fields")
}

fn conflict() -> No {
    reject(
        409,
        "member_conflict",
        "the member or invitation cannot make this transition",
    )
}

fn refused() -> No {
    reject(403, "invite_refused", "no live invitation matches")
}

pub fn manages(actor: Role, target: Role) -> bool {
    actor == Role::Owner || (actor == Role::Admin && target <= Role::Member)
}

pub fn actor_role(db: &ScopedConnection, client: &Client) -> Result<Role> {
    let live = match &client.kind {
        Kind::Browser { session_id } => db.query_row(
            "SELECT 1 FROM ui_sessions WHERE +org_id = :org_id AND id = ?1 AND member_id = ?2 AND expires_at > unixepoch()",
            params![session_id, client.member_id()], |_| Ok(()),
        ),
        _ => db.query_row(
            "SELECT 1 FROM credentials WHERE +org_id = :org_id AND id = ?1 AND member_id = ?2 AND revoked_at IS NULL",
            params![client.id, client.member_id()], |_| Ok(()),
        ),
    }.optional().map_err(|e| No::internal(&e))?;
    if live.is_some()
        && client.kind != Kind::Lead
        && let Some(role) = active_role(db, client.member_id()).map_err(|e| No::internal(&e))?
    {
        return Ok(role);
    }
    Err(reject(
        401,
        "unauthenticated",
        "a live client credential is required",
    ))
}

pub fn active_role(db: &ScopedConnection, id: &str) -> rusqlite::Result<Option<Role>> {
    Ok(access::Member::load(db, id.to_owned())?
        .filter(access::Member::can_authenticate)
        .and_then(|m| m.scope.role))
}

fn member_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(
        json!({"id": r.get::<_, String>(0)?, "display_name": r.get::<_, String>(1)?,
              "role": r.get::<_, String>(2)?, "status": r.get::<_, String>(3)?}),
    )
}

fn member(db: &ScopedConnection, id: &str) -> Result<Value> {
    db.query_row(
        "SELECT id,display_name,role,status FROM members WHERE +org_id = :org_id AND id = ?1",
        [id],
        member_row,
    )
    .optional()
    .map_err(|e| No::internal(&e))?
    .ok_or_else(missing)
}

fn role(value: &Value) -> Result<Role> {
    value.as_str().and_then(Role::parse).ok_or_else(invalid)
}

fn require_target(actor: Role, target: &Value) -> Result<()> {
    if manages(actor, role(&target["role"])?) {
        Ok(())
    } else {
        Err(No(
            403,
            "forbidden",
            "member management is not permitted for this member".into(),
            Value::Null,
        ))
    }
}

fn body(call: &Call) -> Result<Value> {
    serde_json::from_slice::<Value>(&call.body)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(invalid)
}

pub fn handle(state: &State, call: &Call, client: &Client, parts: &[&str]) -> Reply {
    match dispatch(state, call, client, parts) {
        Ok((201, value, _)) => enrollment_reply(201, &value),
        Ok((status, value, replayed)) => {
            let mut reply = json(status, &value);
            if replayed {
                reply.headers_mut().insert(
                    "idempotent-replayed",
                    hyper::header::HeaderValue::from_static("true"),
                );
            }
            reply
        }
        Err(no) => no.reply(),
    }
}

fn dispatch(
    state: &State,
    call: &Call,
    client: &Client,
    parts: &[&str],
) -> Result<(u16, Value, bool)> {
    let mut db = state.store.db();
    if call.method == "GET" {
        let actor = actor_role(&db, client)?;
        if parts == ["members", "directory"] {
            return Ok((200, directory(&db)?, false));
        }
        if actor < Role::Admin {
            return Err(reject(
                403,
                "forbidden",
                "member management is not permitted",
            ));
        }
        return Ok((200, list(&db, actor, parts == ["members"])?, false));
    }
    let tx = write_tx(&mut db).map_err(|e| No::internal(&e))?;
    let actor = actor_role(&tx, client)?;
    let result = match (call.method.as_str(), parts) {
        ("POST", ["invites"]) => (201, issue(&tx, call, client, actor)?, false),
        ("DELETE", ["invites", id]) => {
            let (value, replayed) = cancel(&tx, call, client, id, actor)?;
            (200, value, replayed)
        }
        ("PATCH", ["members", id]) => {
            let (value, replayed) = patch(&state.store, &tx, call, client, id, actor)?;
            (200, value, replayed)
        }
        _ => return Err(missing()),
    };
    tx.commit().map_err(|e| No::internal(&e))?;
    Ok(result)
}

fn directory(db: &ScopedConnection) -> Result<Value> {
    let rows = db.prepare(
        "SELECT id,display_name,role,status='active' FROM members WHERE +org_id=:org_id ORDER BY id",
    ).and_then(|mut stmt| {
        stmt.query_map(params![], |r| Ok(json!({
            "id":r.get::<_,String>(0)?, "display_name":r.get::<_,String>(1)?,
            "role":r.get::<_,String>(2)?, "active":r.get::<_,bool>(3)?
        })))?.collect::<rusqlite::Result<Vec<_>>>()
    }).map_err(|e| No::internal(&e))?;
    Ok(json!(rows))
}

pub fn rename_only(input: &Value) -> bool {
    input
        .as_object()
        .is_some_and(|fields| fields.len() == 1 && fields.contains_key("display_name"))
}

pub fn valid_display_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 100 && !name.chars().any(char::is_control)
}

fn list(db: &ScopedConnection, actor: Role, members: bool) -> Result<Value> {
    let (key, sql) = if members {
        (
            "members",
            "SELECT id,display_name,role,status FROM members
           WHERE +org_id = :org_id AND (?1 OR role IN ('member','viewer')) ORDER BY id",
        )
    } else {
        (
            "invites",
            "SELECT i.id,i.member_id,m.role,i.status,i.expires_at,i.issued_by,m.display_name FROM (SELECT * FROM invites WHERE +org_id = :org_id) i
           JOIN (SELECT id, role, display_name FROM members WHERE +org_id = :org_id) m ON m.id=i.member_id
           WHERE (?1 OR m.role IN ('member','viewer')) ORDER BY i.id",
        )
    };
    let items = db
        .prepare(sql)
        .and_then(|mut stmt| {
            stmt.query_map([actor == Role::Owner], |r| {
                if members {
                    return member_row(r);
                }
                let status: String = r.get(3)?;
                let expires: String = r.get(4)?;
                let status = if status == "pending" && expires <= now_utc() {
                    "expired"
                } else {
                    &status
                };
                Ok(
                    json!({"id":r.get::<_,String>(0)?, "member_id":r.get::<_,String>(1)?,
                "role":r.get::<_,String>(2)?, "status":status, "expires_at":expires,
                "issued_by":r.get::<_,String>(5)?, "display_name":r.get::<_,String>(6)?}),
                )
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(|e| No::internal(&e))?;
    Ok(json!({key:items}))
}

fn issue(db: &ScopedConnection, call: &Call, client: &Client, actor: Role) -> Result<Value> {
    let input = body(call)?;
    let (target, target_role) = if let Some(id) = input.get("member_id") {
        if input.get("display_name").is_some() || input.get("role").is_some() {
            return Err(invalid());
        }
        let id = id.as_str().ok_or_else(invalid)?;
        let target = member(db, id)?;
        require_target(actor, &target)?;
        if target["status"] != "pending" {
            return Err(conflict());
        }
        (id.to_owned(), role(&target["role"])?)
    } else {
        let name = input["display_name"]
            .as_str()
            .map(str::trim)
            .filter(|n| valid_display_name(n))
            .ok_or_else(invalid)?;
        let target_role = role(&input["role"])?;
        if target_role == Role::Owner {
            return Err(invalid());
        }
        if !manages(actor, target_role) {
            return Err(reject(
                403,
                "forbidden",
                "member management is not permitted",
            ));
        }
        let id = secret("mem_").map_err(|e| No::internal(&e))?;
        db.execute(
            "INSERT INTO members (org_id,id,display_name,role,status) VALUES (:org_id,?1,?2,?3,'pending')",
            params![id, name, input["role"].as_str()],
        )
        .map_err(|e| No::internal(&e))?;
        (id, target_role)
    };
    let id = secret("inv_").map_err(|e| No::internal(&e))?;
    let code = secret("csi_").map_err(|e| No::internal(&e))?;
    let expires = utc_in(86400);
    db.execute("UPDATE invites SET status='revoked' WHERE +org_id=:org_id AND member_id=?1 AND status='pending'",
        [&target]).map_err(|e| No::internal(&e))?;
    db.execute(
        "INSERT INTO invites (org_id,id,member_id,issuer_member_id,issued_by,code_hash,expires_at,status) VALUES (:org_id,?1,?2,?3,?4,?5,?6,'pending')",
        params![
            id,
            target,
            client.member_id(),
            client.id,
            sha256_hex(code.as_bytes()),
            expires
        ],
    )
    .map_err(|e| No::internal(&e))?;
    let link = crate::enrollment_origin::link(db, call, "invite", &code)?;
    Ok(
        json!({"id":id,"member_id":target,"role":target_role,"code":code,"expires_at":expires,"link":link}),
    )
}

fn cancel(
    db: &ScopedConnection,
    call: &Call,
    client: &Client,
    id: &str,
    actor: Role,
) -> Result<(Value, bool)> {
    let key = work::key(call)?;
    let hash = work::content("invite_delete", id, &Value::Null);
    let target: String = db
        .query_row(
            "SELECT member_id FROM invites WHERE +org_id=:org_id AND id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| No::internal(&e))?
        .ok_or_else(missing)?;
    require_target(actor, &member(db, &target)?)?;
    if let Some(response) = crate::settings_write::prior(db, &key, &hash, client, &target)? {
        return Ok((response, true));
    }
    let changed = db
        .execute(
            "UPDATE invites SET status='revoked'
        WHERE +org_id=:org_id AND id=?1 AND status='pending'",
            [id],
        )
        .map_err(|e| No::internal(&e))?;
    if changed == 0 {
        return Err(conflict());
    }
    let response = json!({"id":id,"revoked":true});
    crate::settings_write::save(db, &key, &hash, client, &target, &response, "invite_delete")?;
    Ok((response, false))
}

fn patch(
    store: &crate::store::Store,
    db: &ScopedConnection,
    call: &Call,
    client: &Client,
    id: &str,
    actor: Role,
) -> Result<(Value, bool)> {
    let key = work::key(call)?;
    let input = body(call)?;
    let before = member(db, id)?;
    let rename = rename_only(&input);
    if !rename || id != client.member_id() {
        require_target(actor, &before)?;
    }
    let hash = sha256_hex(&call.body);
    if let Some(response) = crate::settings_write::prior(db, &key, &hash, client, id)? {
        return Ok((response, true));
    }
    if rename {
        let name = input["display_name"]
            .as_str()
            .map(str::trim)
            .filter(|n| valid_display_name(n))
            .ok_or_else(invalid)?;
        db.execute(
            "UPDATE members SET display_name=?2 WHERE +org_id=:org_id AND id=?1",
            params![id, name],
        )
        .map_err(|e| No::internal(&e))?;
    } else {
        patch_authority(store, db, client, id, actor, &input, &before)?;
    }
    let response = json!({"member":member(db,id)?});
    crate::settings_write::save(db, &key, &hash, client, id, &response, "member_patch")?;
    Ok((response, false))
}

fn patch_authority(
    store: &crate::store::Store,
    db: &ScopedConnection,
    client: &Client,
    id: &str,
    actor: Role,
    input: &Value,
    before: &Value,
) -> Result<()> {
    if input
        .as_object()
        .is_none_or(|o| o.is_empty() || o.keys().any(|k| !matches!(k.as_str(), "role" | "status")))
    {
        return Err(invalid());
    }
    let next_role = input.get("role").unwrap_or(&before["role"]);
    let next_status = input.get("status").unwrap_or(&before["status"]);
    if input.get("status").is_some() && !matches!(next_status.as_str(), Some("active" | "disabled"))
    {
        return Err(invalid());
    }
    if !manages(actor, role(next_role)?) {
        return Err(reject(
            403,
            "forbidden",
            "member management is not permitted",
        ));
    }
    if before["status"] == "pending" && (next_status != "pending" || next_role == "owner") {
        return Err(conflict());
    }
    if id == db.scope().member_id && (next_role != "owner" || next_status != "active") {
        return Err(conflict());
    }
    if before["role"] == "owner" && before["status"] == "active" && (next_role != "owner" || next_status != "active")
        && db.query_row("SELECT count(*) FROM members WHERE +org_id=:org_id AND role='owner' AND status='active'",
            params![], |r| r.get::<_,u32>(0)).map_err(|e| No::internal(&e))? <= 1
    { return Err(conflict()); }
    let was_eligible = crate::policy::eligible_sponsor(db, id).map_err(|e| No::internal(&e))?;
    db.execute(
        "UPDATE members SET role=?2,status=?3 WHERE +org_id=:org_id AND id=?1",
        params![id, next_role.as_str(), next_status.as_str()],
    )
    .map_err(|e| No::internal(&e))?;
    db.execute(
        "UPDATE team_members SET role=?2 WHERE +org_id=:org_id AND member_id=?1",
        params![id, next_role.as_str()],
    )
    .map_err(|e| No::internal(&e))?;
    if next_status == "disabled" && before["status"] != "disabled" {
        disable(db, id, &client.id).map_err(|e| No::internal(&e))?;
    }
    if next_role != &before["role"] || next_status != &before["status"] {
        crate::sponsorship::reroute_attention(db, id, client).map_err(|e| No::internal(&e))?;
        db.execute("INSERT INTO changes (org_id,kind,record_id,at) VALUES (:org_id,'authorization_changed',?1,?2)",
            params![id,now_utc()]).map_err(|e| No::internal(&e))?;
    }
    if !was_eligible && crate::policy::eligible_sponsor(db, id).map_err(|e| No::internal(&e))? {
        crate::sponsorship::resume_member(store, db, id)?;
    }
    Ok(())
}

fn disable(db: &ScopedConnection, id: &str, actor: &str) -> rusqlite::Result<()> {
    let now = now_utc();
    db.execute("UPDATE credentials SET revoked_at=?2,revoked_by=?3
        WHERE +org_id=:org_id AND member_id=?1 AND revoked_at IS NULL AND kind IN ('device','lead')",
        params![id,now,actor])?;
    db.execute(
        "UPDATE pairings SET expires_at=?2 WHERE +org_id=:org_id AND used_at IS NULL
        AND (member_id=?1 OR code_hash IN (SELECT code_hash FROM pairing_issuers
            WHERE +org_id=:org_id AND issuer_member_id=?1)) AND expires_at>?2",
        params![id, now],
    )?;
    for table in ["ui_codes", "ui_sessions"] {
        db.execute(
            &format!("DELETE FROM {table} WHERE +org_id=:org_id AND member_id=?1"),
            [id],
        )?;
    }
    db.execute("UPDATE invites SET status='revoked' WHERE +org_id=:org_id AND issuer_member_id=?1 AND status='pending'", [id])?;
    Ok(())
}

pub fn accept(state: &State, call: &Call, peer: Peer) -> Reply {
    match exchange(state, call, peer) {
        Ok(value) => enrollment_reply(201, &value),
        Err(no) if no.0 == 429 => {
            let mut reply = no.reply();
            reply
                .headers_mut()
                .insert("retry-after", hyper::header::HeaderValue::from_static("60"));
            reply
        }
        Err(no) => no.reply(),
    }
}

fn exchange(state: &State, call: &Call, peer: Peer) -> Result<Value> {
    let db = state.store.db();
    // Behind a reverse proxy, clients share its bucket unless the Director sees their source IPs.
    let bucket = sha256_hex(peer.ip.to_canonical().to_string().as_bytes());
    db.execute(
        "DELETE FROM invite_throttle WHERE +org_id=:org_id AND minute < unixepoch()/60",
        params![],
    )
    .map_err(|e| No::internal(&e))?;
    let count: u32 = db
        .query_row(
            "INSERT INTO invite_throttle (org_id,bucket,minute,count) VALUES (:org_id,?1,unixepoch()/60,1)
        ON CONFLICT(org_id,bucket) DO UPDATE SET count=CASE WHEN minute=excluded.minute
        THEN min(count+1,31) ELSE 1 END,minute=excluded.minute WHERE +org_id = :org_id RETURNING count",
            [bucket],
            |r| r.get(0),
        )
        .map_err(|e| No::internal(&e))?;
    if count > 30 {
        return Err(reject(429, "rate_limited", "too many invitation exchanges"));
    }
    drop(db);
    if let Some(token) = call.bearer()
        && access::authenticate(&state.store, token, false)
            .map_err(|e| No::internal(&e))?
            .is_some_and(|c| c.kind == Kind::Lead)
    {
        return Err(reject(
            403,
            "owner_only",
            "a lead cannot exchange enrollment codes",
        ));
    }
    let input = body(call).map_err(|_| refused())?;
    if input.as_object().is_none_or(|fields| fields.len() != 2) {
        return Err(refused());
    }
    let code = input["code"]
        .as_str()
        .filter(|c| {
            c.strip_prefix("csi_").is_some_and(|secret| {
                secret.len() == 64 && secret.bytes().all(|b| b.is_ascii_hexdigit())
            })
        })
        .ok_or_else(refused)?;
    let name = input["name"]
        .as_str()
        .filter(|n| crate::util::plain_name(n, 100))
        .ok_or_else(refused)?;
    let hash = sha256_hex(code.as_bytes());
    let lookup = |db: &ScopedConnection| -> Result<Option<(String, String, String)>> {
        db.query_row(
            "SELECT id,member_id,issuer_member_id FROM invites WHERE +org_id=:org_id AND code_hash=?1
             AND status='pending' AND expires_at>?2", params![hash,now_utc()],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))
            .optional().map_err(|e| No::internal(&e))
    };
    let mut db = state.store.db();
    lookup(&db)?.ok_or_else(refused)?;
    let tx = write_tx(&mut db).map_err(|e| No::internal(&e))?;
    let found = lookup(&tx)?;
    let (id, target, issuer) = found.ok_or_else(refused)?;
    let member = member(&tx, &target)?;
    let target_role = role(&member["role"])?;
    let authority = active_role(&tx, &issuer).map_err(|e| No::internal(&e))?;
    if member["status"] != "pending"
        || target_role == Role::Owner
        || !authority.is_some_and(|r| manages(r, target_role))
    {
        return Err(refused());
    }
    tx.execute(
        "UPDATE invites SET status='consumed' WHERE +org_id=:org_id AND id=?1",
        [&id],
    )
    .map_err(|e| No::internal(&e))?;
    tx.execute(
        "UPDATE members SET status='active' WHERE +org_id=:org_id AND id=?1",
        [&target],
    )
    .map_err(|e| No::internal(&e))?;
    tx.execute(
        "INSERT INTO team_members (org_id,team_id,member_id,role)
        VALUES (:org_id,(SELECT value FROM meta WHERE key='default_team_id'),?1,?2)",
        params![target, member["role"].as_str()],
    )
    .map_err(|e| No::internal(&e))?;
    let (credential, token) =
        access::mint_device(&tx, &target, name).map_err(|e| No::internal(&e))?;
    tx.commit().map_err(|e| No::internal(&e))?;
    Ok(json!({"credential_id":credential,"name":name,"token":token,
        "member":{"id":target,"display_name":member["display_name"]},"role":target_role}))
}

pub fn pairing(state: &State, call: &Call, client: &Client) -> Reply {
    match issue_pairing(state, call, client) {
        Ok(value) => enrollment_reply(200, &value),
        Err(no) => no.reply(),
    }
}

pub fn enrollment_reply(status: u16, value: &Value) -> Reply {
    let mut reply = json(status, value);
    reply.headers_mut().insert(
        hyper::header::CACHE_CONTROL,
        hyper::header::HeaderValue::from_static("no-store"),
    );
    reply
}

fn issue_pairing(state: &State, call: &Call, client: &Client) -> Result<Value> {
    let input = body(call)?;
    let name = input["name"]
        .as_str()
        .filter(|n| crate::util::plain_name(n, 100))
        .ok_or_else(invalid)?;
    let target = input
        .get("member_id")
        .map(|m| m.as_str().ok_or_else(invalid))
        .transpose()?
        .unwrap_or_else(|| client.member_id());
    let mut db = state.store.db();
    let tx = write_tx(&mut db).map_err(|e| No::internal(&e))?;
    let actor = actor_role(&tx, client)?;
    if target != client.member_id() {
        if actor < Role::Admin {
            return Err(reject(
                403,
                "forbidden",
                "recovery pairing requires member management",
            ));
        }
        let target = member(&tx, target)?;
        require_target(actor, &target)?;
        if target["status"] != "active" {
            return Err(conflict());
        }
    }
    let (code, expires_at) = access::insert_pairing(
        &tx,
        name,
        &client.id,
        client.member_id(),
        target,
        actor == Role::Owner,
    )
    .map_err(|e| No::internal(&e))?;
    let link = crate::enrollment_origin::link(&tx, call, "pair", &code)?;
    tx.commit().map_err(|e| No::internal(&e))?;
    Ok(json!({"code":code,"expires_at":expires_at,"name":name,"member_id":target,"link":link}))
}

pub fn revoke(state: &State, client: &Client, id: &str) -> Reply {
    match revoke_device(state, client, id) {
        Ok(()) => json(200, &json!({"id":id,"revoked":true})),
        Err(no) => no.reply(),
    }
}

fn revoke_device(state: &State, client: &Client, id: &str) -> Result<()> {
    let mut db = state.store.db();
    let tx = write_tx(&mut db).map_err(|e| No::internal(&e))?;
    let actor = actor_role(&tx, client)?;
    let found: Option<(String,String,String,bool)> = tx.query_row(
        "SELECT c.kind,c.member_id,m.role,COALESCE(c.created_by=?3 OR EXISTS (
            SELECT 1 FROM credentials a WHERE +org_id = :org_id AND a.id=c.created_by AND a.member_id=?2
        ),0) FROM (SELECT * FROM credentials WHERE +org_id = :org_id AND id=?1 AND revoked_at IS NULL) c
        JOIN (SELECT id,role FROM members WHERE +org_id = :org_id) m ON m.id=c.member_id",
        params![id,client.member_id(),client.id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))
    ).optional().map_err(|e| No::internal(&e))?;
    let (kind, target, target_role, minted) = found.ok_or_else(missing)?;
    let managed = Role::parse(&target_role).is_some_and(|r| manages(actor, r));
    match kind.as_str() {
        "local" if actor == Role::Owner => {
            return Err(reject(
                409,
                "protected_credential",
                "the host credential is protected",
            ));
        }
        "device" if target == client.member_id() || managed => {
            if client.kind == Kind::Device && client.id == id {
                return Err(reject(
                    409,
                    "self_revoke",
                    "Use Forget this device to remove the credential locally.",
                ));
            }
        }
        "lead" if minted || managed => {}
        _ => return Err(missing()),
    }
    let now = now_utc();
    tx.execute(
        "UPDATE credentials SET revoked_at=?2,revoked_by=?3 WHERE +org_id=:org_id AND id=?1",
        params![id, now, client.id],
    )
    .map_err(|e| No::internal(&e))?;
    if kind == "device" {
        access::void_device_pairings(&tx, id, &now).map_err(|e| No::internal(&e))?;
    }
    tx.execute("INSERT INTO changes (org_id,kind,record_id,at) VALUES (:org_id,'credential_revoked',?1,?2)",
        params![id,now]).map_err(|e| No::internal(&e))?;
    tx.commit().map_err(|e| No::internal(&e))?;
    Ok(())
}
