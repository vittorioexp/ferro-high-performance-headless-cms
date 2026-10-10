//! Data promotion between environments.
//!
//! **Export** computes the dependency closure of a selection:
//! * m2o/file targets are always included (shallow) unless `sync: reference|skip`;
//! * children (o2m/m2m/m2a) are included for *deep* items: roots, children and
//!   m2a targets (composition), or m2o targets marked `sync: deep`;
//! * files bring their binary and their folder chain;
//! * users are never exported (system user fields are dropped, other user
//!   references are nulled on import when the user does not exist).
//!
//! **Import** runs in one transaction with deferred constraints (any order, cycles),
//! checks schema compatibility first, remaps ids through natural keys, applies a
//! conflict strategy, optionally prunes removed children, verifies every foreign
//! key before commit, and returns a field-level report. With `dry_run` the
//! transaction is rolled back: the report is an exact diff.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use chrono::Utc;
use ferro_core::bundle::{
    item_changes, Bundle, ImportOptions, ImportReport, Item, ItemAction, ItemResult, Selection, Strategy, BUNDLE_FORMAT,
};
use ferro_core::graph::topo_order;
use ferro_core::hash::hash_json;
use ferro_core::system::{is_system_collection, FILES, FOLDERS, USERS};
use ferro_core::{FieldType, SyncMode};
use serde_json::{Map, Value};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::activity;
use crate::ddl::q;
use crate::files::Storage;
use crate::items::{fetch_json, raw_by_ids, raw_delete, raw_get, raw_ids_where_in, raw_select, raw_upsert};
use crate::permissions::Accountability;
use crate::query::{compile_filter, Sql};
use crate::schema::{CollectionInfo, Schema};
use crate::{FerroError, Result};

