use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use ferro_core::fields::FieldTree;
use ferro_core::system::{ACTIVITY, REVISIONS};
use ferro_engine::items::{self, Ctx, ReadArgs};
use ferro_engine::{Accountability, FerroError};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::extract::Acc;
use crate::{ApiError, ApiResult, AppState};

/// Query parameters shared by every listing (Directus compatible names).
#[derive(Debug, Default, Deserialize)]
pub struct ItemsQuery {
    pub fields: Option<String>,
    /// JSON filter, e.g. `{"status":{"_eq":"published"}}`
    pub filter: Option<String>,
    pub sort: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub page: Option<i64>,
    pub search: Option<String>,
    /// `total_count`, `filter_count` or `*`
    pub meta: Option<String>,
    /// Language preference, e.g. `it-IT,en-US`: the translatable fields of collections with a `translations` field are
    /// returned on the item, each from the first language that has a value (see docs, "Translations").
    pub lang: Option<String>,
}

/// Read rows, resolving translations when `lang` was asked for.
pub async fn read_rows(ctx: &Ctx, conn: &mut sqlx::PgConnection, collection: &str, args: ReadArgs, lang: Option<&str>) -> ApiResult<Vec<Value>> {
    match lang.map(str::trim).filter(|l| !l.is_empty()) {
        None => Ok(items::read_many(ctx, conn, collection, args).await?),
        Some(l) => {
            let langs = ferro_engine::translations::parse_languages(l)?;
            Ok(ferro_engine::translations::read_translated(ctx, conn, collection, args, &langs).await?)
        }
    }
}

pub fn field_tree(fields: Option<&str>) -> FieldTree {
    FieldTree::parse(&[fields.unwrap_or("*")])
}

pub fn read_args(q: &ItemsQuery, max_limit: i64) -> ApiResult<ReadArgs> {
    let filter = match q.filter.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(f) => Some(serde_json::from_str::<Value>(f).map_err(|e| ApiError::bad_request(format!("invalid filter: {e}")))?),
        None => None,
    };
    let limit = match q.limit {
        None => None,
        Some(l) if l < 0 => Some(if max_limit < 0 { -1 } else { max_limit }),
        Some(l) if max_limit >= 0 => Some(l.min(max_limit)),
        Some(l) => Some(l),
    };
    let effective = match limit {
        None => items::DEFAULT_LIMIT,
        Some(l) => l,
    };
    let offset = match (q.offset, q.page) {
        (Some(o), _) => Some(o),
        (None, Some(p)) if p > 1 && effective > 0 => Some((p - 1) * effective),
        _ => None,
    };
    Ok(ReadArgs {
        fields: field_tree(q.fields.as_deref()),
        filter,
        sort: q.sort.as_deref().map(|s| s.split(',').map(|x| x.trim().to_string()).collect()).unwrap_or_default(),
        limit,
        offset,
        search: q.search.clone(),
        lenient: false,
    })
}

pub fn ctx(st: &AppState, acc: Accountability) -> Ctx {
    Ctx::new(st.schema.get(), acc)
}

/// Shared listing implementation (also used by /files, /users, /activity...).
pub async fn list_collection(st: &AppState, acc: Accountability, collection: &str, q: &ItemsQuery) -> ApiResult<Json<Value>> {
    let ctx = ctx(st, acc);
    let info = ctx.schema.collection(collection)?;
    let mut conn = st.pool.acquire().await?;
    if info.def.meta.singleton {
        let args = ReadArgs { fields: field_tree(q.fields.as_deref()), limit: Some(1), ..Default::default() };
        let rows = read_rows(&ctx, &mut conn, collection, args, q.lang.as_deref()).await?;
        return Ok(Json(json!({ "data": rows.into_iter().next().unwrap_or(Value::Null) })));
    }
    let args = read_args(q, st.config.query_limit_max)?;
    let filter = args.filter.clone();
    let search = args.search.clone();
    let rows = read_rows(&ctx, &mut conn, collection, args, q.lang.as_deref()).await?;
    let mut body = json!({ "data": rows });
    if let Some(meta) = q.meta.as_deref() {
        let mut m = serde_json::Map::new();
        if meta.contains("total_count") || meta == "*" {
            m.insert("total_count".into(), json!(items::count(&ctx, &mut conn, collection, None, None).await?));
        }
        if meta.contains("filter_count") || meta == "*" {
            m.insert(
                "filter_count".into(),
                json!(items::count(&ctx, &mut conn, collection, filter.as_ref(), search.as_deref()).await?),
            );
        }
        body["meta"] = Value::Object(m);
    }
    Ok(Json(body))
}

