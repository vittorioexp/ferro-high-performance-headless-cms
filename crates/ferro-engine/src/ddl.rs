//! Plan -> ordered PostgreSQL DDL.
//!
//! Every statement runs inside a single transaction (PostgreSQL has transactional
//! DDL): a plan is applied completely or not at all.
//!
//! Constraint names are derived from `table.column` with a short hash so they are
//! identical in every environment and can be dropped/renamed deterministically.
//! Foreign keys are `DEFERRABLE` so that data imports can run with deferred
//! constraints (cycles, arbitrary insert order).

use std::collections::BTreeMap;

use ferro_core::diff::{Change, Plan};
use ferro_core::hash::short_hash;
use ferro_core::{Collection, Field, FieldType, Snapshot};
use serde_json::Value;

use crate::{FerroError, Result};

pub fn q(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

pub fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub fn fk_name(table: &str, col: &str) -> String {
    format!("fk_{}", short_hash(&format!("{table}.{col}")))
}
pub fn uq_name(table: &str, col: &str) -> String {
    format!("uq_{}", short_hash(&format!("{table}.{col}")))
}
pub fn ix_name(table: &str, col: &str) -> String {
    format!("ix_{}", short_hash(&format!("{table}.{col}")))
}

fn wants_fk(f: &Field) -> bool {
    f.ty.is_m2o_like() && f.related_collection().is_some()
}
fn wants_unique(f: &Field) -> bool {
    f.has_column() && f.unique
}
/// m2o / m2a_item columns are always indexed (joins, o2m lookups).
fn wants_index(f: &Field) -> bool {
    f.has_column() && !f.unique && (f.indexed || f.ty.is_m2o_like() || f.ty == FieldType::M2aItem)
}

fn default_sql(f: &Field) -> Option<String> {
    let v = f.default.as_ref()?;
    let ty = f.pg_type()?;
    let literal = match v {
        Value::Null => return None,
        Value::String(s) if s == "$NOW" => return Some("now()".into()),
        Value::String(s) => lit(s),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => lit(&n.to_string()),
        other => lit(&other.to_string()),
    };
    Some(format!("CAST({literal} AS {ty})"))
}

fn column_def(f: &Field) -> Option<String> {
    let ty = f.pg_type()?;
    let mut s = format!("{} {}", q(&f.name), ty);
    if let Some(d) = default_sql(f) {
        s.push_str(&format!(" DEFAULT {d}"));
    }
    if f.required {
        s.push_str(" NOT NULL");
    }
    Some(s)
}

fn add_fk(table: &str, f: &Field) -> Option<String> {
    if !wants_fk(f) {
        return None;
    }
    let related = f.related_collection()?;
    Some(format!(
        "ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} (\"id\") ON DELETE {} DEFERRABLE INITIALLY IMMEDIATE",
        q(table),
        q(&fk_name(table, &f.name)),
        q(&f.name),
        q(related),
        f.on_delete().sql()
    ))
}

fn add_constraints(table: &str, f: &Field, out: &mut Vec<String>) {
    if let Some(s) = add_fk(table, f) {
        out.push(s);
    }
    if wants_unique(f) {
        out.push(format!(
            "ALTER TABLE {} ADD CONSTRAINT {} UNIQUE ({})",
            q(table),
            q(&uq_name(table, &f.name)),
            q(&f.name)
        ));
    }
    if wants_index(f) {
        out.push(format!("CREATE INDEX {} ON {} ({})", q(&ix_name(table, &f.name)), q(table), q(&f.name)));
    }
}

fn drop_constraints(table: &str, f: &Field, fk: bool, uq: bool, ix: bool, out: &mut Vec<String>) {
    if fk && wants_fk(f) {
        out.push(format!("ALTER TABLE {} DROP CONSTRAINT IF EXISTS {}", q(table), q(&fk_name(table, &f.name))));
    }
    if uq && wants_unique(f) {
        out.push(format!("ALTER TABLE {} DROP CONSTRAINT IF EXISTS {}", q(table), q(&uq_name(table, &f.name))));
    }
    if ix && wants_index(f) {
        out.push(format!("DROP INDEX IF EXISTS {}", q(&ix_name(table, &f.name))));
    }
}

/// Rename the constraints of `f` from (old_table, old_col) to (new_table, new_col).
fn rename_constraints(old_table: &str, new_table: &str, old_col: &str, new_col: &str, f: &Field, out: &mut Vec<String>) {
    if wants_fk(f) {
        out.push(format!(
            "ALTER TABLE {} RENAME CONSTRAINT {} TO {}",
            q(new_table),
            q(&fk_name(old_table, old_col)),
            q(&fk_name(new_table, new_col))
        ));
    }
    if wants_unique(f) {
        out.push(format!(
            "ALTER TABLE {} RENAME CONSTRAINT {} TO {}",
            q(new_table),
            q(&uq_name(old_table, old_col)),
            q(&uq_name(new_table, new_col))
        ));
    }
    if wants_index(f) {
        out.push(format!(
            "ALTER INDEX IF EXISTS {} RENAME TO {}",
            q(&ix_name(old_table, old_col)),
            q(&ix_name(new_table, new_col))
        ));
    }
}

#[derive(Default)]
struct Phases {
    rename_tables: Vec<String>,
    rename_columns: Vec<String>,
    drop_constraints: Vec<String>,
    drop_columns: Vec<String>,
    drop_tables: Vec<String>,
    create_tables: Vec<String>,
    add_columns: Vec<String>,
    alter_columns: Vec<String>,
    add_constraints: Vec<String>,
}

impl Phases {
    fn into_vec(self) -> Vec<String> {
        let mut out = vec![];
        out.extend(self.rename_tables);
        out.extend(self.rename_columns);
        out.extend(self.drop_constraints);
        out.extend(self.drop_columns);
        out.extend(self.drop_tables);
        out.extend(self.create_tables);
        out.extend(self.add_columns);
        out.extend(self.alter_columns);
        out.extend(self.add_constraints);
        out
    }

    fn add_field(&mut self, table: &str, f: &Field) {
        if let Some(def) = column_def(f) {
            self.add_columns.push(format!("ALTER TABLE {} ADD COLUMN {}", q(table), def));
            add_constraints(table, f, &mut self.add_constraints);
        }
    }
}

/// Generate the SQL for `plan` (computed as `diff(current, _)`).
pub fn plan_to_sql(plan: &Plan, current: &Snapshot) -> Result<Vec<String>> {
    let mut p = Phases::default();

    // new collection name -> old collection name
    let renamed: BTreeMap<&str, &str> = plan
        .changes
        .iter()
        .filter_map(|c| match c {
            Change::RenameCollection { from, to } => Some((to.as_str(), from.as_str())),
            _ => None,
        })
        .collect();

    for change in &plan.changes {
        match change {
            Change::RenameCollection { from, to } => {
                p.rename_tables.push(format!("ALTER TABLE {} RENAME TO {}", q(from), q(to)));
                let cur = current
                    .collection(from)
                    .ok_or_else(|| FerroError::Internal(format!("unknown collection `{from}`")))?;
                for f in cur.resolved_fields() {
                    rename_constraints(from, to, &f.name, &f.name, &f, &mut p.rename_tables);
                }
            }
            Change::CreateCollection { collection } => {
                p.create_tables.push(format!(
                    "CREATE TABLE {} (\"id\" uuid PRIMARY KEY DEFAULT gen_random_uuid())",
                    q(&collection.name)
                ));
                for f in collection.resolved_fields().iter().filter(|f| f.name != "id") {
                    p.add_field(&collection.name, f);
                }
            }
            Change::DropCollection { collection } => {
                p.drop_tables.push(format!("DROP TABLE IF EXISTS {} CASCADE", q(&collection.name)));
            }
            Change::UpdateCollection { name, before, after } => {
                if before.system_fields != after.system_fields {
                    for f in ferro_core::system_fields() {
                        if after.system_fields {
                            p.add_field(name, &f);
                        } else {
                            p.drop_columns.push(format!("ALTER TABLE {} DROP COLUMN IF EXISTS {}", q(name), q(&f.name)));
                        }
                    }
                }
            }
            Change::AddField { collection, field } => p.add_field(collection, field),
            Change::DropField { collection, field } => {
                if field.has_column() {
                    p.drop_columns.push(format!(
                        "ALTER TABLE {} DROP COLUMN IF EXISTS {} CASCADE",
                        q(collection),
                        q(&field.name)
                    ));
                }
            }
            Change::RenameField { collection, from, to } => {
                let cur = lookup_current(current, &renamed, collection)?;
                let Some(f) = cur.field(from) else {
                    return Err(FerroError::Internal(format!("unknown field `{collection}.{from}`")));
                };
                if f.has_column() {
                    p.rename_columns.push(format!(
                        "ALTER TABLE {} RENAME COLUMN {} TO {}",
                        q(collection),
                        q(from),
                        q(to)
                    ));
                    rename_constraints(collection, collection, from, to, f, &mut p.rename_columns);
                }
            }
            Change::AlterField { collection, before, after } => {
                alter_field(&mut p, collection, before, after);
            }
            Change::UpsertRole { .. }
            | Change::DeleteRole { .. }
            | Change::UpsertPolicy { .. }
            | Change::DeletePolicy { .. }
            | Change::UpsertFlow { .. }
            | Change::DeleteFlow { .. } => {}
        }
    }
    Ok(p.into_vec())
}

fn lookup_current<'a>(current: &'a Snapshot, renamed: &BTreeMap<&str, &str>, name: &str) -> Result<&'a Collection> {
    let old = renamed.get(name).copied().unwrap_or(name);
    current
        .collection(old)
        .ok_or_else(|| FerroError::Internal(format!("plan references unknown collection `{old}`")))
}

