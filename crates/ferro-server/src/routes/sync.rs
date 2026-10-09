//! Environment-to-environment data promotion. Administrator only.
//!
//! Typical flow (driven by `ferro data push --from dev --to prod`):
//! 1. `POST /sync/export` on the source → bundle (items + file metadata)
//! 2. `POST /sync/missing-files` on the target → which binaries are needed
//! 3. `GET /sync/files/:id` on the source, `PUT /sync/files/:id` on the target
//! 4. `POST /sync/import` on the target (with `dry_run: true` first: the report is the diff)

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Json;
use bytes::Bytes;
use ferro_core::bundle::{Bundle, ImportOptions, Selection};
use ferro_core::hash::sha256_hex;
use ferro_engine::sync;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::extract::Acc;
use crate::{ApiError, ApiResult, AppState};

#[derive(Deserialize)]
pub struct ExportBody {
    selections: Vec<Selection>,
}

pub async fn export(State(st): State<AppState>, Acc(acc): Acc, Json(body): Json<ExportBody>) -> ApiResult<Json<Bundle>> {
    acc.require_admin()?;
    if body.selections.is_empty() {
        return Err(ApiError::bad_request("at least one selection is required"));
    }
    let schema = st.schema.get();
    let mut conn = st.pool.acquire().await?;
    let bundle = sync::export(&mut conn, &schema, &body.selections, Some(st.config.env_name.clone())).await?;
    Ok(Json(bundle))
}

#[derive(Deserialize)]
pub struct FileRef {
    id: Uuid,
    #[serde(default)]
    checksum: Option<String>,
}

pub async fn missing_files(State(st): State<AppState>, Acc(acc): Acc, Json(files): Json<Vec<FileRef>>) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    let schema = st.schema.get();
    let mut conn = st.pool.acquire().await?;
    let list: Vec<(Uuid, Option<String>)> = files.into_iter().map(|f| (f.id, f.checksum)).collect();
    let missing = sync::missing_blobs(&mut conn, &schema, st.storages.default_storage().as_ref(), &list).await?;
    Ok(Json(json!({ "data": missing })))
}

pub async fn put_file(State(st): State<AppState>, Acc(acc): Acc, Path(id): Path<Uuid>, body: Bytes) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    let checksum = sha256_hex(&body);
    let size = body.len();
    st.storages.default_storage().put(&id.to_string(), body, None).await?;
    Ok(Json(json!({ "data": { "id": id, "checksum": checksum, "filesize": size } })))
}

pub async fn get_file(State(st): State<AppState>, Acc(acc): Acc, Path(id): Path<Uuid>) -> ApiResult<Response> {
    acc.require_admin()?;
    let mut conn = st.pool.acquire().await?;
    let storage = ferro_engine::files::storage_of(&mut conn, &st.storages, id).await?;
    drop(conn);
    let data = storage.get(&id.to_string()).await?;
    Response::builder()
        .header(CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(data))
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL", e.to_string()))
}

#[derive(Deserialize)]
pub struct ImportBody {
    bundle: Bundle,
    #[serde(default)]
    options: ImportOptions,
}

pub async fn import(State(st): State<AppState>, Acc(acc): Acc, Json(body): Json<ImportBody>) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    let schema = st.schema.get();
    let report = sync::import(
        &st.pool,
        &schema,
        &acc,
        st.storages.default_storage().as_ref(),
        body.bundle,
        &body.options,
        Some(st.config.env_name.clone()),
    )
    .await?;
    let t = report.totals();
    tracing::info!(
        dry_run = report.dry_run,
        committed = report.committed,
        create = t.create,
        update = t.update,
        delete = t.delete,
        conflict = t.conflict,
        "bundle import"
    );
    Ok(Json(json!({ "data": report })))
}

#[derive(Deserialize)]
pub struct ResetBody {
    collection: Option<String>,
}

/// Forget sync history (the `safe` strategy will then treat every difference as a conflict).
pub async fn reset_state(State(st): State<AppState>, Acc(acc): Acc, body: Option<Json<ResetBody>>) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    let collection = body.and_then(|Json(b)| b.collection);
    let n = match &collection {
        Some(c) => sqlx::query("DELETE FROM ferro_sync_state WHERE collection = $1").bind(c).execute(&st.pool).await?,
        None => sqlx::query("DELETE FROM ferro_sync_state").execute(&st.pool).await?,
    }
    .rows_affected();
    Ok(Json(json!({ "data": { "deleted": n } })))
}
