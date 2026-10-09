use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const SNAPSHOT_VERSION: u32 = 1;
/// Prefix reserved for system tables. User collections cannot use it.
pub const SYSTEM_PREFIX: &str = "ferro_";
pub const PRIMARY_KEY: &str = "id";
pub const SYSTEM_FIELDS: [&str; 4] = ["date_created", "user_created", "date_updated", "user_updated"];

pub(crate) fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}
pub(crate) fn is_false(v: &bool) -> bool {
    !*v
}
pub(crate) fn is_true(v: &bool) -> bool {
    *v
}
pub(crate) fn yes() -> bool {
    true
}
fn default_version() -> u32 {
    SNAPSHOT_VERSION
}
fn all_fields() -> Vec<String> {
    vec!["*".to_string()]
}
fn is_all_fields(v: &[String]) -> bool {
    v.len() == 1 && v[0] == "*"
}

/// The complete, environment-independent configuration of a Ferro project:
/// data model + access control. This is what is versioned in git and promoted
/// between environments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub collections: Vec<Collection>,
    #[serde(default)]
    pub roles: Vec<Role>,
    #[serde(default)]
    pub policies: Vec<Policy>,
    /// Automations (webhooks, hooks, schedules, custom endpoints).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flows: Vec<crate::flow::Flow>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Snapshot { version: SNAPSHOT_VERSION, collections: vec![], roles: vec![], policies: vec![], flows: vec![] }
    }
}

impl Snapshot {
    pub fn collection(&self, name: &str) -> Option<&Collection> {
        self.collections.iter().find(|c| c.name == name)
    }
    pub fn collection_mut(&mut self, name: &str) -> Option<&mut Collection> {
        self.collections.iter_mut().find(|c| c.name == name)
    }
    pub fn role(&self, id: Uuid) -> Option<&Role> {
        self.roles.iter().find(|r| r.id == id)
    }
    pub fn policy(&self, id: Uuid) -> Option<&Policy> {
        self.policies.iter().find(|p| p.id == id)
    }

    /// Deterministic ordering so that hashing and serialisation are stable.
    pub fn normalize(&mut self) {
        self.version = SNAPSHOT_VERSION;
        self.collections.sort_by(|a, b| a.name.cmp(&b.name));
        self.roles.sort_by(|a, b| a.id.cmp(&b.id));
        self.policies.sort_by(|a, b| a.id.cmp(&b.id));
        self.flows.sort_by(|a, b| a.id.cmp(&b.id));
        for r in &mut self.roles {
            r.policies.sort();
            r.policies.dedup();
        }
    }

    /// The Administrator role and policy always exist, with their well-known ids:
    /// a snapshot that omits them (hand-written files) means "unchanged", never "delete".
    pub fn ensure_admin(&mut self) {
        if self.policy(ADMIN_POLICY_ID).is_none() {
            self.policies.push(admin_policy());
        }
        if self.role(ADMIN_ROLE_ID).is_none() {
            self.roles.push(admin_role());
        }
    }

    /// Remove one-shot hints (`renamed_from`) once they have been applied.
    pub fn strip_hints(&mut self) {
        for c in &mut self.collections {
            c.renamed_from = None;
            for f in &mut c.fields {
                f.renamed_from = None;
            }
        }
    }