/// Read back an item after a write; callers without read access get only the id.
pub async fn read_back(st: &AppState, ctx: &Ctx, collection: &str, id: Uuid, fields: Option<&str>) -> ApiResult<Value> {
    let mut conn = st.pool.acquire().await?;
    match items::read_one(ctx, &mut conn, collection, &id.to_string(), field_tree(fields)).await {
        Ok(v) => Ok(v),
        Err(FerroError::Forbidden(_)) | Err(FerroError::NotFound(_)) => Ok(json!({ "id": id })),
        Err(e) => Err(e.into()),
    }
}

fn parse_id(id: &str) -> ApiResult<Uuid> {
    Uuid::parse_str(id).map_err(|_| ApiError::bad_request(format!("`{id}` is not a valid id")))
}

pub async fn list(State(st): State<AppState>, Acc(acc): Acc, Path(collection): Path<String>, Query(q): Query<ItemsQuery>) -> ApiResult<Json<Value>> {
    list_collection(&st, acc, &collection, &q).await
}

pub async fn get_one(
    State(st): State<AppState>,
    Acc(acc): Acc,
    Path((collection, id)): Path<(String, String)>,
    Query(q): Query<ItemsQuery>,
) -> ApiResult<Json<Value>> {
    let ctx = ctx(&st, acc);
    let mut conn = st.pool.acquire().await?;
    if q.lang.as_deref().map(|l| !l.trim().is_empty()).unwrap_or(false) {
        Uuid::parse_str(&id).map_err(|_| ApiError::bad_request(format!("`{id}` is not a valid id")))?;
        let args = ReadArgs { fields: field_tree(q.fields.as_deref()), filter: Some(json!({ "id": { "_eq": id } })), limit: Some(1), ..Default::default() };
        let mut rows = read_rows(&ctx, &mut conn, &collection, args, q.lang.as_deref()).await?;
        if rows.is_empty() {
            return Err(FerroError::not_found(format!("item `{id}` not found in `{collection}`")).into());
        }
        return Ok(Json(json!({ "data": rows.remove(0) })));
    }
    let item = items::read_one(&ctx, &mut conn, &collection, &id, field_tree(q.fields.as_deref())).await?;
    Ok(Json(json!({ "data": item })))
}

pub async fn create(
    State(st): State<AppState>,
    Acc(acc): Acc,
    Path(collection): Path<String>,
    Query(q): Query<ItemsQuery>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let ctx = ctx(&st, acc);
    let info = ctx.schema.collection(&collection)?;
    let mut tx = st.pool.begin().await?;
    if info.def.meta.singleton {
        let id = match items::singleton_id(&mut tx, &info).await? {
            Some(id) => {
                items::update_one(&ctx, &mut tx, &collection, id, body).await?;
                id
            }
            None => items::create_one(&ctx, &mut tx, &collection, body).await?,
        };
        tx.commit().await?;
        return Ok(Json(json!({ "data": read_back(&st, &ctx, &collection, id, q.fields.as_deref()).await? })));
    }
    let data = match body {
        Value::Array(list) => {
            let mut ids = vec![];
            for item in list {
                ids.push(items::create_one(&ctx, &mut tx, &collection, item).await?);
            }
            tx.commit().await?;
            let mut out = vec![];
            for id in ids {
                out.push(read_back(&st, &ctx, &collection, id, q.fields.as_deref()).await?);
            }
            Value::Array(out)
        }
        other => {
            let id = items::create_one(&ctx, &mut tx, &collection, other).await?;
            tx.commit().await?;
            read_back(&st, &ctx, &collection, id, q.fields.as_deref()).await?
        }
    };
    Ok(Json(json!({ "data": data })))
}

