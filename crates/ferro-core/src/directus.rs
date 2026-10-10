//! Conversion of a Directus schema snapshot (`npx directus schema snapshot ...`
//! or `GET /schema/snapshot`) into a Ferro [`Snapshot`].
//!
//! Converted: collections, fields, M2O / O2M / M2M / M2A / file relations,
//! field and collection metadata. Not converted (by design): Directus roles,
//! policies and permissions (recreate them once as Ferro policies), flows,
//! dashboards, presets and translations of the Directus UI.
//!
//! Directus ids are usually integers or random uuids. Ferro ids are uuid:
//! [`directus_uuid`] derives the same uuid for the same `(collection, id)` on
//! every run and in every environment, so converted data keeps stable ids.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use uuid::Uuid;

use crate::model::*;
use crate::system::{FILES, USERS};
use crate::validate::validate_snapshot;

const NAMESPACE: Uuid = Uuid::from_u128(0x0199_a000_0000_7000_8000_0000_d1ec_7005);

/// Deterministic Ferro id for a Directus item (`collection`, `id`).
pub fn directus_uuid(collection: &str, id: &str) -> Uuid {
    Uuid::new_v5(&NAMESPACE, format!("{collection}:{id}").as_bytes())
}

pub struct Converted {
    pub snapshot: Snapshot,
    pub warnings: Vec<String>,
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(String::from)
}

fn b(v: &Value, k: &str) -> bool {
    v.get(k).and_then(|x| x.as_bool()).unwrap_or(false)
}

fn non_null<'a>(v: &'a Value, k: &str) -> Option<&'a Value> {
    v.get(k).filter(|x| !x.is_null())
}

fn map_type(t: &str) -> Option<FieldType> {
    use FieldType as T;
    Some(match t {
        "string" | "hash" => T::String,
        "text" | "csv" => T::Text,
        "integer" => T::Integer,
        "bigInteger" => T::BigInteger,
        "float" => T::Float,
        "decimal" => T::Decimal,
        "boolean" => T::Boolean,
        "json" => T::Json,
        "uuid" => T::Uuid,
        "date" => T::Date,
        "dateTime" | "timestamp" => T::Datetime,
        "time" => T::Time,
        "alias" => T::O2m,
        _ => return None,
    })
}

fn collection_meta(m: &Value) -> CollectionMeta {
    CollectionMeta {
        icon: s(m, "icon"),
        note: s(m, "note"),
        hidden: b(m, "hidden"),
        singleton: b(m, "singleton"),
        display_template: s(m, "display_template"),
        sort_field: s(m, "sort_field"),
        archive_field: s(m, "archive_field"),
        archive_value: non_null(m, "archive_value").cloned(),
        group: s(m, "group"),
        translations: non_null(m, "translations").cloned(),
        revisions: true,
    }
}

fn field_meta(m: &Value) -> FieldMeta {
    FieldMeta {
        interface: s(m, "interface"),
        options: non_null(m, "options").cloned(),
        display: s(m, "display"),
        display_options: non_null(m, "display_options").cloned(),
        note: s(m, "note"),
        hidden: b(m, "hidden"),
        readonly: b(m, "readonly"),
        width: s(m, "width"),
        group: s(m, "group"),
        translations: non_null(m, "translations").cloned(),
    }
}

fn on_delete(r: &Value) -> OnDelete {
    let raw = r
        .get("schema")
        .and_then(|x| x.get("on_delete"))
        .and_then(|v| v.as_str())
        .map(|x| x.to_ascii_uppercase());
    match raw.as_deref() {
        Some("CASCADE") => OnDelete::Cascade,
        Some("RESTRICT") => OnDelete::Restrict,
        Some("NO ACTION") => OnDelete::NoAction,
        _ => OnDelete::SetNull,
    }
}

fn ensure_field<'a>(col: &'a mut Collection, name: &str) -> &'a mut Field {
    match col.fields.iter().position(|f| f.name == name) {
        Some(i) => &mut col.fields[i],
        None => {
            col.fields.push(Field::new(name, FieldType::Uuid));
            col.fields.last_mut().expect("just pushed")
        }
    }
}

