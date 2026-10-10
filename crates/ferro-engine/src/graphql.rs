//! GraphQL executor. Root fields are routed to the same item services used by
//! REST, so permissions, nested writes, activity and revisions behave identically.
//!
//! Queries run on one connection; all root fields of a mutation run in one
//! transaction (any error rolls everything back and `data` is `null`).
//! The response is shaped from the requested selection: aliases, `__typename`
//! and `... on <collection>` branches of M2A (union) fields.

use std::collections::HashMap;

use ferro_core::fields::FieldTree;
use ferro_core::graphql::{collect_fields, parse_document, Document, FieldNode, OpKind};
use ferro_core::{Field, FieldType};
use serde_json::{json, Map, Value};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::items::{self, Ctx, ReadArgs};
use crate::schema::Schema;
use crate::{FerroError, Result};

pub struct Request {
    pub query: String,
    pub variables: Map<String, Value>,
    pub operation_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Options {
    /// Same meaning as `FERRO_QUERY_LIMIT_MAX` (-1 = unlimited).
    pub limit_max: i64,
    /// GET requests must not run mutations.
    pub allow_mutations: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    List,
    ById,
    Count,
    Singleton,
    Create,
    CreateMany,
    Update,
    UpdateMany,
    UpdateSingleton,
    Delete,
    DeleteMany,
}

type Table = HashMap<String, (Op, String)>;

fn inv(e: String) -> FerroError {
    FerroError::invalid(e)
}

fn code(e: &FerroError) -> &'static str {
    match e {
        FerroError::NotFound(_) => "NOT_FOUND",
        FerroError::Forbidden(_) => "FORBIDDEN",
        FerroError::Unauthorized(_) => "UNAUTHORIZED",
        FerroError::AuthCode(code, _) => code,
        FerroError::Invalid(_) => "INVALID_PAYLOAD",
        FerroError::Conflict(_) => "CONFLICT",
        FerroError::Db(_) => match e.sqlstate().as_deref() {
            Some("23505") => "RECORD_NOT_UNIQUE",
            Some("23503") => "INVALID_FOREIGN_KEY",
            Some("23502") => "NOT_NULL_VIOLATION",
            Some(c) if c.starts_with("22") || c == "23514" => "INVALID_VALUE",
            _ => "INTERNAL",
        },
        FerroError::Internal(_) => "INTERNAL",
    }
}

fn error_value(e: &FerroError, key: &str) -> Value {
    json!({ "message": e.to_string(), "path": [key], "extensions": { "code": code(e) } })
}

/// Errors that happen before execution starts: no `data` member.
fn fail(message: impl Into<String>, code: &str) -> Value {
    let message: String = message.into();
    json!({ "errors": [{ "message": message, "extensions": { "code": code } }] })
}

fn response(data: Value, errors: Vec<Value>) -> Value {
    if errors.is_empty() {
        json!({ "data": data })
    } else {
        json!({ "data": data, "errors": errors })
    }
}

fn insert(table: &mut Table, name: String, op: Op, collection: &str) {
    table.entry(name).or_insert((op, collection.to_string()));
}

fn root_table(schema: &Schema, mutation: bool) -> Table {
    let mut t = Table::new();
    for c in schema.all_collections() {
        let n = c.name.as_str();
        let single = c.def.meta.singleton;
        if !mutation {
            if single {
                insert(&mut t, n.to_string(), Op::Singleton, n);
            } else {
                insert(&mut t, n.to_string(), Op::List, n);
                insert(&mut t, format!("{n}_by_id"), Op::ById, n);
                insert(&mut t, format!("{n}_count"), Op::Count, n);
            }
        } else if !c.system {
            if single {
                insert(&mut t, format!("update_{n}"), Op::UpdateSingleton, n);
            } else {
                insert(&mut t, format!("create_{n}_item"), Op::Create, n);
                insert(&mut t, format!("create_{n}_items"), Op::CreateMany, n);
                insert(&mut t, format!("update_{n}_item"), Op::Update, n);
                insert(&mut t, format!("update_{n}_items"), Op::UpdateMany, n);
                insert(&mut t, format!("delete_{n}_item"), Op::Delete, n);
                insert(&mut t, format!("delete_{n}_items"), Op::DeleteMany, n);
            }
        }
    }
    t
}

