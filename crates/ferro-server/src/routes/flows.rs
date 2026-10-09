//! Flows: management (through the same plan/apply pipeline as the model),
//! manual runs, and the inbound endpoints `/hooks/:id` and `/x/:slug`.

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use ferro_core::flow::{Flow, Trigger, WebhookAccess};
use ferro_engine::{flows, Accountability, FerroError};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::collections::{apply, current, result_json, with_id, ChangeQuery};
use crate::extract::Acc;
use crate::{ApiError, ApiResult, AppState};

pub async fn list(State(st): State<AppState>, Acc(acc): Acc) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    Ok(Json(json!({ "data": st.schema.get().snapshot.flows })))
}

pub async fn put(
    State(st): State<AppState>,
    Acc(acc): Acc,
    Path(id): Path<Uuid>,
    Query(q): Query<ChangeQuery>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    let flow: Flow = with_id(body, id)?;
    let mut snap = current(&st);
    snap.flows.retain(|f| f.id != id);
    snap.flows.push(flow.clone());
    let res = apply(&st, &acc, snap, &q, false).await?;
    Ok(result_json(&res, json!(flow)))
}

pub async fn remove(State(st): State<AppState>, Acc(acc): Acc, Path(id): Path<Uuid>, Query(q): Query<ChangeQuery>) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    let mut snap = current(&st);
    snap.flows.retain(|f| f.id != id);
    let res = apply(&st, &acc, snap, &q, false).await?;
    Ok(result_json(&res, Value::Null))
}

#[derive(Deserialize)]
pub struct RunsQuery {
    limit: Option<i64>,
}

pub async fn runs(State(st): State<AppState>, Acc(acc): Acc, Path(id): Path<Uuid>, Query(q): Query<RunsQuery>) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    let data = flows::list_runs(&st.pool, id, q.limit.unwrap_or(50)).await?;
    Ok(Json(json!({ "data": data })))
}

// ---- manual flows -----------------------------------------------------------

fn allowed_manual(acc: &Accountability, flow: &Flow) -> bool {
    match &flow.trigger {
        Trigger::Manual { roles, .. } => {
            acc.admin
                || (acc.user.is_some()
                    && if roles.is_empty() {
                        acc.app
                    } else {
                        acc.role.map(|r| roles.contains(&r)).unwrap_or(false)
                    })
        }
        _ => false,
    }
}

#[derive(Deserialize)]
pub struct ActionsQuery {
    collection: Option<String>,
}

/// Manual flows the caller may run (used by the item editor to show buttons).
pub async fn actions(State(st): State<AppState>, Acc(acc): Acc, Query(q): Query<ActionsQuery>) -> ApiResult<Json<Value>> {
    if acc.user.is_none() {
        return Err(FerroError::Unauthorized("sign in first".into()).into());
    }
    let schema = st.schema.get();
    let list: Vec<Value> = schema
        .snapshot
        .flows
        .iter()
        .filter(|f| f.active && allowed_manual(&acc, f))
        .filter(|f| match &f.trigger {
            Trigger::Manual { collections, .. } => match &q.collection {
                Some(c) => collections.is_empty() || collections.contains(c),
                None => true,
            },
            _ => false,
        })
        .map(|f| json!({ "id": f.id, "name": f.name, "description": f.description }))
        .collect();
    Ok(Json(json!({ "data": list })))
}

#[derive(Deserialize, Default)]
pub struct ManualBody {
    #[serde(default)]
    collection: Option<String>,
    #[serde(default)]
    keys: Vec<Uuid>,
    #[serde(default)]
    payload: Option<Value>,
}

