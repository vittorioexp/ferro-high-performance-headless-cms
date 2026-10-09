use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use ferro_core::system::USERS;
use ferro_engine::auth::{self, NewUser};
use ferro_engine::items::{self, Ctx};
use ferro_engine::{Accountability, FerroError};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use super::items::{field_tree, list_collection, ItemsQuery};
use crate::extract::Acc;
use crate::{ApiError, ApiResult, AppState};

pub async fn list(State(st): State<AppState>, Acc(acc): Acc, Query(q): Query<ItemsQuery>) -> ApiResult<Json<Value>> {
    list_collection(&st, acc, USERS, &q).await
}

/// `GET /users/:id` — `me` returns the caller (always allowed when authenticated).
pub async fn get_one(State(st): State<AppState>, Acc(acc): Acc, Path(id): Path<String>, Query(q): Query<ItemsQuery>) -> ApiResult<Json<Value>> {
    let mut conn = st.pool.acquire().await?;
    if id == "me" {
        let user = acc.user.ok_or_else(|| FerroError::Unauthorized("not authenticated".into()))?;
        let ctx = Ctx::new(st.schema.get(), Accountability::system());
        let mut me = items::read_one(&ctx, &mut conn, USERS, &user.to_string(), field_tree(q.fields.as_deref())).await?;
        me["admin_access"] = json!(acc.admin);
        me["app_access"] = json!(acc.app);
        return Ok(Json(json!({ "data": me })));
    }
    let ctx = Ctx::new(st.schema.get(), acc);
    let user = items::read_one(&ctx, &mut conn, USERS, &id, field_tree(q.fields.as_deref())).await?;
    Ok(Json(json!({ "data": user })))
}

#[derive(Deserialize)]
pub struct CreateUser {
    email: String,
    password: Option<String>,
    role: Option<Uuid>,
    first_name: Option<String>,
    last_name: Option<String>,
}

fn check_role(st: &AppState, role: Option<Uuid>) -> ApiResult<()> {
    if let Some(r) = role {
        if st.schema.get().role(r).is_none() {
            return Err(ApiError::bad_request(format!("role {r} does not exist")));
        }
    }
    Ok(())
}

pub async fn create(State(st): State<AppState>, Acc(acc): Acc, Json(body): Json<CreateUser>) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    check_role(&st, body.role)?;
    let mut conn = st.pool.acquire().await?;
    let id = auth::create_user(
        &mut conn,
        NewUser {
            email: &body.email,
            password: body.password.as_deref(),
            role: body.role,
            first_name: body.first_name.as_deref(),
            last_name: body.last_name.as_deref(),
        },
    )
    .await?;
    let ctx = Ctx::new(st.schema.get(), acc);
    let user = items::read_one(&ctx, &mut conn, USERS, &id.to_string(), field_tree(None)).await?;
    Ok(Json(json!({ "data": user })))
}

/// Distinguish an absent field (`None`) from an explicit `null` (`Some(None)`).
fn double_option<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<Uuid>>, D::Error> {
    Option::<Uuid>::deserialize(d).map(Some)
}

#[derive(Deserialize)]
pub struct UpdateUser {
    email: Option<String>,
    password: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    role: Option<Option<Uuid>>,
    status: Option<String>,
    first_name: Option<String>,
    last_name: Option<String>,
    language: Option<String>,
}

pub async fn update(State(st): State<AppState>, Acc(acc): Acc, Path(id): Path<Uuid>, Json(body): Json<UpdateUser>) -> ApiResult<Json<Value>> {
    let own = acc.user == Some(id);
    if !acc.admin && !own {
        return Err(FerroError::forbidden("you can only update your own user").into());
    }
    if !acc.admin && (body.role.is_some() || body.status.is_some() || body.email.is_some()) {
        return Err(FerroError::forbidden("only administrators can change email, role or status").into());
    }
    if let Some(r) = &body.role {
        check_role(&st, *r)?;
    }
    if let Some(s) = &body.status {
        if !matches!(s.as_str(), "active" | "suspended" | "invited") {
            return Err(ApiError::bad_request("status must be active, suspended or invited"));
        }
    }
    let mut tx = st.pool.begin().await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM ferro_users WHERE id = $1)")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    if !exists {
        return Err(FerroError::not_found("user not found").into());
    }
    if let Some(v) = &body.email {
        sqlx::query("UPDATE ferro_users SET email = $2 WHERE id = $1").bind(id).bind(v).execute(&mut *tx).await?;
    }
    if let Some(v) = &body.first_name {
        sqlx::query("UPDATE ferro_users SET first_name = $2 WHERE id = $1").bind(id).bind(v).execute(&mut *tx).await?;
    }
    if let Some(v) = &body.last_name {
        sqlx::query("UPDATE ferro_users SET last_name = $2 WHERE id = $1").bind(id).bind(v).execute(&mut *tx).await?;
    }
    if let Some(v) = &body.language {
        sqlx::query("UPDATE ferro_users SET language = $2 WHERE id = $1").bind(id).bind(v).execute(&mut *tx).await?;
    }
    if let Some(v) = &body.role {
        sqlx::query("UPDATE ferro_users SET role = $2 WHERE id = $1").bind(id).bind(*v).execute(&mut *tx).await?;
    }
    if let Some(v) = &body.status {
        sqlx::query("UPDATE ferro_users SET status = $2 WHERE id = $1").bind(id).bind(v).execute(&mut *tx).await?;
        if v != "active" {
            sqlx::query("DELETE FROM ferro_sessions WHERE user_id = $1").bind(id).execute(&mut *tx).await?;
        }
    }
    if let Some(p) = &body.password {
        if p.len() < 8 {
            return Err(ApiError::bad_request("password must be at least 8 characters"));
        }
        auth::set_password(&mut tx, id, p).await?;
    }
    tx.commit().await?;
    let mut conn = st.pool.acquire().await?;
    let ctx = Ctx::new(st.schema.get(), Accountability::system());
    let user = items::read_one(&ctx, &mut conn, USERS, &id.to_string(), field_tree(None)).await?;
    Ok(Json(json!({ "data": user })))
}