/// Fields never compared nor transported as-is.
const VOLATILE: &[&str] = &["date_created", "date_updated", "user_created", "user_updated"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Visit {
    /// The item only (selection without dependencies).
    Only,
    /// Item + its m2o dependencies.
    Shallow,
    /// Item + m2o dependencies + owned children.
    Deep,
}

fn parse_uuid(v: &Value) -> Option<Uuid> {
    v.as_str().and_then(|s| Uuid::parse_str(s).ok())
}

fn item_id(item: &Item) -> Option<Uuid> {
    item.get("id").and_then(parse_uuid)
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

pub async fn export(conn: &mut PgConnection, schema: &Schema, selections: &[Selection], source: Option<String>) -> Result<Bundle> {
    let mut visited: BTreeMap<String, BTreeMap<Uuid, Visit>> = BTreeMap::new();
    let mut rows: BTreeMap<String, BTreeMap<Uuid, Item>> = BTreeMap::new();
    let mut complete: BTreeMap<String, BTreeSet<Uuid>> = BTreeMap::new();
    let mut file_ids: BTreeSet<Uuid> = BTreeSet::new();
    let mut queue: VecDeque<(String, Uuid, Visit)> = VecDeque::new();

    for sel in selections {
        let info = schema.collection(&sel.collection)?;
        if info.system {
            return Err(FerroError::invalid(format!("system collection `{}` cannot be exported", info.name)));
        }
        if info.def.sync.exclude {
            return Err(FerroError::invalid(format!("collection `{}` is marked sync.exclude", info.name)));
        }
        let ids: Vec<Uuid> = if !sel.ids.is_empty() {
            sel.ids.clone()
        } else {
            let mut sql = Sql::new();
            let where_sql = match &sel.filter {
                Some(f) => compile_filter(schema, &info, "t0", f, &mut sql)?,
                None => "TRUE".into(),
            };
            let text = format!("SELECT COALESCE(jsonb_agg(t0.\"id\"), '[]'::jsonb) FROM {} AS t0 WHERE {where_sql}", q(&info.name));
            let v = fetch_json(&mut *conn, &text, sql.params).await?;
            v.as_array().map(|a| a.iter().filter_map(parse_uuid).collect()).unwrap_or_default()
        };
        let visit = if sel.with_dependencies { Visit::Deep } else { Visit::Only };
        for id in ids {
            queue.push_back((info.name.clone(), id, visit));
        }
    }

    while !queue.is_empty() {
        let mut batch: BTreeMap<String, Vec<(Uuid, Visit)>> = BTreeMap::new();
        while let Some((c, id, v)) = queue.pop_front() {
            batch.entry(c).or_default().push((id, v));
        }
        for (coll, entries) in batch {
            let info = schema.collection(&coll)?;
            let seen = visited.entry(coll.clone()).or_default();
            let mut todo: Vec<(Uuid, Visit)> = vec![];
            for (id, v) in entries {
                if seen.get(&id).map(|prev| v > *prev).unwrap_or(true) {
                    seen.insert(id, v);
                    todo.push((id, v));
                }
            }
            if todo.is_empty() {
                continue;
            }
            let have = rows.entry(coll.clone()).or_default();
            let need: Vec<Uuid> = todo.iter().map(|(id, _)| *id).filter(|id| !have.contains_key(id)).collect();
            for item in raw_by_ids(&mut *conn, &info, &need).await? {
                if let Some(id) = item_id(&item) {
                    have.insert(id, item);
                }
            }

            let mut deep_ids = vec![];
            for (id, visit) in &todo {
                let Some(item) = rows.get(&coll).and_then(|m| m.get(id)).cloned() else {
                    // dangling reference in the source: reported by the importer as a FK error
                    continue;
                };
                if *visit == Visit::Only {
                    continue;
                }
                if *visit == Visit::Deep {
                    deep_ids.push(*id);
                }
                for f in info.columns() {
                    match f.ty {
                        FieldType::M2o | FieldType::File => {
                            let Some(target) = item.get(&f.name).and_then(parse_uuid) else { continue };
                            let Some(related) = f.related_collection() else { continue };
                            if related == FILES {
                                file_ids.insert(target);
                                continue;
                            }
                            if is_system_collection(related) || schema.get(related).map(|r| r.def.sync.exclude).unwrap_or(true) {
                                continue;
                            }
                            match f.sync_mode() {
                                SyncMode::Reference | SyncMode::Skip => {}
                                SyncMode::Deep => queue.push_back((related.to_string(), target, Visit::Deep)),
                                SyncMode::Default => queue.push_back((related.to_string(), target, Visit::Shallow)),
                            }
                        }
                        FieldType::M2aItem => {
                            let Some(cf) = f.relation.as_ref().and_then(|r| r.collection_field.as_deref()) else { continue };
                            let (Some(target_coll), Some(target)) =
                                (item.get(cf).and_then(|v| v.as_str()), item.get(&f.name).and_then(parse_uuid))
                            else {
                                continue;
                            };
                            let excluded = schema.get(target_coll).map(|r| r.system || r.def.sync.exclude).unwrap_or(true);
                            if excluded || matches!(f.sync_mode(), SyncMode::Reference | SyncMode::Skip) {
                                continue;
                            }
                            queue.push_back((target_coll.to_string(), target, Visit::Deep));
                        }
                        _ => {}
                    }
                }
            }

            if deep_ids.is_empty() {
                continue;
            }
            complete.entry(coll.clone()).or_default().extend(deep_ids.iter().copied());
            for f in info.fields.iter().filter(|f| f.ty.is_alias()) {
                if matches!(f.sync_mode(), SyncMode::Reference | SyncMode::Skip) {
                    continue;
                }
                let (Some(related), Some(ff)) = (f.related_collection(), f.foreign_field()) else { continue };
                let rinfo = schema.collection(related)?;
                if rinfo.def.sync.exclude {
                    continue;
                }
                for child in raw_ids_where_in(&mut *conn, &rinfo, ff, &deep_ids).await? {
                    queue.push_back((related.to_string(), child, Visit::Deep));
                }
            }
        }
    }

    // files + folder chains
    let files_info = schema.collection(FILES)?;
    let folders_info = schema.collection(FOLDERS)?;
    let ids: Vec<Uuid> = file_ids.into_iter().collect();
    let files = raw_by_ids(&mut *conn, &files_info, &ids).await?;
    let mut folder_rows: BTreeMap<Uuid, Item> = BTreeMap::new();
    let mut pending: Vec<Uuid> = files.iter().filter_map(|f| f.get("folder").and_then(parse_uuid)).collect();
    while !pending.is_empty() {
        pending.retain(|id| !folder_rows.contains_key(id));
        pending.sort();
        pending.dedup();
        if pending.is_empty() {
            break;
        }
        let found = raw_by_ids(&mut *conn, &folders_info, &pending).await?;
        pending = vec![];
        for f in found {
            if let Some(id) = item_id(&f) {
                if let Some(parent) = f.get("parent").and_then(parse_uuid) {
                    pending.push(parent);
                }
                folder_rows.insert(id, f);
            }
        }
    }

    let mut collections = vec![];
    let mut items = BTreeMap::new();
    for (coll, map) in rows {
        if map.is_empty() {
            continue;
        }
        let info = schema.collection(&coll)?;
        collections.push(info.def.clone());
        let list: Vec<Item> = map
            .into_values()
            .map(|mut it| {
                if info.def.system_fields {
                    it.remove("user_created");
                    it.remove("user_updated");
                    it.remove("date_updated");
                }
                it
            })
            .collect();
        items.insert(coll, list);
    }

    let files = files
        .into_iter()
        .map(|mut f| {
            f.remove("uploaded_by");
            f
        })
        .collect();

    Ok(Bundle {
        format: BUNDLE_FORMAT.to_string(),
        created_at: Utc::now(),
        source,
        schema_hash: schema.hash.clone(),
        selections: selections.to_vec(),
        collections,
        items,
        complete: complete.into_iter().map(|(k, v)| (k, v.into_iter().collect())).collect(),
        folders: folder_rows.into_values().collect(),
        files,
    })
}

// ---------------------------------------------------------------------------
// files
// ---------------------------------------------------------------------------

/// Which of the given files (id, checksum) need their binary uploaded.
pub async fn missing_blobs(conn: &mut PgConnection, schema: &Schema, storage: &dyn Storage, files: &[(Uuid, Option<String>)]) -> Result<Vec<Uuid>> {
    let info = schema.collection(FILES)?;
    let ids: Vec<Uuid> = files.iter().map(|(id, _)| *id).collect();
    let existing: BTreeMap<Uuid, Item> = raw_by_ids(conn, &info, &ids)
        .await?
        .into_iter()
        .filter_map(|it| item_id(&it).map(|id| (id, it)))
        .collect();
    let mut missing = vec![];
    for (id, checksum) in files {
        let blob = storage.exists(&id.to_string()).await?;
        let same = match (existing.get(id).and_then(|r| r.get("checksum")).and_then(|v| v.as_str()), checksum) {
            (Some(a), Some(b)) => a == b.as_str(),
            // no row yet: the blob may have been uploaded already for this import
            (None, _) => true,
            _ => false,
        };
        if !blob || !same {
            missing.push(*id);
        }
    }
    Ok(missing)
}

// ---------------------------------------------------------------------------
// import
// ---------------------------------------------------------------------------

fn label_of(info: &CollectionInfo, item: &Item) -> Option<String> {
    let mut keys: Vec<&str> = info.def.sync.natural_key.iter().map(|s| s.as_str()).collect();
    keys.extend(["title", "name", "slug", "code", "filename_download", "email", "collection"]);
    for k in keys {
        if let Some(Value::String(s)) = item.get(k) {
            if !s.is_empty() {
                return Some(s.clone());
            }
        }
    }
    None
}

/// Hash of the target content restricted to the keys transported by the bundle.
fn content_hash(item: &Item, keys: &[String]) -> String {
    let mut m = Map::new();
    for k in keys {
        if !VOLATILE.contains(&k.as_str()) {
            m.insert(k.clone(), item.get(k).cloned().unwrap_or(Value::Null));
        }
    }
    hash_json(&Value::Object(m))
}

async fn sync_state(conn: &mut PgConnection, collection: &str, id: Uuid) -> Result<Option<String>> {
    Ok(sqlx::query_scalar::<sqlx::Postgres, String>("SELECT hash FROM ferro_sync_state WHERE collection = $1 AND item = $2")
        .bind(collection)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?)
}

async fn set_sync_state(conn: &mut PgConnection, collection: &str, id: Uuid, source: Option<&str>, hash: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO ferro_sync_state (collection, item, source, hash, synced_at) VALUES ($1, $2, $3, $4, now()) \
         ON CONFLICT (collection, item) DO UPDATE SET source = EXCLUDED.source, hash = EXCLUDED.hash, synced_at = now()",
    )
    .bind(collection)
    .bind(id)
    .bind(source)
    .bind(hash)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

struct Importer<'a> {
    schema: &'a Schema,
    acc: &'a Accountability,
    opts: &'a ImportOptions,
    source: Option<String>,
    report: ImportReport,
}

impl<'a> Importer<'a> {
    /// Upsert a list of items of one collection, recording actions in the report.
    async fn apply(&mut self, conn: &mut PgConnection, info: &CollectionInfo, items: Vec<Item>, remapped: &BTreeMap<Uuid, Uuid>) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let user = self.acc.user.map(|u| Value::String(u.to_string())).unwrap_or(Value::Null);
        for mut item in items {
            let Some(id) = item_id(&item) else {
                self.report.errors.push(format!("{}: item without id", info.name));
                continue;
            };
            let source_id = remapped.iter().find(|(_, t)| **t == id).map(|(s, _)| *s);
            let keys: Vec<String> = item.keys().filter(|k| k.as_str() != "id").cloned().collect();
            let label = label_of(info, &item);
            let current = raw_get(&mut *conn, info, id).await?;

            let (action, changes, message) = match &current {
                None => {
                    let changes = item_changes(&Map::new(), &item, &["id"]);
                    (ItemAction::Create, changes, None)
                }
                Some(cur) => {
                    let mut ignore: Vec<&str> = VOLATILE.to_vec();
                    ignore.push("id");
                    let changes = item_changes(cur, &item, &ignore);
                    if changes.is_empty() {
                        (ItemAction::Unchanged, changes, None)
                    } else {
                        match self.opts.strategy {
                            Strategy::InsertOnly => (ItemAction::Skip, changes, Some("exists in target (insert-only)".to_string())),
                            Strategy::Overwrite => (ItemAction::Update, changes, None),
                            Strategy::Safe => {
                                let last = sync_state(&mut *conn, &info.name, id).await?;
                                let now_hash = content_hash(cur, &keys);
                                if last.as_deref() == Some(now_hash.as_str()) {
                                    (ItemAction::Update, changes, None)
                                } else if self.opts.force {
                                    (ItemAction::Update, changes, Some("modified in target since last sync: overwritten (--force)".to_string()))
                                } else {
                                    let why = if last.is_none() { "never synced and different in target" } else { "modified in target since last sync" };
                                    (ItemAction::Conflict, changes, Some(why.to_string()))
                                }
                            }
                        }
                    }
                }
            };

            if matches!(action, ItemAction::Create | ItemAction::Update) {
                if info.def.system_fields {
                    if action == ItemAction::Create {
                        item.insert("user_created".into(), user.clone());
                        item.entry("date_created".to_string()).or_insert_with(|| Value::String(now.clone()));
                    } else {
                        item.remove("date_created");
                        item.insert("user_updated".into(), user.clone());
                        item.insert("date_updated".into(), Value::String(now.clone()));
                    }
                }
                raw_upsert(&mut *conn, info, &item).await?;
            }
            if matches!(action, ItemAction::Create | ItemAction::Update | ItemAction::Unchanged) && !info.system {
                if let Some(after) = raw_get(&mut *conn, info, id).await? {
                    let h = content_hash(&after, &keys);
                    set_sync_state(&mut *conn, &info.name, id, self.source.as_deref(), &h).await?;
                }
            }
            self.report.push(ItemResult { collection: info.name.clone(), id, source_id, action, changes, label, message });
        }
        Ok(())
    }
}