pub async fn run_manual(State(st): State<AppState>, Acc(acc): Acc, Path(id): Path<Uuid>, body: Option<Json<ManualBody>>) -> ApiResult<Json<Value>> {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let schema = st.schema.get();
    let flow = schema.snapshot.flow(id).filter(|f| f.active).ok_or_else(|| FerroError::not_found("flow not found"))?;
    if !allowed_manual(&acc, flow) {
        return Err(FerroError::forbidden("you cannot run this flow").into());
    }
    if let (Trigger::Manual { collections, .. }, Some(c)) = (&flow.trigger, &body.collection) {
        if !collections.is_empty() && !collections.contains(c) {
            return Err(ApiError::bad_request(format!("this flow cannot run on `{c}`")));
        }
    }
    let trigger = json!({
        "event": "manual",
        "collection": body.collection,
        "keys": body.keys,
        "key": body.keys.first(),
        "payload": body.payload,
        "accountability": { "user": acc.user, "role": acc.role },
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    let (outcome, response) = flows::run_sync(&st.pool, schema.clone(), flow, "manual", trigger, &acc).await?;
    Ok(Json(json!({ "data": response, "meta": { "ok": outcome.ok, "error": outcome.error, "log": outcome.log } })))
}

// ---- inbound webhooks / custom endpoints ----------------------------------------

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn webhook(st: &AppState, acc: Accountability, flow: &Flow, method: Method, query: HashMap<String, String>, headers: HeaderMap, body: Bytes) -> ApiResult<Response> {
    let Trigger::Webhook { methods, access, secret_env, run_async, .. } = &flow.trigger else {
        return Err(FerroError::not_found("not found").into());
    };
    let allowed: Vec<String> = if methods.is_empty() { vec!["POST".to_string()] } else { methods.clone() };
    if !allowed.iter().any(|m| m.as_str() == method.as_str()) {
        return Err(ApiError::new(StatusCode::METHOD_NOT_ALLOWED, "METHOD_NOT_ALLOWED", format!("use {}", allowed.join(", "))));
    }
    match access {
        WebhookAccess::Public => {}
        WebhookAccess::Authenticated => {
            if acc.user.is_none() {
                return Err(FerroError::Unauthorized("authentication required".into()).into());
            }
        }
        WebhookAccess::Admin => acc.require_admin()?,
    }
    if let Some(var) = secret_env {
        let expected = std::env::var(var).unwrap_or_default();
        let given = headers.get("x-ferro-secret").and_then(|v| v.to_str().ok()).unwrap_or("");
        if expected.is_empty() || !ct_eq(expected.as_bytes(), given.as_bytes()) {
            return Err(FerroError::forbidden("invalid secret").into());
        }
    }

    let parsed = serde_json::from_slice::<Value>(&body).unwrap_or_else(|_| {
        if body.is_empty() {
            Value::Null
        } else {
            Value::String(String::from_utf8_lossy(&body).into_owned())
        }
    });
    let mut hdrs = Map::new();
    for (k, v) in headers.iter() {
        let name = k.as_str();
        if matches!(name, "authorization" | "cookie" | "x-ferro-secret") {
            continue;
        }
        if let Ok(s) = v.to_str() {
            hdrs.insert(name.to_string(), Value::String(s.to_string()));
        }
    }
    let trigger = json!({
        "event": "webhook",
        "method": method.as_str(),
        "query": query,
        "headers": hdrs,
        "body": parsed,
        "accountability": { "user": acc.user, "role": acc.role },
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });

    if *run_async {
        flows::enqueue_flow(&st.pool, flow, &trigger).await?;
        return Ok((StatusCode::ACCEPTED, Json(json!({ "queued": true }))).into_response());
    }
    let (outcome, response) = flows::run_sync(&st.pool, st.schema.get(), flow, "webhook", trigger, &acc).await?;
    if !outcome.ok {
        return Err(ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "FLOW_FAILED", outcome.error.unwrap_or_default()));
    }
    Ok(Json(response).into_response())
}

pub async fn hook_by_id(
    State(st): State<AppState>,
    Acc(acc): Acc,
    Path(id): Path<Uuid>,
    method: Method,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let schema = st.schema.get();
    let flow = schema
        .snapshot
        .flow(id)
        .filter(|f| f.active && matches!(f.trigger, Trigger::Webhook { .. }))
        .ok_or_else(|| FerroError::not_found("not found"))?;
    webhook(&st, acc, flow, method, query, headers, body).await
}

pub async fn hook_by_slug(
    State(st): State<AppState>,
    Acc(acc): Acc,
    Path(slug): Path<String>,
    method: Method,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let schema = st.schema.get();
    let flow = schema
        .snapshot
        .flows
        .iter()
        .find(|f| f.active && matches!(&f.trigger, Trigger::Webhook { slug: Some(s), .. } if *s == slug))
        .ok_or_else(|| FerroError::not_found("not found"))?;
    webhook(&st, acc, flow, method, query, headers, body).await
}