pub async fn update(
    State(st): State<AppState>,
    Acc(acc): Acc,
    Path((collection, id)): Path<(String, String)>,
    Query(q): Query<ItemsQuery>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let id = parse_id(&id)?;
    let ctx = ctx(&st, acc);
    let mut tx = st.pool.begin().await?;
    items::update_one(&ctx, &mut tx, &collection, id, body).await?;
    tx.commit().await?;
    Ok(Json(json!({ "data": read_back(&st, &ctx, &collection, id, q.fields.as_deref()).await? })))
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum BatchUpdate {
    /// `{ "keys": [..], "data": {..} }` – same change on many items
    Keys { keys: Vec<Uuid>, data: Value },
    /// `[ { "id": .., ... }, ... ]` – different change per item
    List(Vec<Value>),
}

pub async fn update_many(
    State(st): State<AppState>,
    Acc(acc): Acc,
    Path(collection): Path<String>,
    Query(q): Query<ItemsQuery>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let ctx = ctx(&st, acc);
    let info = ctx.schema.collection(&collection)?;
    let mut tx = st.pool.begin().await?;
    if info.def.meta.singleton {
        let id = match items::singleton_id(&mut tx, &info).await? {
            Some(id) => {
                items::update_one(&ctx, &mut tx, &collection, id, body).await?;
                id
            }
            None => items::create_one(&ctx, &mut tx, &collection, body).await?,
        };
        tx.commit().await?;
        return Ok(Json(json!({ "data": read_back(&st, &ctx, &collection, id, q.fields.as_deref()).await? })));
    }
    let batch: BatchUpdate = serde_json::from_value(body)?;
    let mut ids = vec![];
    match batch {
        BatchUpdate::Keys { keys, data } => {
            for id in keys {
                items::update_one(&ctx, &mut tx, &collection, id, data.clone()).await?;
                ids.push(id);
            }
        }
        BatchUpdate::List(list) => {
            for mut item in list {
                let id = item
                    .get("id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| Uuid::parse_str(s).ok())
                    .ok_or_else(|| ApiError::bad_request("every item needs an `id`"))?;
                if let Some(o) = item.as_object_mut() {
                    o.remove("id");
                }
                items::update_one(&ctx, &mut tx, &collection, id, item).await?;
                ids.push(id);
            }
        }
    }
    tx.commit().await?;
    let mut out = vec![];
    for id in ids {
        out.push(read_back(&st, &ctx, &collection, id, q.fields.as_deref()).await?);
    }
    Ok(Json(json!({ "data": out })))
}

pub async fn delete_one(State(st): State<AppState>, Acc(acc): Acc, Path((collection, id)): Path<(String, String)>) -> ApiResult<StatusCode> {
    let id = parse_id(&id)?;
    let ctx = ctx(&st, acc);
    let mut tx = st.pool.begin().await?;
    items::delete_many(&ctx, &mut tx, &collection, vec![id]).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum BatchDelete {
    Keys { keys: Vec<Uuid> },
    List(Vec<Uuid>),
}

pub async fn delete_many(
    State(st): State<AppState>,
    Acc(acc): Acc,
    Path(collection): Path<String>,
    Json(body): Json<BatchDelete>,
) -> ApiResult<StatusCode> {
    let ids = match body {
        BatchDelete::Keys { keys } => keys,
        BatchDelete::List(l) => l,
    };
    let ctx = ctx(&st, acc);
    let mut tx = st.pool.begin().await?;
    items::delete_many(&ctx, &mut tx, &collection, ids).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn activity(State(st): State<AppState>, Acc(acc): Acc, Query(q): Query<ItemsQuery>) -> ApiResult<Json<Value>> {
    list_collection(&st, acc, ACTIVITY, &q).await
}

pub async fn revisions(State(st): State<AppState>, Acc(acc): Acc, Query(q): Query<ItemsQuery>) -> ApiResult<Json<Value>> {
    list_collection(&st, acc, REVISIONS, &q).await
}