/// Import a bundle. Binaries of `bundle.files` must already be in `storage`.
pub async fn import(
    pool: &PgPool,
    schema: &Schema,
    acc: &Accountability,
    storage: &dyn Storage,
    mut bundle: Bundle,
    opts: &ImportOptions,
    target: Option<String>,
) -> Result<ImportReport> {
    acc.require_admin()?;
    let mut report = ImportReport {
        dry_run: opts.dry_run,
        source: bundle.source.clone(),
        target,
        ..Default::default()
    };
    if bundle.format != BUNDLE_FORMAT {
        return Err(FerroError::invalid(format!("unsupported bundle format `{}` (expected {BUNDLE_FORMAT})", bundle.format)));
    }
    if bundle.schema_hash != schema.hash {
        report.warnings.push(format!(
            "source schema {} differs from target schema {}: field compatibility is checked item by item",
            &bundle.schema_hash[..12.min(bundle.schema_hash.len())],
            &schema.hash[..12]
        ));
    }

    // 1. compatibility
    for bc in &bundle.collections {
        let Some(tc) = schema.get(&bc.name) else {
            report.errors.push(format!("collection `{}` does not exist in target: sync the schema first", bc.name));
            continue;
        };
        if tc.system {
            report.errors.push(format!("`{}` is a system collection", bc.name));
            continue;
        }
        let mut strip = vec![];
        for f in bc.resolved_fields().iter().filter(|f| f.has_column()) {
            match tc.field(&f.name) {
                None if opts.ignore_missing_fields => {
                    report.warnings.push(format!("{}.{}: missing in target, ignored", bc.name, f.name));
                    strip.push(f.name.clone());
                }
                None => report.errors.push(format!("{}.{}: missing in target (sync the schema, or use ignore_missing_fields)", bc.name, f.name)),
                Some(tf) if tf.pg_type() != f.pg_type() => report.errors.push(format!(
                    "{}.{}: type {} in source, {} in target",
                    bc.name,
                    f.name,
                    f.pg_type().unwrap_or_default(),
                    tf.pg_type().unwrap_or_default()
                )),
                _ => {}
            }
        }
        if !strip.is_empty() {
            if let Some(list) = bundle.items.get_mut(&bc.name) {
                for it in list.iter_mut() {
                    for s in &strip {
                        it.remove(s);
                    }
                }
            }
        }
    }
    for name in bundle.items.keys() {
        if !bundle.collections.iter().any(|c| &c.name == name) {
            report.errors.push(format!("bundle has items for `{name}` but no definition"));
        }
    }
    if !report.errors.is_empty() {
        return Ok(report);
    }

    let mut tx = pool.begin().await?;
    sqlx::query("SET CONSTRAINTS ALL DEFERRED").execute(&mut *tx).await?;

    let mut importer = Importer { schema, acc, opts, source: bundle.source.clone(), report };
    let outcome = run_import(&mut tx, &mut importer, storage, &mut bundle).await;
    let mut report = importer.report;

    if let Err(e) = outcome {
        report.errors.push(match e.sqlstate().as_deref() {
            Some("23503") => format!("reference check failed (an item points to something that is neither in the bundle nor in the target): {e}"),
            Some("23505") => format!("unique constraint violated (same business key with a different id? declare sync.natural_key): {e}"),
            _ => e.to_string(),
        });
        tx.rollback().await?;
        return Ok(report);
    }
    let conflicts = report.conflicts();
    if conflicts > 0 && !opts.force {
        report.errors.push(format!("{conflicts} conflict(s): nothing applied (resolve them or use --force)"));
        tx.rollback().await?;
        return Ok(report);
    }
    if opts.dry_run {
        tx.rollback().await?;
        report.ok = true;
        return Ok(report);
    }
    let t = report.totals();
    let summary = format!("create {} update {} delete {} unchanged {}", t.create, t.update, t.delete, t.unchanged);
    activity::record(&mut tx, acc, "import", "ferro_sync", &summary, activity::ORIGIN_SYNC, None, Some(&serde_json::to_value(&report.summary)?)).await?;
    tx.commit().await?;
    report.ok = true;
    report.committed = true;
    Ok(report)
}

