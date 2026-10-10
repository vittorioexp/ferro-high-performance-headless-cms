//! Ferro core: everything that can be expressed without a database.
//!
//! * [`model`]    – the data model (collections, fields, relations, roles, policies) = the "snapshot"
//! * [`validate`] – structural validation of a snapshot
//! * [`directus`] – conversion of a Directus schema snapshot
//! * [`diff`]     – snapshot vs snapshot diff producing an ordered, explainable [`diff::Plan`]
//! * [`flow`]     – flows (automations): model, templating, validation; [`cron`] schedules
//! * [`filter`]   – in-memory evaluation of Directus-style filters (permissions validation)
//! * [`fields`]   – parsing of `fields=a,b.c,*` selections into a tree
//! * [`graphql`]  – GraphQL lexer/parser and execution helpers; [`sdl`] generates the schema
//! * [`graph`]    – deterministic topological ordering (data import order)
//! * [`bundle`]   – portable data bundle format + import options/report
//! * [`project`]  – read/write a snapshot as a git-friendly directory of YAML files
//! * [`report`]   – standalone HTML reports for plans and imports

pub mod bundle;
pub mod cron;
pub mod diff;
pub mod directus;
pub mod fields;
pub mod filter;
pub mod flow;
pub mod graph;
pub mod graphql;
pub mod hash;
pub mod model;
pub mod project;
pub mod report;
pub mod sdl;
pub mod system;
pub mod translations;
pub mod validate;

pub use model::*;
