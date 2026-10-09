//! Ferro engine: everything that talks to PostgreSQL or to the file storage.

pub mod activity;
pub mod auth;
pub mod db;
pub mod ddl;
pub mod error;
pub mod files;
pub mod flows;
pub mod graphql;
pub mod items;
pub mod permissions;
pub mod query;
pub mod realtime;
pub mod schema;
pub mod schema_store;
pub mod sync;

pub use error::{FerroError, Result};
pub use permissions::Accountability;
pub use schema::{Schema, SchemaHolder};
