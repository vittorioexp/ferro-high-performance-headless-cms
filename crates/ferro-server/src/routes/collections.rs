//! Data model and access control editing (used by the admin UI). Every change is
//! applied as a new snapshot through the same plan/apply pipeline used by the
//! CLI, so UI edits and git-driven deployments can never diverge.

use axum::extract::{Path, Query, State};
use axum::Json;
use ferro_core::{Action, Collection, Field, Policy, Role, Snapshot, ADMIN_POLICY_ID};
use ferro_engine::permissions;
use ferro_engine::schema_store::{apply_snapshot, ApplyOptions, ApplyResult};
use ferro_engine::{Accountability, FerroError};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::extract::Acc;
use crate::{ApiError, ApiResult, AppState};

#[derive(Debug, Default, Deserialize)]
pub struct ChangeQuery {
    #[serde(default)]
    allow_destructive: bool,
    #[serde(default)]
    dry_run: bool,
}

pub(crate) async fn apply(st: &AppState, acc: &Accountability, snap: Snapshot, q: &ChangeQuery, destructive: bool) -> ApiResult<ApplyResult> {
    let opts = ApplyOptions {
        allow_destructive: destructive || q.allow_destructive,
        expected_hash: Some(st.schema.get().snapshot.hash()),
        dry_run: q.dry_run,
    };
    let res = apply_snapshot(&st.pool, snap, &opts, acc.user).await?;
    if res.applied {
        st.reload_schema().await?;
    }
    Ok(res)
}

/// Body of a `PUT /<resource>/:id`: the id of the path is authoritative, the body may omit it.
pub(crate) fn with_id<T: serde::de::DeserializeOwned>(mut body: Value, id: Uuid) -> ApiResult<T> {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("id".to_string(), json!(id));
    }
    serde_json::from_value(body).map_err(|e| ApiError::bad_request(e.to_string()))
}

pub(crate) fn current(st: &AppState) -> Snapshot {
    st.schema.get().snapshot.clone()
}

pub(crate) fn result_json(res: &ApplyResult, data: Value) -> Json<Value> {
    Json(json!({
        "data": data,
        "meta": { "applied": res.applied, "changes": res.plan.summary_lines(), "sql": res.sql }
    }))
}

pub async fn list(State(st): State<AppState>, Acc(acc): Acc) -> ApiResult<Json<Value>> {
    let schema = st.schema.get();
    let list: Vec<&Collection> = schema
        .snapshot
        .collections
        .iter()
        .filter(|c| acc.admin || acc.app || permissions::access(&acc, &c.name, Action::Read).is_ok())
        .collect();
    Ok(Json(json!({ "data": list })))
}

pub async fn get_one(State(st): State<AppState>, Acc(acc): Acc, Path(name): Path<String>) -> ApiResult<Json<Value>> {
    let schema = st.schema.get();
    let c = schema
        .snapshot
        .collection(&name)
        .ok_or_else(|| FerroError::not_found(format!("collection `{name}` not found")))?;
    if !(acc.admin || acc.app || permissions::access(&acc, &name, Action::Read).is_ok()) {
        return Err(FerroError::forbidden("forbidden").into());
    }
    Ok(Json(json!({ "data": c })))
}

pub async fn create(State(st): State<AppState>, Acc(acc): Acc, Query(q): Query<ChangeQuery>, Json(c): Json<Collection>) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    let mut snap = current(&st);
    if snap.collection(&c.name).is_some() {
        return Err(ApiError::bad_request(format!("collection `{}` already exists", c.name)));
    }
    snap.collections.push(c.clone());
    let res = apply(&st, &acc, snap, &q, false).await?;
    Ok(result_json(&res, json!(c)))
}

pub async fn update(
    State(st): State<AppState>,
    Acc(acc): Acc,
    Path(name): Path<String>,
    Query(q): Query<ChangeQuery>,
    Json(c): Json<Collection>,
) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    let mut snap = current(&st);
    let Some(pos) = snap.collections.iter().position(|x| x.name == name) else {
        return Err(FerroError::not_found(format!("collection `{name}` not found")).into());
    };
    let mut c = c;
    if c.name != name && c.renamed_from.is_none() {
        c.renamed_from = Some(name.clone());
    }
    snap.collections[pos] = c.clone();
    let res = apply(&st, &acc, snap, &q, false).await?;
    Ok(result_json(&res, json!(c)))
}