// ---------------------------------------------------------------------------
// entry point
// ---------------------------------------------------------------------------

pub async fn execute(pool: &PgPool, ctx: &Ctx, req: Request, opts: Options) -> Value {
    let doc = match parse_document(&req.query) {
        Ok(d) => d,
        Err(e) => return fail(e, "GRAPHQL_PARSE_FAILED"),
    };
    let op = match doc.operation(req.operation_name.as_deref()) {
        Ok(o) => o,
        Err(e) => return fail(e, "GRAPHQL_VALIDATION_FAILED"),
    };
    let mut vars = Map::new();
    for vd in &op.variables {
        if let Some(d) = &vd.default {
            match d.resolve(&Map::new()) {
                Ok(v) => {
                    vars.insert(vd.name.clone(), v);
                }
                Err(e) => return fail(e, "GRAPHQL_VALIDATION_FAILED"),
            }
        }
    }
    for (k, v) in req.variables {
        vars.insert(k, v);
    }
    let mut roots: Vec<&FieldNode> = Vec::new();
    if let Err(e) = collect_fields(&op.selection, &doc, &vars, None, &mut roots, 0) {
        return fail(e, "GRAPHQL_VALIDATION_FAILED");
    }
    match op.kind {
        OpKind::Subscription => fail("Subscriptions are not supported", "GRAPHQL_VALIDATION_FAILED"),
        OpKind::Query => run_query(pool, ctx, &doc, &vars, &roots, &opts).await,
        OpKind::Mutation => {
            if !opts.allow_mutations {
                return fail("Mutations are only allowed with POST", "METHOD_NOT_ALLOWED");
            }
            run_mutation(pool, ctx, &doc, &vars, &roots, &opts).await
        }
    }
}

async fn run_query(pool: &PgPool, ctx: &Ctx, doc: &Document, vars: &Map<String, Value>, roots: &[&FieldNode], opts: &Options) -> Value {
    let mut conn = match pool.acquire().await {
        Ok(c) => c,
        Err(e) => return fail(e.to_string(), "INTERNAL"),
    };
    let table = root_table(&ctx.schema, false);
    let mut data = Map::new();
    let mut errors = Vec::new();
    for f in roots {
        let key = f.response_key().to_string();
        match run_root(ctx, &mut conn, &table, f, doc, vars, opts, false).await {
            Ok(v) => {
                data.insert(key, v);
            }
            Err(e) => {
                errors.push(error_value(&e, &key));
                data.insert(key, Value::Null);
            }
        }
    }
    response(Value::Object(data), errors)
}

async fn run_mutation(pool: &PgPool, ctx: &Ctx, doc: &Document, vars: &Map<String, Value>, roots: &[&FieldNode], opts: &Options) -> Value {
    let mut tx = match pool.begin().await {
        Ok(t) => t,
        Err(e) => return fail(e.to_string(), "INTERNAL"),
    };
    let table = root_table(&ctx.schema, true);
    let mut data = Map::new();
    for f in roots {
        let key = f.response_key().to_string();
        match run_root(ctx, &mut tx, &table, f, doc, vars, opts, true).await {
            Ok(v) => {
                data.insert(key, v);
            }
            Err(e) => {
                let _ = tx.rollback().await;
                return json!({ "data": Value::Null, "errors": [error_value(&e, &key)] });
            }
        }
    }
    if let Err(e) = tx.commit().await {
        return fail(e.to_string(), "INTERNAL");
    }
    json!({ "data": Value::Object(data) })
}

// ---------------------------------------------------------------------------
// root fields
// ---------------------------------------------------------------------------

fn data_object(f: &FieldNode, vars: &Map<String, Value>) -> Result<Map<String, Value>> {
    match f.arg("data", vars).map_err(inv)? {
        Some(Value::Object(m)) => Ok(m),
        _ => Err(FerroError::invalid("argument \"data\" must be an object")),
    }
}

