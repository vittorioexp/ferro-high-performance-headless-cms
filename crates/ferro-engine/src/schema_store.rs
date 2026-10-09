//! Persistence of the snapshot and transactional application of plans.

use ferro_core::diff::{diff, Plan};
use ferro_core::validate::validate_snapshot;
use ferro_core::{admin_policy, admin_role, Collection, Policy, Role, Snapshot, ADMIN_POLICY_ID, ADMIN_ROLE_ID};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::auth::{create_user, NewUser};
use crate::ddl::plan_to_sql;
use crate::schema::Schema;
use crate::{FerroError, Result};

pub const SCHEMA_CHANNEL: &str = "ferro_schema";
const SCHEMA_LOCK: i64 = 7466525;

pub async fn load_snapshot(conn: &mut PgConnection) -> Result<Snapshot> {
    let mut snap = Snapshot::default();
    let cols: Vec<Value> = sqlx::query_scalar("SELECT definition FROM ferro_collections ORDER BY name")
        .fetch_all(&mut *conn)
        .await?;
    for c in cols {
        snap.collections.push(serde_json::from_value::<Collection>(c).map_err(|e| FerroError::Internal(format!("corrupted collection definition: {e}")))?);
    }
    let roles: Vec<Value> = sqlx::query_scalar("SELECT definition FROM ferro_roles").fetch_all(&mut *conn).await?;
    for r in roles {
        snap.roles.push(serde_json::from_value::<Role>(r).map_err(|e| FerroError::Internal(format!("corrupted role: {e}")))?);
    }
    let policies: Vec<Value> = sqlx::query_scalar("SELECT definition FROM ferro_policies").fetch_all(&mut *conn).await?;
    for p in policies {
        snap.policies.push(serde_json::from_value::<Policy>(p).map_err(|e| FerroError::Internal(format!("corrupted policy: {e}")))?);
    }
    let flows: Vec<Value> = sqlx::query_scalar("SELECT definition FROM ferro_flows").fetch_all(&mut *conn).await?;
    for f in flows {
        snap.flows.push(serde_json::from_value::<ferro_core::flow::Flow>(f).map_err(|e| FerroError::Internal(format!("corrupted flow: {e}")))?);
    }
    snap.normalize();
    Ok(snap)
}

pub async fn load_schema(pool: &PgPool) -> Result<Schema> {
    let mut conn = pool.acquire().await?;
    Ok(Schema::new(load_snapshot(&mut conn).await?))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApplyOptions {
    /// Allow plans that drop collections/fields or change types lossy.
    #[serde(default)]
    pub allow_destructive: bool,
    /// Refuse to apply if the target snapshot hash is not this one (optimistic lock).
    #[serde(default)]
    pub expected_hash: Option<String>,
    /// Execute everything (DDL included) and roll back: validates the plan on real data.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyResult {
    pub plan: Plan,
    pub sql: Vec<String>,
    pub applied: bool,
}

/// Compute the plan and SQL to reach `desired` without touching anything.
pub async fn plan(pool: &PgPool, desired: &Snapshot) -> Result<ApplyResult> {
    let errors = validate_snapshot(desired);
    if !errors.is_empty() {
        return Err(FerroError::Invalid(format!("invalid snapshot:\n- {}", errors.join("\n- "))));
    }
    let mut conn = pool.acquire().await?;
    let current = load_snapshot(&mut conn).await?;
    let plan = diff(&current, desired);
    let sql = plan_to_sql(&plan, &current)?;
    Ok(ApplyResult { plan, sql, applied: false })
}

/// Bring the database to `desired` in a single transaction.
pub async fn apply_snapshot(pool: &PgPool, desired: Snapshot, opts: &ApplyOptions, user: Option<Uuid>) -> Result<ApplyResult> {
    let mut desired = desired;
    desired.ensure_admin();
    desired.normalize();
    let errors = validate_snapshot(&desired);
    if !errors.is_empty() {
        return Err(FerroError::Invalid(format!("invalid snapshot:\n- {}", errors.join("\n- "))));
    }
    let mut tx = pool.begin().await?;
    // an error drops `tx`, which rolls the transaction back
    let result = apply_in(&mut tx, desired, opts, user).await?;
    if opts.dry_run {
        tx.rollback().await?;
    } else {
        tx.commit().await?;
    }
    Ok(result)
}

/// The body of [`apply_snapshot`], on a plain connection (keeps the future simple for the compiler).
async fn apply_in(conn: &mut PgConnection, desired: Snapshot, opts: &ApplyOptions, user: Option<Uuid>) -> Result<ApplyResult> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(SCHEMA_LOCK).execute(&mut *conn).await?;
    let current = load_snapshot(&mut *conn).await?;
    if let Some(expected) = &opts.expected_hash {
        let actual = current.hash();
        if &actual != expected {
            return Err(FerroError::Conflict(format!(
                "the target schema changed since the plan was computed (expected {}, found {}): compute the plan again",
                &expected[..12.min(expected.len())],
                &actual[..12]
            )));
        }
    }
    let plan = diff(&current, &desired);
    if plan.has_destructive() && !opts.allow_destructive {
        let lines: Vec<String> = plan
            .changes
            .iter()
            .filter(|c| c.risk() == ferro_core::diff::Risk::Destructive)
            .map(|c| c.summary())
            .collect();
        return Err(FerroError::Conflict(format!(
            "the plan contains destructive changes (use allow_destructive):\n- {}",
            lines.join("\n- ")
        )));
    }
    let sql = plan_to_sql(&plan, &current)?;
    for statement in &sql {
        run_statement(&mut *conn, statement).await?;
    }

    let mut stored = desired.clone();
    stored.strip_hints();
    store_snapshot(&mut *conn, &stored).await?;

    if !plan.is_empty() {
        let plan_json = serde_json::to_value(&plan)?;
        let snapshot_json = serde_json::to_value(&stored)?;
        sqlx::query("INSERT INTO ferro_schema_history (id, applied_by, from_hash, to_hash, plan, snapshot) VALUES ($1, $2, $3, $4, $5, $6)")
            .bind(Uuid::now_v7())
            .bind(user)
            .bind(plan.from_hash.clone())
            .bind(plan.to_hash.clone())
            .bind(plan_json)
            .bind(snapshot_json)
            .execute(&mut *conn)
            .await?;
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(SCHEMA_CHANNEL)
            .bind(plan.to_hash.clone())
            .execute(&mut *conn)
            .await?;
    }

    let applied = !opts.dry_run && !plan.is_empty();
    Ok(ApplyResult { plan, sql, applied })
}

