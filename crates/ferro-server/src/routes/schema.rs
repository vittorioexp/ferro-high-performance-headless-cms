use axum::extract::{Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum::Json;
use ferro_core::Snapshot;
use ferro_engine::schema_store::{self, ApplyOptions};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::extract::Acc;
use crate::{ApiError, ApiResult, AppState};

#[derive(Deserialize)]
pub struct SnapshotQuery {
    format: Option<String>,
}

/// The environment's model + access control, in the same format as the git files.
pub async fn snapshot(State(st): State<AppState>, Acc(acc): Acc, Query(q): Query<SnapshotQuery>) -> ApiResult<Response> {
    acc.require_admin()?;
    let schema = st.schema.get();
    if q.format.as_deref() == Some("yaml") {
        let text = serde_yaml::to_string(&schema.snapshot).map_err(|e| ApiError::bad_request(e.to_string()))?;
        return Ok(([(CONTENT_TYPE, "application/yaml")], text).into_response());
    }
    Ok(Json(json!({ "data": schema.snapshot, "meta": { "hash": schema.hash, "environment": st.config.env_name } })).into_response())
}

/// Plan only: what would change to reach the given snapshot.
pub async fn plan(State(st): State<AppState>, Acc(acc): Acc, Json(desired): Json<Snapshot>) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    let res = schema_store::plan(&st.pool, &desired).await?;
    Ok(Json(json!({
        "data": res,
        "meta": { "environment": st.config.env_name, "risk": res.plan.max_risk(), "changes": res.plan.summary_lines(), "sql": res.sql }
    })))
}

#[derive(Deserialize)]
pub struct ApplyBody {
    snapshot: Snapshot,
    #[serde(flatten)]
    options: ApplyOptions,
}

pub async fn apply(State(st): State<AppState>, Acc(acc): Acc, Json(body): Json<ApplyBody>) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    let res = schema_store::apply_snapshot(&st.pool, body.snapshot, &body.options, acc.user).await?;
    if res.applied {
        let s = st.reload_schema().await?;
        tracing::info!(hash = %s.hash, changes = res.plan.changes.len(), "schema applied");
    }
    Ok(Json(json!({
        "data": res,
        "meta": { "environment": st.config.env_name, "changes": res.plan.summary_lines(), "sql": res.sql }
    })))
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    limit: Option<i64>,
}

pub async fn history(State(st): State<AppState>, Acc(acc): Acc, Query(q): Query<HistoryQuery>) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    let v = schema_store::history(&st.pool, q.limit.unwrap_or(50)).await?;
    Ok(Json(json!({ "data": v })))
}
