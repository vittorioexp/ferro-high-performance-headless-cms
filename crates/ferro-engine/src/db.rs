pub use sqlx::postgres::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::Result;

pub async fn connect(url: &str, max_connections: u32) -> Result<PgPool> {
    let pool = PgPoolOptions::new().max_connections(max_connections).connect(url).await?;
    Ok(pool)
}

/// Versioned DDL of the system tables. Append only: never edit a released entry.
const SYSTEM_MIGRATIONS: &[(i32, &str)] = &[(
    1,
    r#"
CREATE TABLE ferro_collections (
    name        text PRIMARY KEY,
    definition  jsonb NOT NULL
);
CREATE TABLE ferro_roles (
    id          uuid PRIMARY KEY,
    definition  jsonb NOT NULL
);
CREATE TABLE ferro_policies (
    id          uuid PRIMARY KEY,
    definition  jsonb NOT NULL
);
CREATE TABLE ferro_schema_history (
    id          uuid PRIMARY KEY,
    applied_at  timestamptz NOT NULL DEFAULT now(),
    applied_by  uuid,
    from_hash   text NOT NULL,
    to_hash     text NOT NULL,
    plan        jsonb NOT NULL,
    snapshot    jsonb NOT NULL
);
CREATE TABLE ferro_users (
    id            uuid PRIMARY KEY,
    email         varchar(255) NOT NULL UNIQUE,
    password_hash text,
    first_name    varchar(255),
    last_name     varchar(255),
    role          uuid,
    status        varchar(32) NOT NULL DEFAULT 'active',
    language      varchar(32),
    token_hash    text UNIQUE,
    last_access   timestamptz,
    date_created  timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE ferro_sessions (
    token_hash  text PRIMARY KEY,
    user_id     uuid NOT NULL REFERENCES ferro_users(id) ON DELETE CASCADE,
    expires     timestamptz NOT NULL,
    created     timestamptz NOT NULL DEFAULT now(),
    ip          text,
    user_agent  text
);
CREATE TABLE ferro_folders (
    id      uuid PRIMARY KEY,
    name    varchar(255) NOT NULL,
    parent  uuid REFERENCES ferro_folders(id) ON DELETE SET NULL DEFERRABLE INITIALLY IMMEDIATE
);
CREATE TABLE ferro_files (
    id                uuid PRIMARY KEY,
    storage           varchar(64) NOT NULL,
    filename_disk     varchar(255),
    filename_download varchar(255) NOT NULL,
    title             varchar(255),
    description       text,
    type              varchar(255),
    folder            uuid REFERENCES ferro_folders(id) ON DELETE SET NULL DEFERRABLE INITIALLY IMMEDIATE,
    filesize          bigint,
    checksum          varchar(64),
    width             integer,
    height            integer,
    tags              jsonb,
    metadata          jsonb,
    uploaded_by       uuid REFERENCES ferro_users(id) ON DELETE SET NULL DEFERRABLE INITIALLY IMMEDIATE,
    uploaded_on       timestamptz NOT NULL DEFAULT now(),
    modified_on       timestamptz
);
CREATE TABLE ferro_activity (
    id          uuid PRIMARY KEY,
    action      varchar(32) NOT NULL,
    "user"      uuid REFERENCES ferro_users(id) ON DELETE SET NULL,
    timestamp   timestamptz NOT NULL DEFAULT now(),
    ip          varchar(64),
    user_agent  text,
    collection  varchar(64) NOT NULL,
    item        varchar(255) NOT NULL,
    origin      varchar(32),
    comment     text
);
CREATE INDEX ferro_activity_item_idx ON ferro_activity (collection, item);
CREATE TABLE ferro_revisions (
    id          uuid PRIMARY KEY,
    activity    uuid NOT NULL REFERENCES ferro_activity(id) ON DELETE CASCADE,
    collection  varchar(64) NOT NULL,
    item        varchar(255) NOT NULL,
    data        jsonb,
    delta       jsonb
);
CREATE INDEX ferro_revisions_item_idx ON ferro_revisions (collection, item);
CREATE TABLE ferro_sync_state (
    collection  varchar(64) NOT NULL,
    item        uuid NOT NULL,
    source      varchar(255),
    hash        varchar(64) NOT NULL,
    synced_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (collection, item)
);
CREATE TABLE ferro_settings (
    key    text PRIMARY KEY,
    value  jsonb NOT NULL
);
"#,
), (
    2,
    r#"
CREATE TABLE ferro_flows (
    id          uuid PRIMARY KEY,
    definition  jsonb NOT NULL
);
CREATE TABLE ferro_flow_queue (
    id         uuid PRIMARY KEY,
    flow_id    uuid NOT NULL,
    trigger    jsonb NOT NULL,
    status     varchar(16) NOT NULL DEFAULT 'pending',
    attempts   integer NOT NULL DEFAULT 0,
    created    timestamptz NOT NULL DEFAULT now(),
    started    timestamptz,
    run_at     timestamptz NOT NULL DEFAULT now(),
    dedupe_key text UNIQUE
);
CREATE INDEX ferro_flow_queue_pending_idx ON ferro_flow_queue (status, run_at);
CREATE TABLE ferro_flow_runs (
    id         uuid PRIMARY KEY,
    flow_id    uuid NOT NULL,
    flow_name  text,
    source     varchar(16) NOT NULL,
    status     varchar(16) NOT NULL,
    started    timestamptz NOT NULL,
    finished   timestamptz NOT NULL,
    trigger    jsonb,
    log        jsonb,
    error      text
);
CREATE INDEX ferro_flow_runs_flow_idx ON ferro_flow_runs (flow_id, started DESC);
"#,
), (
    3,
    r#"
ALTER TABLE ferro_users
    ADD COLUMN provider            varchar(64),
    ADD COLUMN external_identifier varchar(255),
    ADD COLUMN tfa_secret          text,
    ADD COLUMN tfa_last_step       bigint,
    ADD COLUMN tfa_recovery        jsonb,
    ADD COLUMN tfa_enabled         boolean GENERATED ALWAYS AS (tfa_secret IS NOT NULL) STORED;
CREATE UNIQUE INDEX ferro_users_external_idx ON ferro_users (provider, external_identifier) WHERE external_identifier IS NOT NULL;
CREATE TABLE ferro_auth_tokens (
    token_hash  text PRIMARY KEY,
    user_id     uuid NOT NULL REFERENCES ferro_users(id) ON DELETE CASCADE,
    kind        varchar(16) NOT NULL,
    expires     timestamptz NOT NULL,
    created     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ferro_auth_tokens_user_idx ON ferro_auth_tokens (user_id, kind);
"#,
)];

/// Create / upgrade the system tables. Safe to run concurrently (advisory lock).
pub async fn migrate_system(pool: &PgPool) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(7466524)").execute(&mut *tx).await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ferro_system_migrations (version integer PRIMARY KEY, applied_at timestamptz NOT NULL DEFAULT now())",
    )
    .execute(&mut *tx)
    .await?;
    let current: Option<i32> = sqlx::query_scalar("SELECT max(version) FROM ferro_system_migrations")
        .fetch_one(&mut *tx)
        .await?;
    let current = current.unwrap_or(0);
    for (version, sql) in SYSTEM_MIGRATIONS {
        if *version <= current {
            continue;
        }
        tracing::info!(version, "applying system migration");
        sqlx::raw_sql(sql).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO ferro_system_migrations (version) VALUES ($1)")
            .bind(*version)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
