//! Filter / sort / search compilation to parameterised SQL.
//!
//! Every value is bound as text and cast explicitly (`CAST($n AS <type>)`): the
//! cast type comes from the schema, never from user input, and identifiers are
//! validated identifiers quoted with [`crate::ddl::q`]. Nothing user-provided is
//! ever concatenated into SQL.

use ferro_core::filter::is_condition;
use ferro_core::{Field, FieldType};
use serde_json::Value;

use crate::ddl::q;
use crate::schema::{CollectionInfo, Schema};
use crate::{FerroError, Result};

#[derive(Debug, Default)]
pub struct Sql {
    pub params: Vec<Option<String>>,
    aliases: usize,
}

impl Sql {
    pub fn new() -> Self {
        Sql::default()
    }

    /// Bind a parameter, returns its placeholder (`$n`).
    pub fn bind(&mut self, v: Option<String>) -> String {
        self.params.push(v);
        format!("${}", self.params.len())
    }

    pub fn bind_cast(&mut self, v: Option<String>, ty: &str) -> String {
        let p = self.bind(v);
        format!("CAST({p} AS {ty})")
    }

    pub fn alias(&mut self) -> String {
        self.aliases += 1;
        format!("t{}", self.aliases)
    }
}

/// JSON value -> text parameter (None = SQL NULL).
pub fn to_param(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        other => Some(other.to_string()),
    }
}

/// Value to bind for a column: json columns receive the JSON text of any value.
pub fn column_param(f: &Field, v: &Value) -> Option<String> {
    if f.ty == FieldType::Json {
        match v {
            Value::Null => None,
            other => Some(other.to_string()),
        }
    } else {
        to_param(v)
    }
}

/// PostgreSQL array literal (`{"a","b",NULL}`) for `= ANY(CAST($n AS type[]))`.
pub fn pg_array_literal(items: &[Option<String>]) -> String {
    let mut out = String::from("{");
    for (i, it) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match it {
            None => out.push_str("NULL"),
            Some(s) => {
                out.push('"');
                for c in s.chars() {
                    if c == '"' || c == '\\' {
                        out.push('\\');
                    }
                    out.push(c);
                }
                out.push('"');
            }
        }
    }
    out.push('}');
    out
}

fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Compile a filter object to a SQL boolean expression on `alias`.
pub fn compile_filter(schema: &Schema, info: &CollectionInfo, alias: &str, filter: &Value, sql: &mut Sql) -> Result<String> {
    let Some(obj) = filter.as_object() else {
        if filter.is_null() {
            return Ok("TRUE".into());
        }
        return Err(FerroError::invalid("filter must be an object"));
    };
    if obj.is_empty() {
        return Ok("TRUE".into());
    }
    let mut parts = Vec::with_capacity(obj.len());
    for (key, cond) in obj {
        match key.as_str() {
            "_and" | "_or" => {
                let list = cond
                    .as_array()
                    .ok_or_else(|| FerroError::invalid(format!("{key} expects an array")))?;
                if list.is_empty() {
                    parts.push("TRUE".to_string());
                    continue;
                }
                let mut sub = vec![];
                for f in list {
                    sub.push(compile_filter(schema, info, alias, f, sql)?);
                }
                let joiner = if key == "_and" { " AND " } else { " OR " };
                parts.push(format!("({})", sub.join(joiner)));
            }
            field_name => {
                let field = info.require_field(field_name)?;
                parts.push(compile_field(schema, info, alias, field, cond, sql)?);
            }
        }
    }
    Ok(parts.join(" AND "))
}