    /// Content hash of the snapshot (hints excluded). Two environments with the
    /// same hash have exactly the same model and access configuration.
    pub fn hash(&self) -> String {
        let mut s = self.clone();
        s.strip_hints();
        s.normalize();
        crate::hash::hash_json(&serde_json::to_value(&s).unwrap_or(Value::Null))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Collection {
    pub name: String,
    /// One-shot hint: this collection was previously called `renamed_from`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renamed_from: Option<String>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub meta: CollectionMeta,
    #[serde(default, skip_serializing_if = "is_default")]
    pub sync: CollectionSync,
    /// Adds date_created / user_created / date_updated / user_updated managed by the server.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub system_fields: bool,
    #[serde(default)]
    pub fields: Vec<Field>,
}

impl Collection {
    pub fn new(name: impl Into<String>) -> Self {
        Collection {
            name: name.into(),
            renamed_from: None,
            meta: CollectionMeta::default(),
            sync: CollectionSync::default(),
            system_fields: true,
            fields: vec![],
        }
    }

    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }

    pub fn header(&self) -> CollectionHeader {
        CollectionHeader { meta: self.meta.clone(), sync: self.sync.clone(), system_fields: self.system_fields }
    }

    /// Fields that exist on every collection without being declared.
    pub fn implicit_fields(&self) -> Vec<Field> {
        let mut out = vec![Field { required: true, ..Field::new(PRIMARY_KEY, FieldType::Uuid) }];
        if self.system_fields {
            out.extend(system_fields());
        }
        out
    }

    /// Implicit + declared fields: the full shape of an item.
    pub fn resolved_fields(&self) -> Vec<Field> {
        let mut out = vec![Field { required: true, ..Field::new(PRIMARY_KEY, FieldType::Uuid) }];
        out.extend(self.fields.iter().cloned());
        if self.system_fields {
            out.extend(system_fields());
        }
        out
    }
}

pub fn system_fields() -> Vec<Field> {
    let user_rel = Relation { related: Some("ferro_users".into()), ..Relation::default() };
    vec![
        Field { meta: FieldMeta { readonly: true, hidden: true, ..Default::default() }, ..Field::new("date_created", FieldType::Datetime) },
        Field {
            relation: Some(user_rel.clone()),
            meta: FieldMeta { readonly: true, hidden: true, ..Default::default() },
            ..Field::new("user_created", FieldType::M2o)
        },
        Field { meta: FieldMeta { readonly: true, hidden: true, ..Default::default() }, ..Field::new("date_updated", FieldType::Datetime) },
        Field {
            relation: Some(user_rel),
            meta: FieldMeta { readonly: true, hidden: true, ..Default::default() },
            ..Field::new("user_updated", FieldType::M2o)
        },
    ]
}

/// Collection-level attributes that are not fields (used by diff).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CollectionHeader {
    pub meta: CollectionMeta,
    pub sync: CollectionSync,
    pub system_fields: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CollectionMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
    /// A singleton collection holds exactly one item (e.g. site settings).
    #[serde(default, skip_serializing_if = "is_false")]
    pub singleton: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_template: Option<String>,
    /// Default sort field for listings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_value: Option<Value>,
    /// Folder/group in the admin navigation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Translated labels, e.g. `{ "it-IT": "Pagine" }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translations: Option<Value>,
    /// Store revisions (full item history) for this collection.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub revisions: bool,
}

impl Default for CollectionMeta {
    fn default() -> Self {
        CollectionMeta {
            icon: None,
            note: None,
            hidden: false,
            singleton: false,
            display_template: None,
            sort_field: None,
            archive_field: None,
            archive_value: None,
            group: None,
            translations: None,
            revisions: true,
        }
    }
}

/// How the collection participates in data promotion between environments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CollectionSync {
    /// Business key used to match items across environments when ids differ
    /// (e.g. `["slug"]` or `["code", "language"]`). Ids are remapped on import.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub natural_key: Vec<String>,
    /// Never export items of this collection (environment specific data).
    #[serde(default, skip_serializing_if = "is_false")]
    pub exclude: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: FieldType,
    /// One-shot hint: this field was previously called `renamed_from`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renamed_from: Option<String>,
    /// NOT NULL
    #[serde(default, skip_serializing_if = "is_false")]
    pub required: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub unique: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub indexed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    /// `string` only (varchar length, default 255).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<u32>,
    /// `decimal` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precision: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation: Option<Relation>,
    /// Filter the value must satisfy, e.g. `{ "_regex": "^[a-z-]+$" }` is not
    /// supported but `{ "_nempty": true }` is. Evaluated against `{ <field>: value }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<Value>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub meta: FieldMeta,
}

impl Field {
    pub fn new(name: impl Into<String>, ty: FieldType) -> Self {
        Field { name: name.into(), ty, ..Default::default() }
    }

    pub fn has_column(&self) -> bool {
        !self.ty.is_alias()
    }

    /// Target collection of an m2o / file / o2m-like field.
    pub fn related_collection(&self) -> Option<&str> {
        match self.ty {
            FieldType::File => Some(
                self.relation.as_ref().and_then(|r| r.related.as_deref()).unwrap_or("ferro_files"),
            ),
            FieldType::M2o | FieldType::O2m | FieldType::M2m | FieldType::M2a => {
                self.relation.as_ref().and_then(|r| r.related.as_deref())
            }
            _ => None,
        }
    }

    pub fn foreign_field(&self) -> Option<&str> {
        self.relation.as_ref().and_then(|r| r.foreign_field.as_deref())
    }

    pub fn sync_mode(&self) -> SyncMode {
        self.relation.as_ref().map(|r| r.sync).unwrap_or_default()
    }

    pub fn on_delete(&self) -> OnDelete {
        self.relation.as_ref().map(|r| r.on_delete).unwrap_or_default()
    }

