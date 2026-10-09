use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use ferro_core::system::system_collections;
use ferro_core::{Collection, Field, Policy, Role, Snapshot};

use crate::{FerroError, Result};

/// A collection with its fully resolved field list (implicit + declared).
#[derive(Debug, Clone)]
pub struct CollectionInfo {
    pub name: String,
    pub def: Collection,
    pub fields: Vec<Field>,
    pub system: bool,
}

impl CollectionInfo {
    fn new(def: Collection, system: bool) -> Self {
        CollectionInfo { name: def.name.clone(), fields: def.resolved_fields(), def, system }
    }
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }
    pub fn require_field(&self, name: &str) -> Result<&Field> {
        self.field(name)
            .ok_or_else(|| FerroError::invalid(format!("field `{name}` does not exist in `{}`", self.name)))
    }
    pub fn columns(&self) -> impl Iterator<Item = &Field> {
        self.fields.iter().filter(|f| f.has_column())
    }
}

/// In-memory, immutable view of the current model. Replaced atomically on change.
#[derive(Debug, Clone)]
pub struct Schema {
    pub snapshot: Snapshot,
    pub hash: String,
    collections: BTreeMap<String, Arc<CollectionInfo>>,
}

impl Schema {
    pub fn new(snapshot: Snapshot) -> Schema {
        let mut collections = BTreeMap::new();
        for c in system_collections() {
            collections.insert(c.name.clone(), Arc::new(CollectionInfo::new(c, true)));
        }
        for c in &snapshot.collections {
            collections.insert(c.name.clone(), Arc::new(CollectionInfo::new(c.clone(), false)));
        }
        Schema { hash: snapshot.hash(), snapshot, collections }
    }

    pub fn get(&self, name: &str) -> Option<Arc<CollectionInfo>> {
        self.collections.get(name).cloned()
    }

    pub fn collection(&self, name: &str) -> Result<Arc<CollectionInfo>> {
        self.get(name).ok_or_else(|| FerroError::not_found(format!("collection `{name}` not found")))
    }

    pub fn user_collections(&self) -> impl Iterator<Item = &Arc<CollectionInfo>> {
        self.collections.values().filter(|c| !c.system)
    }

    pub fn all_collections(&self) -> impl Iterator<Item = &Arc<CollectionInfo>> {
        self.collections.values()
    }

    pub fn role(&self, id: uuid::Uuid) -> Option<&Role> {
        self.snapshot.role(id)
    }

    pub fn policy(&self, id: uuid::Uuid) -> Option<&Policy> {
        self.snapshot.policy(id)
    }

    pub fn public_policies(&self) -> Vec<Policy> {
        self.snapshot.policies.iter().filter(|p| p.public).cloned().collect()
    }
}

/// Shared holder; readers clone the `Arc` and never keep the lock across awaits.
#[derive(Debug)]
pub struct SchemaHolder(RwLock<Arc<Schema>>);

impl SchemaHolder {
    pub fn new(schema: Schema) -> Self {
        SchemaHolder(RwLock::new(Arc::new(schema)))
    }
    pub fn get(&self) -> Arc<Schema> {
        self.0.read().expect("schema lock poisoned").clone()
    }
    pub fn set(&self, schema: Schema) {
        *self.0.write().expect("schema lock poisoned") = Arc::new(schema);
    }
}