fn id_arg(f: &FieldNode, vars: &Map<String, Value>) -> Result<Uuid> {
    let v = f
        .arg("id", vars)
        .map_err(inv)?
        .ok_or_else(|| FerroError::invalid("argument \"id\" is required"))?;
    v.as_str().and_then(|s| Uuid::parse_str(s).ok()).ok_or_else(|| FerroError::invalid("argument \"id\" must be a uuid"))
}

fn ids_arg(f: &FieldNode, vars: &Map<String, Value>) -> Result<Vec<Uuid>> {
    let v = f
        .arg("ids", vars)
        .map_err(inv)?
        .ok_or_else(|| FerroError::invalid("argument \"ids\" is required"))?;
    let list = v.as_array().ok_or_else(|| FerroError::invalid("argument \"ids\" must be a list"))?;
    list.iter()
        .map(|x| x.as_str().and_then(|s| Uuid::parse_str(s).ok()).ok_or_else(|| FerroError::invalid("\"ids\" must contain uuids")))
        .collect()
}

fn list_args(f: &FieldNode, vars: &Map<String, Value>, opts: &Options, tree: FieldTree) -> Result<ReadArgs> {
    let filter = f.arg("filter", vars).map_err(inv)?;
    let sort: Vec<String> = match f.arg("sort", vars).map_err(inv)? {
        Some(Value::String(s)) => s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        _ => Vec::new(),
    };
    let limit = f.arg("limit", vars).map_err(inv)?.and_then(|v| v.as_i64());
    let limit = match limit {
        None => None,
        Some(l) if l < 0 => Some(if opts.limit_max < 0 { -1 } else { opts.limit_max }),
        Some(l) if opts.limit_max >= 0 => Some(l.min(opts.limit_max)),
        Some(l) => Some(l),
    };
    let effective = limit.unwrap_or(items::DEFAULT_LIMIT);
    let page = f.arg("page", vars).map_err(inv)?.and_then(|v| v.as_i64());
    let offset = match f.arg("offset", vars).map_err(inv)?.and_then(|v| v.as_i64()) {
        Some(o) => Some(o),
        None => match page {
            Some(p) if p > 1 && effective > 0 => Some((p - 1) * effective),
            _ => None,
        },
    };
    let search = f.arg("search", vars).map_err(inv)?.and_then(|v| v.as_str().map(String::from));
    Ok(ReadArgs { fields: tree, filter, sort, limit, offset, search, lenient: false })
}

/// `lang: "it-IT,en-US"` on a query root, when given.
fn lang_arg(f: &FieldNode, vars: &Map<String, Value>) -> Result<Option<Vec<String>>> {
    match f.arg("lang", vars).map_err(inv)? {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(crate::translations::parse_languages(&s)?)),
        _ => Ok(None),
    }
}

async fn read_rows(ctx: &Ctx, conn: &mut PgConnection, coll: &str, args: ReadArgs, langs: Option<&[String]>) -> Result<Vec<Value>> {
    match langs {
        Some(l) => crate::translations::read_translated(ctx, conn, coll, args, l).await,
        None => items::read_many(ctx, conn, coll, args).await,
    }
}

async fn read_by_id(ctx: &Ctx, conn: &mut PgConnection, coll: &str, id: Uuid, tree: FieldTree) -> Result<Option<Value>> {
    read_by_id_in(ctx, conn, coll, id, tree, None).await
}

async fn read_by_id_in(ctx: &Ctx, conn: &mut PgConnection, coll: &str, id: Uuid, tree: FieldTree, langs: Option<&[String]>) -> Result<Option<Value>> {
    let args = ReadArgs {
        fields: tree,
        filter: Some(json!({ "id": { "_eq": id.to_string() } })),
        limit: Some(1),
        ..Default::default()
    };
    Ok(read_rows(ctx, conn, coll, args, langs).await?.into_iter().next())
}

