use std::collections::BTreeSet;

use crate::model::*;
use crate::system::{self, is_relatable_system_collection};

/// Structural validation of a snapshot. Returns every problem found (empty = valid).
pub fn validate_snapshot(s: &Snapshot) -> Vec<String> {
    let mut errors = Vec::new();
    let mut names = BTreeSet::new();
    let system = system::system_collections();
    let lookup = |name: &str| lookup_collection(s, &system, name);

    for c in &s.collections {
        let ctx = format!("collection `{}`", c.name);
        if !is_valid_identifier(&c.name) {
            errors.push(format!("{ctx}: invalid name (use letters, digits, underscore; max 63 chars)"));
        }
        if c.name.starts_with(SYSTEM_PREFIX) {
            errors.push(format!("{ctx}: the prefix `{SYSTEM_PREFIX}` is reserved"));
        }
        if !names.insert(c.name.as_str()) {
            errors.push(format!("{ctx}: duplicated"));
        }

        let mut field_names = BTreeSet::new();
        for f in &c.fields {
            let fctx = format!("{ctx}, field `{}`", f.name);
            if !is_valid_identifier(&f.name) {
                errors.push(format!("{fctx}: invalid name"));
            }
            if f.name == PRIMARY_KEY {
                errors.push(format!("{fctx}: `id` is implicit (uuid primary key) and cannot be declared"));
            }
            if c.system_fields && SYSTEM_FIELDS.contains(&f.name.as_str()) {
                errors.push(format!("{fctx}: reserved system field (set `system_fields: false` to declare it yourself)"));
            }
            if !field_names.insert(f.name.as_str()) {
                errors.push(format!("{fctx}: duplicated"));
            }
            if f.ty.is_alias() && (f.required || f.unique || f.indexed || f.default.is_some()) {
                errors.push(format!("{fctx}: alias fields ({}) have no column: required/unique/indexed/default not allowed", f.ty.as_str()));
            }
            if f.ty == FieldType::String {
                if let Some(l) = f.max_length {
                    if l == 0 || l > 10_485_760 {
                        errors.push(format!("{fctx}: max_length out of range"));
                    }
                }
            }
            if f.ty.is_m2o_like() && f.required && f.on_delete() == OnDelete::SetNull {
                errors.push(format!("{fctx}: required m2o cannot use on_delete: set_null (use cascade or restrict)"));
            }
            validate_relation(c, f, s, &system, &fctx, &mut errors);
        }

        for k in &c.sync.natural_key {
            match c.field(k) {
                Some(f) if f.has_column() => {}
                _ => errors.push(format!("{ctx}: natural_key field `{k}` must be a declared column field")),
            }
        }
        if let Some(sf) = &c.meta.sort_field {
            if c.field(sf).is_none() && !SYSTEM_FIELDS.contains(&sf.as_str()) && sf != PRIMARY_KEY {
                errors.push(format!("{ctx}: sort_field `{sf}` does not exist"));
            }
        }
        if let Some(af) = &c.meta.archive_field {
            if c.field(af).is_none() {
                errors.push(format!("{ctx}: archive_field `{af}` does not exist"));
            }
        }
    }

    let mut policy_ids = BTreeSet::new();
    for p in &s.policies {
        let ctx = format!("policy `{}` ({})", p.name, p.id);
        if !policy_ids.insert(p.id) {
            errors.push(format!("{ctx}: duplicated id"));
        }
        for perm in &p.permissions {
            if perm.collection == "*" {
                continue;
            }
            match lookup(&perm.collection) {
                None => errors.push(format!("{ctx}: permission on unknown collection `{}`", perm.collection)),
                Some(col) => {
                    let resolved = col.resolved_fields();
                    for fname in &perm.fields {
                        if fname != "*" && !resolved.iter().any(|f| &f.name == fname) {
                            errors.push(format!(
                                "{ctx}: permission {}:{} references unknown field `{fname}`",
                                perm.collection,
                                perm.action.as_str()
                            ));
                        }
                    }
                }
            }
        }
    }
    let mut role_ids = BTreeSet::new();
    for r in &s.roles {
        let ctx = format!("role `{}` ({})", r.name, r.id);
        if !role_ids.insert(r.id) {
            errors.push(format!("{ctx}: duplicated id"));
        }
        for pid in &r.policies {
            if !policy_ids.contains(pid) {
                errors.push(format!("{ctx}: unknown policy {pid}"));
            }
        }
    }
    errors.extend(crate::flow::validate_flows(s));
    errors
}