async fn run_statement(conn: &mut PgConnection, statement: &str) -> Result<()> {
    // one statement at a time; `raw_sql` makes the future non-Send for the compiler
    sqlx::query(statement)
        .persistent(false)
        .execute(&mut *conn)
        .await
        .map_err(|e| FerroError::Invalid(format!("schema change failed on `{statement}`: {e}")))?;
    Ok(())
}

async fn store_snapshot(conn: &mut PgConnection, s: &Snapshot) -> Result<()> {
    sqlx::query("DELETE FROM ferro_collections").execute(&mut *conn).await?;
    sqlx::query("DELETE FROM ferro_roles").execute(&mut *conn).await?;
    sqlx::query("DELETE FROM ferro_policies").execute(&mut *conn).await?;
    sqlx::query("DELETE FROM ferro_flows").execute(&mut *conn).await?;
    for c in &s.collections {
        sqlx::query("INSERT INTO ferro_collections (name, definition) VALUES ($1, $2)")
            .bind(c.name.clone())
            .bind(serde_json::to_value(c)?)
            .execute(&mut *conn)
            .await?;
    }
    for r in &s.roles {
        sqlx::query("INSERT INTO ferro_roles (id, definition) VALUES ($1, $2)")
            .bind(r.id)
            .bind(serde_json::to_value(r)?)
            .execute(&mut *conn)
            .await?;
    }
    for p in &s.policies {
        sqlx::query("INSERT INTO ferro_policies (id, definition) VALUES ($1, $2)")
            .bind(p.id)
            .bind(serde_json::to_value(p)?)
            .execute(&mut *conn)
            .await?;
    }
    for f in &s.flows {
        sqlx::query("INSERT INTO ferro_flows (id, definition) VALUES ($1, $2)")
            .bind(f.id)
            .bind(serde_json::to_value(f)?)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

pub async fn history(pool: &PgPool, limit: i64) -> Result<Value> {
    let v: Value = sqlx::query_scalar(
        "SELECT COALESCE(jsonb_agg(h ORDER BY h.applied_at DESC), '[]'::jsonb) FROM \
         (SELECT id, applied_at, applied_by, from_hash, to_hash, plan FROM ferro_schema_history ORDER BY applied_at DESC LIMIT $1) h",
    )
    .bind(limit)
    .fetch_one(pool)
    .await?;
    Ok(v)
}

/// First start: make sure the Administrator role/policy exist and create the first
/// administrator when the users table is empty.
pub async fn bootstrap(pool: &PgPool, admin_email: Option<&str>, admin_password: Option<&str>) -> Result<()> {
    let mut snap = {
        let mut conn = pool.acquire().await?;
        load_snapshot(&mut conn).await?
    };
    let mut changed = false;
    if snap.policy(ADMIN_POLICY_ID).is_none() {
        snap.policies.push(admin_policy());
        changed = true;
    }
    if snap.role(ADMIN_ROLE_ID).is_none() {
        snap.roles.push(admin_role());
        changed = true;
    }
    if changed {
        apply_snapshot(pool, snap, &ApplyOptions::default(), None).await?;
        tracing::info!("created Administrator role and policy");
    }
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM ferro_users").fetch_one(pool).await?;
    if users == 0 {
        match (admin_email, admin_password) {
            (Some(email), Some(password)) => {
                let mut conn = pool.acquire().await?;
                create_user(
                    &mut conn,
                    NewUser { email, password: Some(password), role: Some(ADMIN_ROLE_ID), first_name: Some("Admin"), last_name: None },
                )
                .await?;
                tracing::info!(email, "created first administrator");
            }
            _ => tracing::warn!("no users: set FERRO_ADMIN_EMAIL and FERRO_ADMIN_PASSWORD to create the first administrator"),
        }
    }
    Ok(())
}