/// Read the item after a write; callers without read access still get its id.
async fn read_back(ctx: &Ctx, conn: &mut PgConnection, coll: &str, id: Uuid, tree: FieldTree) -> Result<Value> {
    match read_by_id(ctx, conn, coll, id, tree).await {
        Ok(Some(row)) => Ok(row),
        Ok(None) | Err(FerroError::Forbidden(_)) | Err(FerroError::NotFound(_)) => Ok(json!({ "id": id })),
        Err(e) => Err(e),
    }
}

fn pick(fields: &[&FieldNode], obj: &Map<String, Value>, typename: &str) -> Value {
    let mut out = Map::new();
    for f in fields {
        let key = f.response_key().to_string();
        if f.name == "__typename" {
            out.insert(key, json!(typename));
        } else {
            out.insert(key, obj.get(&f.name).cloned().unwrap_or(Value::Null));
        }
    }
    Value::Object(out)
}

#[allow(clippy::too_many_arguments)]
async fn run_root(
    ctx: &Ctx,
    conn: &mut PgConnection,
    table: &Table,
    f: &FieldNode,
    doc: &Document,
    vars: &Map<String, Value>,
    opts: &Options,
    mutation: bool,
) -> Result<Value> {
    let root_type = if mutation { "Mutation" } else { "Query" };
    if f.name == "__typename" {
        return Ok(json!(root_type));
    }
    if f.name == "__schema" || f.name == "__type" {
        return Err(FerroError::invalid("Introspection is not supported: download the schema (SDL) from GET /graphql/schema"));
    }
    let (op, coll) = table
        .get(&f.name)
        .ok_or_else(|| FerroError::invalid(format!("Cannot query field \"{}\" on type \"{root_type}\"", f.name)))?;
    let (op, coll) = (*op, coll.as_str());

    let mut sub: Vec<&FieldNode> = Vec::new();
    if op != Op::Count {
        collect_fields(&f.selection, doc, vars, Some(coll), &mut sub, 0).map_err(inv)?;
    }
    let needs_selection = !matches!(op, Op::Count | Op::Delete | Op::DeleteMany);
    if needs_selection && f.selection.is_empty() {
        return Err(FerroError::invalid(format!("Field \"{}\" must have a selection of subfields", f.name)));
    }
    // translated fields (and `_language`) exist on query roots read with `lang`
    let langs = if matches!(op, Op::List | Op::ById | Op::Singleton) { lang_arg(f, vars)? } else { None };
    let tree = if needs_selection { build_tree(&ctx.schema, coll, &sub, doc, vars, true, langs.is_some())? } else { FieldTree::default() };
    let schema = &ctx.schema;

    match op {
        Op::List => {
            let args = list_args(f, vars, opts, tree)?;
            let rows = read_rows(ctx, conn, coll, args, langs.as_deref()).await?;
            let out = rows.iter().map(|r| project(schema, coll, r, &sub, doc, vars)).collect::<Result<Vec<_>>>()?;
            Ok(Value::Array(out))
        }
        Op::Count => {
            let filter = f.arg("filter", vars).map_err(inv)?;
            let search = f.arg("search", vars).map_err(inv)?.and_then(|v| v.as_str().map(String::from));
            let n = items::count(ctx, conn, coll, filter.as_ref(), search.as_deref()).await?;
            Ok(json!(n))
        }
        Op::ById => {
            let id = id_arg(f, vars)?;
            match read_by_id_in(ctx, conn, coll, id, tree, langs.as_deref()).await? {
                Some(row) => project(schema, coll, &row, &sub, doc, vars),
                None => Ok(Value::Null),
            }
        }
        Op::Singleton => {
            let args = ReadArgs { fields: tree, limit: Some(1), ..Default::default() };
            match read_rows(ctx, conn, coll, args, langs.as_deref()).await?.into_iter().next() {
                Some(row) => project(schema, coll, &row, &sub, doc, vars),
                None => Ok(Value::Null),
            }
        }
        Op::Create => {
            let data = data_object(f, vars)?;
            let id = items::create_one(ctx, conn, coll, Value::Object(data)).await?;
            let row = read_back(ctx, conn, coll, id, tree).await?;
            project(schema, coll, &row, &sub, doc, vars)
        }
        Op::CreateMany => {
            let list = match f.arg("data", vars).map_err(inv)? {
                Some(Value::Array(a)) => a,
                _ => return Err(FerroError::invalid("argument \"data\" must be a list of objects")),
            };
            let mut out = Vec::new();
            for item in list {
                let id = items::create_one(ctx, conn, coll, item).await?;
                let row = read_back(ctx, conn, coll, id, tree.clone()).await?;
                out.push(project(schema, coll, &row, &sub, doc, vars)?);
            }
            Ok(Value::Array(out))
        }
        Op::Update => {
            let id = id_arg(f, vars)?;
            let data = data_object(f, vars)?;
            items::update_one(ctx, conn, coll, id, Value::Object(data)).await?;
            let row = read_back(ctx, conn, coll, id, tree).await?;
            project(schema, coll, &row, &sub, doc, vars)
        }
        Op::UpdateMany => {
            let ids = ids_arg(f, vars)?;
            let data = data_object(f, vars)?;
            let mut out = Vec::new();
            for id in ids {
                items::update_one(ctx, conn, coll, id, Value::Object(data.clone())).await?;
                let row = read_back(ctx, conn, coll, id, tree.clone()).await?;
                out.push(project(schema, coll, &row, &sub, doc, vars)?);
            }
            Ok(Value::Array(out))
        }
        Op::UpdateSingleton => {
            let data = data_object(f, vars)?;
            let info = schema.collection(coll)?;
            let id = match items::singleton_id(conn, &info).await? {
                Some(id) => {
                    items::update_one(ctx, conn, coll, id, Value::Object(data)).await?;
                    id
                }
                None => items::create_one(ctx, conn, coll, Value::Object(data)).await?,
            };
            let row = read_back(ctx, conn, coll, id, tree).await?;
            project(schema, coll, &row, &sub, doc, vars)
        }
        Op::Delete => {
            let id = id_arg(f, vars)?;
            items::delete_many(ctx, conn, coll, vec![id]).await?;
            let obj = json!({ "id": id }).as_object().cloned().unwrap_or_default();
            Ok(pick(&sub, &obj, "delete_one"))
        }
        Op::DeleteMany => {
            let ids = ids_arg(f, vars)?;
            items::delete_many(ctx, conn, coll, ids.clone()).await?;
            let obj = json!({ "ids": ids }).as_object().cloned().unwrap_or_default();
            Ok(pick(&sub, &obj, "delete_many"))
        }
    }
}