fn lookup_collection<'a>(s: &'a Snapshot, system: &'a [Collection], name: &str) -> Option<&'a Collection> {
    s.collection(name).or_else(|| system.iter().find(|c| c.name == name))
}

fn validate_relation(
    c: &Collection,
    f: &Field,
    s: &Snapshot,
    system: &[Collection],
    fctx: &str,
    errors: &mut Vec<String>,
) {
    let lookup = |name: &str| lookup_collection(s, system, name);
    let rel = f.relation.as_ref();
    match f.ty {
        FieldType::M2o | FieldType::File => {
            let Some(related) = f.related_collection() else {
                errors.push(format!("{fctx}: m2o requires relation.related"));
                return;
            };
            if f.ty == FieldType::File && related != system::FILES {
                errors.push(format!("{fctx}: file fields always point to {}", system::FILES));
            }
            if related.starts_with(SYSTEM_PREFIX) && !is_relatable_system_collection(related) {
                errors.push(format!("{fctx}: cannot relate to system collection `{related}`"));
            } else if lookup(related).is_none() {
                errors.push(format!("{fctx}: related collection `{related}` does not exist"));
            }
        }
        FieldType::O2m | FieldType::M2m | FieldType::M2a => {
            let Some(related_name) = f.related_collection() else {
                errors.push(format!("{fctx}: {} requires relation.related", f.ty.as_str()));
                return;
            };
            let Some(related) = lookup(related_name) else {
                errors.push(format!("{fctx}: related collection `{related_name}` does not exist"));
                return;
            };
            match f.foreign_field() {
                None => errors.push(format!("{fctx}: {} requires relation.foreign_field", f.ty.as_str())),
                Some(ff) => match related.field(ff) {
                    Some(back) if back.ty.is_m2o_like() && back.related_collection() == Some(c.name.as_str()) => {}
                    _ => errors.push(format!(
                        "{fctx}: `{related_name}.{ff}` must be an m2o field pointing to `{}`",
                        c.name
                    )),
                },
            }
            if let Some(sort) = rel.and_then(|r| r.sort_field.as_deref()) {
                if related.field(sort).is_none() {
                    errors.push(format!("{fctx}: sort_field `{related_name}.{sort}` does not exist"));
                }
            }
            if f.ty == FieldType::M2m {
                match rel.and_then(|r| r.junction_field.as_deref()) {
                    None => errors.push(format!("{fctx}: m2m requires relation.junction_field")),
                    Some(jf) => match related.field(jf) {
                        Some(j) if j.ty.is_m2o_like() => {}
                        _ => errors.push(format!("{fctx}: junction field `{related_name}.{jf}` must be m2o")),
                    },
                }
            }
            if f.ty == FieldType::M2a && !related.fields.iter().any(|x| x.ty == FieldType::M2aItem) {
                errors.push(format!("{fctx}: junction `{related_name}` has no m2a_item field"));
            }
        }
        FieldType::M2aItem => {
            let Some(r) = rel else {
                errors.push(format!("{fctx}: m2a_item requires relation.collection_field and relation.allowed"));
                return;
            };
            match r.collection_field.as_deref().and_then(|cf| c.field(cf)) {
                Some(cf) if cf.ty.is_textual() => {}
                _ => errors.push(format!("{fctx}: relation.collection_field must be a string field of `{}`", c.name)),
            }
            if r.allowed.is_empty() {
                errors.push(format!("{fctx}: relation.allowed cannot be empty"));
            }
            for a in &r.allowed {
                if lookup(a).is_none() {
                    errors.push(format!("{fctx}: allowed collection `{a}` does not exist"));
                }
            }
        }
        _ => {
            if rel.is_some() {
                errors.push(format!("{fctx}: `relation` only allowed on relational fields"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(yaml: &str) -> Snapshot {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn valid_m2a_model() {
        let s = snapshot(include_str!("../tests/fixtures/pages.yaml"));
        let errors = validate_snapshot(&s);
        assert!(errors.is_empty(), "{errors:#?}");
    }

    #[test]
    fn detects_broken_relations() {
        let s = snapshot(
            r#"
collections:
  - name: pages
    fields:
      - { name: author, type: m2o, relation: { related: people } }
      - { name: blocks, type: o2m, relation: { related: blocks, foreign_field: page } }
      - { name: id, type: uuid }
  - name: blocks
    fields:
      - { name: title, type: string }
"#,
        );
        let errors = validate_snapshot(&s);
        assert!(errors.iter().any(|e| e.contains("`people` does not exist")));
        assert!(errors.iter().any(|e| e.contains("blocks.page")));
        assert!(errors.iter().any(|e| e.contains("`id` is implicit")));
    }
}
