use std::collections::BTreeSet;

use ferro_core::filter::substitute_vars;
use ferro_core::{Action, Policy};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::{FerroError, Result};

/// Who is performing an operation.
#[derive(Debug, Clone)]
pub struct Accountability {
    pub user: Option<Uuid>,
    pub role: Option<Uuid>,
    pub admin: bool,
    pub app: bool,
    pub policies: Vec<Policy>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

impl Accountability {
    /// Internal operations (bootstrap, CLI on the database).
    pub fn system() -> Self {
        Accountability { user: None, role: None, admin: true, app: true, policies: vec![], ip: None, user_agent: None }
    }

    pub fn public(policies: Vec<Policy>) -> Self {
        Accountability { user: None, role: None, admin: false, app: false, policies, ip: None, user_agent: None }
    }

    pub fn from_policies(user: Uuid, role: Option<Uuid>, policies: Vec<Policy>) -> Self {
        let admin = policies.iter().any(|p| p.admin_access);
        let app = admin || policies.iter().any(|p| p.app_access);
        Accountability { user: Some(user), role, admin, app, policies, ip: None, user_agent: None }
    }

    pub fn require_admin(&self) -> Result<()> {
        if self.admin {
            Ok(())
        } else {
            Err(FerroError::forbidden("administrator access required"))
        }
    }

    pub fn vars(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("$CURRENT_USER".into(), self.user.map(|u| json!(u.to_string())).unwrap_or(Value::Null));
        m.insert("$CURRENT_ROLE".into(), self.role.map(|u| json!(u.to_string())).unwrap_or(Value::Null));
        m.insert("$NOW".into(), json!(chrono::Utc::now().to_rfc3339()));
        m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum FieldAccess {
    All,
    Some(BTreeSet<String>),
}

/// Effective permission of an accountability on (collection, action).
#[derive(Debug, Clone)]
pub struct Access {
    pub fields: FieldAccess,
    /// `None` = every item.
    pub filter: Option<Value>,
    pub validation: Option<Value>,
    pub presets: Map<String, Value>,
}

impl Access {
    pub fn full() -> Self {
        Access { fields: FieldAccess::All, filter: None, validation: None, presets: Map::new() }
    }
    pub fn allows(&self, field: &str) -> bool {
        match &self.fields {
            FieldAccess::All => true,
            FieldAccess::Some(set) => field == "id" || set.contains(field),
        }
    }
}

/// Merge every permission of every policy for (collection, action).
/// Fields are united; item filters are OR-ed (a permission without filter grants all items).
pub fn access(acc: &Accountability, collection: &str, action: Action) -> Result<Access> {
    if acc.admin {
        return Ok(Access::full());
    }
    let perms: Vec<_> = acc
        .policies
        .iter()
        .flat_map(|p| p.permissions.iter())
        .filter(|p| p.action == action && (p.collection == collection || (p.collection == "*" && !ferro_core::system::is_system_collection(collection))))
        .collect();
    if perms.is_empty() {
        return Err(FerroError::forbidden(format!(
            "you don't have permission to {} `{collection}`",
            action.as_str()
        )));
    }
    let vars = acc.vars();
    let mut fields = BTreeSet::new();
    let mut all_fields = false;
    let mut filters = vec![];
    let mut unrestricted = false;
    let mut validations = vec![];
    let mut presets = Map::new();
    for p in perms {
        if p.fields.iter().any(|f| f == "*") {
            all_fields = true;
        } else {
            fields.extend(p.fields.iter().cloned());
        }
        match &p.filter {
            Some(f) if !ferro_core::filter::is_empty_filter(f) => filters.push(substitute_vars(f, &vars)),
            _ => unrestricted = true,
        }
        if let Some(v) = &p.validation {
            validations.push(substitute_vars(v, &vars));
        }
        if let Some(Value::Object(pr)) = &p.presets {
            for (k, v) in pr {
                presets.insert(k.clone(), substitute_vars(v, &vars));
            }
        }
    }
    let filter = if unrestricted {
        None
    } else if filters.len() == 1 {
        filters.pop()
    } else {
        Some(json!({ "_or": filters }))
    };
    let validation = match validations.len() {
        0 => None,
        1 => validations.pop(),
        _ => Some(json!({ "_or": validations })),
    };
    Ok(Access {
        fields: if all_fields { FieldAccess::All } else { FieldAccess::Some(fields) },
        filter,
        validation,
        presets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferro_core::Permission;

    fn perm(collection: &str, action: Action, fields: &[&str], filter: Option<Value>) -> Permission {
        Permission {
            collection: collection.into(),
            action,
            fields: fields.iter().map(|s| s.to_string()).collect(),
            filter,
            validation: None,
            presets: None,
        }
    }

    #[test]
    fn merges_policies() {
        let user = Uuid::now_v7();
        let p1 = Policy {
            id: Uuid::now_v7(),
            name: "a".into(),
            description: None,
            admin_access: false,
            app_access: true,
            public: false,
            permissions: vec![perm("pages", Action::Read, &["title"], Some(json!({"status": {"_eq": "published"}})))],
        };
        let mut p2 = p1.clone();
        p2.permissions = vec![perm("pages", Action::Read, &["slug"], Some(json!({"user_created": {"_eq": "$CURRENT_USER"}})))];
        let acc = Accountability::from_policies(user, None, vec![p1, p2]);
        let a = access(&acc, "pages", Action::Read).unwrap();
        assert!(a.allows("title") && a.allows("slug") && a.allows("id") && !a.allows("body"));
        let f = a.filter.unwrap();
        assert_eq!(f["_or"][1]["user_created"]["_eq"], json!(user.to_string()));
        assert!(access(&acc, "pages", Action::Delete).is_err());
    }

    #[test]
    fn wildcard_never_grants_system_collections() {
        let wildcard = Policy {
            id: Uuid::now_v7(),
            name: "all".into(),
            description: None,
            admin_access: false,
            app_access: true,
            public: false,
            permissions: vec![perm("*", Action::Read, &["*"], None), perm("ferro_files", Action::Read, &["*"], None)],
        };
        let acc = Accountability::from_policies(Uuid::now_v7(), None, vec![wildcard]);
        assert!(access(&acc, "pages", Action::Read).is_ok(), "user collections are covered");
        assert!(access(&acc, "ferro_users", Action::Read).is_err());
        assert!(access(&acc, "ferro_activity", Action::Read).is_err());
        assert!(access(&acc, "ferro_revisions", Action::Read).is_err());
        assert!(access(&acc, "ferro_files", Action::Read).is_ok(), "an explicit grant still works");
    }
}
