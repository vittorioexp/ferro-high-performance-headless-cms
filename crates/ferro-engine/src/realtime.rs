//! Realtime: change notifications for WebSocket subscribers.
//!
//! Every item write publishes a small notification (`collection`, `action`, the
//! ids, never the data) with `pg_notify` **inside the transaction of the write**:
//! PostgreSQL delivers it only if the transaction commits, and to every instance.
//! Each instance listens on the channel and fans the notification out, in memory,
//! to its connected clients ([`Hub`]).
//!
//! The data a client receives is *read again* with that client's own permissions
//! ([`deliver`]): field restrictions, permission filters and the filter of the
//! subscription all apply, so a client never sees what it could not read over REST.

use std::sync::Arc;
use std::time::Duration;

use ferro_core::fields::FieldTree;
use ferro_core::filter::and_all;
use ferro_core::Action;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::postgres::PgListener;
use sqlx::{PgConnection, PgPool};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::items::{self, Ctx, ReadArgs};
use crate::permissions;
use crate::{FerroError, Result};

pub const CHANNEL: &str = "ferro_events";
/// A NOTIFY payload is limited to 8000 bytes: bigger writes are reported as `truncated`.
pub const MAX_KEYS: usize = 40;
pub const MAX_SUBSCRIPTIONS: usize = 50;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeEvent {
    pub collection: String,
    /// `create`, `update`, `delete`, or `resync` (events may have been missed).
    pub action: String,
    pub keys: Vec<Uuid>,
    #[serde(default)]
    pub truncated: bool,
}

/// Publish a change. Call it on the connection of the write, before the commit.
pub async fn notify(conn: &mut PgConnection, collection: &str, action: &str, keys: &[Uuid]) -> Result<()> {
    let event = ChangeEvent {
        collection: collection.to_string(),
        action: action.to_string(),
        keys: keys.iter().take(MAX_KEYS).copied().collect(),
        truncated: keys.len() > MAX_KEYS,
    };
    let payload = serde_json::to_string(&event)?;
    sqlx::query("SELECT pg_notify($1, $2)").bind(CHANNEL).bind(payload).execute(&mut *conn).await?;
    Ok(())
}

/// In-process fan-out of the notifications received by this instance.
pub struct Hub {
    tx: broadcast::Sender<Arc<ChangeEvent>>,
}

impl Default for Hub {
    fn default() -> Self {
        Hub::new()
    }
}