pub async fn remove(State(st): State<AppState>, Acc(acc): Acc, Path(id): Path<Uuid>) -> ApiResult<StatusCode> {
    acc.require_admin()?;
    if acc.user == Some(id) {
        return Err(ApiError::bad_request("you cannot delete yourself"));
    }
    let n = sqlx::query("DELETE FROM ferro_users WHERE id = $1").bind(id).execute(&st.pool).await?.rows_affected();
    if n == 0 {
        return Err(FerroError::not_found("user not found").into());
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Generate a static token (for CI and `ferro` CLI). Returned only once.
pub async fn rotate_token(State(st): State<AppState>, Acc(acc): Acc, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    if !acc.admin && acc.user != Some(id) {
        return Err(FerroError::forbidden("forbidden").into());
    }
    let mut conn = st.pool.acquire().await?;
    let token = auth::rotate_static_token(&mut conn, id).await?;
    Ok(Json(json!({ "data": { "token": token } })))
}

#[derive(Deserialize)]
pub struct InviteBody {
    email: String,
    role: Option<Uuid>,
    /// Where the link in the email should point (an allowed origin); default: the admin app.
    invite_url: Option<String>,
}

/// Invite someone by email: they get a link to choose a password. Without outgoing email configured the link is
/// returned to the administrator, who can pass it on by other means.
pub async fn invite(State(st): State<AppState>, Acc(acc): Acc, Json(body): Json<InviteBody>) -> ApiResult<Json<Value>> {
    acc.require_admin()?;
    check_role(&st, body.role)?;
    let base = st.link_base(body.invite_url.as_deref(), "/admin/#/invite")?;
    let mut tx = st.pool.begin().await?;
    let invitation = auth::invite_user(&mut tx, &body.email, body.role).await?;
    tx.commit().await?;
    tracing::info!(target: "ferro::audit", admin = ?acc.user, user = %invitation.user, "user invited");
    let link = crate::with_token(&base, &invitation.token);
    let mut delivery = "link";
    if st.mailer.enabled() {
        let m = ferro_identity::mail::invitation_message(&st.config.tfa_issuer, &link);
        match st.mailer.send(body.email.trim(), &m.subject, &m.text, &m.html).await {
            Ok(()) => delivery = "email",
            Err(e) => tracing::error!(error = %e, "invitation email failed: the link is returned to the administrator instead"),
        }
    }
    let mut data = json!({ "id": invitation.user, "email": body.email.trim(), "status": "invited", "created": invitation.created, "delivery": delivery });
    if delivery == "link" {
        data["invite_url"] = json!(link);
    }
    Ok(Json(json!({ "data": data })))
}

/// An administrator removes a user's second factor (lost phone, no recovery codes).
pub async fn reset_tfa(State(st): State<AppState>, Acc(acc): Acc, Path(id): Path<Uuid>) -> ApiResult<StatusCode> {
    acc.require_admin()?;
    let mut conn = st.pool.acquire().await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM ferro_users WHERE id = $1)").bind(id).fetch_one(&mut *conn).await?;
    if !exists {
        return Err(FerroError::not_found("user not found").into());
    }
    auth::tfa_reset(&mut conn, id).await?;
    tracing::info!(target: "ferro::audit", admin = ?acc.user, user = %id, "two-factor authentication removed by an administrator");
    Ok(StatusCode::NO_CONTENT)
}
