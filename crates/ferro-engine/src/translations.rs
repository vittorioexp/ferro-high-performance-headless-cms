//! Translations at run time: reading an item in the language(s) you ask for, and writing a translation by language code.
//!
//! The model side (the `translations` field type and its guarantees) lives in `ferro_core::translations`; reading and writing
//! the *rows* is ordinary relational work done by [`crate::items`]. This module adds the two things that make the dedicated
//! type worth having:
//!
//! * **`lang=it-IT,en-US`** on a read: the translatable fields come back directly on the item, filled from the first language
//!   of the list that has a value for each of them (a per-field fallback chain), plus `_language`, the language that
//!   answered first. A field the item has itself (say `title`, holding the default language) is *replaced* by the translation
//!   when there is one and keeps its own value otherwise; a field that only exists in the translations is `null` when none of
//!   the languages has it.
//! * **`{"translations": {"it-IT": {"title": "Ciao"}}}`** on a write: each key is a language *code*; the translation is
//!   created or updated, `null` removes it, and languages that are not mentioned are left alone.

use std::collections::{BTreeMap, BTreeSet};

use ferro_core::fields::FieldTree;
use ferro_core::translations::translatable_fields;
use ferro_core::{Action, Field, FieldType};
use serde_json::{json, Map, Value};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::ddl::q;
use crate::items::{create_one, delete_many, fetch_json, read_many, update_one, Ctx, ReadArgs};
use crate::permissions;
use crate::{FerroError, Result};

/// Key added to an item read with `lang`: the code of the first language that had a translation for it.
pub const LANGUAGE_KEY: &str = "_language";
const MAX_LANGUAGES: usize = 8;

/// `it-IT,en-US` → `["it-IT", "en-US"]`: the preference order, most wanted first.
pub fn parse_languages(raw: &str) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if part.len() > 32 || !part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')) {
            return Err(FerroError::invalid(format!("`{part}` is not a language code (letters, digits, - and _)")));
        }
        if !out.iter().any(|x| x == part) {
            out.push(part.to_string());
        }
    }
    if out.is_empty() {
        return Err(FerroError::invalid("`lang` needs at least one language code, e.g. lang=it-IT,en-US"));
    }
    if out.len() > MAX_LANGUAGES {
        return Err(FerroError::invalid(format!("`lang` takes at most {MAX_LANGUAGES} languages")));
    }
    Ok(out)
}

/// `[(language id, code)]` for the codes that exist. Looked up directly, not through the caller's permissions: all it reveals
/// is whether a code the caller typed is a language, and reading translations must not require access to the language list.
async fn resolve_languages(conn: &mut PgConnection, languages: &str, key: &str, codes: &[String]) -> Result<Vec<(String, String)>> {
    let text = format!(
        "SELECT COALESCE(jsonb_agg(jsonb_build_object('id', \"id\", 'code', {k})), '[]'::jsonb) FROM {t} WHERE {k} IN (SELECT jsonb_array_elements_text(CAST($1 AS jsonb)))",
        t = q(languages),
        k = q(key)
    );
    let rows = fetch_json(conn, &text, vec![Some(json!(codes).to_string())]).await?;
    Ok(rows
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| Some((r.get("id")?.as_str()?.to_string(), r.get("code")?.as_str()?.to_string())))
        .collect())
}

struct Plan {
    related: String,
    parent_field: String,
    language_field: String,
    languages: String,
    key: String,
    names: Vec<String>,
    /// Names that are also fields of the item itself: the translation replaces the value when there is one.
    overrides: BTreeSet<String>,
    /// False when the caller may not read the translations: the names are still answered, as null.
    readable: bool,
}

