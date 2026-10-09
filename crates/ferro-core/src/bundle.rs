//! Portable data bundle: the unit of data promotion between environments.
//!
//! On disk a bundle is a directory:
//! ```text
//! bundle/
//!   bundle.json        # Bundle (items, file metadata, definitions of the involved collections)
//!   files/<file-id>    # binary content of every file referenced by the items
//! ```

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::model::Collection;

pub const BUNDLE_FORMAT: &str = "ferro-bundle/1";

pub type Item = Map<String, Value>;

/// What to export: a collection, optionally restricted by ids or a filter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Selection {
    pub collection: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Value>,
    /// Follow relations (dependency closure). `false` exports only the matched items.
    #[serde(default = "crate::model::yes")]
    pub with_dependencies: bool,
}

impl Selection {
    /// CLI syntax: `pages`, `pages:id=<uuid>,<uuid>`, `pages:{"slug":{"_eq":"home"}}`
    pub fn parse(spec: &str) -> Result<Selection, String> {
        let (collection, rest) = match spec.split_once(':') {
            Some((c, r)) => (c.trim(), Some(r.trim())),
            None => (spec.trim(), None),
        };
        if collection.is_empty() {
            return Err(format!("invalid selection `{spec}`"));
        }
        let mut sel = Selection { collection: collection.to_string(), ids: vec![], filter: None, with_dependencies: true };
        match rest {
            None | Some("") => {}
            Some(r) if r.starts_with('{') => {
                sel.filter = Some(serde_json::from_str(r).map_err(|e| format!("invalid filter in `{spec}`: {e}"))?);
            }
            Some(r) if r.starts_with("id=") => {
                for id in r[3..].split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    sel.ids.push(Uuid::parse_str(id).map_err(|e| format!("invalid id `{id}`: {e}"))?);
                }
            }
            Some(r) => return Err(format!("invalid selection `{spec}`: expected `id=...` or a JSON filter after `:`, got `{r}`")),
        }
        Ok(sel)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bundle {
    pub format: String,
    pub created_at: DateTime<Utc>,
    /// Name of the source environment.
    #[serde(default)]
    pub source: Option<String>,
    /// Snapshot hash of the source environment at export time.
    pub schema_hash: String,
    pub selections: Vec<Selection>,
    /// Definitions of every collection that has items in the bundle (compatibility check).
    pub collections: Vec<Collection>,
    /// Items per collection. All column fields, system user fields excluded.
    pub items: BTreeMap<String, Vec<Item>>,
    /// Items whose owned children (o2m/m2m/m2a) are fully contained in the bundle.
    /// Only these parents are eligible for `prune` on import.
    #[serde(default)]
    pub complete: BTreeMap<String, Vec<Uuid>>,
    #[serde(default)]
    pub folders: Vec<Item>,
    #[serde(default)]
    pub files: Vec<Item>,
}

impl Bundle {
    pub fn item_count(&self) -> usize {
        self.items.values().map(|v| v.len()).sum()
    }
    pub fn file_ids(&self) -> Vec<Uuid> {
        self.files
            .iter()
            .filter_map(|f| f.get("id").and_then(|v| v.as_str()).and_then(|s| Uuid::parse_str(s).ok()))
            .collect()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    /// Create missing items, overwrite differing ones.
    #[default]
    Overwrite,
    /// Like overwrite, but an item modified in the target since the last sync is a conflict.
    Safe,
    /// Only create missing items, never touch existing ones.
    InsertOnly,
}

impl std::str::FromStr for Strategy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "overwrite" => Ok(Strategy::Overwrite),
            "safe" => Ok(Strategy::Safe),
            "insert-only" => Ok(Strategy::InsertOnly),
            other => Err(format!("unknown strategy `{other}` (overwrite | safe | insert-only)")),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ImportOptions {
    #[serde(default)]
    pub strategy: Strategy,
    /// Delete children in the target that no longer exist in the source (complete parents only).
    #[serde(default)]
    pub prune: bool,
    /// Run everything in a transaction and roll back: the report is the diff.
    #[serde(default)]
    pub dry_run: bool,
    /// Ignore bundle fields missing in the target schema instead of failing.
    #[serde(default)]
    pub ignore_missing_fields: bool,
    /// Apply even if conflicts were detected (conflicting items are overwritten).
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemAction {
    Create,
    Update,
    Unchanged,
    Skip,
    Conflict,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldChange {
    pub field: String,
    pub before: Value,
    pub after: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemResult {
    pub collection: String,
    pub id: Uuid,
    /// Id in the source environment when it was remapped through the natural key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<Uuid>,
    pub action: ItemAction,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<FieldChange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CollectionSummary {
    pub create: usize,
    pub update: usize,
    pub unchanged: usize,
    pub skip: usize,
    pub conflict: usize,
    pub delete: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ImportReport {
    pub ok: bool,
    pub dry_run: bool,
    pub committed: bool,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    pub summary: BTreeMap<String, CollectionSummary>,
    /// Every non-unchanged item (unchanged ones are only counted).
    pub items: Vec<ItemResult>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl ImportReport {
    pub fn push(&mut self, r: ItemResult) {
        let s = self.summary.entry(r.collection.clone()).or_default();
        match r.action {
            ItemAction::Create => s.create += 1,
            ItemAction::Update => s.update += 1,
            ItemAction::Unchanged => s.unchanged += 1,
            ItemAction::Skip => s.skip += 1,
            ItemAction::Conflict => s.conflict += 1,
            ItemAction::Delete => s.delete += 1,
        }
        if r.action != ItemAction::Unchanged {
            self.items.push(r);
        }
    }

    pub fn conflicts(&self) -> usize {
        self.summary.values().map(|s| s.conflict).sum()
    }

    pub fn totals(&self) -> CollectionSummary {
        let mut t = CollectionSummary::default();
        for s in self.summary.values() {
            t.create += s.create;
            t.update += s.update;
            t.unchanged += s.unchanged;
            t.skip += s.skip;
            t.conflict += s.conflict;
            t.delete += s.delete;
        }
        t
    }
}

/// Field-level differences between two items (keys of `after` only; `ignore` excluded).
pub fn item_changes(before: &Item, after: &Item, ignore: &[&str]) -> Vec<FieldChange> {
    let mut out = vec![];
    let mut keys: Vec<&String> = after.keys().collect();
    keys.sort();
    for k in keys {
        if ignore.contains(&k.as_str()) {
            continue;
        }
        let a = &after[k.as_str()];
        let b = before.get(k.as_str()).unwrap_or(&Value::Null);
        if !values_equal(b, a) {
            out.push(FieldChange { field: k.clone(), before: b.clone(), after: a.clone() });
        }
    }
    out
}

/// Equality tolerant to representation differences coming from different sources
/// (e.g. `1.50` vs `1.5`, timestamps with different offsets are NOT normalised).
pub fn values_equal(a: &Value, b: &Value) -> bool {
    if a == b {
        return true;
    }
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::String(x), Value::String(y)) => match (DateTime::parse_from_rfc3339(x), DateTime::parse_from_rfc3339(y)) {
            (Ok(dx), Ok(dy)) => dx == dy,
            _ => false,
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn selection_syntax() {
        let s = Selection::parse("pages").unwrap();
        assert_eq!(s.collection, "pages");
        let s = Selection::parse(r#"pages:{"slug":{"_eq":"home"}}"#).unwrap();
        assert_eq!(s.filter, Some(json!({"slug": {"_eq": "home"}})));
        let s = Selection::parse("pages:id=0199a000-0000-7000-8000-000000000001").unwrap();
        assert_eq!(s.ids.len(), 1);
        assert!(Selection::parse("pages:nonsense").is_err());
    }

    #[test]
    fn changes_and_timestamps() {
        let a: Item = serde_json::from_value(json!({"t": "x", "d": "2026-01-01T10:00:00+00:00", "n": 1.5})).unwrap();
        let b: Item = serde_json::from_value(json!({"t": "y", "d": "2026-01-01T12:00:00+02:00", "n": 1.50})).unwrap();
        let ch = item_changes(&a, &b, &[]);
        assert_eq!(ch.len(), 1);
        assert_eq!(ch[0].field, "t");
    }
}
