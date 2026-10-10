//! Translations: the dedicated `translations` field type and the guided set-up of everything it needs.
//!
//! A translated collection `pages` has an alias field `translations` (type `translations`) that points to a collection
//! `pages_translations` with one row per language and page. That collection holds the translatable fields, an m2o back to
//! the page and an m2o to a `languages` collection. [`scaffold`] builds the whole set in a single snapshot change, so it
//! goes through the same plan / dry-run / apply pipeline as every other model change.

use crate::model::*;

/// What to create. Everything but `collection` and `fields` has a sensible default.
#[derive(Debug, Clone, Default)]
pub struct TranslationsSpec {
    /// The collection that gets translations.
    pub collection: String,
    /// Name of the alias field on `collection` (default `translations`).
    pub field_name: Option<String>,
    /// The translations collection (default `<collection>_translations`).
    pub translations_collection: Option<String>,
    /// The languages collection, reused when it exists (default `languages`).
    pub languages_collection: Option<String>,
    /// The translatable fields: plain column fields (text, string, json, a file for a translated image, ...).
    pub fields: Vec<Field>,
}

/// The names a [`TranslationsSpec`] resolves to.
#[derive(Debug, Clone, PartialEq)]
pub struct Names {
    pub field: String,
    pub translations: String,
    pub languages: String,
    /// The m2o in the translations collection pointing back to the item.
    pub parent_field: String,
    /// The m2o in the translations collection pointing to the language.
    pub language_field: String,
}

impl TranslationsSpec {
    pub fn names(&self) -> Names {
        let languages = self.languages_collection.clone().unwrap_or_else(|| "languages".into());
        Names {
            field: self.field_name.clone().unwrap_or_else(|| "translations".into()),
            translations: self.translations_collection.clone().unwrap_or_else(|| format!("{}_translations", self.collection)),
            parent_field: format!("{}_id", self.collection),
            language_field: format!("{languages}_id"),
            languages,
        }
    }
}

/// The columns of a translations collection that carry content: everything except the link to the item, the link to the
/// language, system fields and aliases.
pub fn translatable_fields<'a>(translations: &'a Collection, f: &Field) -> Vec<&'a Field> {
    let (ff, lf) = (f.foreign_field().unwrap_or_default(), f.language_field().unwrap_or_default());
    translations
        .fields
        .iter()
        .filter(|x| x.has_column() && x.name != ff && x.name != lf && !(translations.system_fields && SYSTEM_FIELDS.contains(&x.name.as_str())))
        .collect()
}

/// The fields a collection gains when it is read with `lang`: the translatable fields of its `translations` fields that the
/// collection does not have itself (a field it has is already part of its type: `lang` replaces its value by the translation
/// when there is one). Each name appears once.
pub fn translated_fields<'a>(snapshot: &'a Snapshot, c: &'a Collection) -> Vec<&'a Field> {
    let mut out: Vec<&Field> = Vec::new();
    for tf in c.fields.iter().filter(|f| f.ty == FieldType::Translations) {
        let Some(related) = tf.related_collection().and_then(|r| snapshot.collection(r)) else { continue };
        for f in translatable_fields(related, tf) {
            let clashes = c.fields.iter().any(|x| x.name == f.name) || SYSTEM_FIELDS.contains(&f.name.as_str()) && c.system_fields;
            if !clashes && !out.iter().any(|x| x.name == f.name) {
                out.push(f);
            }
        }
    }
    out
}

/// The languages collection created when none exists: a unique, required `code` (the natural key, so languages keep their
/// identity across environments), a `name` and a text direction.
fn default_languages(name: &str) -> Collection {
    let mut c = Collection::new(name);
    c.sync.natural_key = vec!["code".into()];
    c.meta.icon = Some("translate".into());
    c.meta.display_template = Some("{{name}} ({{code}})".into());
    c.fields = vec![
        Field { required: true, unique: true, max_length: Some(32), ..Field::new("code", FieldType::String) },
        Field { required: true, ..Field::new("name", FieldType::String) },
        Field { default: Some(serde_json::json!("ltr")), max_length: Some(3), ..Field::new("direction", FieldType::String) },
    ];
    c
}

