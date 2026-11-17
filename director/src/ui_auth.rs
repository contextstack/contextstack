//! One-time browser pairing and bounded browser sessions, separate from bearer credentials.
//! Only hashes persist; neither codes nor cookie values are logged or returned by read APIs.

use crate::store::Store;
use crate::util::{secret, sha256_hex};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};

/// A link can be exchanged for five minutes.
pub const CODE_TTL_SECONDS: i64 = 300;
/// A browser must pair again after eight hours, including across service restarts.
pub const SESSION_TTL_SECONDS: i64 = 8 * 60 * 60;

/// Issue a high-entropy code. Only the local administrative command may call this.
pub fn issue(store: &Store) -> Result<String, String> {
    let code = secret("")?;
    let now = Utc::now().timestamp();
    let db = store.db();
    cleanup(&db, now).map_err(|e| e.to_string())?;
    db.execute(
        "INSERT INTO ui_codes (code_hash, expires_at) VALUES (?1, ?2)",
        params![sha256_hex(code.as_bytes()), now + CODE_TTL_SECONDS],
    )
    .map_err(|e| e.to_string())?;
    Ok(code)
}

/// Exchange once, returning `(cookie_value, csrf_token)`. Unknown, used and expired codes return none.
pub fn exchange(store: &Store, code: &str) -> Result<Option<(String, String)>, String> {
    if !crate::util::is_sha256(code) {
        return Ok(None);
    }
    let token = secret("")?;
    let now = Utc::now().timestamp();
    let mut db = store.db();
    let tx = crate::store::write_tx(&mut db).map_err(|e| e.to_string())?;
    cleanup(&tx, now).map_err(|e| e.to_string())?;
    let claimed = tx
        .execute(
            "DELETE FROM ui_codes WHERE code_hash = ?1 AND expires_at > ?2",
            params![sha256_hex(code.as_bytes()), now],
        )
        .map_err(|e| e.to_string())?;
    if claimed == 0 {
        tx.commit().map_err(|e| e.to_string())?;
        return Ok(None);
    }
    tx.execute(
        "INSERT INTO ui_sessions (token_hash, expires_at) VALUES (?1, ?2)",
        params![sha256_hex(token.as_bytes()), now + SESSION_TTL_SECONDS],
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    let csrf = csrf_token(&token);
    Ok(Some((token, csrf)))
}

/// Return the session's CSRF token when the cookie is live; never accept a bearer credential here.
pub fn authenticate(store: &Store, token: &str) -> Result<Option<String>, String> {
    if !crate::util::is_sha256(token) {
        return Ok(None);
    }
    let live = store
        .db()
        .query_row(
            "SELECT 1 FROM ui_sessions WHERE token_hash = ?1 AND expires_at > ?2",
            params![sha256_hex(token.as_bytes()), Utc::now().timestamp()],
            |_| Ok(()),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    Ok(live.map(|()| csrf_token(token)))
}

/// Revoke just this browser session, including any copies of its cookie.
pub fn logout(store: &Store, token: &str) -> Result<(), String> {
    store
        .db()
        .execute(
            "DELETE FROM ui_sessions WHERE token_hash = ?1",
            params![sha256_hex(token.as_bytes())],
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn csrf_token(token: &str) -> String {
    // Domain separation keeps the readable CSRF token distinct from the session's stored hash.
    sha256_hex(format!("director-ui-csrf:{token}").as_bytes())
}

fn cleanup(db: &Connection, now: i64) -> rusqlite::Result<()> {
    // Bound the work a request can trigger even after a long period without UI use.
    for table in ["ui_codes", "ui_sessions"] {
        db.execute(
            &format!(
                "DELETE FROM {table} WHERE rowid IN \
                 (SELECT rowid FROM {table} WHERE expires_at <= ?1 LIMIT 1000)"
            ),
            params![now],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    type Res = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn logout_deletes_only_the_selected_session() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = Store::open(&tmp.path().join("data"))?;
        let (token, _) = exchange(&store, &issue(&store)?)?.ok_or("exchange refused")?;
        let (other, csrf) = exchange(&store, &issue(&store)?)?.ok_or("exchange refused")?;
        logout(&store, &token)?;
        assert!(authenticate(&store, &token)?.is_none());
        assert_eq!(authenticate(&store, &other)?, Some(csrf));
        let remaining: i64 = store
            .db()
            .query_row("SELECT COUNT(*) FROM ui_sessions", [], |r| r.get(0))?;
        assert_eq!(remaining, 1);
        Ok(())
    }

    #[test]
    fn pairing_is_one_time_and_only_hashes_are_stored() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = Store::open(&tmp.path().join("data"))?;
        let code = issue(&store)?;
        let hash: String = store
            .db()
            .query_row("SELECT code_hash FROM ui_codes", [], |r| r.get(0))?;
        assert_eq!(hash, sha256_hex(code.as_bytes()));
        let (token, csrf) = exchange(&store, &code)?.ok_or("exchange refused")?;
        assert_eq!(authenticate(&store, &token)?, Some(csrf));
        assert!(exchange(&store, &code)?.is_none());
        assert!(authenticate(&store, &code)?.is_none());
        let hash: String = store
            .db()
            .query_row("SELECT token_hash FROM ui_sessions", [], |r| r.get(0))?;
        assert_eq!(hash, sha256_hex(token.as_bytes()));
        Ok(())
    }

    #[test]
    fn expired_codes_and_sessions_are_refused() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = Store::open(&tmp.path().join("data"))?;
        let code = issue(&store)?;
        store
            .db()
            .execute("UPDATE ui_codes SET expires_at = 0", [])?;
        assert!(exchange(&store, &code)?.is_none());
        let code = issue(&store)?;
        let (token, _) = exchange(&store, &code)?.ok_or("exchange refused")?;
        store
            .db()
            .execute("UPDATE ui_sessions SET expires_at = 0", [])?;
        assert!(authenticate(&store, &token)?.is_none());
        assert!(exchange(&store, "bad code")?.is_none());
        assert!(authenticate(&store, "bad cookie")?.is_none());
        Ok(())
    }

    #[test]
    fn failed_session_creation_does_not_consume_the_code() -> Res {
        let tmp = tempfile::tempdir()?;
        let store = Store::open(&tmp.path().join("data"))?;
        let code = issue(&store)?;
        store.db().execute_batch(
            "CREATE TRIGGER refuse_session BEFORE INSERT ON ui_sessions
             BEGIN SELECT RAISE(ABORT, 'test storage failure'); END;",
        )?;
        assert!(exchange(&store, &code).is_err());
        store.db().execute_batch("DROP TRIGGER refuse_session;")?;
        assert!(exchange(&store, &code)?.is_some());
        Ok(())
    }

    #[test]
    fn concurrent_exchanges_have_exactly_one_winner() -> Res {
        let tmp = tempfile::tempdir()?;
        let data = tmp.path().join("data");
        let store = Store::open(&data)?;
        let other = Store::open(&data)?;
        let code = issue(&store)?;
        let other_code = code.clone();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let other_barrier = std::sync::Arc::clone(&barrier);
        let worker = std::thread::spawn(move || {
            other_barrier.wait();
            exchange(&other, &other_code)
        });
        barrier.wait();
        let first = exchange(&store, &code)?.is_some();
        let second = worker
            .join()
            .map_err(|_| "exchange thread panicked")??
            .is_some();
        assert_ne!(first, second);
        Ok(())
    }
}
