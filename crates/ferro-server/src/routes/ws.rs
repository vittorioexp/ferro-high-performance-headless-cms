//! Realtime over WebSocket: `GET /websocket`.
//!
//! Client -> server (JSON text frames):
//! ```text
//! { "type": "auth", "access_token": "…" }            sign in (or use ?access_token= when connecting)
//! { "type": "subscribe", "uid": "a", "collection": "pages",
//!   "event": "update",                                optional: create | update | delete
//!   "query": { "fields": ["title", "blocks.*"], "filter": { "status": { "_eq": "published" } } } }
//! { "type": "unsubscribe", "uid": "a" }
//! { "type": "ping" }
//! ```
//! Server -> client:
//! ```text
//! { "type": "auth", "status": "ok" | "error" }
//! { "type": "subscription", "event": "init" | "create" | "update" | "delete" | "resync", "uid": "a", "data": [...] }
//! { "type": "error", "uid": "a", "code": "...", "message": "…" }
//! { "type": "pong" }
//! ```
//! `data` is read with the permissions of the connected user at the moment of the
//! event (see `ferro_engine::realtime`). A `resync` event means notifications may
//! have been missed: refetch.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use ferro_engine::items::Ctx;
use ferro_engine::realtime::{self, ChangeEvent, Subscription, MAX_SUBSCRIPTIONS};
use ferro_engine::{auth, Accountability, FerroError};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;

use crate::AppState;

const MAX_CONNECTIONS: usize = 2000;
const MAX_MESSAGE_BYTES: usize = 64 * 1024;
const HEARTBEAT: Duration = Duration::from_secs(30);
const IDLE_TIMEOUT: Duration = Duration::from_secs(100);

#[derive(Deserialize)]
pub struct WsQuery {
    access_token: Option<String>,
}

/// Counts open connections; released when the session ends, however it ends.
struct Slot<'a>(&'a AppState);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.ws_active.fetch_sub(1, Ordering::SeqCst);
    }
}

pub async fn upgrade(ws: WebSocketUpgrade, State(st): State<AppState>, headers: HeaderMap, Query(q): Query<WsQuery>) -> Response {
    if st.ws_active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
        return (StatusCode::SERVICE_UNAVAILABLE, "too many realtime connections").into_response();
    }
    let token = q.access_token.or_else(|| {
        headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|t| t.trim().to_string())
    });
    ws.max_message_size(MAX_MESSAGE_BYTES).on_upgrade(move |socket| session(st, socket, token))
}

async fn resolve(st: &AppState, token: Option<&str>) -> Result<Accountability, FerroError> {
    auth::accountability(&st.pool, &st.schema.get(), &st.auth, token).await
}

fn code(e: &FerroError) -> &'static str {
    match e {
        FerroError::NotFound(_) => "NOT_FOUND",
        FerroError::Forbidden(_) => "FORBIDDEN",
        FerroError::Unauthorized(_) => "UNAUTHORIZED",
        FerroError::Invalid(_) => "INVALID_PAYLOAD",
        _ => "INTERNAL",
    }
}

async fn send(tx: &mut futures::stream::SplitSink<WebSocket, Message>, v: Value) -> bool {
    tx.send(Message::Text(v.to_string())).await.is_ok()
}

