use axum::body::Body;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE, ETAG, IF_NONE_MATCH, LOCATION};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Json;
use bytes::Bytes;
use ferro_core::system::{FILES, FOLDERS};
use ferro_engine::files::{self as engine_files, BlobRef, Upload};
use ferro_core::hash::sha256_hex;
use ferro_engine::images::{self, RawParams, TransformParams};
use ferro_engine::items;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::items::{ctx, field_tree, list_collection, read_back, ItemsQuery};
use crate::extract::Acc;
use crate::{ApiError, ApiResult, AppState};

pub async fn list(State(st): State<AppState>, Acc(acc): Acc, Query(q): Query<ItemsQuery>) -> ApiResult<Json<Value>> {
    list_collection(&st, acc, FILES, &q).await
}

pub async fn get_one(State(st): State<AppState>, Acc(acc): Acc, Path(id): Path<String>, Query(q): Query<ItemsQuery>) -> ApiResult<Json<Value>> {
    let ctx = ctx(&st, acc);
    let mut conn = st.pool.acquire().await?;
    let f = items::read_one(&ctx, &mut conn, FILES, &id, field_tree(q.fields.as_deref())).await?;
    Ok(Json(json!({ "data": f })))
}

#[derive(Deserialize, Default)]
pub struct UploadQuery {
    /// Replace the binary of an existing file, keeping id and references.
    replace: Option<Uuid>,
}

/// `multipart/form-data`: optional text parts `folder`, `title` **before** the file part(s).
pub async fn upload(State(st): State<AppState>, Acc(acc): Acc, Query(q): Query<UploadQuery>, multipart: Multipart) -> ApiResult<Json<Value>> {
    let ctx = ctx(&st, acc);
    // blobs written before the transaction commits: removed again if anything fails (no orphans)
    let mut created: Vec<BlobRef> = vec![];
    let mut stale: Vec<BlobRef> = vec![];
    match upload_parts(&st, &ctx, q.replace, multipart, &mut created, &mut stale).await {
        Ok(ids) => {
            st.storages.purge(&stale).await;
            let mut out = vec![];
            for id in &ids {
                out.push(read_back(&st, &ctx, FILES, *id, None).await?);
            }
            let data = if out.len() == 1 { out.remove(0) } else { Value::Array(out) };
            Ok(Json(json!({ "data": data })))
        }
        Err(e) => {
            st.storages.purge(&created).await;
            Err(e)
        }
    }
}

async fn upload_parts(
    st: &AppState,
    ctx: &ferro_engine::items::Ctx,
    replace: Option<Uuid>,
    mut multipart: Multipart,
    created: &mut Vec<BlobRef>,
    stale: &mut Vec<BlobRef>,
) -> ApiResult<Vec<Uuid>> {
    let storage = st.storages.default_storage().clone();
    let mut folder: Option<Uuid> = None;
    let mut title: Option<String> = None;
    let mut ids = vec![];
    let mut tx = st.pool.begin().await?;
    while let Some(field) = multipart.next_field().await.map_err(|e| ApiError::bad_request(e.to_string()))? {
        let name = field.name().unwrap_or("").to_string();
        let file_name = field.file_name().map(String::from);
        match file_name {
            None => {
                let text = field.text().await.map_err(|e| ApiError::bad_request(e.to_string()))?;
                match name.as_str() {
                    "folder" if !text.is_empty() => {
                        folder = Some(Uuid::parse_str(&text).map_err(|_| ApiError::bad_request("invalid folder id"))?)
                    }
                    "title" => title = Some(text),
                    _ => {}
                }
            }
            Some(filename) => {
                let content_type = field.content_type().map(String::from);
                let data: Bytes = field.bytes().await.map_err(|e| ApiError::bad_request(e.to_string()))?;
                let replacing = if ids.is_empty() { replace } else { None };
                let up = engine_files::upload(
                    ctx,
                    &mut tx,
                    storage.as_ref(),
                    Upload { filename, content_type, data, folder, title: title.take(), replace: replacing },
                )
                .await?;
                if replacing.is_none() {
                    created.push(BlobRef { storage: storage.name().to_string(), key: up.id.to_string() });
                }
                stale.extend(up.stale);
                ids.push(up.id);
            }
        }
    }
    if ids.is_empty() {
        return Err(ApiError::bad_request("no file in the request"));
    }
    tx.commit().await?;
    Ok(ids)
}