// ---------------------------------------------------------------------------
// selection -> field tree, rows -> response
// ---------------------------------------------------------------------------

fn targets(def: &Field) -> Vec<String> {
    match def.ty {
        FieldType::M2aItem => def.relation.as_ref().map(|r| r.allowed.clone()).unwrap_or_default(),
        _ => def.related_collection().map(String::from).into_iter().collect(),
    }
}

/// What the item engine must load to answer `fields` on `coll`.
fn build_tree(schema: &Schema, coll: &str, fields: &[&FieldNode], doc: &Document, vars: &Map<String, Value>, strict: bool, translated_ok: bool) -> Result<FieldTree> {
    let info = schema.collection(coll)?;
    let mut tree = FieldTree::default();
    for f in fields {
        if f.name == "__typename" {
            continue;
        }
        let Some(def) = info.field(&f.name) else {
            // translated fields exist on the root of a query that passes `lang`; the engine fills them (and `_language`)
            if translated_ok && (f.name == crate::translations::LANGUAGE_KEY || ferro_core::translations::translated_fields(&schema.snapshot, &info.def).iter().any(|t| t.name == f.name)) {
                if f.name != crate::translations::LANGUAGE_KEY {
                    tree.fields.insert(f.name.clone());
                }
                continue;
            }
            if strict {
                return Err(FerroError::invalid(format!("Cannot query field \"{}\" on type \"{coll}\"", f.name)));
            }
            continue;
        };
        if def.ty.is_relational() && !f.selection.is_empty() {
            let union = def.ty == FieldType::M2aItem;
            let mut child = FieldTree::default();
            for t in targets(def) {
                if schema.get(&t).is_none() {
                    continue;
                }
                let mut sub: Vec<&FieldNode> = Vec::new();
                collect_fields(&f.selection, doc, vars, Some(&t), &mut sub, 0).map_err(inv)?;
                child = child.merged(&build_tree(schema, &t, &sub, doc, vars, strict && !union, false)?);
            }
            let merged = match tree.children.get(&f.name) {
                Some(existing) => existing.merged(&child),
                None => child,
            };
            tree.children.insert(f.name.clone(), merged);
        } else {
            tree.fields.insert(f.name.clone());
        }
    }
    Ok(tree)
}

