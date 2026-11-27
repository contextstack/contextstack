//! Live updates (CLIENT-API.md §8): one server-sent-events stream per client carrying every
//! record change with its revision. Writes go through ordinary requests.
//!
//! Every change is a row in `changes`, written in the transaction that made it (`store::touch`). After each
//! request, and once a second for changes made by the administrative commands, the hub reads the rows it has
//! not published yet, renders each changed record as it now is, and sends the batch to every open stream.
//! Commits are serialised by the database lock, so revisions are published in order and without gaps. A stream
//! starts at the present: older changes are not replayed.

use crate::access::{Client, Kind};
use crate::records;
use crate::server::{Reply, State};
use crate::store::{self, Record, Store};
use bytes::Bytes;
use hyper::body::{Body, Frame};
use rusqlite::params;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

/// How often an idle stream gets a comment line, and its credential is checked again.
const HEARTBEAT: Duration = Duration::from_secs(15);
/// Batches a slow stream may fall behind by before it is told to resync.
const BACKLOG: usize = 256;

/// What the hub sends to every stream.
#[derive(Clone)]
enum Msg {
    /// Changed records, in revision order.
    Changes(Arc<Vec<Value>>),
    /// A credential was revoked: its streams end.
    Revoked(String),
    AuthorizationChanged(String, i64),
    /// Live events of runs, as recorded (CLIENT-API.md §8.1).
    RunEvents(Arc<Vec<Value>>),
    /// A run's land record was set (CLIENT-API.md §8.2).
    LandEvents(Arc<Vec<Value>>),
}

/// The publisher every stream listens to.
pub struct Hub {
    tx: broadcast::Sender<Msg>,
    published: Mutex<i64>,
    /// Cancelled at shutdown: every stream ends.
    pub stop: CancellationToken,
    /// Woken whenever changes are published: held-open requests for work look again.
    pub changed: tokio::sync::Notify,
}

impl Hub {
    /// A hub starting at the store's newest revision.
    pub fn new(store: &Store) -> Result<Self, String> {
        let now = store::revision(&store.db()).map_err(|e| e.to_string())?;
        Ok(Self {
            tx: broadcast::channel(BACKLOG).0,
            published: Mutex::new(now),
            stop: CancellationToken::new(),
            changed: tokio::sync::Notify::new(),
        })
    }