pub async fn update(State(st): State<AppState>, Acc(acc): Acc, Path(id): Path<Uuid>, Json(body): Json<Value>) -> ApiResult<Json<Value>> {
    let ctx = ctx(&st, acc);
    // only descriptive metadata can be changed through the API
    let allowed = ["title", "description", "folder", "tags", "metadata", "filename_download"];
    let mut patch = serde_json::Map::new();
    if let Value::Object(m) = body {
        for (k, v) in m {
            if allowed.contains(&k.as_str()) {
                patch.insert(k, v);
            } else {
                return Err(ApiError::bad_request(format!("field `{k}` of a file cannot be changed")));
            }
        }
    }
    let mut tx = st.pool.begin().await?;
    items::update_one(&ctx, &mut tx, FILES, id, Value::Object(patch)).await?;
    tx.commit().await?;
    Ok(Json(json!({ "data": read_back(&st, &ctx, FILES, id, None).await? })))
}

pub async fn remove(State(st): State<AppState>, Acc(acc): Acc, Path(id): Path<Uuid>) -> ApiResult<StatusCode> {
    let ctx = ctx(&st, acc);
    let mut tx = st.pool.begin().await?;
    let blobs = engine_files::delete_files(&ctx, &mut tx, vec![id]).await?;
    tx.commit().await?;
    // only after the commit: a rollback must never lose the binary
    st.storages.purge(&blobs).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default)]
pub struct AssetQuery {
    download: Option<String>,
    // image transformations, see `ferro_engine::images`
    width: Option<String>,
    height: Option<String>,
    fit: Option<String>,
    position: Option<String>,
    crop: Option<String>,
    format: Option<String>,
    quality: Option<String>,
    #[serde(alias = "withoutEnlargement")]
    without_enlargement: Option<String>,
    background: Option<String>,
}

impl AssetQuery {
    fn raw_params(&self) -> RawParams {
        RawParams {
            width: self.width.clone(),
            height: self.height.clone(),
            fit: self.fit.clone(),
            position: self.position.clone(),
            crop: self.crop.clone(),
            format: self.format.clone(),
            quality: self.quality.clone(),
            without_enlargement: self.without_enlargement.clone(),
            background: self.background.clone(),
        }
    }
}

/// `If-None-Match` may carry several (possibly weak) validators.
fn etag_matches(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').any(|t| t.trim().trim_start_matches("W/").trim_matches('"') == etag || t.trim() == "*"))
        .unwrap_or(false)
}

/// `photo.png` + jpg → `photo.jpg`
fn with_extension(filename: &str, ext: &str) -> String {
    let stem = filename.rsplit_once('.').map(|(s, _)| s).filter(|s| !s.is_empty()).unwrap_or(filename);
    format!("{stem}.{ext}")
}

fn internal(msg: impl ToString) -> ApiError {
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL", msg.to_string())
}

