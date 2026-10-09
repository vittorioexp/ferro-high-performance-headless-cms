//! `fields=*,author.name,blocks.item.*` → tree of requested fields.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct FieldTree {
    /// `*` at this level: every (permitted) field.
    pub all: bool,
    /// Explicit plain fields at this level.
    pub fields: BTreeSet<String>,
    /// Relations to expand. The key `*` applies to every relational field.
    pub children: BTreeMap<String, FieldTree>,
}

impl FieldTree {
    pub fn star() -> Self {
        FieldTree { all: true, ..Default::default() }
    }

    pub fn only(fields: &[&str]) -> Self {
        FieldTree { fields: fields.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    /// Parse a list of dotted paths. Items may also contain commas.
    pub fn parse<S: AsRef<str>>(paths: &[S]) -> Self {
        let mut tree = FieldTree::default();
        for p in paths {
            for path in p.as_ref().split(',') {
                let path = path.trim();
                if path.is_empty() {
                    continue;
                }
                let segments: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
                tree.insert(&segments);
            }
        }
        if tree.is_empty() {
            tree.all = true;
        }
        tree
    }

    fn insert(&mut self, segments: &[&str]) {
        match segments {
            [] => {}
            ["*"] => self.all = true,
            [one] => {
                self.fields.insert(one.to_string());
            }
            [first, rest @ ..] => self.children.entry(first.to_string()).or_default().insert(rest),
        }
    }

    pub fn is_empty(&self) -> bool {
        !self.all && self.fields.is_empty() && self.children.is_empty()
    }

    /// Is `name` requested as a plain value (not expanded)?
    pub fn wants(&self, name: &str) -> bool {
        self.all || self.fields.contains(name)
    }

    /// Sub-tree for relation `name`, honouring `*.xxx` wildcards.
    pub fn child(&self, name: &str) -> Option<FieldTree> {
        match (self.children.get(name), self.children.get("*")) {
            (Some(c), Some(w)) => Some(c.merged(w)),
            (Some(c), None) => Some(c.clone()),
            (None, Some(w)) => Some(w.clone()),
            (None, None) => None,
        }
    }

    pub fn merged(&self, other: &FieldTree) -> FieldTree {
        let mut out = self.clone();
        out.all |= other.all;
        out.fields.extend(other.fields.iter().cloned());
        for (k, v) in &other.children {
            let merged = match out.children.get(k) {
                Some(existing) => existing.merged(v),
                None => v.clone(),
            };
            out.children.insert(k.clone(), merged);
        }
        out
    }

    pub fn with_field(mut self, name: &str) -> FieldTree {
        if !self.all {
            self.fields.insert(name.to_string());
        }
        self
    }

    /// Names explicitly mentioned at this level (plain or expanded), excluding `*`.
    pub fn mentioned(&self) -> BTreeSet<String> {
        let mut out: BTreeSet<String> = self.fields.clone();
        out.extend(self.children.keys().filter(|k| k.as_str() != "*").cloned());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_paths() {
        let t = FieldTree::parse(&["*,author.name", "blocks.item.*", "blocks.sort"]);
        assert!(t.all);
        assert!(t.child("author").unwrap().wants("name"));
        assert!(!t.child("author").unwrap().wants("email"));
        let blocks = t.child("blocks").unwrap();
        assert!(blocks.wants("sort"));
        assert!(blocks.child("item").unwrap().all);
        assert_eq!(FieldTree::parse::<&str>(&[]), FieldTree::star());
    }

    #[test]
    fn wildcard_children() {
        let t = FieldTree::parse(&["*.*"]);
        assert!(t.child("anything").unwrap().all);
    }
}