async fn run_import(conn: &mut PgConnection, imp: &mut Importer<'_>, storage: &dyn Storage, bundle: &mut Bundle) -> Result<()> {
    let schema = imp.schema;

    // 2. natural key remapping (target id wins). Parents are matched before their children and each collection's remapped
    //    ids are written into the bundle at once: a natural key made of references (a translation is "this page in this
    //    language") then compares the *target's* ids, so the same translation on both sides is recognised as one.
    let mut remap: BTreeMap<String, BTreeMap<Uuid, Uuid>> = BTreeMap::new();
    let names: Vec<String> = bundle.items.keys().cloned().collect();
    let deps = dependency_edges(schema, &names)?;
    for coll in topo_order(&names, &deps) {
        let info = schema.collection(&coll)?;
        let nk = info.def.sync.natural_key.clone();
        if nk.is_empty() {
            continue;
        }
        let mut local: BTreeMap<Uuid, Uuid> = BTreeMap::new();
        for it in bundle.items.get_mut(&coll).map(|l| l.iter_mut()).into_iter().flatten() {
            let Some(id) = item_id(it) else { continue };
            if crate::items::exists(&mut *conn, &coll, id).await? {
                continue;
            }
            let mut sql = Sql::new();
            let mut conds = vec![];
            let mut complete_key = true;
            for k in &nk {
                let f = info.require_field(k)?;
                match it.get(k) {
                    None | Some(Value::Null) => complete_key = false,
                    Some(v) => {
                        let p = sql.bind_cast(crate::query::column_param(f, v), &f.cast_type());
                        conds.push(format!("t0.{} = {p}", q(k)));
                    }
                }
            }
            if !complete_key {
                continue;
            }
            let found = raw_select(&mut *conn, &info, &conds.join(" AND "), sql.params).await?;
            if let Some(target_id) = found.first().and_then(item_id) {
                local.insert(id, target_id);
                it.insert("id".into(), Value::String(target_id.to_string()));
            }
        }
        if !local.is_empty() {
            let mut single = BTreeMap::new();
            single.insert(coll.clone(), local.clone());
            apply_remap(schema, bundle, &single)?;
            remap.insert(coll, local);
        }
    }
    if !remap.is_empty() {
        let n: usize = remap.values().map(|m| m.len()).sum();
        imp.report.warnings.push(format!("{n} item(s) matched by natural key: ids remapped to the target ones"));
    }

    // 3. user references that do not exist in the target -> null
    let users_info = schema.collection(USERS)?;
    for (coll, list) in bundle.items.iter_mut() {
        let info = schema.collection(coll)?;
        let user_fields: Vec<String> = info
            .columns()
            .filter(|f| f.ty.is_m2o_like() && f.related_collection() == Some(USERS))
            .map(|f| f.name.clone())
            .collect();
        if user_fields.is_empty() {
            continue;
        }
        let ids: Vec<Uuid> = list
            .iter()
            .flat_map(|it| user_fields.iter().filter_map(|f| it.get(f).and_then(parse_uuid)).collect::<Vec<_>>())
            .collect();
        let existing: BTreeSet<Uuid> = raw_by_ids(&mut *conn, &users_info, &ids).await?.iter().filter_map(item_id).collect();
        for it in list.iter_mut() {
            for f in &user_fields {
                if let Some(u) = it.get(f).and_then(parse_uuid) {
                    if !existing.contains(&u) {
                        it.insert(f.clone(), Value::Null);
                        imp.report.warnings.push(format!("{coll}.{f}: user {u} does not exist in target, set to null"));
                    }
                }
            }
        }
    }

    // 4. folders and files
    let no_remap = BTreeMap::new();
    let folders_info = schema.collection(FOLDERS)?;
    let folders = std::mem::take(&mut bundle.folders);
    imp.apply(&mut *conn, &folders_info, folders, &no_remap).await?;
    let files_info = schema.collection(FILES)?;
    let mut files = std::mem::take(&mut bundle.files);
    for f in files.iter_mut() {
        let Some(id) = item_id(f) else { continue };
        if !storage.exists(&id.to_string()).await? {
            if imp.opts.dry_run {
                imp.report.warnings.push(format!("binary of file {id} not yet in the target storage (will be transferred)"));
            } else {
                return Err(FerroError::invalid(format!(
                    "binary of file {id} is missing in the target storage: upload it first (PUT /sync/files/{id})"
                )));
            }
        }
        f.insert("storage".into(), Value::String(storage.name().to_string()));
        f.insert("filename_disk".into(), Value::String(id.to_string()));
        f.remove("uploaded_by");
    }
    imp.apply(&mut *conn, &files_info, files, &no_remap).await?;

    // 5. collections in dependency order
    let names: Vec<String> = bundle.items.keys().cloned().collect();
    let deps = dependency_edges(schema, &names)?;
    for name in topo_order(&names, &deps) {
        let info = schema.collection(&name)?;
        let list = bundle.items.remove(&name).unwrap_or_default();
        let ids: BTreeSet<Uuid> = list.iter().filter_map(item_id).collect();
        let empty = BTreeMap::new();
        let remapped = remap.get(&name).unwrap_or(&empty);
        imp.apply(&mut *conn, &info, list, remapped).await?;
        imp_ids_insert(&mut bundle.complete, &name, &ids);
    }

    // 6. prune children removed in the source
    if imp.opts.prune {
        prune(&mut *conn, imp, bundle).await?;
    }

    // 7. every foreign key must hold now (also in dry-run)
    sqlx::query("SET CONSTRAINTS ALL IMMEDIATE").execute(&mut *conn).await?;
    Ok(())
}