    /// The newest revision published.
    fn published(&self) -> i64 {
        *self
            .published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Send every change recorded since the last publish.
    pub fn publish(&self, store: &Store) {
        let mut published = self
            .published
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let db = store.db();
        let rows: Result<Vec<(i64, String, String)>, _> = db
            .prepare_cached("SELECT seq, kind, record_id FROM changes WHERE +org_id = :org_id AND seq > ?1 ORDER BY seq")
            .and_then(|mut stmt| {
                stmt.query_map(params![*published], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?
                .collect()
            });
        let rows = match rows {
            Ok(r) if !r.is_empty() => r,
            Ok(_) => return,
            Err(e) => {
                crate::logf!("events: {e}");
                return;
            }
        };
        // Each record once, at its newest change, rendered as it is now.
        let mut seen = HashSet::new();
        let mut events = Vec::new();
        for (seq, kind, id) in rows.iter().rev() {
            if kind == "credential_revoked" {
                self.revoked(id);
                continue;
            }
            if kind == "authorization_changed" {
                let _ = self.tx.send(Msg::AuthorizationChanged(id.clone(), *seq));
                continue;
            }
            if kind == "chief_of_staff" {
                if seen.insert((Record::Conversation, "chief_of_staff"))
                    && let Ok(record) = crate::chief_of_staff::read(&db)
                {
                    events.push(json!({"kind": kind, "id": id, "revision": record["revision"], "record": record}));
                }
                continue;
            }
            let Some(kind) = Record::named(kind) else {
                continue;
            };
            if !seen.insert((kind, id.as_str())) {
                continue;
            }
            match records::one(&db, kind, id) {
                Ok(Some(record)) => events.push(json!({"kind": kind.name(), "id": id,
                                                       "revision": record["revision"], "record": record})),
                Ok(None) if kind == Record::Conversation => events.push(json!({"kind": "conversation", "id": id, "revision": seq, "deleted": true})),
                Ok(None) => {}
                Err(e) => crate::logf!("events: {} {id}: {e}", kind.name()),
            }
        }
        events.reverse();
        *published = rows.last().map_or(*published, |r| r.0);
        if !events.is_empty() {
            let _ = self.tx.send(Msg::Changes(Arc::new(events)));
        }
        self.changed.notify_waiters();
    }

    /// Send runs' live events to every stream.
    pub fn run_events(&self, events: Vec<Value>) {
        if !events.is_empty() {
            let _ = self.tx.send(Msg::RunEvents(Arc::new(events)));
        }
    }

    /// Send land events to every stream.
    pub fn land_events(&self, events: Vec<Value>) {
        if !events.is_empty() {
            let _ = self.tx.send(Msg::LandEvents(Arc::new(events)));
        }
    }

    /// End every stream of a revoked credential.
    pub fn revoked(&self, credential: &str) {
        let _ = self.tx.send(Msg::Revoked(credential.to_string()));
    }
}

/// A streamed response body fed by a channel.
pub struct Stream(mpsc::Receiver<Bytes>);

impl Body for Stream {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.get_mut()
            .0
            .poll_recv(cx)
            .map(|chunk| chunk.map(|b| Ok(Frame::data(b))))
    }
}

fn frame(event: &str, id: Option<&Value>, data: &Value) -> Bytes {
    let id = id.map(|i| format!("id: {i}\n")).unwrap_or_default();
    Bytes::from(format!("{id}event: {event}\ndata: {data}\n\n"))
}

fn live(store: &Store, client: &Client) -> bool {
    let db = store.db();
    db
        .query_row(
            "SELECT member_id FROM credentials WHERE +org_id = :org_id AND id = ?1 AND revoked_at IS NULL",
            params![client.id],
            |row| row.get(0),
        )
        .and_then(|member| crate::access::Member::load(&db, member))
        .is_ok_and(|member| member.is_some_and(|member| member.can_authenticate() && member.scope.role == client.member.scope.role && member.scope.member_id == client.member_id()))
}

fn visible(store: &Store, client: &Client, event: &Value, change: bool) -> bool {
    if change && event["kind"] == "action_approval" {
        return event["id"].as_str().is_some_and(|id| {
            crate::policy::approval_visible(&store.db(), client, id).unwrap_or(false)
        });
    }
    if client.kind != Kind::Lead {
        return true;
    }
    let target = if change {
        event["kind"]
            .as_str()
            .and_then(Record::named)
            .zip(event["id"].as_str())
    } else {
        event["run_id"].as_str().map(|id| (Record::Run, id))
    };
    let Some((kind, id)) = target else {
        return false;
    };
    crate::lead::visible(&store.db(), client, kind, id).unwrap_or(false)
}

fn change_frame(store: &Store, client: &Client, event: &Value) -> Bytes {
    let kind = event["kind"].as_str().and_then(Record::named);
    if let Some(kind @ (Record::Intent | Record::Job | Record::Run | Record::Conversation)) = kind
        && event["deleted"] != true
        && let Some(id) = event["id"].as_str()
    {
        let _visibility = crate::policy::enter_read_context(client);
        let db = store.db();
        let mut filtered = event.clone();
        if kind == Record::Conversation {
            filtered["record"] = records::one(&db, kind, id)
                .ok()
                .flatten()
                .unwrap_or_default();
        } else if let Err(error) = crate::projections::record(&db, kind, &mut filtered["record"]) {
            crate::logf!("events: {} {id}: {error}", kind.name());
        }
        return frame("change", event.get("revision"), &filtered);
    }
    frame("change", event.get("revision"), event)
}

/// `GET events`: open a stream. It begins with `hello` naming the newest published revision; every later
/// change is a `change` event; it ends with `revoked` or `resync` when it must.
///
/// With `conversation`, only that conversation's events pass: records whose `conversation_id` is it,
/// `change` events for intents and jobs whose `lead_conversation_id` is it, and its runs' live events.
pub fn stream(state: &Arc<State>, client: &Client, conversation: Option<&str>) -> Reply {
    stream_inner(state, client, conversation, None, HEARTBEAT)
}

/// A conversation stream whose authority expires with its browser session.
pub fn browser_stream(
    state: &Arc<State>,
    client: &Client,
    conversation: &str,
    session: &str,
) -> Reply {
    stream_inner(state, client, Some(conversation), Some(session), HEARTBEAT)
}

fn event_stream_response(body: mpsc::Receiver<Bytes>) -> Reply {
    let mut reply = hyper::Response::new(http_body_util::Either::Right(Stream(body)));
    for (name, value) in [
        ("content-type", "text/event-stream"),
        ("cache-control", "no-store"),
        ("x-accel-buffering", "no"),
    ] {
        if let Ok(v) = value.parse() {
            reply.headers_mut().insert(name, v);
        }
    }
    reply
}

// Bound delivery of the entire authorized batch for slow subscribers.
async fn deliver(
    out: &mpsc::Sender<Bytes>,
    chunk: impl IntoIterator<Item = Bytes>,
    timeout: Duration,
) -> bool {
    tokio::time::timeout(timeout, async {
        for c in chunk {
            out.send(c).await?;
        }
        Ok::<_, mpsc::error::SendError<Bytes>>(())
    })
    .await
    .is_ok_and(|result| result.is_ok())
}

#[allow(clippy::too_many_lines)]
fn stream_inner(
    state: &Arc<State>,
    client: &Client,
    conversation: Option<&str>,
    session: Option<&str>,
    heartbeat: Duration,
) -> Reply {
    if let Err(no) = crate::policy::stream_target(state, client, conversation) {
        return no;
    }
    let session = session.map(String::from);
    let mut changes = state.hub.tx.subscribe();
    let from = state.hub.published();
    let (out, body) = mpsc::channel::<Bytes>(64);
    let state = Arc::clone(state);
    let client = client.clone();
    let me = client.id.clone();
    let only = conversation.map(String::from);
    let ours = move |e: &Value, record: bool| {
        only.as_ref().is_none_or(|c| {
            if record && e["kind"] == "chief_of_staff" {
                return false;
            }
            let v = if record { &e["record"] } else { e };
            (record && e["kind"] == "conversation" && e["id"] == c.as_str())
                || v.get("conversation_id").and_then(Value::as_str) == Some(c.as_str())
                || v.get("lead_conversation_id").and_then(Value::as_str) == Some(c.as_str())
        })
    };
    let hello_conversation = conversation.map(String::from);
    tokio::spawn(async move {
        let revoke = || deliver(&out, [frame("revoked", None, &json!({}))], heartbeat);
        let authorized = || match &session {
            Some(token) => crate::ui_auth::authenticate(&state.store, token).is_ok_and(|session| {
                session.is_some_and(|(browser, _)| {
                    browser.kind == client.kind
                        && browser.member.scope.role == client.member.scope.role
                        && browser.member_id() == client.member_id()
                })
            }),
            None => live(&state.store, &client),
        };
        if !authorized() {
            if session.is_none() {
                revoke().await;
            }
            return;
        }
        let mut hello = json!({"revision": from, "replay": false, "credential_id": me});
        if let Some(c) = hello_conversation {
            hello["conversation"] = json!(c);
        }
        if out
            .send(Bytes::from_static(b"retry: 2000\n\n"))
            .await
            .is_err()
            || out.send(frame("hello", None, &hello)).await.is_err()
        {
            return;
        }
        let mut beat = tokio::time::interval(heartbeat);
        beat.tick().await;
        loop {
            let chunk = tokio::select! {
                () = state.hub.stop.cancelled() => break,
                () = out.closed() => break,
                _ = beat.tick() => vec![Bytes::from_static(b": ping\n\n")],
                m = changes.recv() => match m {
                    Ok(Msg::Changes(events)) => events
                        .iter()
                        .filter(|e| ours(e, true) && visible(&state.store, &client, e, true))
                        .map(|e| change_frame(&state.store, &client, e))
                        .collect(),
                    Ok(Msg::Revoked(id)) if id == me => {
                        revoke().await;
                        break;
                    }
                    Ok(Msg::AuthorizationChanged(id, seq)) if id == client.member_id() => {
                        if client.kind != Kind::Lead {
                            deliver(&out, [frame("change", Some(&json!(seq)),
                                &json!({"kind":"authorization_changed","id":id,"revision":seq}))], heartbeat).await;
                        }
                        revoke().await;
                        break;
                    }
                    Ok(Msg::Revoked(_) | Msg::AuthorizationChanged(..)) => continue,
                    Ok(Msg::RunEvents(events)) => events
                        .iter()
                        .filter(|e| ours(e, false) && visible(&state.store, &client, e, false))
                        .map(|e| frame("run_event", None, e))
                        .collect(),
                    Ok(Msg::LandEvents(events)) => events
                        .iter()
                        .filter(|e| ours(e, false) && visible(&state.store, &client, e, false))
                        .map(|e| frame("run.land", None, e))
                        .collect(),
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let _ = out
                            .try_send(frame("resync", None, &json!({"reason": "this stream fell behind"})));
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            };
            if chunk.is_empty() {
                continue;
            }
            if !authorized() {
                revoke().await;
                break;
            }
            if !deliver(&out, chunk, heartbeat).await {
                break;
            }
        }
    });
    event_stream_response(body)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::server::{Call, Peer};
    use crate::settings::{Args, Settings};
    use http_body_util::BodyExt;

    type Res = Result<(), Box<dyn std::error::Error + Send + Sync>>;

    pub(crate) type Fixture = (tempfile::TempDir, Arc<State>, Client, String);

    pub(crate) fn fixture() -> Result<Fixture, Box<dyn std::error::Error + Send + Sync>> {
        let tmp = tempfile::tempdir()?;
        let store = Store::open(&tmp.path().join("data"))?;
        crate::access::ensure_local_credential(&store)?;
        let bearer = std::fs::read_to_string(crate::access::local_credential_path(&store))?;
        let client = crate::access::client(&store, bearer.trim())?.ok_or("missing local actor")?;
        let (token, _) = crate::ui_auth::exchange(&store, &crate::ui_auth::issue(&store)?)?
            .ok_or("pairing failed")?;
        let args = Args::parse(&[
            "--data-dir".into(),
            store.dir.to_string_lossy().into_owned(),
        ])?;
        let hub = Hub::new(&store)?;
        let state = Arc::new(State {
            store,
            settings: Settings::from_args(&args)?,
            hub,
        });
        Ok((tmp, state, client, token))
    }

    #[test]
    fn conversation_filter_matches_lead_lanes() {
        let matches = |v: &Value, c: &str| {
            v.get("conversation_id").and_then(Value::as_str) == Some(c)
                || v.get("lead_conversation_id").and_then(Value::as_str) == Some(c)
        };
        assert!(matches(&json!({"conversation_id": "c1"}), "c1"));
        assert!(matches(&json!({"lead_conversation_id": "c1"}), "c1"));
        assert!(!matches(&json!({"lead_conversation_id": "c2"}), "c1"));
    }

    #[tokio::test]
    async fn conversation_stream_skips_chief_of_staff_change() -> Res {
        let (_tmp, state, client, _) = fixture()?;
        let mut reply = stream(&state, &client, Some("c1"));
        opened(&mut reply).await?;
        let _ = state.hub.tx.send(Msg::Changes(
            vec![
                json!({"kind":"chief_of_staff", "record":{"conversation_id":"c1"}}),
                json!({"kind":"conversation", "id":"c1", "record":{}}),
            ]
            .into(),
        ));
        let frame = tokio::time::timeout(Duration::from_secs(1), reply.body_mut().frame())
            .await?
            .ok_or("stream closed")??
            .into_data()
            .map_err(|_| "not data")?;
        assert!(String::from_utf8_lossy(&frame).contains("\"kind\":\"conversation\""));
        Ok(())
    }

    #[tokio::test]
    async fn rotating_lead_credential_closes_open_stream() -> Res {
        let (_tmp, state, _client, _) = fixture()?;
        crate::access::register_node(&state.store, "home")?;
        let node = {
            let db = state.store.db();
            db.fixture_batch(
                "INSERT INTO conversations (org_id, conversation_id,node,spec,spec_version,model,
                instructions_tokens,tools,created_by,created_at,lead)
                VALUES(:org_id, 'c1','home','s','v','m',0,0,'local','now',1);",
            )?;
            db.query_row(
                "SELECT id FROM nodes WHERE +org_id = :org_id AND name='home'",
                rusqlite::params![],
                |r| r.get(0),
            )?
        };
        let minted = crate::access::mint_lead(&state.store, "first", "c1", None, "cred_test")?;
        let lead = crate::access::client(
            &state.store,
            minted["token"].as_str().ok_or("missing token")?,
        )?
        .ok_or("missing lead actor")?;
        let mut reply = stream(&state, &lead, None);
        opened(&mut reply).await?;
        assert_eq!(
            crate::lead_node::issue_credential(&state, node, "home", "c1").status(),
            201
        );
        let body =
            tokio::time::timeout(Duration::from_secs(1), reply.into_body().collect()).await??;
        assert!(String::from_utf8_lossy(&body.to_bytes()).contains("event: revoked"));
        Ok(())
    }

    #[test]
    fn ui_refuses_unscoped_and_other_event_scopes() -> Res {
        let (_tmp, state, _client, token) = fixture()?;
        let mut headers = hyper::HeaderMap::new();
        headers.insert("cookie", format!("cs_ui={token}").parse()?);
        for query in [
            "",
            "conversation=",
            "node=n1",
            "conversation=c1&node=n1",
            "conversation=c1&conversation=c2",
        ] {
            let call = Call {
                method: "GET".into(),
                path: "/ui/api/events".into(),
                query: query.into(),
                headers: headers.clone(),
                body: Bytes::new(),
            };
            let peer = Peer {
                ip: std::net::Ipv4Addr::LOCALHOST.into(),
                tls: false,
                local: ([127, 0, 0, 1], 8780).into(),
            };
            assert_eq!(
                crate::ui::handle(&state, &call, peer).status(),
                400,
                "{query}"
            );
        }
        Ok(())
    }

    async fn opened(body: &mut crate::server::Reply) -> Res {
        for _ in 0..2 {
            assert!(body.body_mut().frame().await.transpose()?.is_some());
        }
        Ok(())
    }

    #[tokio::test]
    async fn bearer_stream_reports_revocation_when_the_next_batch_arrives() -> Res {
        let (_tmp, state, owner, _) = fixture()?;
        let (code, _) = crate::access::issue_pairing(&state.store, "device", &owner.id, None)?;
        let (id, _, token) =
            crate::access::exchange_pairing(&state.store, &code)?.ok_or("pairing refused")?;
        let client = crate::access::client(&state.store, &token)?.ok_or("device refused")?;
        let mut reply = stream(&state, &client, None);
        opened(&mut reply).await?;
        state.hub.run_events(vec![json!({"message": "first"})]);
        let first = tokio::time::timeout(Duration::from_secs(1), reply.body_mut().frame())
            .await?
            .ok_or("stream closed")??
            .into_data()
            .map_err(|_| "not data")?;
        assert!(String::from_utf8_lossy(&first).contains("first"));
        // A direct store update leaves the stream to detect revocation on delivery.
        state.store.db().execute(
            "UPDATE credentials SET revoked_at = 'now' WHERE +org_id = :org_id AND id = ?1",
            params![id],
        )?;
        state
            .hub
            .run_events(vec![json!({"message": "after revocation"})]);
        let bytes = tokio::time::timeout(Duration::from_secs(1), reply.into_body().collect())
            .await??
            .to_bytes();
        assert_eq!(bytes, frame("revoked", None, &json!({})));
        Ok(())
    }

    #[tokio::test]
    async fn ui_stream_closes_on_logout_before_forwarding_an_event() -> Res {
        let (_tmp, state, _, token) = fixture()?;
        let (client, _) =
            crate::ui_auth::authenticate(&state.store, &token)?.ok_or("missing browser")?;
        let mut reply = browser_stream(&state, &client, "c1", &token);
        opened(&mut reply).await?;
        crate::ui_auth::logout(&state.store, &token)?;
        state.hub.run_events(vec![
            json!({"conversation_id": "c1", "message": "private reply"}),
        ]);
        let bytes = tokio::time::timeout(Duration::from_secs(1), reply.into_body().collect())
            .await??
            .to_bytes();
        assert_eq!(bytes, frame("revoked", None, &json!({})));
        Ok(())
    }

    #[tokio::test]
    async fn idle_ui_stream_closes_when_session_expires() -> Res {
        let (_tmp, state, _, token) = fixture()?;
        let (client, _) =
            crate::ui_auth::authenticate(&state.store, &token)?.ok_or("missing browser")?;
        // A short heartbeat exercises the same idle check without a 15-second test delay.
        assert!(HEARTBEAT <= Duration::from_secs(15));
        let mut reply = stream_inner(
            &state,
            &client,
            Some("c1"),
            Some(&token),
            Duration::from_millis(20),
        );
        opened(&mut reply).await?;
        state.store.db().execute(
            "UPDATE ui_sessions SET expires_at = 0 WHERE +org_id = :org_id",
            rusqlite::params![],
        )?;
        let bytes = tokio::time::timeout(Duration::from_secs(1), reply.into_body().collect())
            .await??
            .to_bytes();
        assert!(!String::from_utf8_lossy(&bytes).contains("private reply"));
        assert!(String::from_utf8_lossy(&bytes).contains("event: revoked"));
        Ok(())
    }

    #[tokio::test]
    async fn ui_stream_only_forwards_its_conversation() -> Res {
        let (_tmp, state, _, token) = fixture()?;
        let (client, _) =
            crate::ui_auth::authenticate(&state.store, &token)?.ok_or("missing browser")?;
        let mut reply = browser_stream(&state, &client, "c1", &token);
        opened(&mut reply).await?;
        state.hub.run_events(vec![
            json!({"conversation_id": "c2", "message": "other reply"}),
            json!({"conversation_id": "c1", "message": "our reply"}),
        ]);
        let frame = tokio::time::timeout(Duration::from_secs(1), reply.body_mut().frame())
            .await?
            .ok_or("stream closed")??
            .into_data()
            .map_err(|_| "not data")?;
        let text = String::from_utf8_lossy(&frame);
        assert!(text.contains("our reply"));
        assert!(!text.contains("other reply"));
        Ok(())
    }
}
