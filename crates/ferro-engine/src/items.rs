//! Generic item service: CRUD on any collection with permissions, relational
//! expansion (`fields=blocks.item.*`), nested writes, activity and revisions.
//!
//! Recursive functions return `BoxFuture` so their futures are `Send` and have a
//! finite size.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::Utc;
use ferro_core::bundle::Item;
use ferro_core::fields::FieldTree;
use ferro_core::filter::{and_all, matches};
use ferro_core::system::{FILES, FOLDERS};
use ferro_core::{Action, Field, FieldType, SYSTEM_FIELDS};
use futures::future::BoxFuture;
use serde_json::{json, Map, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::activity;
use crate::ddl::q;
use crate::permissions::{self, Access, Accountability};
use crate::query::{column_param, compile_filter, compile_search, compile_sort, pg_array_literal, Sql};
use crate::schema::{CollectionInfo, Schema};
use crate::{FerroError, Result};

pub const DEFAULT_LIMIT: i64 = 100;

/// Request context: model + caller.
#[derive(Clone)]
pub struct Ctx {
    pub schema: Arc<Schema>,
    pub acc: Accountability,
    /// `api`, `sync`, ... (stored in activity).
    pub origin: &'static str,
}

impl Ctx {
    pub fn new(schema: Arc<Schema>, acc: Accountability) -> Self {
        Ctx { schema, acc, origin: activity::ORIGIN_API }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ReadArgs {
    pub fields: FieldTree,
    pub filter: Option<Value>,
    pub sort: Vec<String>,
    /// `None` = [`DEFAULT_LIMIT`], `-1` = no limit (callers must clamp user input).
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub search: Option<String>,
    /// Ignore requested fields that do not exist (m2a targets have different shapes).
    pub lenient: bool,
}

// ---------------------------------------------------------------------------
// low level helpers
// ---------------------------------------------------------------------------

pub(crate) async fn fetch_json(conn: &mut PgConnection, text: &str, params: Vec<Option<String>>) -> Result<Value> {
    let mut query = sqlx::query_scalar::<sqlx::Postgres, Value>(text);
    for p in params {
        query = query.bind(p);
    }
    Ok(query.fetch_one(&mut *conn).await?)
}

pub(crate) async fn fetch_i64(conn: &mut PgConnection, text: &str, params: Vec<Option<String>>) -> Result<i64> {
    let mut query = sqlx::query_scalar::<sqlx::Postgres, i64>(text);
    for p in params {
        query = query.bind(p);
    }
    Ok(query.fetch_one(&mut *conn).await?)
}

pub(crate) async fn execute(conn: &mut PgConnection, text: &str, params: Vec<Option<String>>) -> Result<u64> {
    let mut query = sqlx::query::<sqlx::Postgres>(text);
    for p in params {
        query = query.bind(p);
    }
    Ok(query.execute(&mut *conn).await?.rows_affected())
}

fn id_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(m) => m.get("id").and_then(|x| x.as_str()).map(String::from),
        _ => None,
    }
}

fn parse_uuid(v: &Value) -> Option<Uuid> {
    id_of(v).and_then(|s| Uuid::parse_str(&s).ok())
}

fn field_in<I: serde::Serialize>(field: &str, values: I) -> Value {
    let mut m = Map::new();
    m.insert(field.to_string(), json!({ "_in": values }));
    Value::Object(m)
}

fn uuid_array(ids: &[Uuid]) -> Option<String> {
    let items: Vec<Option<String>> = ids.iter().map(|i| Some(i.to_string())).collect();
    Some(pg_array_literal(&items))
}

fn now_param() -> Option<String> {
    Some(Utc::now().to_rfc3339())
}

/// A user-supplied filter may only mention fields the user can read, otherwise filtering (or counting)
/// would reveal the content of protected fields one guess at a time. Relations are followed with the
/// permissions of the related collection. The permission's own filter is not checked: it is the rule.
fn check_filter_access(ctx: &Ctx, info: &CollectionInfo, perm: &Access, filter: &Value, depth: usize) -> Result<()> {
    if depth > 8 {
        return Err(FerroError::invalid("filter is nested too deeply"));
    }
    let Some(obj) = filter.as_object() else { return Ok(()) };
    for (key, cond) in obj {
        match key.as_str() {
            "_and" | "_or" => {
                if let Some(list) = cond.as_array() {
                    for f in list {
                        check_filter_access(ctx, info, perm, f, depth + 1)?;
                    }
                }
            }
            name => {
                if !perm.allows(name) {
                    return Err(FerroError::forbidden(format!("you don't have permission to filter `{}` by `{name}`", info.name)));
                }
                let Some(field) = info.field(name) else { continue }; // unknown fields are reported by the compiler
                if field.ty.is_relational() && cond.is_object() && !ferro_core::filter::is_condition(cond) {
                    if let Some(related) = field.related_collection() {
                        let rinfo = ctx.schema.collection(related)?;
                        let rperm = permissions::access(&ctx.acc, related, Action::Read)?;
                        let inner = match cond.as_object() {
                            Some(m) if m.len() == 1 && (m.contains_key("_some") || m.contains_key("_none")) => m.values().next().unwrap_or(cond),
                            _ => cond,
                        };
                        check_filter_access(ctx, &rinfo, &rperm, inner, depth + 1)?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn check_sort_access(info: &CollectionInfo, perm: &Access, sort: &[String]) -> Result<()> {
    for s in sort {
        let name = s.trim().trim_start_matches('-');
        if !name.is_empty() && !perm.allows(name) {
            return Err(FerroError::forbidden(format!("you don't have permission to sort `{}` by `{name}`", info.name)));
        }
    }
    Ok(())
}

fn build_where(
    schema: &Schema,
    info: &CollectionInfo,
    perm: &Access,
    filter: Option<&Value>,
    search: Option<&str>,
    sql: &mut Sql,
) -> Result<String> {
    let mut parts = vec![];
    if let Some(f) = and_all(vec![filter.cloned(), perm.filter.clone()]) {
        parts.push(compile_filter(schema, info, "t0", &f, sql)?);
    }
    if let Some(term) = search.filter(|s| !s.trim().is_empty()) {
        parts.push(compile_search(info, "t0", term.trim(), sql, &|name| perm.allows(name)));
    }
    Ok(if parts.is_empty() { "TRUE".into() } else { parts.join(" AND ") })
}

// ---------------------------------------------------------------------------
// read
// ---------------------------------------------------------------------------

pub fn read_many<'a>(
    ctx: &'a Ctx,
    conn: &'a mut PgConnection,
    collection: &'a str,
    args: ReadArgs,
) -> BoxFuture<'a, Result<Vec<Value>>> {
    Box::pin(async move {
        let info = ctx.schema.collection(collection)?;
        let perm = permissions::access(&ctx.acc, collection, Action::Read)?;
        if let Some(f) = &args.filter {
            check_filter_access(ctx, &info, &perm, f, 0)?;
        }
        check_sort_access(&info, &perm, &args.sort)?;
        let tree = &args.fields;
        if !args.lenient {
            for name in tree.mentioned() {
                if info.field(&name).is_none() {
                    return Err(FerroError::invalid(format!("field `{name}` does not exist in `{collection}`")));
                }
            }
        }

        let mut select: Vec<String> = vec!["id".to_string()];
        let mut expand: Vec<(Field, FieldTree, bool)> = vec![];
        for f in &info.fields {
            if f.name == "id" || !perm.allows(&f.name) {
                continue;
            }
            let child = if f.ty.is_relational() { tree.child(&f.name) } else { None };
            let plain = tree.wants(&f.name);
            if !plain && child.is_none() {
                continue;
            }
            if f.has_column() {
                select.push(f.name.clone());
            }
            match child {
                Some(c) => expand.push((f.clone(), c, false)),
                // `*` on an alias field returns the list of related ids
                None if f.ty.is_alias() => expand.push((f.clone(), FieldTree::only(&["id"]), true)),
                None => {}
            }
        }
        for (f, _, _) in &expand {
            if f.ty == FieldType::M2aItem {
                if let Some(cf) = f.relation.as_ref().and_then(|r| r.collection_field.clone()) {
                    if !select.contains(&cf) {
                        select.push(cf);
                    }
                }
            }
        }

        let mut sql = Sql::new();
        let where_sql = build_where(&ctx.schema, &info, &perm, args.filter.as_ref(), args.search.as_deref(), &mut sql)?;
        let order = compile_sort(&info, "t0", &args.sort)?;
        let mut tail = String::new();
        match args.limit {
            None => tail.push_str(&format!(" LIMIT {DEFAULT_LIMIT}")),
            Some(l) if l >= 0 => tail.push_str(&format!(" LIMIT {l}")),
            Some(_) => {}
        }
        if let Some(o) = args.offset.filter(|o| *o > 0) {
            tail.push_str(&format!(" OFFSET {o}"));
        }
        let cols = select.iter().map(|c| format!("t0.{}", q(c))).collect::<Vec<_>>().join(", ");
        let text = format!(
            "SELECT COALESCE(jsonb_agg(to_jsonb(s) - '__n' ORDER BY s.__n), '[]'::jsonb) FROM \
             (SELECT {cols}, ROW_NUMBER() OVER (ORDER BY {order}) AS __n FROM {} AS t0 WHERE {where_sql} ORDER BY {order}{tail}) s",
            q(&info.name)
        );
        let mut rows = match fetch_json(&mut *conn, &text, sql.params).await? {
            Value::Array(a) => a,
            _ => vec![],
        };
        for (f, child, ids_only) in expand {
            expand_field(ctx, &mut *conn, &f, child, ids_only, &mut rows).await?;
        }
        Ok(rows)
    })
}

fn index_by_id(items: Vec<Value>) -> BTreeMap<String, Value> {
    items.into_iter().filter_map(|v| id_of(&v).map(|id| (id, v))).collect()
}

async fn expand_field(
    ctx: &Ctx,
    conn: &mut PgConnection,
    f: &Field,
    child: FieldTree,
    ids_only: bool,
    rows: &mut [Value],
) -> Result<()> {
    match f.ty {
        FieldType::M2o | FieldType::File => {
            let Some(related) = f.related_collection().map(String::from) else { return Ok(()) };
            if permissions::access(&ctx.acc, &related, Action::Read).is_err() {
                return Ok(());
            }
            let ids: BTreeSet<String> = rows
                .iter()
                .filter_map(|r| r.get(&f.name).and_then(|v| v.as_str()).map(String::from))
                .collect();
            if ids.is_empty() {
                return Ok(());
            }
            let args = ReadArgs { fields: child, filter: Some(field_in("id", &ids)), limit: Some(-1), ..Default::default() };
            let by_id = index_by_id(read_many(ctx, &mut *conn, &related, args).await?);
            for r in rows.iter_mut() {
                let key = r.get(&f.name).and_then(|v| v.as_str()).map(String::from);
                if let (Some(k), Some(obj)) = (key, r.as_object_mut()) {
                    obj.insert(f.name.clone(), by_id.get(&k).cloned().unwrap_or(Value::Null));
                }
            }
        }
        FieldType::O2m | FieldType::M2m | FieldType::M2a => {
            let (Some(related), Some(ff)) = (f.related_collection(), f.foreign_field()) else { return Ok(()) };
            let (related, ff) = (related.to_string(), ff.to_string());
            if permissions::access(&ctx.acc, &related, Action::Read).is_err() {
                return Ok(());
            }
            let parent_ids: Vec<String> = rows
                .iter()
                .filter_map(|r| r.get("id").and_then(|v| v.as_str()).map(String::from))
                .collect();
            if parent_ids.is_empty() {
                return Ok(());
            }
            let sort = f.relation.as_ref().and_then(|r| r.sort_field.clone()).map(|s| vec![s]).unwrap_or_default();
            let args = ReadArgs {
                fields: child.with_field(&ff),
                filter: Some(field_in(&ff, &parent_ids)),
                sort,
                limit: Some(-1),
                ..Default::default()
            };
            let children = read_many(ctx, &mut *conn, &related, args).await?;
            let mut groups: BTreeMap<String, Vec<Value>> = BTreeMap::new();
            for c in children {
                if let Some(pid) = c.get(&ff).and_then(id_of) {
                    groups.entry(pid).or_default().push(c);
                }
            }
            for r in rows.iter_mut() {
                let pid = r.get("id").and_then(|v| v.as_str()).map(String::from);
                if let (Some(pid), Some(obj)) = (pid, r.as_object_mut()) {
                    let list = groups.remove(&pid).unwrap_or_default();
                    let value = if ids_only {
                        Value::Array(list.iter().filter_map(id_of).map(Value::String).collect())
                    } else {
                        Value::Array(list)
                    };
                    obj.insert(f.name.clone(), value);
                }
            }
        }
        FieldType::M2aItem => {
            let Some(rel) = f.relation.as_ref() else { return Ok(()) };
            let Some(cf) = rel.collection_field.clone() else { return Ok(()) };
            let mut by_coll: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for r in rows.iter() {
                let c = r.get(&cf).and_then(|v| v.as_str());
                let id = r.get(&f.name).and_then(|v| v.as_str());
                if let (Some(c), Some(id)) = (c, id) {
                    if rel.allowed.is_empty() || rel.allowed.iter().any(|a| a == c) {
                        by_coll.entry(c.to_string()).or_default().insert(id.to_string());
                    }
                }
            }
            let mut found: BTreeMap<(String, String), Value> = BTreeMap::new();
            for (coll, ids) in by_coll {
                if ctx.schema.get(&coll).is_none() || permissions::access(&ctx.acc, &coll, Action::Read).is_err() {
                    continue;
                }
                let args = ReadArgs {
                    fields: child.clone(),
                    filter: Some(field_in("id", &ids)),
                    limit: Some(-1),
                    lenient: true,
                    ..Default::default()
                };
                for item in read_many(ctx, &mut *conn, &coll, args).await? {
                    if let Some(id) = id_of(&item) {
                        found.insert((coll.clone(), id), item);
                    }
                }
            }
            for r in rows.iter_mut() {
                let key = match (r.get(&cf).and_then(|v| v.as_str()), r.get(&f.name).and_then(|v| v.as_str())) {
                    (Some(c), Some(id)) => Some((c.to_string(), id.to_string())),
                    _ => None,
                };
                if let (Some(k), Some(obj)) = (key, r.as_object_mut()) {
                    if let Some(v) = found.get(&k) {
                        obj.insert(f.name.clone(), v.clone());
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

pub async fn read_one(ctx: &Ctx, conn: &mut PgConnection, collection: &str, id: &str, fields: FieldTree) -> Result<Value> {
    Uuid::parse_str(id).map_err(|_| FerroError::invalid(format!("`{id}` is not a valid id")))?;
    let args = ReadArgs { fields, filter: Some(json!({ "id": { "_eq": id } })), limit: Some(1), ..Default::default() };
    let mut rows = read_many(ctx, conn, collection, args).await?;
    if rows.is_empty() {
        Err(FerroError::not_found(format!("item `{id}` not found in `{collection}`")))
    } else {
        Ok(rows.remove(0))
    }
}

pub async fn count(
    ctx: &Ctx,
    conn: &mut PgConnection,
    collection: &str,
    filter: Option<&Value>,
    search: Option<&str>,
) -> Result<i64> {
    let info = ctx.schema.collection(collection)?;
    let perm = permissions::access(&ctx.acc, collection, Action::Read)?;
    if let Some(f) = filter {
        check_filter_access(ctx, &info, &perm, f, 0)?;
    }
    let mut sql = Sql::new();
    let where_sql = build_where(&ctx.schema, &info, &perm, filter, search, &mut sql)?;
    let text = format!("SELECT count(*) FROM {} AS t0 WHERE {where_sql}", q(&info.name));
    fetch_i64(conn, &text, sql.params).await
}

// ---------------------------------------------------------------------------
// write
// ---------------------------------------------------------------------------

fn check_writable(info: &CollectionInfo) -> Result<()> {
    if info.system && info.name != FILES && info.name != FOLDERS {
        return Err(FerroError::forbidden(format!(
            "system collection `{}` cannot be modified through the items API",
            info.name
        )));
    }
    Ok(())
}

fn is_managed(info: &CollectionInfo, name: &str) -> bool {
    info.def.system_fields && SYSTEM_FIELDS.contains(&name)
}

fn check_fields(perm: &Access, info: &CollectionInfo, obj: &Map<String, Value>) -> Result<()> {
    for k in obj.keys() {
        if k == "id" || is_managed(info, k) {
            continue;
        }
        info.require_field(k)?;
        if !perm.allows(k) {
            return Err(FerroError::forbidden(format!("you don't have permission to write field `{k}` of `{}`", info.name)));
        }
    }
    Ok(())
}

fn validate_payload(perm: &Access, info: &CollectionInfo, obj: &Map<String, Value>, is_create: bool) -> Result<()> {
    if let Some(v) = &perm.validation {
        if !matches(v, &Value::Object(obj.clone())) {
            return Err(FerroError::invalid("payload does not satisfy the permission validation rules"));
        }
    }
    for f in &info.fields {
        let Some(rule) = &f.validation else { continue };
        if !is_create && !obj.contains_key(&f.name) {
            continue;
        }
        let value = obj.get(&f.name).cloned().unwrap_or(Value::Null);
        let mut filter = Map::new();
        filter.insert(f.name.clone(), rule.clone());
        let mut item = Map::new();
        item.insert(f.name.clone(), value);
        if !matches(&Value::Object(filter), &Value::Object(item)) {
            return Err(FerroError::invalid(format!("field `{}` failed validation", f.name)));
        }
    }
    Ok(())
}

struct Prepared {
    columns: Vec<(Field, Option<String>)>,
    nested: Vec<(Field, Value)>,
}

async fn prepare(ctx: &Ctx, conn: &mut PgConnection, info: &CollectionInfo, obj: &Map<String, Value>) -> Result<Prepared> {
    let mut columns = vec![];
    let mut nested = vec![];
    for (k, v) in obj {
        if k == "id" || is_managed(info, k) {
            continue;
        }
        let f = info.require_field(k)?.clone();
        if f.ty.is_alias() {
            nested.push((f, v.clone()));
            continue;
        }
        if v.is_object() && f.ty.is_m2o_like() {
            let related = f
                .related_collection()
                .ok_or_else(|| FerroError::Internal(format!("{}.{}: missing relation", info.name, f.name)))?
                .to_string();
            let id = upsert_nested(ctx, &mut *conn, &related, v.clone()).await?;
            columns.push((f, Some(id.to_string())));
            continue;
        }
        if v.is_object() && f.ty == FieldType::M2aItem {
            // { collection: "block_hero", item: { headline: "..." } }
            let cf = f.relation.as_ref().and_then(|r| r.collection_field.clone()).unwrap_or_default();
            let target = obj
                .get(&cf)
                .and_then(|c| c.as_str())
                .ok_or_else(|| FerroError::invalid(format!("`{cf}` is required to write a nested `{}`", f.name)))?
                .to_string();
            let allowed = f.relation.as_ref().map(|r| r.allowed.clone()).unwrap_or_default();
            if !allowed.is_empty() && !allowed.contains(&target) {
                return Err(FerroError::invalid(format!("collection `{target}` not allowed in `{}`", f.name)));
            }
            let id = upsert_nested(ctx, &mut *conn, &target, v.clone()).await?;
            columns.push((f, Some(id.to_string())));
            continue;
        }
        let p = column_param(&f, v);
        columns.push((f, p));
    }
    Ok(Prepared { columns, nested })
}

pub async fn exists(conn: &mut PgConnection, collection: &str, id: Uuid) -> Result<bool> {
    let text = format!("SELECT EXISTS (SELECT 1 FROM {} WHERE \"id\" = $1)", q(collection));
    Ok(sqlx::query_scalar::<sqlx::Postgres, bool>(&text).bind(id).fetch_one(&mut *conn).await?)
}

async fn upsert_nested(ctx: &Ctx, conn: &mut PgConnection, collection: &str, value: Value) -> Result<Uuid> {
    if let Some(id) = value.get("id").and_then(parse_uuid) {
        if exists(&mut *conn, collection, id).await? {
            let mut obj = value.as_object().cloned().unwrap_or_default();
            obj.remove("id");
            if !obj.is_empty() {
                update_one(ctx, &mut *conn, collection, id, Value::Object(obj)).await?;
            }
            return Ok(id);
        }
    }
    create_one(ctx, conn, collection, value).await
}

/// Every id must exist and satisfy the caller's permission filter for the action.
pub async fn ensure_permitted(ctx: &Ctx, conn: &mut PgConnection, info: &CollectionInfo, perm: &Access, ids: &[Uuid]) -> Result<()> {
    let unique: BTreeSet<Uuid> = ids.iter().copied().collect();
    let mut sql = Sql::new();
    let p = sql.bind(uuid_array(ids));
    let mut where_sql = format!("t0.\"id\" = ANY(CAST({p} AS uuid[]))");
    if let Some(f) = &perm.filter {
        let compiled = compile_filter(&ctx.schema, info, "t0", f, &mut sql)?;
        where_sql = format!("{where_sql} AND ({compiled})");
    }
    let text = format!("SELECT count(*) FROM {} AS t0 WHERE {where_sql}", q(&info.name));
    let n = fetch_i64(conn, &text, sql.params).await?;
    if (n as usize) < unique.len() {
        return Err(if perm.filter.is_some() {
            FerroError::forbidden(format!("you don't have permission to modify these items of `{}`", info.name))
        } else {
            FerroError::not_found(format!("item not found in `{}`", info.name))
        });
    }
    Ok(())
}

async fn log_change(ctx: &Ctx, conn: &mut PgConnection, info: &CollectionInfo, action: &str, id: Uuid, delta: Value) -> Result<()> {
    let data = if info.def.meta.revisions && !info.system {
        raw_get(&mut *conn, info, id).await?.map(Value::Object)
    } else {
        None
    };
    let delta = if info.system { None } else { Some(delta) };
    activity::record(conn, &ctx.acc, action, &info.name, &id.to_string(), ctx.origin, data.as_ref(), delta.as_ref()).await?;
    Ok(())
}

pub fn create_one<'a>(ctx: &'a Ctx, conn: &'a mut PgConnection, collection: &'a str, payload: Value) -> BoxFuture<'a, Result<Uuid>> {
    Box::pin(async move {
        let info = ctx.schema.collection(collection)?;
        check_writable(&info)?;
        let perm = permissions::access(&ctx.acc, collection, Action::Create)?;
        let Value::Object(mut obj) = payload else {
            return Err(FerroError::invalid("payload must be an object"));
        };
        for (k, v) in &perm.presets {
            if !obj.contains_key(k) {
                obj.insert(k.clone(), v.clone());
            }
        }
        check_fields(&perm, &info, &obj)?;
        validate_payload(&perm, &info, &obj, true)?;
        // blocking flows may reject the write or rewrite the payload; the result is validated again
        let obj = if info.system {
            obj
        } else {
            let new = crate::flows::before_write(ctx, &mut *conn, collection, "create", &[], Some(obj)).await?.unwrap_or_default();
            check_fields(&perm, &info, &new)?;
            validate_payload(&perm, &info, &new, true)?;
            new
        };
        let id = match obj.get("id") {
            None | Some(Value::Null) => Uuid::now_v7(),
            Some(v) => parse_uuid(v).ok_or_else(|| FerroError::invalid("`id` must be a uuid"))?,
        };
        let prepared = prepare(ctx, &mut *conn, &info, &obj).await?;

        let mut sql = Sql::new();
        let mut cols = vec![q("id")];
        let mut vals = vec![sql.bind_cast(Some(id.to_string()), "uuid")];
        for (f, v) in &prepared.columns {
            cols.push(q(&f.name));
            vals.push(sql.bind_cast(v.clone(), &f.cast_type()));
        }
        if info.def.system_fields {
            cols.push(q("date_created"));
            vals.push(sql.bind_cast(now_param(), "timestamptz"));
            cols.push(q("user_created"));
            vals.push(sql.bind_cast(ctx.acc.user.map(|u| u.to_string()), "uuid"));
        }
        let text = format!("INSERT INTO {} ({}) VALUES ({})", q(&info.name), cols.join(", "), vals.join(", "));
        execute(&mut *conn, &text, sql.params).await?;

        for (f, v) in prepared.nested {
            write_children(ctx, &mut *conn, &f, id, v, true).await?;
        }
        if !info.system {
            crate::flows::after_write(ctx, &mut *conn, collection, "create", &[id], Some(&obj)).await?;
            crate::realtime::notify(&mut *conn, collection, "create", &[id]).await?;
        }
        log_change(ctx, &mut *conn, &info, "create", id, Value::Object(obj)).await?;
        Ok(id)
    })
}

pub fn update_one<'a>(
    ctx: &'a Ctx,
    conn: &'a mut PgConnection,
    collection: &'a str,
    id: Uuid,
    payload: Value,
) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let info = ctx.schema.collection(collection)?;
        check_writable(&info)?;
        let perm = permissions::access(&ctx.acc, collection, Action::Update)?;
        let Value::Object(mut obj) = payload else {
            return Err(FerroError::invalid("payload must be an object"));
        };
        obj.remove("id");
        check_fields(&perm, &info, &obj)?;
        validate_payload(&perm, &info, &obj, false)?;
        ensure_permitted(ctx, &mut *conn, &info, &perm, &[id]).await?;
        let obj = if info.system {
            obj
        } else {
            let mut new = crate::flows::before_write(ctx, &mut *conn, collection, "update", &[id], Some(obj)).await?.unwrap_or_default();
            new.remove("id");
            check_fields(&perm, &info, &new)?;
            validate_payload(&perm, &info, &new, false)?;
            new
        };
        let prepared = prepare(ctx, &mut *conn, &info, &obj).await?;

        let mut sql = Sql::new();
        let idp = sql.bind_cast(Some(id.to_string()), "uuid");
        let mut sets = vec![];
        for (f, v) in &prepared.columns {
            sets.push(format!("{} = {}", q(&f.name), sql.bind_cast(v.clone(), &f.cast_type())));
        }
        if info.def.system_fields {
            sets.push(format!("\"date_updated\" = {}", sql.bind_cast(now_param(), "timestamptz")));
            sets.push(format!("\"user_updated\" = {}", sql.bind_cast(ctx.acc.user.map(|u| u.to_string()), "uuid")));
        }
        if !sets.is_empty() {
            let text = format!("UPDATE {} SET {} WHERE \"id\" = {idp}", q(&info.name), sets.join(", "));
            execute(&mut *conn, &text, sql.params).await?;
        }
        for (f, v) in prepared.nested {
            write_children(ctx, &mut *conn, &f, id, v, false).await?;
        }
        if !info.system {
            crate::flows::after_write(ctx, &mut *conn, collection, "update", &[id], Some(&obj)).await?;
            crate::realtime::notify(&mut *conn, collection, "update", &[id]).await?;
        }
        log_change(ctx, &mut *conn, &info, "update", id, Value::Object(obj)).await?;
        Ok(())
    })
}

pub async fn delete_many(ctx: &Ctx, conn: &mut PgConnection, collection: &str, ids: Vec<Uuid>) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let info = ctx.schema.collection(collection)?;
    check_writable(&info)?;
    let perm = permissions::access(&ctx.acc, collection, Action::Delete)?;
    ensure_permitted(ctx, &mut *conn, &info, &perm, &ids).await?;
    if !info.system {
        crate::flows::before_write(ctx, &mut *conn, collection, "delete", &ids, None).await?;
    }
    // keep the last content of deleted items so they can be restored
    let mut last: BTreeMap<Uuid, Value> = BTreeMap::new();
    if info.def.meta.revisions && !info.system {
        for row in raw_by_ids(&mut *conn, &info, &ids).await? {
            if let Some(id) = row.get("id").and_then(parse_uuid) {
                last.insert(id, Value::Object(row));
            }
        }
    }
    let mut sql = Sql::new();
    let p = sql.bind(uuid_array(&ids));
    let text = format!("DELETE FROM {} WHERE \"id\" = ANY(CAST({p} AS uuid[]))", q(&info.name));
    let n = execute(&mut *conn, &text, sql.params).await?;
    for id in &ids {
        let data = last.get(id);
        activity::record(&mut *conn, &ctx.acc, "delete", collection, &id.to_string(), ctx.origin, data, None).await?;
    }
    if !info.system {
        crate::flows::after_write(ctx, &mut *conn, collection, "delete", &ids, None).await?;
        crate::realtime::notify(&mut *conn, collection, "delete", &ids).await?;
    }
    Ok(n)
}

/// Nested o2m/m2m/m2a write. Accepts:
/// * an array (full replacement: listed items are upserted, the others detached/deleted)
/// * `{ "create": [...], "update": [...], "delete": [ids] }`
async fn write_children(ctx: &Ctx, conn: &mut PgConnection, f: &Field, parent: Uuid, value: Value, is_create: bool) -> Result<()> {
    let related = f
        .related_collection()
        .ok_or_else(|| FerroError::Internal(format!("{}: missing relation", f.name)))?
        .to_string();
    let ff = f
        .foreign_field()
        .ok_or_else(|| FerroError::Internal(format!("{}: missing foreign_field", f.name)))?
        .to_string();
    let related_info = ctx.schema.collection(&related)?;
    let ff_required = related_info.field(&ff).map(|x| x.required).unwrap_or(false);
    // junction rows and children that cannot exist without the parent are deleted, others detached
    let delete_detached = ff_required || matches!(f.ty, FieldType::M2m | FieldType::M2a);

    let with_parent = |v: Value| -> Result<Value> {
        let mut obj = match v {
            Value::Object(o) => o,
            Value::String(id) => {
                let mut m = Map::new();
                m.insert("id".into(), Value::String(id));
                m
            }
            _ => return Err(FerroError::invalid(format!("invalid item in `{}`", f.name))),
        };
        obj.insert(ff.clone(), Value::String(parent.to_string()));
        Ok(Value::Object(obj))
    };

    match value {
        Value::Null => {
            if !is_create {
                detach_except(ctx, conn, &related, &ff, parent, &[], delete_detached).await?;
            }
        }
        Value::Array(list) => {
            let mut keep = vec![];
            for v in list {
                let child = with_parent(v)?;
                keep.push(upsert_nested(ctx, &mut *conn, &related, child).await?);
            }
            if !is_create {
                detach_except(ctx, conn, &related, &ff, parent, &keep, delete_detached).await?;
            }
        }
        Value::Object(ops) => {
            if let Some(Value::Array(list)) = ops.get("create") {
                for v in list {
                    create_one(ctx, &mut *conn, &related, with_parent(v.clone())?).await?;
                }
            }
            if let Some(Value::Array(list)) = ops.get("update") {
                for v in list {
                    let id = v
                        .get("id")
                        .and_then(parse_uuid)
                        .ok_or_else(|| FerroError::invalid(format!("`{}.update` entries need an id", f.name)))?;
                    let mut child = with_parent(v.clone())?;
                    if let Some(o) = child.as_object_mut() {
                        o.remove("id");
                    }
                    update_one(ctx, &mut *conn, &related, id, child).await?;
                }
            }
            if let Some(Value::Array(list)) = ops.get("delete") {
                let ids: Vec<Uuid> = list.iter().filter_map(parse_uuid).collect();
                if delete_detached {
                    delete_many(ctx, &mut *conn, &related, ids).await?;
                } else {
                    for id in ids {
                        let mut m = Map::new();
                        m.insert(ff.clone(), Value::Null);
                        update_one(ctx, &mut *conn, &related, id, Value::Object(m)).await?;
                    }
                }
            }
        }
        _ => return Err(FerroError::invalid(format!("invalid value for `{}`", f.name))),
    }
    Ok(())
}

async fn detach_except(
    ctx: &Ctx,
    conn: &mut PgConnection,
    related: &str,
    ff: &str,
    parent: Uuid,
    keep: &[Uuid],
    delete: bool,
) -> Result<()> {
    let text = format!(
        "SELECT COALESCE(jsonb_agg(\"id\"), '[]'::jsonb) FROM {} WHERE {} = CAST($1 AS uuid)",
        q(related),
        q(ff)
    );
    let ids = fetch_json(&mut *conn, &text, vec![Some(parent.to_string())]).await?;
    let stale: Vec<Uuid> = ids
        .as_array()
        .map(|a| a.iter().filter_map(parse_uuid).filter(|id| !keep.contains(id)).collect())
        .unwrap_or_default();
    if stale.is_empty() {
        return Ok(());
    }
    if delete {
        delete_many(ctx, conn, related, stale).await?;
    } else {
        for id in stale {
            let mut m = Map::new();
            m.insert(ff.to_string(), Value::Null);
            update_one(ctx, &mut *conn, related, id, Value::Object(m)).await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// singleton helpers
// ---------------------------------------------------------------------------

pub async fn singleton_id(conn: &mut PgConnection, info: &CollectionInfo) -> Result<Option<Uuid>> {
    let text = format!("SELECT \"id\" FROM {} ORDER BY \"id\" LIMIT 1", q(&info.name));
    Ok(sqlx::query_scalar::<sqlx::Postgres, Uuid>(&text).fetch_optional(&mut *conn).await?)
}

// ---------------------------------------------------------------------------
// raw access (no permissions, no relations) – used by sync and internals
// ---------------------------------------------------------------------------

fn raw_columns(info: &CollectionInfo) -> String {
    info.columns().map(|f| q(&f.name)).collect::<Vec<_>>().join(", ")
}

pub async fn raw_select(conn: &mut PgConnection, info: &CollectionInfo, where_sql: &str, params: Vec<Option<String>>) -> Result<Vec<Item>> {
    let text = format!(
        "SELECT COALESCE(jsonb_agg(to_jsonb(s) ORDER BY s.\"id\"), '[]'::jsonb) FROM (SELECT {} FROM {} AS t0 WHERE {where_sql}) s",
        raw_columns(info),
        q(&info.name)
    );
    let v = fetch_json(conn, &text, params).await?;
    Ok(match v {
        Value::Array(a) => a
            .into_iter()
            .filter_map(|x| match x {
                Value::Object(m) => Some(m),
                _ => None,
            })
            .collect(),
        _ => vec![],
    })
}

pub async fn raw_by_ids(conn: &mut PgConnection, info: &CollectionInfo, ids: &[Uuid]) -> Result<Vec<Item>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    raw_select(conn, info, "t0.\"id\" = ANY(CAST($1 AS uuid[]))", vec![uuid_array(ids)]).await
}

pub async fn raw_get(conn: &mut PgConnection, info: &CollectionInfo, id: Uuid) -> Result<Option<Item>> {
    Ok(raw_by_ids(conn, info, &[id]).await?.into_iter().next())
}

/// Ids of `info` whose column `field` references one of `parents`.
pub async fn raw_ids_where_in(conn: &mut PgConnection, info: &CollectionInfo, field: &str, parents: &[Uuid]) -> Result<Vec<Uuid>> {
    if parents.is_empty() {
        return Ok(vec![]);
    }
    let text = format!(
        "SELECT COALESCE(jsonb_agg(\"id\"), '[]'::jsonb) FROM {} WHERE {} = ANY(CAST($1 AS uuid[]))",
        q(&info.name),
        q(field)
    );
    let v = fetch_json(conn, &text, vec![uuid_array(parents)]).await?;
    Ok(v.as_array().map(|a| a.iter().filter_map(parse_uuid).collect()).unwrap_or_default())
}

/// INSERT .. ON CONFLICT (id) DO UPDATE for the keys present in `item`.
pub async fn raw_upsert(conn: &mut PgConnection, info: &CollectionInfo, item: &Item) -> Result<()> {
    let id = item
        .get("id")
        .and_then(parse_uuid)
        .ok_or_else(|| FerroError::invalid(format!("item of `{}` without a valid id", info.name)))?;
    let mut sql = Sql::new();
    let mut cols = vec![q("id")];
    let mut vals = vec![sql.bind_cast(Some(id.to_string()), "uuid")];
    let mut updates = vec![];
    for f in info.columns() {
        if f.name == "id" {
            continue;
        }
        let Some(v) = item.get(&f.name) else { continue };
        cols.push(q(&f.name));
        vals.push(sql.bind_cast(column_param(f, v), &f.cast_type()));
        updates.push(format!("{c} = EXCLUDED.{c}", c = q(&f.name)));
    }
    let conflict = if updates.is_empty() {
        "DO NOTHING".to_string()
    } else {
        format!("DO UPDATE SET {}", updates.join(", "))
    };
    let text = format!(
        "INSERT INTO {} ({}) VALUES ({}) ON CONFLICT (\"id\") {conflict}",
        q(&info.name),
        cols.join(", "),
        vals.join(", ")
    );
    execute(conn, &text, sql.params).await?;
    Ok(())
}

pub async fn raw_delete(conn: &mut PgConnection, info: &CollectionInfo, ids: &[Uuid]) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let text = format!("DELETE FROM {} WHERE \"id\" = ANY(CAST($1 AS uuid[]))", q(&info.name));
    execute(conn, &text, vec![uuid_array(ids)]).await
}