/// Read like [`read_many`], with the translatable fields resolved in `langs` (see the module documentation).
///
/// With `fields=*` every translatable field is included; otherwise name the ones you want.
pub async fn read_translated(ctx: &Ctx, conn: &mut PgConnection, collection: &str, mut args: ReadArgs, langs: &[String]) -> Result<Vec<Value>> {
    let info = ctx.schema.collection(collection)?;
    let translation_fields: Vec<&Field> = info.fields.iter().filter(|f| f.ty == FieldType::Translations).collect();
    if translation_fields.is_empty() {
        return Err(FerroError::invalid(format!("`lang` needs a translations field, and `{collection}` has none")));
    }
    let perm = permissions::access(&ctx.acc, collection, Action::Read)?;
    let parent_names: BTreeSet<&str> = info.fields.iter().map(|f| f.name.as_str()).collect();

    let mut plans: Vec<Plan> = Vec::new();
    let mut taken: BTreeSet<String> = BTreeSet::new();
    for tf in translation_fields {
        let (Some(related), Some(parent_field), Some(language_field)) = (tf.related_collection(), tf.foreign_field(), tf.language_field()) else { continue };
        // no access to the translations: the item simply has none (no error, as for any relation the caller cannot read)
        let readable = perm.allows(&tf.name) && permissions::access(&ctx.acc, related, Action::Read).is_ok();
        let rinfo = ctx.schema.collection(related)?;
        let Some(languages) = rinfo.field(language_field).and_then(|f| f.related_collection()).map(String::from) else { continue };
        let translatable: Vec<String> = translatable_fields(&rinfo.def, tf).iter().map(|f| f.name.clone()).collect();
        let mut names: Vec<String> = Vec::new();
        let mut overrides: BTreeSet<String> = BTreeSet::new();
        for n in &translatable {
            let wanted = args.fields.all || args.fields.fields.contains(n);
            if wanted && taken.insert(n.clone()) {
                if parent_names.contains(n.as_str()) {
                    overrides.insert(n.clone());
                }
                names.push(n.clone());
            }
        }
        plans.push(Plan { related: related.into(), parent_field: parent_field.into(), language_field: language_field.into(), languages, key: tf.language_key().to_string(), names, overrides, readable });
    }
    // names that exist only in the translations are answered here: the plain read must not reject them as unknown
    // (a name the item also has stays in the read: its value is the default the translation replaces)
    for p in &plans {
        for n in p.names.iter().filter(|n| !p.overrides.contains(*n)) {
            args.fields.fields.remove(n);
        }
    }

    let mut rows = read_many(ctx, &mut *conn, collection, args).await?;
    if rows.is_empty() {
        return Ok(rows);
    }
    let ids: Vec<String> = rows.iter().filter_map(|r| r.get("id").and_then(|v| v.as_str()).map(String::from)).collect();

    for p in &plans {
        let languages = if p.readable { resolve_languages(&mut *conn, &p.languages, &p.key, langs).await? } else { Vec::new() };
        // rank of each language id in the caller's preference list
        let rank: BTreeMap<&str, usize> = languages.iter().filter_map(|(id, code)| langs.iter().position(|l| l == code).map(|i| (id.as_str(), i))).collect();
        let code_of: BTreeMap<&str, &str> = languages.iter().map(|(id, code)| (id.as_str(), code.as_str())).collect();
        let mut by_parent: BTreeMap<String, Vec<Map<String, Value>>> = BTreeMap::new();
        if !languages.is_empty() {
            let mut tree = FieldTree::only(&[p.parent_field.as_str(), p.language_field.as_str()]);
            tree.fields.extend(p.names.iter().cloned());
            let language_ids: Vec<&str> = languages.iter().map(|(id, _)| id.as_str()).collect();
            let rargs = ReadArgs {
                fields: tree,
                filter: Some(json!({ "_and": [ { &p.parent_field: { "_in": ids } }, { &p.language_field: { "_in": language_ids } } ] })),
                limit: Some(-1),
                ..Default::default()
            };
            for row in read_many(ctx, &mut *conn, &p.related, rargs).await? {
                if let (Some(parent), Some(obj)) = (row.get(&p.parent_field).and_then(|v| v.as_str()).map(String::from), row.as_object()) {
                    by_parent.entry(parent).or_default().push(obj.clone());
                }
            }
        }
        for item in rows.iter_mut() {
            let Some(obj) = item.as_object_mut() else { continue };
            let id = obj.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let mut candidates = by_parent.remove(&id).unwrap_or_default();
            candidates.sort_by_key(|r| r.get(&p.language_field).and_then(|v| v.as_str()).and_then(|l| rank.get(l)).copied().unwrap_or(usize::MAX));
            if !obj.contains_key(LANGUAGE_KEY) {
                let first = candidates.first().and_then(|r| r.get(&p.language_field)).and_then(|v| v.as_str()).and_then(|l| code_of.get(l)).map(|c| Value::String((*c).into()));
                obj.insert(LANGUAGE_KEY.into(), first.unwrap_or(Value::Null));
            }
            for n in &p.names {
                match (candidates.iter().filter_map(|r| r.get(n)).find(|v| !v.is_null()).cloned(), p.overrides.contains(n)) {
                    (Some(v), _) => {
                        obj.insert(n.clone(), v);
                    }
                    (None, true) => {} // the item's own value is the default
                    (None, false) => {
                        obj.entry(n.clone()).or_insert(Value::Null);
                    }
                }
            }
        }
    }
    Ok(rows)
}