    /// Copy without one-shot hints, used for comparisons.
    pub fn without_hints(&self) -> Field {
        Field { renamed_from: None, ..self.clone() }
    }

    /// The PostgreSQL column type, `None` for alias fields.
    pub fn pg_type(&self) -> Option<String> {
        Some(match self.ty {
            FieldType::String => format!("varchar({})", self.max_length.unwrap_or(255)),
            FieldType::Text => "text".into(),
            FieldType::Integer => "integer".into(),
            FieldType::BigInteger => "bigint".into(),
            FieldType::Float => "double precision".into(),
            FieldType::Decimal => {
                format!("numeric({},{})", self.precision.unwrap_or(10), self.scale.unwrap_or(5))
            }
            FieldType::Boolean => "boolean".into(),
            FieldType::Uuid | FieldType::M2o | FieldType::File | FieldType::M2aItem => "uuid".into(),
            FieldType::Json => "jsonb".into(),
            FieldType::Date => "date".into(),
            FieldType::Datetime => "timestamptz".into(),
            FieldType::Time => "time".into(),
            FieldType::O2m | FieldType::M2m | FieldType::M2a => return None,
        })
    }

    /// Type used in `CAST($n AS ...)` when binding values.
    pub fn cast_type(&self) -> String {
        match self.ty {
            FieldType::String | FieldType::Text => "text".into(),
            _ => self.pg_type().unwrap_or_else(|| "text".into()),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    #[default]
    String,
    Text,
    Integer,
    #[serde(rename = "bigint")]
    BigInteger,
    Float,
    Decimal,
    Boolean,
    Uuid,
    Json,
    Date,
    Datetime,
    Time,
    /// Many-to-one: uuid column with FK to `relation.related`.
    M2o,
    /// Many-to-one to `ferro_files`.
    File,
    /// One-to-many (alias): items of `relation.related` whose `relation.foreign_field` points here.
    O2m,
    /// Many-to-many (alias): o2m to the junction `relation.related`, which has an m2o
    /// `relation.junction_field` towards the other side.
    M2m,
    /// Many-to-any (alias): o2m to the junction `relation.related`, which has an
    /// `m2a_item` field pointing to items of several collections.
    M2a,
    /// Column of an m2a junction: uuid of an item whose collection is stored in the
    /// sibling field `relation.collection_field`.
    M2aItem,
}

impl FieldType {
    pub fn is_alias(self) -> bool {
        matches!(self, FieldType::O2m | FieldType::M2m | FieldType::M2a)
    }
    pub fn is_m2o_like(self) -> bool {
        matches!(self, FieldType::M2o | FieldType::File)
    }
    pub fn is_relational(self) -> bool {
        self.is_alias() || self.is_m2o_like() || self == FieldType::M2aItem
    }
    pub fn is_textual(self) -> bool {
        matches!(self, FieldType::String | FieldType::Text)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            FieldType::String => "string",
            FieldType::Text => "text",
            FieldType::Integer => "integer",
            FieldType::BigInteger => "bigint",
            FieldType::Float => "float",
            FieldType::Decimal => "decimal",
            FieldType::Boolean => "boolean",
            FieldType::Uuid => "uuid",
            FieldType::Json => "json",
            FieldType::Date => "date",
            FieldType::Datetime => "datetime",
            FieldType::Time => "time",
            FieldType::M2o => "m2o",
            FieldType::File => "file",
            FieldType::O2m => "o2m",
            FieldType::M2m => "m2m",
            FieldType::M2a => "m2a",
            FieldType::M2aItem => "m2a_item",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Relation {
    /// m2o/file: target collection. o2m/m2m/m2a: child or junction collection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub related: Option<String>,
    /// o2m/m2m/m2a: the m2o field in `related` pointing back to this collection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreign_field: Option<String>,
    /// m2m: the m2o field in the junction pointing to the far side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub junction_field: Option<String>,
    /// m2a_item: sibling field (string) that stores the target collection name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection_field: Option<String>,
    /// m2a_item: collections allowed as targets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed: Vec<String>,
    /// o2m-like: field in `related` used for manual ordering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_field: Option<String>,
    /// m2o/file: referential action.
    #[serde(default, skip_serializing_if = "is_default")]
    pub on_delete: OnDelete,
    /// Behaviour of the relation when exporting data bundles.
    #[serde(default, skip_serializing_if = "is_default")]
    pub sync: SyncMode,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnDelete {
    #[default]
    SetNull,
    Cascade,
    Restrict,
    NoAction,
}

impl OnDelete {
    pub fn sql(self) -> &'static str {
        match self {
            OnDelete::SetNull => "SET NULL",
            OnDelete::Cascade => "CASCADE",
            OnDelete::Restrict => "RESTRICT",
            OnDelete::NoAction => "NO ACTION",
        }
    }
}

/// How a relation is followed when computing the dependency closure of an export.
///
/// * m2o/file/m2a_item `default`: the target item is exported (shallow: its own m2o
///   dependencies are followed, its children are not).
/// * m2o `deep`: the target item is exported together with its children.
/// * o2m/m2m/m2a `default`: children are exported when the parent is exported deep
///   (roots and composed children are deep).
/// * `reference`: never exported, must already exist in the target environment.
/// * `skip`: never exported; for m2o the value is preserved and must exist in target.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    #[default]
    Default,
    Deep,
    Reference,
    Skip,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FieldMeta {
    /// Admin UI interface (input, wysiwyg, select-dropdown, list-m2a, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_options: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub readonly: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translations: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Role {
    pub id: Uuid,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default)]
    pub policies: Vec<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    pub id: Uuid,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Full access to everything, including schema and sync.
    #[serde(default, skip_serializing_if = "is_false")]
    pub admin_access: bool,
    /// Allowed to log into the admin app.
    #[serde(default, skip_serializing_if = "is_false")]
    pub app_access: bool,
    /// Applied to unauthenticated requests.
    #[serde(default, skip_serializing_if = "is_false")]
    pub public: bool,
    #[serde(default)]
    pub permissions: Vec<Permission>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Permission {
    pub collection: String,
    pub action: Action,
    #[serde(default = "all_fields", skip_serializing_if = "is_all_fields")]
    pub fields: Vec<String>,
    /// Item-level rule (supports `$CURRENT_USER`, `$CURRENT_ROLE`, `$NOW`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Value>,
    /// Payload must satisfy this filter on create/update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<Value>,
    /// Default values applied on create.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presets: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Create,
    Read,
    Update,
    Delete,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Create => "create",
            Action::Read => "read",
            Action::Update => "update",
            Action::Delete => "delete",
        }
    }
}

/// Well-known ids so that the administrator role/policy are identical in every environment.
pub const ADMIN_POLICY_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_7000_8000_0000_0000_0001);
pub const ADMIN_ROLE_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_7000_8000_0000_0000_0002);
pub const PUBLIC_POLICY_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_7000_8000_0000_0000_0003);