async fn session(st: AppState, socket: WebSocket, token: Option<String>) {
    st.ws_active.fetch_add(1, Ordering::SeqCst);
    let _slot = Slot(&st);
    let (mut tx, mut rx) = socket.split();
    let mut events = st.hub.subscribe();
    let mut token = token;
    let mut subs: Vec<Subscription> = Vec::new();
    let mut last_seen = Instant::now();
    let mut heartbeat = tokio::time::interval(HEARTBEAT);

    let mut acc = match resolve(&st, token.as_deref()).await {
        Ok(a) => a,
        Err(e) => {
            let _ = send(&mut tx, json!({ "type": "auth", "status": "error", "code": code(&e), "message": e.to_string() })).await;
            let _ = tx.close().await;
            return;
        }
    };
    if token.is_some() && !send(&mut tx, json!({ "type": "auth", "status": "ok" })).await {
        return;
    }

    loop {
        tokio::select! {
            incoming = rx.next() => {
                let text = match incoming {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(_)) => { last_seen = Instant::now(); continue; } // ping / pong / binary
                };
                last_seen = Instant::now();
                let Ok(msg) = serde_json::from_str::<Value>(&text) else {
                    if !send(&mut tx, json!({ "type": "error", "code": "INVALID_PAYLOAD", "message": "messages must be JSON" })).await { break; }
                    continue;
                };
                let uid = msg.get("uid").cloned().unwrap_or(Value::Null);
                let reply = match msg.get("type").and_then(|t| t.as_str()) {
                    Some("ping") => json!({ "type": "pong" }),
                    Some("auth") => {
                        let candidate = msg.get("access_token").and_then(|t| t.as_str()).map(String::from);
                        match resolve(&st, candidate.as_deref()).await {
                            Ok(a) if candidate.is_some() => {
                                acc = a;
                                token = candidate;
                                subs.clear(); // subscriptions were validated for the previous identity
                                json!({ "type": "auth", "status": "ok" })
                            }
                            Ok(_) => json!({ "type": "auth", "status": "error", "code": "UNAUTHORIZED", "message": "access_token is required" }),
                            Err(e) => json!({ "type": "auth", "status": "error", "code": code(&e), "message": e.to_string() }),
                        }
                    }
                    Some("subscribe") => {
                        if subs.len() >= MAX_SUBSCRIPTIONS && !subs.iter().any(|s| Some(s.uid.as_str()) == uid.as_str()) {
                            json!({ "type": "error", "uid": uid, "code": "LIMIT", "message": format!("at most {MAX_SUBSCRIPTIONS} subscriptions per connection") })
                        } else {
                            let ctx = Ctx::new(st.schema.get(), acc.clone());
                            let opened = match st.pool.acquire().await {
                                Ok(mut conn) => realtime::open(&ctx, &mut conn, &msg).await,
                                Err(e) => Err(FerroError::from(e)),
                            };
                            match opened {
                                Ok(sub) => {
                                    subs.retain(|s| s.uid != sub.uid);
                                    let uid = sub.uid.clone();
                                    subs.push(sub);
                                    json!({ "type": "subscription", "event": "init", "uid": uid })
                                }
                                Err(e) => json!({ "type": "error", "uid": uid, "code": code(&e), "message": e.to_string() }),
                            }
                        }
                    }
                    Some("unsubscribe") => {
                        let before = subs.len();
                        subs.retain(|s| Some(s.uid.as_str()) != uid.as_str());
                        json!({ "type": "subscription", "event": "unsubscribe", "uid": uid, "removed": before != subs.len() })
                    }
                    _ => json!({ "type": "error", "uid": uid, "code": "INVALID_PAYLOAD", "message": "unknown message type" }),
                };
                if !send(&mut tx, reply).await { break; }
            }

            event = events.recv() => {
                let ev: std::sync::Arc<ChangeEvent> = match event {
                    Ok(ev) => ev,
                    Err(RecvError::Lagged(_)) => std::sync::Arc::new(ChangeEvent { collection: "*".into(), action: "resync".into(), keys: vec![], truncated: false }),
                    Err(RecvError::Closed) => break,
                };
                if subs.is_empty() { continue; }
                if ev.action == "resync" {
                    let mut ok = true;
                    for s in &subs {
                        ok &= send(&mut tx, json!({ "type": "subscription", "event": "resync", "uid": s.uid })).await;
                    }
                    if !ok { break; }
                    continue;
                }
                if !subs.iter().any(|s| s.wants(&ev)) { continue; }
                // identity is re-checked at every delivery: an expired token, a suspended user or a
                // changed role take effect immediately
                if token.is_some() {
                    match resolve(&st, token.as_deref()).await {
                        Ok(a) => acc = a,
                        Err(e) => {
                            let _ = send(&mut tx, json!({ "type": "auth", "status": "error", "code": code(&e), "message": e.to_string() })).await;
                            break;
                        }
                    }
                }
                let ctx = Ctx::new(st.schema.get(), acc.clone());
                let Ok(mut conn) = st.pool.acquire().await else { continue };
                let mut alive = true;
                for s in subs.iter().filter(|s| s.wants(&ev)) {
                    match realtime::deliver(&ctx, &mut conn, s, &ev).await {
                        Ok(Some(data)) => {
                            let mut m = json!({ "type": "subscription", "event": ev.action, "uid": s.uid, "data": data });
                            if ev.truncated { m["truncated"] = json!(true); }
                            alive &= send(&mut tx, m).await;
                        }
                        Ok(None) => {}
                        Err(e) => tracing::warn!(error = %e, collection = %ev.collection, "realtime delivery failed"),
                    }
                }
                if !alive { break; }
            }

            _ = heartbeat.tick() => {
                if last_seen.elapsed() > IDLE_TIMEOUT { break; }
                if tx.send(Message::Ping(Vec::new())).await.is_err() { break; }
            }
        }
    }
    let _ = tx.close().await;
}