/// Is this object `{ "it-IT": {...} }` rather than `{ "create": [...], "update": [...], "delete": [...] }`?
pub(crate) fn is_language_map(ops: &Map<String, Value>) -> bool {
    !ops.is_empty() && !ops.keys().any(|k| matches!(k.as_str(), "create" | "update" | "delete"))
}

/// Write translations keyed by language code. Per language: an object creates or updates that language's translation (the
/// link fields cannot be overridden), `null` deletes it. Languages not mentioned are untouched, which is what makes a
/// partial update possible (unlike the array form, which replaces the whole set).
pub(crate) async fn write_language_map(ctx: &Ctx, conn: &mut PgConnection, f: &Field, parent: Uuid, map: Map<String, Value>) -> Result<()> {
    let (Some(related), Some(ff), Some(lf)) = (f.related_collection(), f.foreign_field(), f.language_field()) else {
        return Err(FerroError::Internal(format!("{}: incomplete translations relation", f.name)));
    };
    let rinfo = ctx.schema.collection(related)?;
    let languages = rinfo
        .field(lf)
        .and_then(|x| x.related_collection())
        .ok_or_else(|| FerroError::Internal(format!("{}: the language field has no relation", f.name)))?
        .to_string();
    let codes: Vec<String> = map.keys().cloned().collect();
    let known = resolve_languages(&mut *conn, &languages, f.language_key(), &codes).await?;
    for code in &codes {
        if !known.iter().any(|(_, c)| c == code) {
            return Err(FerroError::invalid(format!("unknown language `{code}`: add it to `{languages}` first")));
        }
    }
    for (code, payload) in map {
        let language_id = known.iter().find(|(_, c)| *c == code).map(|(id, _)| id.clone()).unwrap_or_default();
        let text = format!("SELECT COALESCE(jsonb_agg(\"id\"), '[]'::jsonb) FROM {} WHERE {} = CAST($1 AS uuid) AND {} = CAST($2 AS uuid)", q(related), q(ff), q(lf));
        let existing = fetch_json(&mut *conn, &text, vec![Some(parent.to_string()), Some(language_id.clone())])
            .await?
            .as_array()
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok());
        match payload {
            Value::Null => {
                if let Some(id) = existing {
                    delete_many(ctx, &mut *conn, related, vec![id]).await?;
                }
            }
            Value::Object(mut fields) => {
                for reserved in ["id", ff, lf] {
                    fields.remove(reserved);
                }
                match existing {
                    Some(id) => {
                        if !fields.is_empty() {
                            update_one(ctx, &mut *conn, related, id, Value::Object(fields)).await?;
                        }
                    }
                    None => {
                        fields.insert(ff.to_string(), Value::String(parent.to_string()));
                        fields.insert(lf.to_string(), Value::String(language_id));
                        create_one(ctx, &mut *conn, related, Value::Object(fields)).await?;
                    }
                }
            }
            _ => return Err(FerroError::invalid(format!("`{}.{code}` must be an object (or null to remove the translation)", f.name))),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_lists() {
        assert_eq!(parse_languages("it-IT, en-US,it-IT,fr_CA").unwrap(), vec!["it-IT", "en-US", "fr_CA"]);
        for bad in ["", " , ", "it IT", "it;drop", "a,b,c,d,e,f,g,h,i", &"x".repeat(40), "it-IT'--"] {
            assert!(parse_languages(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn language_map_versus_operations() {
        let m = |v: Value| v.as_object().unwrap().clone();
        assert!(is_language_map(&m(json!({"it-IT": {"title": "x"}, "en-US": null}))));
        assert!(!is_language_map(&m(json!({"create": []}))));
        assert!(!is_language_map(&m(json!({"it-IT": {}, "delete": []}))), "any operation key makes it the operations form");
        assert!(!is_language_map(&m(json!({}))));
    }
}