pub async fn remove(State(st): State<AppState>, Acc(acc): Acc, Path(name): Path<String>, Query(q): Query<ChangeQuery>) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    let mut snap = current(&st);
    let before = snap.collections.len();
    snap.collections.retain(|c| c.name != name);
    if snap.collections.len() == before {
        return Err(FerroError::not_found(format!("collection `{name}` not found")).into());
    }
    let res = apply(&st, &acc, snap, &q, true).await?;
    Ok(result_json(&res, Value::Null))
}

pub async fn add_field(
    State(st): State<AppState>,
    Acc(acc): Acc,
    Path(collection): Path<String>,
    Query(q): Query<ChangeQuery>,
    Json(f): Json<Field>,
) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    let mut snap = current(&st);
    let c = snap
        .collection_mut(&collection)
        .ok_or_else(|| FerroError::not_found(format!("collection `{collection}` not found")))?;
    if c.field(&f.name).is_some() {
        return Err(ApiError::bad_request(format!("field `{}` already exists", f.name)));
    }
    c.fields.push(f.clone());
    let res = apply(&st, &acc, snap, &q, false).await?;
    Ok(result_json(&res, json!(f)))
}

pub async fn update_field(
    State(st): State<AppState>,
    Acc(acc): Acc,
    Path((collection, field)): Path<(String, String)>,
    Query(q): Query<ChangeQuery>,
    Json(f): Json<Field>,
) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    let mut snap = current(&st);
    let c = snap
        .collection_mut(&collection)
        .ok_or_else(|| FerroError::not_found(format!("collection `{collection}` not found")))?;
    let Some(pos) = c.fields.iter().position(|x| x.name == field) else {
        return Err(FerroError::not_found(format!("field `{collection}.{field}` not found")).into());
    };
    let mut f = f;
    if f.name != field && f.renamed_from.is_none() {
        f.renamed_from = Some(field.clone());
    }
    c.fields[pos] = f.clone();
    let res = apply(&st, &acc, snap, &q, false).await?;
    Ok(result_json(&res, json!(f)))
}

pub async fn remove_field(
    State(st): State<AppState>,
    Acc(acc): Acc,
    Path((collection, field)): Path<(String, String)>,
    Query(q): Query<ChangeQuery>,
) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    let mut snap = current(&st);
    let c = snap
        .collection_mut(&collection)
        .ok_or_else(|| FerroError::not_found(format!("collection `{collection}` not found")))?;
    let before = c.fields.len();
    c.fields.retain(|f| f.name != field);
    if c.fields.len() == before {
        return Err(FerroError::not_found(format!("field `{collection}.{field}` not found")).into());
    }
    let res = apply(&st, &acc, snap, &q, true).await?;
    Ok(result_json(&res, Value::Null))
}

pub async fn list_roles(State(st): State<AppState>, Acc(acc): Acc) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    Ok(Json(json!({ "data": st.schema.get().snapshot.roles })))
}

pub async fn list_policies(State(st): State<AppState>, Acc(acc): Acc) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    Ok(Json(json!({ "data": st.schema.get().snapshot.policies })))
}

pub async fn put_role(
    State(st): State<AppState>,
    Acc(acc): Acc,
    Path(id): Path<Uuid>,
    Query(q): Query<ChangeQuery>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    let role: Role = with_id(body, id)?;
    let mut snap = current(&st);
    snap.roles.retain(|r| r.id != id);
    snap.roles.push(role.clone());
    let res = apply(&st, &acc, snap, &q, false).await?;
    Ok(result_json(&res, json!(role)))
}

pub async fn delete_role(State(st): State<AppState>, Acc(acc): Acc, Path(id): Path<Uuid>, Query(q): Query<ChangeQuery>) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    if id == ferro_core::ADMIN_ROLE_ID {
        return Err(ApiError::bad_request("the Administrator role cannot be deleted"));
    }
    let mut snap = current(&st);
    snap.roles.retain(|r| r.id != id);
    let res = apply(&st, &acc, snap, &q, false).await?;
    Ok(result_json(&res, Value::Null))
}