/// Shape one loaded row according to the selection (aliases, `__typename`, unions).
fn project(schema: &Schema, coll: &str, value: &Value, fields: &[&FieldNode], doc: &Document, vars: &Map<String, Value>) -> Result<Value> {
    let Value::Object(obj) = value else { return Ok(Value::Null) };
    let info = schema.collection(coll)?;
    let mut out = Map::new();
    for f in fields {
        let key = f.response_key().to_string();
        if f.name == "__typename" {
            out.insert(key, json!(coll));
            continue;
        }
        let v = obj.get(&f.name).cloned().unwrap_or(Value::Null);
        let shaped = match info.field(&f.name) {
            Some(def) if def.ty.is_relational() && !f.selection.is_empty() => project_relation(schema, def, &v, obj, f, doc, vars)?,
            _ => v,
        };
        out.insert(key, shaped);
    }
    Ok(Value::Object(out))
}

fn project_relation(schema: &Schema, def: &Field, v: &Value, parent: &Map<String, Value>, f: &FieldNode, doc: &Document, vars: &Map<String, Value>) -> Result<Value> {
    match v {
        Value::Array(list) => Ok(Value::Array(
            list.iter().map(|it| project_relation(schema, def, it, parent, f, doc, vars)).collect::<Result<Vec<_>>>()?,
        )),
        Value::Object(_) => {
            let target = if def.ty == FieldType::M2aItem {
                def.relation
                    .as_ref()
                    .and_then(|r| r.collection_field.as_deref())
                    .and_then(|cf| parent.get(cf))
                    .and_then(|x| x.as_str())
                    .map(String::from)
            } else {
                def.related_collection().map(String::from)
            };
            let Some(target) = target else { return Ok(Value::Null) };
            if schema.get(&target).is_none() {
                return Ok(Value::Null);
            }
            let mut sub: Vec<&FieldNode> = Vec::new();
            collect_fields(&f.selection, doc, vars, Some(&target), &mut sub, 0).map_err(inv)?;
            project(schema, &target, v, &sub, doc, vars)
        }
        // not expanded (no permission) or null
        _ => Ok(Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferro_core::Snapshot;

    fn schema() -> Schema {
        let s: Snapshot = serde_yaml::from_str(include_str!("../../ferro-core/tests/fixtures/pages.yaml")).unwrap();
        Schema::new(s)
    }

    const QUERY: &str = r#"
        query {
          pages {
            id
            title
            category { code }
            blocks {
              collection
              item {
                __typename
                ... on block_text { body }
                ... on block_hero { headline }
              }
            }
          }
        }"#;

    fn root_fields<'a>(doc: &'a Document) -> Vec<&'a FieldNode> {
        let op = doc.operation(None).unwrap();
        let mut roots = Vec::new();
        collect_fields(&op.selection, doc, &Map::new(), None, &mut roots, 0).unwrap();
        let mut sub = Vec::new();
        collect_fields(&roots[0].selection, doc, &Map::new(), Some("pages"), &mut sub, 0).unwrap();
        sub
    }

    #[test]
    fn selection_becomes_a_field_tree() {
        let schema = schema();
        let doc = parse_document(QUERY).unwrap();
        let sub = root_fields(&doc);
        let tree = build_tree(&schema, "pages", &sub, &doc, &Map::new(), true, false).unwrap();
        assert!(tree.fields.contains("title") && tree.fields.contains("id"));
        assert!(!tree.fields.contains("blocks"), "relations are loaded through children");
        assert_eq!(tree.children["category"].fields.iter().collect::<Vec<_>>(), vec!["code"]);
        let blocks = &tree.children["blocks"];
        assert!(blocks.fields.contains("collection"));
        let item = &blocks.children["item"];
        assert!(item.fields.contains("body") && item.fields.contains("headline"), "union of the branches");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let schema = schema();
        let doc = parse_document("{ pages { title nope } }").unwrap();
        let sub = root_fields(&doc);
        let err = build_tree(&schema, "pages", &sub, &doc, &Map::new(), true, false).unwrap_err();
        assert!(err.to_string().contains("Cannot query field \"nope\" on type \"pages\""), "{err}");
    }

    #[test]
    fn rows_are_projected_with_aliases_and_union_branches() {
        let schema = schema();
        let doc = parse_document(
            r#"{ pages { name: title cat: category { code }
                  blocks { collection item { __typename ... on block_text { body } ... on block_hero { headline } } } } }"#,
        )
        .unwrap();
        let sub = root_fields(&doc);
        let row = json!({
            "id": "p1", "title": "Home", "extra": "ignored",
            "category": { "id": "c1", "code": "corp", "label": "ignored" },
            "blocks": [
                { "id": "j1", "collection": "block_text", "item": { "id": "b1", "body": "Hello", "headline": null } },
                { "id": "j2", "collection": "block_hero", "item": { "id": "b2", "headline": "Hi", "body": null } },
                { "id": "j3", "collection": "block_text", "item": "b3" }
            ]
        });
        let out = project(&schema, "pages", &row, &sub, &doc, &Map::new()).unwrap();
        assert_eq!(
            out,
            json!({
                "name": "Home",
                "cat": { "code": "corp" },
                "blocks": [
                    { "collection": "block_text", "item": { "__typename": "block_text", "body": "Hello" } },
                    { "collection": "block_hero", "item": { "__typename": "block_hero", "headline": "Hi" } },
                    { "collection": "block_text", "item": null }
                ]
            })
        );
    }

    #[test]
    fn root_table_routes_names() {
        let schema = schema();
        let q = root_table(&schema, false);
        assert_eq!(q["pages"], (Op::List, "pages".to_string()));
        assert_eq!(q["pages_by_id"].0, Op::ById);
        assert_eq!(q["pages_count"].0, Op::Count);
        assert_eq!(q["ferro_files"].0, Op::List, "system collections are queryable");
        let m = root_table(&schema, true);
        assert_eq!(m["create_pages_item"].0, Op::Create);
        assert_eq!(m["delete_pages_items"].0, Op::DeleteMany);
        assert!(!m.contains_key("create_ferro_users_item"), "system collections are read-only");
    }

    #[test]
    fn list_arguments_follow_rest_rules() {
        let doc = parse_document(r#"{ pages(filter: {status: {_eq: "x"}}, sort: "-title, slug", limit: -1, page: 3) { id } }"#).unwrap();
        let op = doc.operation(None).unwrap();
        let mut roots = Vec::new();
        collect_fields(&op.selection, &doc, &Map::new(), None, &mut roots, 0).unwrap();
        let opts = Options { limit_max: 50, allow_mutations: true };
        let args = list_args(roots[0], &Map::new(), &opts, FieldTree::default()).unwrap();
        assert_eq!(args.sort, vec!["-title", "slug"]);
        assert_eq!(args.limit, Some(50), "-1 is clamped to the server maximum");
        assert_eq!(args.offset, Some(100), "page 3 of 50");
        assert_eq!(args.filter, Some(json!({ "status": { "_eq": "x" } })));
    }
}