/// Binary content. Requires read permission on the file (public policy for public assets).
///
/// Query: `download`, and the image transformations `width`, `height`, `fit`, `position`, `crop`, `format`,
/// `quality`, `withoutEnlargement`, `background` (documented in docs/ARCHITECTURE.md §10).
pub async fn asset(
    State(st): State<AppState>,
    Acc(acc): Acc,
    Path(id): Path<String>,
    Query(q): Query<AssetQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let ctx = ctx(&st, acc);
    let mut conn = st.pool.acquire().await?;
    let meta = items::read_one(
        &ctx,
        &mut conn,
        FILES,
        &id,
        ferro_core::fields::FieldTree::only(&["type", "filename_download", "checksum", "storage"]),
    )
    .await?;
    drop(conn);
    let params = TransformParams::parse(&q.raw_params(), &st.config.image_limits)?;
    let storage = st.storages.resolve(meta.get("storage").and_then(|v| v.as_str())).clone();
    let content_type = meta.get("type").and_then(|v| v.as_str()).unwrap_or("application/octet-stream").to_string();
    let filename = meta.get("filename_download").and_then(|v| v.as_str()).unwrap_or("file").replace(['"', '\\', '\r', '\n'], "_");
    let checksum = meta.get("checksum").and_then(|v| v.as_str()).map(String::from);
    let disposition_kind = if q.download.is_some() { "attachment" } else { "inline" };

    // the validator of a variant depends on the original's checksum and on the (normalised) parameters
    let etag = checksum.as_ref().map(|c| {
        if params.is_noop() {
            c.clone()
        } else {
            format!("{c}-{}", &sha256_hex(params.cache_key().as_bytes())[..12])
        }
    });
    if let Some(e) = &etag {
        if etag_matches(&headers, e) {
            return Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(ETAG, format!("\"{e}\""))
                .header(CACHE_CONTROL, "private, max-age=3600")
                .body(Body::empty())
                .map_err(internal);
        }
    }

    if !params.is_noop() {
        let key = format!("{id}:{}:{}", checksum.as_deref().unwrap_or(""), params.cache_key());
        let (data, format) = match st.image_cache.get(&key) {
            Some(hit) => hit,
            None => {
                let original = storage.get(&id).await?;
                // decoding holds the whole bitmap in memory: only a few at a time
                let _permit = st.image_gate.acquire().await.map_err(internal)?;
                let (p, limits) = (params.clone(), st.config.image_limits);
                let out = tokio::task::spawn_blocking(move || images::transform(&original, &p, &limits))
                    .await
                    .map_err(internal)??;
                st.image_cache.put(key, out.data.clone(), out.format);
                (out.data, out.format)
            }
        };
        let name = if params.format.is_some() { with_extension(&filename, format.extension()) } else { filename };
        let mut builder = Response::builder()
            .header(CONTENT_TYPE, format.content_type())
            .header(CONTENT_DISPOSITION, format!("{disposition_kind}; filename=\"{name}\""))
            .header(CACHE_CONTROL, "private, max-age=3600");
        if let Some(e) = &etag {
            builder = builder.header(ETAG, format!("\"{e}\""));
        }
        return builder.body(Body::from(data)).map_err(internal);
    }

    let disposition = format!("{disposition_kind}; filename=\"{filename}\"");
    // object stores can serve the bytes themselves through a short-lived signed URL (permissions were checked above)
    if let Some(url) = storage.presigned_get(&id, std::time::Duration::from_secs(300), &disposition, &content_type).await? {
        return Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header(LOCATION, url)
            .header(CACHE_CONTROL, "no-store")
            .body(Body::empty())
            .map_err(internal);
    }
    let data = storage.get(&id).await?;
    let mut builder = Response::builder()
        .header(CONTENT_TYPE, content_type)
        .header(CONTENT_DISPOSITION, disposition)
        .header(CACHE_CONTROL, "private, max-age=3600");
    if let Some(e) = &etag {
        builder = builder.header(ETAG, format!("\"{e}\""));
    }
    builder.body(Body::from(data)).map_err(internal)
}

pub async fn list_folders(State(st): State<AppState>, Acc(acc): Acc, Query(q): Query<ItemsQuery>) -> ApiResult<Json<Value>> {
    list_collection(&st, acc, FOLDERS, &q).await
}

pub async fn create_folder(State(st): State<AppState>, Acc(acc): Acc, Json(body): Json<Value>) -> ApiResult<Json<Value>> {
    let ctx = ctx(&st, acc);
    let mut tx = st.pool.begin().await?;
    let id = items::create_one(&ctx, &mut tx, FOLDERS, body).await?;
    tx.commit().await?;
    Ok(Json(json!({ "data": read_back(&st, &ctx, FOLDERS, id, None).await? })))
}