/// Returns `snapshot` plus the translations set-up, or every reason it cannot be built.
pub fn scaffold(snapshot: &Snapshot, spec: &TranslationsSpec) -> Result<Snapshot, Vec<String>> {
    let n = spec.names();
    let mut errors = Vec::new();
    let Some(parent) = snapshot.collection(&spec.collection) else {
        return Err(vec![format!("collection `{}` does not exist", spec.collection)]);
    };
    if parent.fields.iter().any(|f| f.name == n.field) {
        errors.push(format!("`{}` already has a field `{}`", spec.collection, n.field));
    }
    if snapshot.collection(&n.translations).is_some() {
        errors.push(format!("collection `{}` already exists: choose another translations_collection or add the field by hand", n.translations));
    }
    if n.translations == n.languages || n.translations == spec.collection || n.languages == spec.collection {
        errors.push("the item, translations and languages collections must be three different collections".into());
    }
    for (what, name) in [("field", &n.field), ("translations collection", &n.translations), ("languages collection", &n.languages), ("parent field", &n.parent_field), ("language field", &n.language_field)] {
        if !is_valid_identifier(name) {
            errors.push(format!("the {what} name `{name}` is not valid (letters, digits, underscore; max 63 characters)"));
        }
    }
    if spec.fields.is_empty() {
        errors.push("list at least one translatable field".into());
    }
    let mut seen = std::collections::BTreeSet::new();
    for f in &spec.fields {
        if f.name == n.parent_field || f.name == n.language_field || !seen.insert(f.name.as_str()) {
            errors.push(format!("translatable field `{}` is duplicated or clashes with `{}` / `{}`", f.name, n.parent_field, n.language_field));
        }
        if !f.has_column() {
            errors.push(format!("translatable field `{}`: aliases cannot be translated", f.name));
        }
        if f.unique || !f.unique_with.is_empty() {
            errors.push(format!("translatable field `{}`: a translated value cannot be unique", f.name));
        }
    }
    let languages_exists = match snapshot.collection(&n.languages) {
        Some(l) => {
            if !l.field("code").map(|c| c.ty.is_textual()).unwrap_or(false) {
                errors.push(format!("the existing collection `{}` needs a string field `code`", n.languages));
            }
            true
        }
        None => false,
    };
    if !errors.is_empty() {
        return Err(errors);
    }

    let mut out = snapshot.clone();
    if !languages_exists {
        out.collections.push(default_languages(&n.languages));
    }
    let mut tc = Collection::new(&n.translations);
    tc.system_fields = false;
    tc.meta.hidden = true;
    tc.meta.icon = Some("translate".into());
    // the same page translated in the same language on both sides is the same translation: promotion merges instead of colliding
    tc.sync.natural_key = vec![n.parent_field.clone(), n.language_field.clone()];
    tc.fields.push(Field {
        required: true,
        relation: Some(Relation { related: Some(spec.collection.clone()), on_delete: OnDelete::Cascade, ..Default::default() }),
        ..Field::new(n.parent_field.clone(), FieldType::M2o)
    });
    tc.fields.push(Field {
        required: true,
        unique: true,
        unique_with: vec![n.parent_field.clone()],
        relation: Some(Relation { related: Some(n.languages.clone()), on_delete: OnDelete::Restrict, ..Default::default() }),
        ..Field::new(n.language_field.clone(), FieldType::M2o)
    });
    tc.fields.extend(spec.fields.iter().cloned());
    out.collections.push(tc);

    let alias = Field {
        relation: Some(Relation {
            related: Some(n.translations.clone()),
            foreign_field: Some(n.parent_field.clone()),
            language_field: Some(n.language_field.clone()),
            ..Default::default()
        }),
        meta: FieldMeta { interface: Some("translations".into()), ..Default::default() },
        ..Field::new(n.field.clone(), FieldType::Translations)
    };
    out.collection_mut(&spec.collection).expect("checked above").fields.push(alias);

    let problems = crate::validate::validate_snapshot(&out);
    if problems.is_empty() { Ok(out) } else { Err(problems) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::diff;
    use crate::validate::validate_snapshot;

    fn base() -> Snapshot {
        serde_yaml::from_str("version: 1\ncollections:\n  - name: pages\n    fields:\n      - { name: slug, type: string, unique: true }\n").unwrap()
    }

    fn spec(fields: &[(&str, FieldType)]) -> TranslationsSpec {
        TranslationsSpec { collection: "pages".into(), fields: fields.iter().map(|(n, t)| Field::new(*n, *t)).collect(), ..Default::default() }
    }

    #[test]
    fn builds_languages_translations_and_the_alias_in_one_valid_snapshot() {
        let out = scaffold(&base(), &spec(&[("title", FieldType::String), ("body", FieldType::Text)])).unwrap();
        assert!(validate_snapshot(&out).is_empty(), "{:?}", validate_snapshot(&out));
        let langs = out.collection("languages").unwrap();
        assert_eq!(langs.sync.natural_key, vec!["code"]);
        assert!(langs.field("code").unwrap().unique && langs.field("code").unwrap().required);
        let tc = out.collection("pages_translations").unwrap();
        assert!(!tc.system_fields);
        let lf = tc.field("languages_id").unwrap();
        assert!(lf.unique && lf.unique_with == vec!["pages_id"] && lf.required, "one translation per language and page");
        assert_eq!(tc.sync.natural_key, vec!["pages_id", "languages_id"]);
        assert_eq!(tc.field("pages_id").unwrap().on_delete(), OnDelete::Cascade, "deleting a page deletes its translations");
        assert_eq!(lf.on_delete(), OnDelete::Restrict, "a language in use cannot be deleted by accident");
        let alias = out.collection("pages").unwrap().field("translations").unwrap();
        assert_eq!((alias.ty, alias.related_collection(), alias.foreign_field(), alias.language_field(), alias.language_key()), (FieldType::Translations, Some("pages_translations"), Some("pages_id"), Some("languages_id"), "code"));
        let names: Vec<&str> = translatable_fields(tc, alias).iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["title", "body"], "the two link fields are not content");
        // and the plan creates everything, parents before the constraints that point at them
        let plan = diff(&base(), &out);
        let text = plan.changes.iter().map(|c| c.summary()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("+ collection languages") && text.contains("+ collection pages_translations") && text.contains("translations"), "{text}");
    }

    #[test]
    fn a_collection_gains_the_translatable_fields_it_does_not_have() {
        let out = scaffold(&base(), &spec(&[("title", FieldType::String), ("slug", FieldType::String), ("body", FieldType::Text)])).unwrap();
        let names: Vec<&str> = translated_fields(&out, out.collection("pages").unwrap()).iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["title", "body"], "`slug` is the page's own field: already on its type, `lang` can still replace its value");
        assert!(translated_fields(&out, out.collection("languages").unwrap()).is_empty());
    }

    #[test]
    fn reuses_an_existing_languages_collection() {
        let mut s = base();
        s.collections.push(default_languages("langs"));
        let out = scaffold(&s, &TranslationsSpec { languages_collection: Some("langs".into()), ..spec(&[("title", FieldType::String)]) }).unwrap();
        assert_eq!(out.collections.iter().filter(|c| c.name == "langs").count(), 1);
        assert!(out.collection("languages").is_none());
        assert!(out.collection("pages_translations").unwrap().field("langs_id").is_some());
        // a languages collection without a code cannot be used
        let mut bad = base();
        bad.collections.push(Collection { fields: vec![Field::new("name", FieldType::String)], ..Collection::new("languages") });
        let err = scaffold(&bad, &spec(&[("title", FieldType::String)])).unwrap_err();
        assert!(err.iter().any(|e| e.contains("`code`")), "{err:?}");
    }

    #[test]
    fn refuses_what_would_clash() {
        let ok = spec(&[("title", FieldType::String)]);
        let mut s = base();
        s.collection_mut("pages").unwrap().fields.push(Field::new("translations", FieldType::Text));
        assert!(scaffold(&s, &ok).unwrap_err()[0].contains("already has a field"));
        let mut s = base();
        s.collections.push(Collection::new("pages_translations"));
        assert!(scaffold(&s, &ok).unwrap_err()[0].contains("already exists"));
        assert!(scaffold(&base(), &TranslationsSpec { collection: "nope".into(), ..ok.clone() }).unwrap_err()[0].contains("does not exist"));
        assert!(scaffold(&base(), &spec(&[])).unwrap_err()[0].contains("at least one"));
        let dup = spec(&[("title", FieldType::String), ("title", FieldType::Text)]);
        assert!(scaffold(&base(), &dup).is_err());
        assert!(scaffold(&base(), &spec(&[("pages_id", FieldType::String)])).is_err(), "clash with the link field");
        let alias = spec(&[("things", FieldType::O2m)]);
        assert!(scaffold(&base(), &alias).unwrap_err().iter().any(|e| e.contains("aliases")));
        let unique = TranslationsSpec { fields: vec![Field { unique: true, ..Field::new("slug", FieldType::String) }], ..ok.clone() };
        assert!(scaffold(&base(), &unique).unwrap_err().iter().any(|e| e.contains("cannot be unique")));
        let bad_name = TranslationsSpec { field_name: Some("bad name".into()), ..ok };
        assert!(scaffold(&base(), &bad_name).is_err());
    }

    #[test]
    fn validation_requires_the_database_guarantee() {
        let mut s = scaffold(&base(), &spec(&[("title", FieldType::String)])).unwrap();
        assert!(validate_snapshot(&s).is_empty());
        // dropping the composite unique leaves duplicates possible: the model is rejected
        let tc = s.collection_mut("pages_translations").unwrap();
        tc.fields.iter_mut().find(|f| f.name == "languages_id").unwrap().unique_with.clear();
        let errs = validate_snapshot(&s);
        assert!(errs.iter().any(|e| e.contains("one translation per language and item")), "{errs:?}");
        // a nullable link would let NULLs escape the constraint
        let mut s = scaffold(&base(), &spec(&[("title", FieldType::String)])).unwrap();
        s.collection_mut("pages_translations").unwrap().fields.iter_mut().find(|f| f.name == "pages_id").unwrap().required = false;
        assert!(validate_snapshot(&s).iter().any(|e| e.contains("must be required")));
        // no language field / wrong kind / missing code
        let mut s = scaffold(&base(), &spec(&[("title", FieldType::String)])).unwrap();
        s.collection_mut("pages").unwrap().fields.iter_mut().find(|f| f.name == "translations").unwrap().relation.as_mut().unwrap().language_field = None;
        assert!(validate_snapshot(&s).iter().any(|e| e.contains("requires relation.language_field")));
        let mut s = scaffold(&base(), &spec(&[("title", FieldType::String)])).unwrap();
        s.collection_mut("pages").unwrap().fields.iter_mut().find(|f| f.name == "translations").unwrap().relation.as_mut().unwrap().language_key = Some("iso".into());
        assert!(validate_snapshot(&s).iter().any(|e| e.contains("string field `iso`")));
        let mut s = scaffold(&base(), &spec(&[("title", FieldType::String)])).unwrap();
        s.collection_mut("pages").unwrap().fields.iter_mut().find(|f| f.name == "translations").unwrap().relation.as_mut().unwrap().language_field = Some("title".into());
        assert!(validate_snapshot(&s).iter().any(|e| e.contains("must be an m2o")));
    }

    #[test]
    fn unique_with_is_validated_and_diffed() {
        let mut s = scaffold(&base(), &spec(&[("title", FieldType::String)])).unwrap();
        s.collection_mut("pages").unwrap().fields.iter_mut().find(|f| f.name == "slug").unwrap().unique_with = vec!["nope".into()];
        assert!(validate_snapshot(&s).iter().any(|e| e.contains("unique_with field `nope`")));
        let mut s = base();
        s.collection_mut("pages").unwrap().fields.push(Field { unique_with: vec!["slug".into()], ..Field::new("lang", FieldType::String) });
        assert!(validate_snapshot(&s).iter().any(|e| e.contains("needs `unique: true`")));
        // changing the composite is a change (a riskier one: existing rows may violate it)
        let before = scaffold(&base(), &spec(&[("title", FieldType::String)])).unwrap();
        let mut after = before.clone();
        after.collection_mut("pages_translations").unwrap().fields.iter_mut().find(|f| f.name == "languages_id").unwrap().unique_with.clear();
        let plan = diff(&before, &after);
        assert_eq!(plan.changes.len(), 1);
        assert!(crate::diff::describe_field_change(
            before.collection("pages_translations").unwrap().field("languages_id").unwrap(),
            after.collection("pages_translations").unwrap().field("languages_id").unwrap()
        )
        .iter()
        .any(|d| d.contains("unique alone")));
    }
}