impl Hub {
    pub fn new() -> Hub {
        let (tx, _) = broadcast::channel(1024);
        Hub { tx }
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<ChangeEvent>> {
        self.tx.subscribe()
    }
    pub fn publish(&self, event: ChangeEvent) {
        let _ = self.tx.send(Arc::new(event)); // no receivers = nobody connected
    }
}

/// Listen for notifications forever and publish them on the hub. After a
/// reconnection a `resync` event tells clients that events may have been missed.
pub async fn run_listener(pool: PgPool, hub: Arc<Hub>) {
    let mut first = true;
    loop {
        match PgListener::connect_with(&pool).await {
            Ok(mut listener) => {
                if listener.listen(CHANNEL).await.is_ok() {
                    if !first {
                        hub.publish(ChangeEvent { collection: "*".into(), action: "resync".into(), keys: vec![], truncated: false });
                    }
                    first = false;
                    while let Ok(n) = listener.recv().await {
                        match serde_json::from_str::<ChangeEvent>(n.payload()) {
                            Ok(ev) => hub.publish(ev),
                            Err(e) => tracing::warn!(error = %e, "ignored a malformed realtime notification"),
                        }
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "realtime listener: connection failed"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

// ---------------------------------------------------------------------------
// subscriptions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Subscription {
    pub uid: String,
    pub collection: String,
    /// `None` = every action.
    pub event: Option<String>,
    pub fields: FieldTree,
    pub filter: Option<Value>,
}

impl Subscription {
    pub fn wants(&self, ev: &ChangeEvent) -> bool {
        self.collection == ev.collection && self.event.as_deref().map(|e| e == ev.action).unwrap_or(true)
    }
}

/// `{ "type": "subscribe", "uid": "a", "collection": "pages", "event": "update",
///    "query": { "fields": ["title", "blocks.*"], "filter": { … } } }`
pub fn parse_subscription(msg: &Value) -> std::result::Result<Subscription, String> {
    let uid = msg.get("uid").and_then(|v| v.as_str()).filter(|u| !u.is_empty() && u.len() <= 64).ok_or("`uid` (1-64 characters) is required")?;
    let collection = msg.get("collection").and_then(|v| v.as_str()).filter(|c| !c.is_empty()).ok_or("`collection` is required")?;
    let event = match msg.get("event") {
        None | Some(Value::Null) => None,
        Some(Value::String(e)) if ["create", "update", "delete"].contains(&e.as_str()) => Some(e.clone()),
        Some(_) => return Err("`event` must be create, update or delete".to_string()),
    };
    let query = msg.get("query").unwrap_or(&Value::Null);
    let fields = match query.get("fields") {
        None | Some(Value::Null) => FieldTree::star(),
        Some(Value::String(s)) => FieldTree::parse(&[s.as_str()]),
        Some(Value::Array(a)) => {
            let list: Vec<&str> = a.iter().filter_map(|x| x.as_str()).collect();
            FieldTree::parse(&list)
        }
        Some(_) => return Err("`query.fields` must be a string or a list".to_string()),
    };
    let filter = match query.get("filter") {
        None | Some(Value::Null) => None,
        Some(f @ Value::Object(_)) => Some(f.clone()),
        Some(_) => return Err("`query.filter` must be an object".to_string()),
    };
    Ok(Subscription { uid: uid.to_string(), collection: collection.to_string(), event, fields, filter })
}

/// Parse and *check* a subscription for the caller: the collection must exist, be a
/// user collection that the caller can read, and the fields / filter must be valid
/// (verified with a read that returns no rows).
pub async fn open(ctx: &Ctx, conn: &mut PgConnection, msg: &Value) -> Result<Subscription> {
    let sub = parse_subscription(msg).map_err(|e| FerroError::invalid(e))?;
    let info = ctx.schema.collection(&sub.collection)?;
    if info.system {
        return Err(FerroError::invalid("system collections cannot be subscribed to"));
    }
    permissions::access(&ctx.acc, &sub.collection, Action::Read)?;
    let probe = ReadArgs { fields: sub.fields.clone(), filter: sub.filter.clone(), limit: Some(0), ..Default::default() };
    items::read_many(ctx, conn, &sub.collection, probe).await?;
    Ok(sub)
}

/// What `ctx`'s user may see of `ev` for `sub`: `None` = nothing to send.
///
/// * create / update: the items, read again with the user's permissions, the
///   subscription fields and (and-ed) the subscription filter;
/// * delete: the ids (the items are gone, the filter cannot be evaluated).
pub async fn deliver(ctx: &Ctx, conn: &mut PgConnection, sub: &Subscription, ev: &ChangeEvent) -> Result<Option<Value>> {
    if permissions::access(&ctx.acc, &ev.collection, Action::Read).is_err() {
        return Ok(None);
    }
    if ev.action == "delete" {
        return Ok(Some(json!(ev.keys)));
    }
    if ev.keys.is_empty() {
        return Ok(None);
    }
    let ids: Vec<String> = ev.keys.iter().map(|k| k.to_string()).collect();
    let filter = and_all(vec![sub.filter.clone(), Some(json!({ "id": { "_in": ids } }))]);
    let args = ReadArgs { fields: sub.fields.clone(), filter, limit: Some(-1), ..Default::default() };
    let rows = items::read_many(ctx, conn, &ev.collection, args).await?;
    Ok(if rows.is_empty() { None } else { Some(Value::Array(rows)) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_subscriptions() {
        let s = parse_subscription(&json!({
            "uid": "a1", "collection": "pages", "event": "update",
            "query": { "fields": ["title", "blocks.*"], "filter": { "status": { "_eq": "published" } } }
        }))
        .unwrap();
        assert_eq!(s.uid, "a1");
        assert_eq!(s.event.as_deref(), Some("update"));
        assert!(s.fields.wants("title") && s.fields.child("blocks").is_some());
        assert_eq!(s.filter, Some(json!({ "status": { "_eq": "published" } })));

        let minimal = parse_subscription(&json!({ "uid": "b", "collection": "pages" })).unwrap();
        assert!(minimal.fields.all && minimal.event.is_none() && minimal.filter.is_none());
        let as_string = parse_subscription(&json!({ "uid": "c", "collection": "pages", "query": { "fields": "id,title" } })).unwrap();
        assert!(as_string.fields.wants("title") && !as_string.fields.wants("slug"));
    }

    #[test]
    fn rejects_bad_subscriptions() {
        for bad in [
            json!({ "collection": "pages" }),
            json!({ "uid": "", "collection": "pages" }),
            json!({ "uid": "x" }),
            json!({ "uid": "x", "collection": "pages", "event": "explode" }),
            json!({ "uid": "x", "collection": "pages", "query": { "filter": "nope" } }),
            json!({ "uid": "x", "collection": "pages", "query": { "fields": 3 } }),
        ] {
            assert!(parse_subscription(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn matching_events() {
        let all = parse_subscription(&json!({ "uid": "a", "collection": "pages" })).unwrap();
        let only_delete = parse_subscription(&json!({ "uid": "b", "collection": "pages", "event": "delete" })).unwrap();
        let ev = |c: &str, a: &str| ChangeEvent { collection: c.into(), action: a.into(), keys: vec![], truncated: false };
        assert!(all.wants(&ev("pages", "create")));
        assert!(!all.wants(&ev("posts", "create")));
        assert!(only_delete.wants(&ev("pages", "delete")));
        assert!(!only_delete.wants(&ev("pages", "update")));
    }

    #[test]
    fn events_roundtrip_through_the_notify_payload() {
        let keys: Vec<Uuid> = (0..3).map(|_| Uuid::now_v7()).collect();
        let ev = ChangeEvent { collection: "pages".into(), action: "update".into(), keys, truncated: false };
        let text = serde_json::to_string(&ev).unwrap();
        assert!(text.len() < 8000);
        assert_eq!(serde_json::from_str::<ChangeEvent>(&text).unwrap(), ev);
        // 40 ids stay well under the NOTIFY limit
        let many = ChangeEvent { keys: (0..MAX_KEYS).map(|_| Uuid::now_v7()).collect(), ..ev };
        assert!(serde_json::to_string(&many).unwrap().len() < 2500);
    }

    #[tokio::test]
    async fn hub_fans_out_to_every_subscriber() {
        let hub = Hub::new();
        let mut a = hub.subscribe();
        let mut b = hub.subscribe();
        hub.publish(ChangeEvent { collection: "pages".into(), action: "create".into(), keys: vec![], truncated: false });
        assert_eq!(a.recv().await.unwrap().action, "create");
        assert_eq!(b.recv().await.unwrap().collection, "pages");
    }
}
