//! Definitions of the system collections exposed through the generic item engine.
//! Their tables are created by the system migrations (not by schema plans) and
//! sensitive columns (password_hash, token_hash) are deliberately not declared,
//! so they can never be selected through the API.

use crate::model::*;

pub const USERS: &str = "ferro_users";
pub const FILES: &str = "ferro_files";
pub const FOLDERS: &str = "ferro_folders";
pub const ACTIVITY: &str = "ferro_activity";
pub const REVISIONS: &str = "ferro_revisions";

pub fn is_system_collection(name: &str) -> bool {
    name.starts_with(SYSTEM_PREFIX)
}

/// System collections that may be the target of user relations.
pub fn is_relatable_system_collection(name: &str) -> bool {
    matches!(name, USERS | FILES | FOLDERS)
}

fn sys(name: &str, fields: Vec<Field>) -> Collection {
    Collection { system_fields: false, fields, ..Collection::new(name) }
}

fn f(name: &str, ty: FieldType) -> Field {
    Field::new(name, ty)
}

fn m2o(name: &str, related: &str) -> Field {
    Field {
        relation: Some(Relation { related: Some(related.into()), ..Relation::default() }),
        ..Field::new(name, FieldType::M2o)
    }
}

pub fn system_collections() -> Vec<Collection> {
    vec![
        sys(
            USERS,
            vec![
                Field { required: true, unique: true, ..f("email", FieldType::String) },
                f("first_name", FieldType::String),
                f("last_name", FieldType::String),
                f("role", FieldType::Uuid),
                f("status", FieldType::String),
                f("language", FieldType::String),
                f("provider", FieldType::String),
                f("tfa_enabled", FieldType::Boolean),
                f("last_access", FieldType::Datetime),
                f("date_created", FieldType::Datetime),
            ],
        ),
        sys(
            FOLDERS,
            vec![Field { required: true, ..f("name", FieldType::String) }, m2o("parent", FOLDERS)],
        ),
        sys(
            FILES,
            vec![
                f("storage", FieldType::String),
                f("filename_disk", FieldType::String),
                f("filename_download", FieldType::String),
                f("title", FieldType::String),
                f("description", FieldType::Text),
                f("type", FieldType::String),
                m2o("folder", FOLDERS),
                f("filesize", FieldType::BigInteger),
                f("checksum", FieldType::String),
                f("width", FieldType::Integer),
                f("height", FieldType::Integer),
                f("tags", FieldType::Json),
                f("metadata", FieldType::Json),
                m2o("uploaded_by", USERS),
                f("uploaded_on", FieldType::Datetime),
                f("modified_on", FieldType::Datetime),
            ],
        ),
        sys(
            ACTIVITY,
            vec![
                f("action", FieldType::String),
                m2o("user", USERS),
                f("timestamp", FieldType::Datetime),
                f("ip", FieldType::String),
                f("user_agent", FieldType::Text),
                f("collection", FieldType::String),
                f("item", FieldType::String),
                f("origin", FieldType::String),
                f("comment", FieldType::Text),
            ],
        ),
        sys(
            REVISIONS,
            vec![
                m2o("activity", ACTIVITY),
                f("collection", FieldType::String),
                f("item", FieldType::String),
                f("data", FieldType::Json),
                f("delta", FieldType::Json),
            ],
        ),
    ]
}