fn allowed_collections(meta: &Value) -> Vec<String> {
    meta.get("one_allowed_collections")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

pub fn convert(src: &Value) -> Result<Converted, String> {
    let null = Value::Null;
    let empty: Vec<Value> = vec![];
    let cols = src
        .get("collections")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "missing `collections`: is this the output of `directus schema snapshot`?".to_string())?;
    let fields = src.get("fields").and_then(|v| v.as_array()).unwrap_or(&empty);
    let relations = src.get("relations").and_then(|v| v.as_array()).unwrap_or(&empty);

    let mut warnings: Vec<String> = vec![];
    let mut built: BTreeMap<String, Collection> = BTreeMap::new();
    // alias fields not (yet) bound to a relation -> silent when purely presentational
    let mut aliases: BTreeMap<(String, String), bool> = BTreeMap::new();
    // alias fields Directus created with the "translations" interface (`special: ["translations"]`)
    let mut translation_aliases: BTreeSet<(String, String)> = BTreeSet::new();
    // collections whose primary key is a string other than `id` (Directus `languages` uses `code`): kept as a unique field
    let mut pk_names: BTreeMap<String, String> = BTreeMap::new();

    // ---- collections and plain fields -------------------------------------
    for c in cols {
        let Some(name) = s(c, "collection") else { continue };
        if name.starts_with("directus_") {
            continue;
        }
        // folders (grouping only) have no table
        if c.get("schema").map(|x| x.is_null()).unwrap_or(true) {
            continue;
        }
        let mut col = Collection::new(name.clone());
        col.meta = collection_meta(c.get("meta").unwrap_or(&null));

        let mine: Vec<&Value> = fields.iter().filter(|f| s(f, "collection").as_deref() == Some(name.as_str())).collect();
        let present: BTreeSet<String> = mine.iter().filter_map(|f| s(f, "field")).collect();
        col.system_fields = SYSTEM_FIELDS.iter().all(|n| present.contains(*n));

        for f in mine {
            let Some(fname) = s(f, "field") else { continue };
            let schema = f.get("schema").unwrap_or(&null);
            let meta = f.get("meta").unwrap_or(&null);
            let tname = s(f, "type").unwrap_or_default();
            let is_pk = b(schema, "is_primary_key");
            if is_pk {
                if fname == "id" {
                    continue;
                }
                if !matches!(map_type(&tname), Some(FieldType::String | FieldType::Text)) {
                    warnings.push(format!("{name}.{fname}: primary key not named `id` is skipped (Ferro uses an implicit uuid `id`)"));
                    continue;
                }
                // a text key (languages.code, ...) carries meaning: keep it as a unique required field and use it to match items
                warnings.push(format!("{name}.{fname}: the primary key is kept as a unique, required string field and used as the natural key (Ferro's primary key is an implicit uuid `id`; references are converted to it)"));
                pk_names.insert(name.clone(), fname.clone());
            }
            if col.system_fields && SYSTEM_FIELDS.contains(&fname.as_str()) {
                continue;
            }
            let ty = match map_type(&tname) {
                Some(t) => t,
                None => {
                    warnings.push(format!("{name}.{fname}: unknown type `{tname}`, converted to string"));
                    FieldType::String
                }
            };
            let mut fld = Field::new(fname.clone(), ty);
            fld.meta = field_meta(meta);
            if ty == FieldType::O2m && meta.get("special").and_then(|v| v.as_array()).map(|a| a.iter().any(|x| x.as_str() == Some("translations"))).unwrap_or(false) {
                translation_aliases.insert((name.clone(), fname.clone()));
            }
            if ty == FieldType::O2m {
                let iface = fld.meta.interface.clone().unwrap_or_default();
                let silent = iface.starts_with("presentation-") || iface.starts_with("group-");
                aliases.insert((name.clone(), fname.clone()), silent);
            } else {
                fld.required = schema.get("is_nullable").and_then(|v| v.as_bool()) == Some(false) || b(meta, "required");
                fld.unique = b(schema, "is_unique");
                if is_pk {
                    fld.required = true;
                    fld.unique = true;
                    col.sync.natural_key = vec![fname.clone()];
                }
                if ty == FieldType::String {
                    fld.max_length = schema.get("max_length").and_then(|v| v.as_u64()).map(|n| n as u32);
                }
                if ty == FieldType::Decimal {
                    fld.precision = schema.get("numeric_precision").and_then(|v| v.as_u64()).map(|n| n as u32);
                    fld.scale = schema.get("numeric_scale").and_then(|v| v.as_u64()).map(|n| n as u32);
                }
                fld.default = match non_null(schema, "default_value") {
                    None => None,
                    Some(Value::String(x)) if x.eq_ignore_ascii_case("current_timestamp") => Some(Value::String("$NOW".into())),
                    Some(Value::String(x)) if x.contains('(') => {
                        warnings.push(format!("{name}.{fname}: default `{x}` is an expression, dropped"));
                        None
                    }
                    Some(v) => Some(v.clone()),
                };
            }
            col.fields.push(fld);
        }
        built.insert(name, col);
    }
    let known: BTreeSet<String> = built.keys().cloned().collect();

    // ---- child side: m2o / file / m2a_item --------------------------------
    for r in relations {
        let (Some(cname), Some(fname)) = (s(r, "collection"), s(r, "field")) else { continue };
        let Some(col) = built.get_mut(&cname) else { continue };
        if col.system_fields && SYSTEM_FIELDS.contains(&fname.as_str()) {
            continue;
        }
        let meta = r.get("meta").unwrap_or(&null);

        let allowed_all = allowed_collections(meta);
        if !allowed_all.is_empty() {
            let allowed: Vec<String> = allowed_all.iter().filter(|a| known.contains(*a)).cloned().collect();
            if allowed.len() != allowed_all.len() {
                warnings.push(format!("{cname}.{fname}: some allowed collections are not converted and were removed"));
            }
            let f = ensure_field(col, &fname);
            f.ty = FieldType::M2aItem;
            f.default = None;
            f.max_length = None;
            f.relation = Some(Relation { collection_field: s(meta, "one_collection_field"), allowed, ..Default::default() });
            continue;
        }

        let Some(rel) = s(r, "related_collection") else { continue };
        let (ty, target) = if rel == "directus_files" {
            (FieldType::File, Some(FILES.to_string()))
        } else if rel == "directus_users" {
            (FieldType::M2o, Some(USERS.to_string()))
        } else if known.contains(&rel) {
            (FieldType::M2o, Some(rel.clone()))
        } else {
            (FieldType::Uuid, None)
        };
        let mut od = on_delete(r);
        let f = ensure_field(col, &fname);
        f.default = None;
        f.max_length = None;
        f.precision = None;
        f.scale = None;
        f.ty = ty;
        match target {
            Some(t) => {
                if f.required && od == OnDelete::SetNull {
                    od = OnDelete::Restrict;
                    warnings.push(format!("{cname}.{fname}: required relation with SET NULL changed to RESTRICT"));
                }
                f.relation = Some(Relation { related: Some(t), on_delete: od, ..Default::default() });
            }
            None => {
                warnings.push(format!("{cname}.{fname}: relation to `{rel}` is not converted, kept as a plain uuid"));
                f.relation = None;
            }
        }
    }

    // ---- parent side: o2m / m2m / m2a -------------------------------------
    for r in relations {
        let meta = r.get("meta").unwrap_or(&null);
        let (Some(child), Some(cfield), Some(parent), Some(alias)) =
            (s(r, "collection"), s(r, "field"), s(r, "related_collection"), s(meta, "one_field"))
        else {
            continue;
        };
        if !known.contains(&child) {
            continue;
        }
        let Some(pcol) = built.get_mut(&parent) else { continue };
        let junction = s(meta, "junction_field");
        let is_translations = junction.is_some() && translation_aliases.contains(&(parent.clone(), alias.clone()));
        let ty = match &junction {
            None => FieldType::O2m,
            Some(_) if is_translations => FieldType::Translations,
            Some(j) => {
                let far_is_m2a = relations.iter().any(|o| {
                    s(o, "collection").as_deref() == Some(child.as_str())
                        && s(o, "field").as_deref() == Some(j.as_str())
                        && !allowed_collections(o.get("meta").unwrap_or(&null)).is_empty()
                });
                if far_is_m2a {
                    FieldType::M2a
                } else {
                    FieldType::M2m
                }
            }
        };
        let f = ensure_field(pcol, &alias);
        f.ty = ty;
        f.required = false;
        f.unique = false;
        f.default = None;
        f.relation = Some(Relation {
            related: Some(child.clone()),
            foreign_field: Some(cfield.clone()),
            junction_field: if ty == FieldType::M2m { junction.clone() } else { None },
            language_field: if is_translations { junction.clone() } else { None },
            sort_field: s(meta, "sort_field"),
            ..Default::default()
        });
        aliases.remove(&(parent.clone(), alias.clone()));

        // Ferro enforces "one translation per language and item" in the database, and a NULL would escape that rule
        if let (true, Some(lang_field), Some(ccol)) = (is_translations, junction, built.get_mut(&child)) {
            let language_collection = ccol.field(&lang_field).and_then(|x| x.related_collection()).map(String::from);
            if let Some(x) = ccol.fields.iter_mut().find(|x| x.name == cfield) {
                x.required = true;
                if x.on_delete() == OnDelete::SetNull {
                    x.relation.get_or_insert_with(Relation::default).on_delete = OnDelete::Cascade;
                }
            }
            if let Some(x) = ccol.fields.iter_mut().find(|x| x.name == lang_field) {
                x.required = true;
                x.unique = true;
                x.unique_with = vec![cfield.clone()];
                if x.on_delete() == OnDelete::SetNull {
                    x.relation.get_or_insert_with(Relation::default).on_delete = OnDelete::Restrict;
                }
            }
            warnings.push(format!(
                "{parent}.{alias}: converted to a translations field; {child}.{lang_field} is now required and unique together with {child}.{cfield} (one translation per language and item): check the data for duplicates before importing it"
            ));
            if let Some(key) = language_collection.and_then(|l| pk_names.get(&l).cloned()) {
                if key != "code" {
                    if let Some(rel) = built.get_mut(&parent).and_then(|p| p.fields.iter_mut().find(|x| x.name == alias)).and_then(|x| x.relation.as_mut()) {
                        rel.language_key = Some(key);
                    }
                }
            }
        }
    }

    // ---- aliases without a relation (dividers, notices, groups) ----------
    for ((c, f), silent) in aliases {
        if let Some(col) = built.get_mut(&c) {
            col.fields.retain(|x| x.name != f);
        }
        if !silent {
            warnings.push(format!("{c}.{f}: alias field without relation dropped"));
        }
    }

    let mut snapshot = Snapshot {
        collections: built.into_values().collect(),
        roles: vec![admin_role()],
        policies: vec![admin_policy()],
        ..Snapshot::default()
    };
    snapshot.normalize();
    for e in validate_snapshot(&snapshot) {
        warnings.push(format!("validation: {e}"));
    }
    Ok(Converted { snapshot, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pk() -> Value {
        json!({ "is_primary_key": true })
    }

    fn sample() -> Value {
        json!({
            "collections": [
                { "collection": "pages", "meta": { "icon": "article" }, "schema": { "name": "pages" } },
                { "collection": "pages_blocks", "meta": { "hidden": true }, "schema": {} },
                { "collection": "block_text", "meta": null, "schema": {} },
                { "collection": "tags", "meta": null, "schema": {} },
                { "collection": "pages_tags", "meta": null, "schema": {} },
                { "collection": "content", "meta": { "icon": "folder" }, "schema": null }
            ],
            "fields": [
                { "collection": "pages", "field": "id", "type": "integer", "schema": pk() },
                { "collection": "pages", "field": "title", "type": "string", "meta": { "interface": "input" }, "schema": { "is_nullable": false, "max_length": 120 } },
                { "collection": "pages", "field": "status", "type": "string", "schema": { "default_value": "draft" } },
                { "collection": "pages", "field": "cover", "type": "uuid", "schema": {} },
                { "collection": "pages", "field": "blocks", "type": "alias", "meta": { "interface": "list-m2a" } },
                { "collection": "pages", "field": "tags", "type": "alias", "meta": { "interface": "list-m2m" } },
                { "collection": "pages", "field": "divider", "type": "alias", "meta": { "interface": "presentation-divider" } },
                { "collection": "pages", "field": "date_created", "type": "timestamp", "schema": {} },
                { "collection": "pages", "field": "user_created", "type": "uuid", "schema": {} },
                { "collection": "pages", "field": "date_updated", "type": "timestamp", "schema": {} },
                { "collection": "pages", "field": "user_updated", "type": "uuid", "schema": {} },
                { "collection": "pages_blocks", "field": "id", "type": "integer", "schema": pk() },
                { "collection": "pages_blocks", "field": "pages_id", "type": "integer", "schema": {} },
                { "collection": "pages_blocks", "field": "item", "type": "string", "schema": {} },
                { "collection": "pages_blocks", "field": "collection", "type": "string", "schema": {} },
                { "collection": "pages_blocks", "field": "sort", "type": "integer", "schema": {} },
                { "collection": "block_text", "field": "id", "type": "integer", "schema": pk() },
                { "collection": "block_text", "field": "body", "type": "text", "schema": {} },
                { "collection": "tags", "field": "id", "type": "integer", "schema": pk() },
                { "collection": "tags", "field": "name", "type": "string", "schema": { "is_unique": true } },
                { "collection": "pages_tags", "field": "id", "type": "integer", "schema": pk() },
                { "collection": "pages_tags", "field": "pages_id", "type": "integer", "schema": {} },
                { "collection": "pages_tags", "field": "tags_id", "type": "integer", "schema": {} }
            ],
            "relations": [
                { "collection": "pages", "field": "cover", "related_collection": "directus_files", "meta": null, "schema": { "on_delete": "SET NULL" } },
                { "collection": "pages", "field": "user_created", "related_collection": "directus_users", "meta": null, "schema": { "on_delete": "NO ACTION" } },
                { "collection": "pages_blocks", "field": "item", "related_collection": null,
                  "meta": { "one_allowed_collections": ["block_text"], "one_collection_field": "collection", "junction_field": "pages_id" } },
                { "collection": "pages_blocks", "field": "pages_id", "related_collection": "pages",
                  "meta": { "one_field": "blocks", "junction_field": "item", "sort_field": "sort" }, "schema": { "on_delete": "CASCADE" } },
                { "collection": "pages_tags", "field": "pages_id", "related_collection": "pages",
                  "meta": { "one_field": "tags", "junction_field": "tags_id" }, "schema": { "on_delete": "CASCADE" } },
                { "collection": "pages_tags", "field": "tags_id", "related_collection": "tags",
                  "meta": { "one_field": null, "junction_field": "pages_id" }, "schema": { "on_delete": "SET NULL" } }
            ]
        })
    }

    #[test]
    fn converts_relations_and_fields() {
        let out = convert(&sample()).unwrap();
        let snap = &out.snapshot;
        let errors: Vec<_> = out.warnings.iter().filter(|w| w.starts_with("validation:")).collect();
        assert!(errors.is_empty(), "{errors:#?}");

        // folder skipped, six - 1 collections
        assert_eq!(snap.collections.len(), 5);
        let pages = snap.collection("pages").unwrap();
        assert!(pages.system_fields);
        assert!(pages.field("id").is_none() && pages.field("date_created").is_none());
        assert!(pages.field("divider").is_none());
        let title = pages.field("title").unwrap();
        assert!(title.required);
        assert_eq!(title.max_length, Some(120));
        assert_eq!(pages.field("status").unwrap().default, Some(json!("draft")));
        assert_eq!(pages.field("cover").unwrap().ty, FieldType::File);

        let blocks = pages.field("blocks").unwrap();
        assert_eq!(blocks.ty, FieldType::M2a);
        let rel = blocks.relation.as_ref().unwrap();
        assert_eq!(rel.related.as_deref(), Some("pages_blocks"));
        assert_eq!(rel.foreign_field.as_deref(), Some("pages_id"));
        assert_eq!(rel.sort_field.as_deref(), Some("sort"));

        let tags = pages.field("tags").unwrap();
        assert_eq!(tags.ty, FieldType::M2m);
        assert_eq!(tags.relation.as_ref().unwrap().junction_field.as_deref(), Some("tags_id"));

        let junction = snap.collection("pages_blocks").unwrap();
        assert!(!junction.system_fields);
        let item = junction.field("item").unwrap();
        assert_eq!(item.ty, FieldType::M2aItem);
        assert_eq!(item.relation.as_ref().unwrap().allowed, vec!["block_text".to_string()]);
        let pages_id = junction.field("pages_id").unwrap();
        assert_eq!(pages_id.ty, FieldType::M2o);
        assert_eq!(pages_id.on_delete(), OnDelete::Cascade);

        assert!(snap.role(ADMIN_ROLE_ID).is_some());
    }

    #[test]
    fn translations_fields_become_the_dedicated_type() {
        let src = json!({
            "collections": [
                { "collection": "pages", "meta": null, "schema": {} },
                { "collection": "pages_translations", "meta": { "hidden": true }, "schema": {} },
                { "collection": "languages", "meta": null, "schema": {} }
            ],
            "fields": [
                { "collection": "pages", "field": "id", "type": "integer", "schema": pk() },
                { "collection": "pages", "field": "slug", "type": "string", "schema": {} },
                { "collection": "pages", "field": "translations", "type": "alias", "meta": { "special": ["translations"], "interface": "translations" } },
                { "collection": "pages_translations", "field": "id", "type": "integer", "schema": pk() },
                { "collection": "pages_translations", "field": "pages_id", "type": "integer", "schema": {} },
                { "collection": "pages_translations", "field": "languages_code", "type": "string", "schema": {} },
                { "collection": "pages_translations", "field": "title", "type": "string", "schema": {} },
                { "collection": "languages", "field": "code", "type": "string", "schema": { "is_primary_key": true, "max_length": 255 } },
                { "collection": "languages", "field": "name", "type": "string", "schema": {} }
            ],
            "relations": [
                { "collection": "pages_translations", "field": "pages_id", "related_collection": "pages",
                  "meta": { "one_field": "translations", "junction_field": "languages_code" }, "schema": { "on_delete": "SET NULL" } },
                { "collection": "pages_translations", "field": "languages_code", "related_collection": "languages",
                  "meta": { "one_field": null, "junction_field": "pages_id" }, "schema": { "on_delete": "SET NULL" } }
            ]
        });
        let out = convert(&src).unwrap();
        let bad: Vec<_> = out.warnings.iter().filter(|w| w.starts_with("validation:")).collect();
        assert!(bad.is_empty(), "{bad:#?}");
        let snap = &out.snapshot;
        let alias = snap.collection("pages").unwrap().field("translations").unwrap();
        assert_eq!((alias.ty, alias.related_collection(), alias.foreign_field(), alias.language_field(), alias.language_key()), (FieldType::Translations, Some("pages_translations"), Some("pages_id"), Some("languages_code"), "code"));
        let tc = snap.collection("pages_translations").unwrap();
        let lf = tc.field("languages_code").unwrap();
        assert!(lf.unique && lf.required && lf.unique_with == vec!["pages_id"], "{lf:?}");
        assert!(tc.field("pages_id").unwrap().required);
        assert_eq!(tc.field("pages_id").unwrap().on_delete(), OnDelete::Cascade, "a required link cannot be SET NULL");
        let langs = snap.collection("languages").unwrap();
        let code = langs.field("code").unwrap();
        assert!(code.required && code.unique, "the text primary key is kept");
        assert_eq!(langs.sync.natural_key, vec!["code"]);
        assert!(out.warnings.iter().any(|w| w.contains("languages.code") && w.contains("primary key is kept")));
        assert!(out.warnings.iter().any(|w| w.contains("one translation per language and item")));
    }

    #[test]
    fn a_languages_collection_keyed_by_another_name_sets_language_key() {
        let src = json!({
            "collections": [
                { "collection": "pages", "meta": null, "schema": {} },
                { "collection": "pages_translations", "meta": null, "schema": {} },
                { "collection": "languages", "meta": null, "schema": {} }
            ],
            "fields": [
                { "collection": "pages", "field": "id", "type": "integer", "schema": pk() },
                { "collection": "pages", "field": "t", "type": "alias", "meta": { "special": ["translations"] } },
                { "collection": "pages_translations", "field": "id", "type": "integer", "schema": pk() },
                { "collection": "pages_translations", "field": "pages_id", "type": "integer", "schema": {} },
                { "collection": "pages_translations", "field": "lang", "type": "string", "schema": {} },
                { "collection": "languages", "field": "iso", "type": "string", "schema": { "is_primary_key": true } }
            ],
            "relations": [
                { "collection": "pages_translations", "field": "pages_id", "related_collection": "pages", "meta": { "one_field": "t", "junction_field": "lang" } },
                { "collection": "pages_translations", "field": "lang", "related_collection": "languages", "meta": { "junction_field": "pages_id" } }
            ]
        });
        let out = convert(&src).unwrap();
        assert!(out.warnings.iter().all(|w| !w.starts_with("validation:")), "{:#?}", out.warnings);
        let alias = out.snapshot.collection("pages").unwrap().field("t").unwrap();
        assert_eq!((alias.ty, alias.language_key()), (FieldType::Translations, "iso"));
    }

    #[test]
    fn stable_ids() {
        assert_eq!(directus_uuid("pages", "42"), directus_uuid("pages", "42"));
        assert_ne!(directus_uuid("pages", "42"), directus_uuid("tags", "42"));
    }

    #[test]
    fn rejects_other_json() {
        assert!(convert(&json!({ "foo": 1 })).is_err());
    }
}
