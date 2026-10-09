use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::permissions::Accountability;
use crate::Result;

pub const ORIGIN_API: &str = "api";
pub const ORIGIN_SYNC: &str = "sync";
pub const ORIGIN_SCHEMA: &str = "schema";

/// Record an activity row and, if `data`/`delta` are given, a revision.
#[allow(clippy::too_many_arguments)]
pub async fn record(
    conn: &mut PgConnection,
    acc: &Accountability,
    action: &str,
    collection: &str,
    item: &str,
    origin: &str,
    data: Option<&Value>,
    delta: Option<&Value>,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO ferro_activity (id, action, \"user\", ip, user_agent, collection, item, origin) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(id)
    .bind(action)
    .bind(acc.user)
    .bind(acc.ip.as_deref())
    .bind(acc.user_agent.as_deref())
    .bind(collection)
    .bind(item)
    .bind(origin)
    .execute(&mut *conn)
    .await?;
    if data.is_some() || delta.is_some() {
        sqlx::query("INSERT INTO ferro_revisions (id, activity, collection, item, data, delta) VALUES ($1, $2, $3, $4, $5, $6)")
            .bind(Uuid::now_v7())
            .bind(id)
            .bind(collection)
            .bind(item)
            .bind(data.cloned())
            .bind(delta.cloned())
            .execute(&mut *conn)
            .await?;
    }
    Ok(id)
}