/// `(child, parent)` for every m2o-like column of a collection in `names` that points to another collection in `names`.
fn dependency_edges(schema: &Schema, names: &[String]) -> Result<Vec<(String, String)>> {
    let mut deps = vec![];
    for name in names {
        let info = schema.collection(name)?;
        for f in info.columns() {
            if let Some(r) = f.related_collection() {
                if f.ty.is_m2o_like() && names.iter().any(|n| n == r) {
                    deps.push((name.clone(), r.to_string()));
                }
            }
        }
    }
    Ok(deps)
}

/// Keep the set of imported ids per collection for prune (stored under a reserved key).
fn imp_ids_insert(complete: &mut BTreeMap<String, Vec<Uuid>>, coll: &str, ids: &BTreeSet<Uuid>) {
    complete.insert(format!("@imported:{coll}"), ids.iter().copied().collect());
}

fn apply_remap(schema: &Schema, bundle: &mut Bundle, remap: &BTreeMap<String, BTreeMap<Uuid, Uuid>>) -> Result<()> {
    let lookup = |coll: &str, v: &Value| -> Option<Uuid> { parse_uuid(v).and_then(|id| remap.get(coll).and_then(|m| m.get(&id)).copied()) };
    for (coll, list) in bundle.items.iter_mut() {
        let info = schema.collection(coll)?;
        for it in list.iter_mut() {
            for f in info.columns() {
                let new = match f.ty {
                    FieldType::M2o | FieldType::File => match (f.related_collection(), it.get(&f.name)) {
                        (Some(r), Some(v)) => lookup(r, v),
                        _ => None,
                    },
                    FieldType::M2aItem => {
                        let cf = f.relation.as_ref().and_then(|r| r.collection_field.as_deref()).unwrap_or("");
                        match (it.get(cf).and_then(|v| v.as_str()), it.get(&f.name)) {
                            (Some(c), Some(v)) => lookup(c, v),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                if let Some(n) = new {
                    it.insert(f.name.clone(), Value::String(n.to_string()));
                }
            }
        }
    }
    for (coll, ids) in bundle.complete.iter_mut() {
        if let Some(m) = remap.get(coll) {
            for id in ids.iter_mut() {
                if let Some(n) = m.get(&*id) {
                    *id = *n;
                }
            }
        }
    }
    Ok(())
}

async fn prune(conn: &mut PgConnection, imp: &mut Importer<'_>, bundle: &Bundle) -> Result<()> {
    let schema = imp.schema;
    let parents_by_coll: Vec<(String, Vec<Uuid>)> = bundle
        .complete
        .iter()
        .filter(|(k, _)| !k.starts_with('@'))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (coll, parents) in parents_by_coll {
        let info = schema.collection(&coll)?;
        for f in info.fields.iter().filter(|f| f.ty.is_alias()) {
            if matches!(f.sync_mode(), SyncMode::Reference | SyncMode::Skip) {
                continue;
            }
            let (Some(related), Some(ff)) = (f.related_collection(), f.foreign_field()) else { continue };
            let rinfo = schema.collection(related)?;
            if rinfo.def.sync.exclude {
                continue;
            }
            let in_bundle: BTreeSet<Uuid> = bundle
                .complete
                .get(&format!("@imported:{related}"))
                .map(|v| v.iter().copied().collect())
                .unwrap_or_default();
            let target_children = raw_ids_where_in(&mut *conn, &rinfo, ff, &parents).await?;
            let stale: Vec<Uuid> = target_children.into_iter().filter(|id| !in_bundle.contains(id)).collect();
            if stale.is_empty() {
                continue;
            }
            let required = rinfo.field(ff).map(|x| x.required).unwrap_or(false);
            let delete = required || matches!(f.ty, FieldType::M2m | FieldType::M2a | FieldType::Translations);
            let before = raw_by_ids(&mut *conn, &rinfo, &stale).await?;
            for row in &before {
                let Some(id) = item_id(row) else { continue };
                let label = label_of(&rinfo, row);
                if delete {
                    imp.report.push(ItemResult {
                        collection: rinfo.name.clone(),
                        id,
                        source_id: None,
                        action: ItemAction::Delete,
                        changes: vec![],
                        label,
                        message: Some(format!("removed from {coll}.{} in the source", f.name)),
                    });
                } else {
                    imp.report.push(ItemResult {
                        collection: rinfo.name.clone(),
                        id,
                        source_id: None,
                        action: ItemAction::Update,
                        changes: vec![ferro_core::bundle::FieldChange {
                            field: ff.to_string(),
                            before: row.get(ff).cloned().unwrap_or(Value::Null),
                            after: Value::Null,
                        }],
                        label,
                        message: Some(format!("detached from {coll}.{}", f.name)),
                    });
                }
            }
            if delete {
                raw_delete(&mut *conn, &rinfo, &stale).await?;
            } else {
                for id in &stale {
                    let mut m = Map::new();
                    m.insert("id".into(), Value::String(id.to_string()));
                    m.insert(ff.to_string(), Value::Null);
                    raw_upsert(&mut *conn, &rinfo, &m).await?;
                }
            }
        }
    }
    Ok(())
}