pub fn admin_policy() -> Policy {
    Policy {
        id: ADMIN_POLICY_ID,
        name: "Administrator".into(),
        description: Some("Full access".into()),
        admin_access: true,
        app_access: true,
        public: false,
        permissions: vec![],
    }
}

pub fn admin_role() -> Role {
    Role {
        id: ADMIN_ROLE_ID,
        name: "Administrator".into(),
        description: None,
        icon: Some("verified".into()),
        policies: vec![ADMIN_POLICY_ID],
    }
}

/// `^[A-Za-z_][A-Za-z0-9_]{0,62}$`
pub fn is_valid_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    s.len() <= 63 && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers() {
        assert!(is_valid_identifier("PW_Pages"));
        assert!(is_valid_identifier("_x1"));
        assert!(!is_valid_identifier("1abc"));
        assert!(!is_valid_identifier("a-b"));
        assert!(!is_valid_identifier("a\"b"));
        assert!(!is_valid_identifier(""));
    }

    #[test]
    fn yaml_roundtrip_is_compact() {
        let yaml = r#"
name: pages
sync: { natural_key: [slug] }
fields:
  - { name: title, type: string, required: true }
  - { name: slug, type: string, unique: true }
  - name: blocks
    type: m2a
    relation: { related: pages_blocks, foreign_field: pages_id, sort_field: sort }
"#;
        let c: Collection = serde_yaml::from_str(yaml).unwrap();
        assert!(c.system_fields);
        assert_eq!(c.fields.len(), 3);
        assert_eq!(c.fields[2].ty, FieldType::M2a);
        let out = serde_yaml::to_string(&c).unwrap();
        assert!(!out.contains("system_fields"));
        assert!(!out.contains("unique: false"));
        let back: Collection = serde_yaml::from_str(&out).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn hash_ignores_order_and_hints() {
        let mut a = Snapshot::default();
        a.collections.push(Collection::new("b"));
        a.collections.push(Collection::new("a"));
        let mut b = a.clone();
        b.collections.reverse();
        b.collections[0].renamed_from = Some("old".into());
        assert_eq!(a.hash(), b.hash());
    }
}