fn compile_field(schema: &Schema, info: &CollectionInfo, alias: &str, field: &Field, cond: &Value, sql: &mut Sql) -> Result<String> {
    // o2m-like: { blocks: { _some: {...} } } | { blocks: { _none: {...} } } | { blocks: {...} } (= _some)
    if field.ty.is_alias() {
        let related_name = field
            .related_collection()
            .ok_or_else(|| FerroError::Internal(format!("{}.{}: missing relation", info.name, field.name)))?;
        let ff = field
            .foreign_field()
            .ok_or_else(|| FerroError::Internal(format!("{}.{}: missing foreign_field", info.name, field.name)))?;
        let related = schema.collection(related_name)?;
        let (negate, inner) = match cond.as_object() {
            Some(m) if m.len() == 1 && m.contains_key("_some") => (false, &m["_some"]),
            Some(m) if m.len() == 1 && m.contains_key("_none") => (true, &m["_none"]),
            Some(_) => (false, cond),
            None => return Err(FerroError::invalid(format!("filter on `{}` must be an object", field.name))),
        };
        let a2 = sql.alias();
        let inner_sql = compile_filter(schema, &related, &a2, inner, sql)?;
        let sub = format!(
            "SELECT {a2}.{ff} FROM {} AS {a2} WHERE {a2}.{ff} IS NOT NULL AND ({inner_sql})",
            q(&related.name),
            ff = q(ff)
        );
        return Ok(if negate {
            format!("{alias}.\"id\" NOT IN ({sub})")
        } else {
            format!("{alias}.\"id\" IN ({sub})")
        });
    }

    // m2o nested: { author: { name: { _eq: .. } } }
    if field.ty.is_m2o_like() && cond.is_object() && !is_condition(cond) {
        let related_name = field
            .related_collection()
            .ok_or_else(|| FerroError::Internal(format!("{}.{}: missing relation", info.name, field.name)))?;
        let related = schema.collection(related_name)?;
        let a2 = sql.alias();
        let inner_sql = compile_filter(schema, &related, &a2, cond, sql)?;
        return Ok(format!(
            "{alias}.{col} IN (SELECT {a2}.\"id\" FROM {} AS {a2} WHERE {inner_sql})",
            q(&related.name),
            col = q(&field.name)
        ));
    }

    let col = format!("{alias}.{}", q(&field.name));
    let ty = field.cast_type();
    let ops = match cond {
        Value::Object(m) if is_condition(cond) => m.clone(),
        Value::Object(_) => {
            return Err(FerroError::invalid(format!("invalid filter on `{}`", field.name)));
        }
        // shorthand { field: value }
        other => {
            let mut m = serde_json::Map::new();
            m.insert("_eq".into(), other.clone());
            m
        }
    };
    let mut parts = vec![];
    for (op, operand) in &ops {
        parts.push(compile_op(&col, &ty, field, op, operand, sql)?);
    }
    Ok(parts.join(" AND "))
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::String(s) => s == "true" || s == "1",
        Value::Number(n) => n.as_f64() != Some(0.0),
        _ => false,
    }
}