pub async fn put_policy(
    State(st): State<AppState>,
    Acc(acc): Acc,
    Path(id): Path<Uuid>,
    Query(q): Query<ChangeQuery>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    let policy: Policy = with_id(body, id)?;
    if id == ADMIN_POLICY_ID && !policy.admin_access {
        return Err(ApiError::bad_request("the Administrator policy must keep admin_access"));
    }
    let mut snap = current(&st);
    snap.policies.retain(|p| p.id != id);
    snap.policies.push(policy.clone());
    let res = apply(&st, &acc, snap, &q, false).await?;
    Ok(result_json(&res, json!(policy)))
}

pub async fn delete_policy(State(st): State<AppState>, Acc(acc): Acc, Path(id): Path<Uuid>, Query(q): Query<ChangeQuery>) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    if id == ADMIN_POLICY_ID {
        return Err(ApiError::bad_request("the Administrator policy cannot be deleted"));
    }
    let mut snap = current(&st);
    snap.policies.retain(|p| p.id != id);
    for r in &mut snap.roles {
        r.policies.retain(|p| *p != id);
    }
    let res = apply(&st, &acc, snap, &q, false).await?;
    Ok(result_json(&res, Value::Null))
}

#[derive(Debug, Deserialize)]
pub struct NewLanguage {
    code: String,
    name: String,
    #[serde(default)]
    direction: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TranslationsBody {
    /// The translatable fields, as for `POST /fields/:collection` (plain column fields).
    fields: Vec<Field>,
    #[serde(default)]
    field_name: Option<String>,
    #[serde(default)]
    translations_collection: Option<String>,
    #[serde(default)]
    languages_collection: Option<String>,
    /// Languages to add to the languages collection (existing codes are left alone).
    #[serde(default)]
    languages: Vec<NewLanguage>,
}

/// `POST /collections/:name/translations` — make a collection translatable in one step: creates the languages collection
/// (unless it exists), the translations collection with the translatable fields and the one-translation-per-language
/// guarantee, and the `translations` field. It is one schema change, so `?dry_run=true` previews the plan and SQL.
pub async fn add_translations(
    State(st): State<AppState>,
    Acc(acc): Acc,
    Path(name): Path<String>,
    Query(q): Query<ChangeQuery>,
    Json(body): Json<TranslationsBody>,
) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    let spec = ferro_core::translations::TranslationsSpec {
        collection: name.clone(),
        field_name: body.field_name.clone(),
        translations_collection: body.translations_collection.clone(),
        languages_collection: body.languages_collection.clone(),
        fields: body.fields.clone(),
    };
    let names = spec.names();
    let snap = ferro_core::translations::scaffold(&current(&st), &spec).map_err(|errs| ApiError::bad_request(errs.join("; ")))?;
    let res = apply(&st, &acc, snap, &q, false).await?;
    let mut out = result_json(&res, json!({ "field": names.field, "translations_collection": names.translations, "languages_collection": names.languages }));

    // seed the languages (after the schema change, so the collection exists)
    if res.applied && !body.languages.is_empty() {
        let ctx = ferro_engine::items::Ctx::new(st.schema.get(), acc.clone());
        let (mut added, mut skipped) = (vec![], vec![]);
        for l in &body.languages {
            let mut tx = st.pool.begin().await?;
            let exists: bool = sqlx::query_scalar(&format!("SELECT EXISTS (SELECT 1 FROM {} WHERE \"code\" = $1)", ferro_engine::ddl::q(&names.languages)))
                .bind(&l.code)
                .fetch_one(&mut *tx)
                .await?;
            if exists {
                skipped.push(json!({ "code": l.code, "reason": "already exists" }));
                continue;
            }
            let mut item = json!({ "code": l.code, "name": l.name });
            if let Some(d) = &l.direction {
                item["direction"] = json!(d);
            }
            match ferro_engine::items::create_one(&ctx, &mut tx, &names.languages, item).await {
                Ok(_) => {
                    tx.commit().await?;
                    added.push(l.code.clone());
                }
                Err(e) => skipped.push(json!({ "code": l.code, "reason": e.to_string() })),
            }
        }
        out.0["meta"]["languages"] = json!({ "added": added, "skipped": skipped });
    }
    Ok(out)
}