fn alter_field(p: &mut Phases, table: &str, before: &Field, after: &Field) {
    if !after.has_column() {
        return; // alias: metadata only
    }
    let col = q(&after.name);
    let t = q(table);
    let type_changed = before.pg_type() != after.pg_type();
    let fk_changed = wants_fk(before) != wants_fk(after)
        || (wants_fk(after) && (before.related_collection() != after.related_collection() || before.on_delete() != after.on_delete()))
        || type_changed;
    let uq_changed = wants_unique(before) != wants_unique(after);
    let ix_changed = wants_index(before) != wants_index(after);

    drop_constraints(table, before, fk_changed, uq_changed, ix_changed, &mut p.drop_constraints);

    if type_changed {
        let ty = after.pg_type().unwrap_or_default();
        p.alter_columns.push(format!("ALTER TABLE {t} ALTER COLUMN {col} DROP DEFAULT"));
        p.alter_columns.push(format!("ALTER TABLE {t} ALTER COLUMN {col} TYPE {ty} USING {col}::{ty}"));
    }
    if type_changed || before.default != after.default {
        match default_sql(after) {
            Some(d) => p.alter_columns.push(format!("ALTER TABLE {t} ALTER COLUMN {col} SET DEFAULT {d}")),
            None if !type_changed => p.alter_columns.push(format!("ALTER TABLE {t} ALTER COLUMN {col} DROP DEFAULT")),
            None => {}
        }
    }
    if before.required != after.required {
        let action = if after.required { "SET" } else { "DROP" };
        p.alter_columns.push(format!("ALTER TABLE {t} ALTER COLUMN {col} {action} NOT NULL"));
    }

    if fk_changed {
        if let Some(s) = add_fk(table, after) {
            p.add_constraints.push(s);
        }
    }
    if uq_changed && wants_unique(after) {
        p.add_constraints.push(format!(
            "ALTER TABLE {t} ADD CONSTRAINT {} UNIQUE ({col})",
            q(&uq_name(table, &after.name))
        ));
    }
    if ix_changed && wants_index(after) {
        p.add_constraints.push(format!("CREATE INDEX {} ON {t} ({col})", q(&ix_name(table, &after.name))));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferro_core::diff::diff;

    #[test]
    fn create_from_fixture_orders_tables_before_constraints() {
        let s: Snapshot = serde_yaml::from_str(include_str!("../../ferro-core/tests/fixtures/pages.yaml")).unwrap();
        let plan = diff(&Snapshot::default(), &s);
        let sql = plan_to_sql(&plan, &Snapshot::default()).unwrap();
        let first_fk = sql.iter().position(|s| s.contains("FOREIGN KEY")).unwrap();
        let last_create = sql.iter().rposition(|s| s.starts_with("CREATE TABLE")).unwrap();
        assert!(last_create < first_fk);
        assert!(sql.iter().any(|s| s.contains("\"slug\" varchar(255) NOT NULL")));
        assert!(sql.iter().any(|s| s.contains("REFERENCES \"pages\" (\"id\") ON DELETE CASCADE DEFERRABLE")));
        assert!(sql.iter().any(|s| s.contains("DEFAULT CAST('draft' AS varchar(255))")));
    }

    #[test]
    fn rename_keeps_constraint_names_consistent() {
        let cur: Snapshot = serde_yaml::from_str(
            "collections:\n  - name: a\n    fields:\n      - { name: code, type: string, unique: true }\n",
        )
        .unwrap();
        let des: Snapshot = serde_yaml::from_str(
            "collections:\n  - name: b\n    renamed_from: a\n    fields:\n      - { name: key, renamed_from: code, type: string, unique: true }\n",
        )
        .unwrap();
        let plan = diff(&cur, &des);
        let sql = plan_to_sql(&plan, &cur).unwrap();
        assert_eq!(sql[0], "ALTER TABLE \"a\" RENAME TO \"b\"");
        assert!(sql.contains(&format!(
            "ALTER TABLE \"b\" RENAME CONSTRAINT \"{}\" TO \"{}\"",
            uq_name("a", "code"),
            uq_name("b", "code")
        )));
        assert!(sql.contains(&format!(
            "ALTER TABLE \"b\" RENAME CONSTRAINT \"{}\" TO \"{}\"",
            uq_name("b", "code"),
            uq_name("b", "key")
        )));
    }
}