fn compile_op(col: &str, ty: &str, field: &Field, op: &str, operand: &Value, sql: &mut Sql) -> Result<String> {
    let text_col = format!("CAST({col} AS text)");
    let like = |sql: &mut Sql, pattern: String, insensitive: bool, negate: bool| -> String {
        let p = sql.bind(Some(pattern));
        let kw = if insensitive { "ILIKE" } else { "LIKE" };
        let not = if negate { "NOT " } else { "" };
        format!("{not}({text_col} {kw} {p})")
    };
    let text_operand = || -> Result<String> {
        to_param(operand).ok_or_else(|| FerroError::invalid(format!("{op} on `{}` needs a value", field.name)))
    };
    let list = |sql: &mut Sql| -> Result<String> {
        let arr = operand
            .as_array()
            .ok_or_else(|| FerroError::invalid(format!("{op} on `{}` expects an array", field.name)))?;
        let items: Vec<Option<String>> = arr.iter().map(|v| column_param(field, v)).collect();
        let p = sql.bind(Some(pg_array_literal(&items)));
        Ok(format!("{col} = ANY(CAST({p} AS {ty}[]))"))
    };
    Ok(match op {
        "_eq" if operand.is_null() => format!("{col} IS NULL"),
        "_neq" if operand.is_null() => format!("{col} IS NOT NULL"),
        "_eq" => format!("{col} = {}", sql.bind_cast(column_param(field, operand), ty)),
        "_neq" => format!("{col} IS DISTINCT FROM {}", sql.bind_cast(column_param(field, operand), ty)),
        "_lt" => format!("{col} < {}", sql.bind_cast(to_param(operand), ty)),
        "_lte" => format!("{col} <= {}", sql.bind_cast(to_param(operand), ty)),
        "_gt" => format!("{col} > {}", sql.bind_cast(to_param(operand), ty)),
        "_gte" => format!("{col} >= {}", sql.bind_cast(to_param(operand), ty)),
        "_in" => {
            if operand.as_array().map(|a| a.is_empty()).unwrap_or(false) {
                "FALSE".into()
            } else {
                list(sql)?
            }
        }
        "_nin" => {
            if operand.as_array().map(|a| a.is_empty()).unwrap_or(false) {
                "TRUE".into()
            } else {
                format!("({col} IS NULL OR NOT ({}))", list(sql)?)
            }
        }
        "_null" => {
            if truthy(operand) {
                format!("{col} IS NULL")
            } else {
                format!("{col} IS NOT NULL")
            }
        }
        "_nnull" => {
            if truthy(operand) {
                format!("{col} IS NOT NULL")
            } else {
                format!("{col} IS NULL")
            }
        }
        "_empty" | "_nempty" => {
            let empty = format!("({col} IS NULL OR {text_col} = '' OR {text_col} = '[]' OR {text_col} = '{{}}')");
            if truthy(operand) == (op == "_empty") {
                empty
            } else {
                format!("NOT {empty}")
            }
        }
        "_contains" | "_ncontains" if field.ty == FieldType::Json => {
            let p = sql.bind_cast(column_param(field, &Value::Array(vec![operand.clone()])), "jsonb");
            let p2 = sql.bind_cast(column_param(field, operand), "jsonb");
            let expr = format!("({col} @> {p} OR {col} @> {p2})");
            if op == "_contains" {
                expr
            } else {
                format!("NOT {expr}")
            }
        }
        "_contains" => like(sql, format!("%{}%", escape_like(&text_operand()?)), false, false),
        "_ncontains" => like(sql, format!("%{}%", escape_like(&text_operand()?)), false, true),
        "_icontains" => like(sql, format!("%{}%", escape_like(&text_operand()?)), true, false),
        "_nicontains" => like(sql, format!("%{}%", escape_like(&text_operand()?)), true, true),
        "_starts_with" => like(sql, format!("{}%", escape_like(&text_operand()?)), false, false),
        "_nstarts_with" => like(sql, format!("{}%", escape_like(&text_operand()?)), false, true),
        "_istarts_with" => like(sql, format!("{}%", escape_like(&text_operand()?)), true, false),
        "_ends_with" => like(sql, format!("%{}", escape_like(&text_operand()?)), false, false),
        "_nends_with" => like(sql, format!("%{}", escape_like(&text_operand()?)), false, true),
        "_iends_with" => like(sql, format!("%{}", escape_like(&text_operand()?)), true, false),
        "_between" | "_nbetween" => {
            let Some([lo, hi]) = operand.as_array().map(|a| a.as_slice()).and_then(|s| <&[Value; 2]>::try_from(s).ok()) else {
                return Err(FerroError::invalid(format!("{op} expects [from, to]")));
            };
            let lo = sql.bind_cast(to_param(lo), ty);
            let hi = sql.bind_cast(to_param(hi), ty);
            let not = if op == "_nbetween" { "NOT " } else { "" };
            format!("{col} {not}BETWEEN {lo} AND {hi}")
        }
        other => return Err(FerroError::invalid(format!("unsupported filter operator `{other}`"))),
    })
}

/// `search=` → OR of ILIKE over textual columns (+ exact id match when it is a uuid).
pub fn compile_search(info: &CollectionInfo, alias: &str, term: &str, sql: &mut Sql, allowed: &dyn Fn(&str) -> bool) -> String {
    let mut parts = vec![];
    let pattern = format!("%{}%", escape_like(term));
    for f in info.columns().filter(|f| f.ty.is_textual() && allowed(&f.name)) {
        let p = sql.bind(Some(pattern.clone()));
        parts.push(format!("{alias}.{} ILIKE {p}", q(&f.name)));
    }
    if uuid::Uuid::parse_str(term).is_ok() {
        let p = sql.bind_cast(Some(term.to_string()), "uuid");
        parts.push(format!("{alias}.\"id\" = {p}"));
    }
    if parts.is_empty() {
        "FALSE".into()
    } else {
        format!("({})", parts.join(" OR "))
    }
}

/// `sort=-date_created,title` → ORDER BY clause (columns only).
pub fn compile_sort(info: &CollectionInfo, alias: &str, sort: &[String]) -> Result<String> {
    let mut parts = vec![];
    for s in sort {
        let s = s.trim();
        if s.is_empty() {
            continue;
        }
        let (name, dir) = match s.strip_prefix('-') {
            Some(n) => (n, "DESC"),
            None => (s, "ASC"),
        };
        let f = info.require_field(name)?;
        if !f.has_column() {
            return Err(FerroError::invalid(format!("cannot sort by alias field `{name}`")));
        }
        parts.push(format!("{alias}.{} {dir} NULLS LAST", q(name)));
    }
    if parts.is_empty() {
        let default = info
            .def
            .meta
            .sort_field
            .as_deref()
            .filter(|n| info.field(n).map(|f| f.has_column()).unwrap_or(false))
            .unwrap_or("id");
        parts.push(format!("{alias}.{} ASC", q(default)));
    }
    // stable pagination
    if !parts.iter().any(|p| p.starts_with(&format!("{alias}.\"id\" "))) {
        parts.push(format!("{alias}.\"id\" ASC"));
    }
    Ok(parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferro_core::Snapshot;
    use serde_json::json;

    fn schema() -> Schema {
        let s: Snapshot = serde_yaml::from_str(include_str!("../../ferro-core/tests/fixtures/pages.yaml")).unwrap();
        Schema::new(s)
    }

    #[test]
    fn compiles_nested_filters() {
        let schema = schema();
        let pages = schema.collection("pages").unwrap();
        let mut sql = Sql::new();
        let out = compile_filter(
            &schema,
            &pages,
            "t0",
            &json!({
                "_or": [{"status": {"_eq": "published"}}, {"title": {"_icontains": "50%"}}],
                "category": {"code": {"_in": ["a", "b\"c"]}},
                "blocks": {"_none": {"collection": {"_eq": "block_hero"}}}
            }),
            &mut sql,
        )
        .unwrap();
        assert!(out.contains("t0.\"status\" = CAST($1 AS text)"), "{out}");
        assert!(out.contains("ILIKE $2"), "{out}");
        assert_eq!(sql.params[1].as_deref(), Some("%50\\%%"));
        // keys are visited in order: _or, blocks, category
        assert!(out.contains("t0.\"id\" NOT IN (SELECT t1.\"pages_id\" FROM \"pages_blocks\" AS t1"), "{out}");
        assert_eq!(sql.params[2].as_deref(), Some("block_hero"));
        assert!(out.contains("t0.\"category\" IN (SELECT t2.\"id\" FROM \"categories\" AS t2"), "{out}");
        assert_eq!(sql.params[3].as_deref(), Some("{\"a\",\"b\\\"c\"}"));
    }

    #[test]
    fn rejects_unknown_fields_and_operators() {
        let schema = schema();
        let pages = schema.collection("pages").unwrap();
        let mut sql = Sql::new();
        assert!(compile_filter(&schema, &pages, "t0", &json!({"nope": {"_eq": 1}}), &mut sql).is_err());
        assert!(compile_filter(&schema, &pages, "t0", &json!({"title": {"_regex": "x"}}), &mut sql).is_err());
    }
}
