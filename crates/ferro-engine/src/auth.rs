//! Passwords (argon2id), access tokens (JWT HS256), refresh sessions and static
//! tokens (for CI / environment-to-environment sync). Only hashes are stored.

use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use ferro_core::hash::sha256_hex;

use crate::permissions::Accountability;
use crate::tfa;
use crate::schema::Schema;
use crate::{FerroError, Result};

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub secret: String,
    pub access_ttl_secs: i64,
    pub refresh_ttl_secs: i64,
    /// Key material for secrets stored in the database (two-factor seeds). Defaults to `secret`; set a separate
    /// one if you want to be able to rotate the JWT secret without re-enrolling everybody.
    pub encryption_key: String,
    /// Name shown in the authenticator app.
    pub issuer: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: String,
    exp: i64,
    iat: i64,
    iss: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Access token lifetime in milliseconds.
    pub expires: i64,
}

pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| FerroError::Internal(format!("password hashing failed: {e}")))
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok(),
        Err(_) => false,
    }
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn issue_access_token(cfg: &AuthConfig, user: Uuid) -> Result<String> {
    let now = Utc::now().timestamp();
    let claims = Claims { sub: user.to_string(), iat: now, exp: now + cfg.access_ttl_secs, iss: "ferro".into() };
    encode(&Header::default(), &claims, &EncodingKey::from_secret(cfg.secret.as_bytes()))
        .map_err(|e| FerroError::Internal(format!("token encoding failed: {e}")))
}

fn decode_access_token(cfg: &AuthConfig, token: &str) -> Result<Uuid> {
    let mut validation = Validation::default();
    validation.set_issuer(&["ferro"]);
    validation.leeway = 5;
    let data = decode::<Claims>(token, &DecodingKey::from_secret(cfg.secret.as_bytes()), &validation)
        .map_err(|e| FerroError::Unauthorized(format!("invalid token: {e}")))?;
    Uuid::parse_str(&data.claims.sub).map_err(|_| FerroError::Unauthorized("invalid token subject".into()))
}

async fn new_session(conn: &mut PgConnection, cfg: &AuthConfig, user: Uuid, ip: Option<&str>, ua: Option<&str>) -> Result<Tokens> {
    let refresh = random_token();
    let expires = Utc::now() + Duration::seconds(cfg.refresh_ttl_secs);
    sqlx::query("INSERT INTO ferro_sessions (token_hash, user_id, expires, ip, user_agent) VALUES ($1, $2, $3, $4, $5)")
        .bind(sha256_hex(refresh.as_bytes()))
        .bind(user)
        .bind(expires)
        .bind(ip)
        .bind(ua)
        .execute(&mut *conn)
        .await?;
    sqlx::query("UPDATE ferro_users SET last_access = now() WHERE id = $1").bind(user).execute(&mut *conn).await?;
    Ok(Tokens { access_token: issue_access_token(cfg, user)?, refresh_token: refresh, expires: cfg.access_ttl_secs * 1000 })
}

pub async fn refresh(pool: &PgPool, cfg: &AuthConfig, refresh_token: &str) -> Result<Tokens> {
    let mut tx = pool.begin().await?;
    let hash = sha256_hex(refresh_token.as_bytes());
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "DELETE FROM ferro_sessions s USING ferro_users u WHERE s.token_hash = $1 AND s.user_id = u.id AND s.expires > now() RETURNING u.id, u.status",
    )
    .bind(hash.clone())
    .fetch_optional(&mut *tx)
    .await?;
    let Some((user, status)) = row else {
        return Err(FerroError::Unauthorized("invalid or expired refresh token".into()));
    };
    if status != "active" {
        return Err(FerroError::Unauthorized("user is not active".into()));
    }
    let tokens = new_session(&mut tx, cfg, user, None, None).await?;
    tx.commit().await?;
    Ok(tokens)
}

pub async fn logout(pool: &PgPool, refresh_token: &str) -> Result<()> {
    sqlx::query("DELETE FROM ferro_sessions WHERE token_hash = $1")
        .bind(sha256_hex(refresh_token.as_bytes()))
        .execute(pool)
        .await?;
    Ok(())
}

/// Resolve the caller from a bearer token (JWT or static token).
pub async fn accountability(pool: &PgPool, schema: &Schema, cfg: &AuthConfig, token: Option<&str>) -> Result<Accountability> {
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        return Ok(Accountability::public(schema.public_policies()));
    };
    let looks_like_jwt = token.split('.').count() == 3;
    let row: Option<(Uuid, Option<Uuid>, String)> = if looks_like_jwt {
        let user = decode_access_token(cfg, token)?;
        sqlx::query_as("SELECT id, role, status FROM ferro_users WHERE id = $1")
            .bind(user)
            .fetch_optional(pool)
            .await?
    } else {
        sqlx::query_as("SELECT id, role, status FROM ferro_users WHERE token_hash = $1")
            .bind(sha256_hex(token.as_bytes()))
            .fetch_optional(pool)
            .await?
    };
    let Some((user, role, status)) = row else {
        return Err(FerroError::Unauthorized("invalid token".into()));
    };
    if status != "active" {
        return Err(FerroError::Unauthorized("user is not active".into()));
    }
    let mut policies = schema.public_policies();
    if let Some(r) = role.and_then(|r| schema.role(r)) {
        for pid in &r.policies {
            if let Some(p) = schema.policy(*pid) {
                if !policies.iter().any(|x| x.id == p.id) {
                    policies.push(p.clone());
                }
            }
        }
    }
    Ok(Accountability::from_policies(user, role, policies))
}

pub struct NewUser<'a> {
    pub email: &'a str,
    pub password: Option<&'a str>,
    pub role: Option<Uuid>,
    pub first_name: Option<&'a str>,
    pub last_name: Option<&'a str>,
}

pub async fn create_user(conn: &mut PgConnection, u: NewUser<'_>) -> Result<Uuid> {
    let id = Uuid::now_v7();
    let hash = match u.password {
        Some(p) => {
            validate_new_password(p)?;
            Some(hash_password(p)?)
        }
        None => None,
    };
    sqlx::query("INSERT INTO ferro_users (id, email, password_hash, role, first_name, last_name) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(id)
        .bind(u.email)
        .bind(hash)
        .bind(u.role)
        .bind(u.first_name)
        .bind(u.last_name)
        .execute(&mut *conn)
        .await?;
    Ok(id)
}

pub async fn set_password(conn: &mut PgConnection, user: Uuid, password: &str) -> Result<()> {
    validate_new_password(password)?;
    sqlx::query("UPDATE ferro_users SET password_hash = $2 WHERE id = $1")
        .bind(user)
        .bind(hash_password(password)?)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM ferro_sessions WHERE user_id = $1").bind(user).execute(&mut *conn).await?;
    Ok(())
}

pub async fn user_id_by_email(conn: &mut PgConnection, email: &str) -> Result<Option<Uuid>> {
    Ok(sqlx::query_scalar::<sqlx::Postgres, Uuid>("SELECT id FROM ferro_users WHERE lower(email) = lower($1)")
        .bind(email)
        .fetch_optional(&mut *conn)
        .await?)
}

/// Generate a new static token for a user. The plain token is returned once.
pub async fn rotate_static_token(conn: &mut PgConnection, user: Uuid) -> Result<String> {
    let token = format!("ferro_{}", random_token());
    let n = sqlx::query("UPDATE ferro_users SET token_hash = $2 WHERE id = $1")
        .bind(user)
        .bind(sha256_hex(token.as_bytes()))
        .execute(&mut *conn)
        .await?
        .rows_affected();
    if n == 0 {
        return Err(FerroError::not_found("user not found"));
    }
    Ok(token)
}

// ---- login with an optional second factor ------------------------------------------------------------------

/// Shortest password accepted anywhere (the CLI, the API, reset and invitations).
pub const MIN_PASSWORD_LEN: usize = 8;
/// Longest: argon2 cost grows with the input, so an unbounded password is a cheap way to burn CPU.
pub const MAX_PASSWORD_LEN: usize = 1024;

pub fn validate_new_password(p: &str) -> Result<()> {
    if p.chars().count() < MIN_PASSWORD_LEN {
        return Err(FerroError::invalid(format!("password must be at least {MIN_PASSWORD_LEN} characters")));
    }
    if p.len() > MAX_PASSWORD_LEN {
        return Err(FerroError::invalid(format!("password must be at most {MAX_PASSWORD_LEN} bytes")));
    }
    Ok(())
}

struct LoginRow {
    id: Uuid,
    password_hash: Option<String>,
    status: String,
    tfa_secret: Option<String>,
    tfa_last_step: Option<i64>,
}

pub async fn login(pool: &PgPool, cfg: &AuthConfig, email: &str, password: &str, otp: Option<&str>, ip: Option<&str>, ua: Option<&str>) -> Result<Tokens> {
    let mut conn = pool.acquire().await?;
    let row: Option<(Uuid, Option<String>, String, Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT id, password_hash, status, tfa_secret, tfa_last_step FROM ferro_users WHERE lower(email) = lower($1)",
    )
    .bind(email)
    .fetch_optional(&mut *conn)
    .await?;
    let invalid = || FerroError::Unauthorized("invalid credentials".into());
    let Some((id, password_hash, status, tfa_secret, tfa_last_step)) = row else {
        // same work for unknown accounts: the response time does not tell which emails exist
        let _ = hash_password(password);
        return Err(invalid());
    };
    let user = LoginRow { id, password_hash, status, tfa_secret, tfa_last_step };
    if user.status != "active" {
        return Err(invalid());
    }
    match &user.password_hash {
        Some(h) if verify_password(password, h) => {}
        _ => return Err(invalid()),
    }
    if user.tfa_secret.is_some() {
        second_factor(&mut conn, cfg, &user, otp).await?;
    }
    new_session(&mut conn, cfg, user.id, ip, ua).await
}

/// Check the one-time code (or a recovery code) of a user who has two-factor authentication on.
async fn second_factor(conn: &mut PgConnection, cfg: &AuthConfig, user: &LoginRow, otp: Option<&str>) -> Result<()> {
    let Some(code) = otp.map(str::trim).filter(|c| !c.is_empty()) else {
        return Err(FerroError::AuthCode("OTP_REQUIRED", "a two-factor code is required".into()));
    };
    let bad = || FerroError::AuthCode("INVALID_OTP", "invalid two-factor code".into());
    if tfa::looks_like_recovery_code(code) {
        // single use: removing the hash from the list is the check (atomic, so two logins cannot both spend it)
        let hash = tfa::hash_recovery_code(code);
        let spent = sqlx::query(
            "UPDATE ferro_users SET tfa_recovery = tfa_recovery - $2::text WHERE id = $1 AND tfa_recovery ? $2::text",
        )
        .bind(user.id)
        .bind(&hash)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        return if spent == 1 { Ok(()) } else { Err(bad()) };
    }
    let secret = tfa::open(&cfg.encryption_key, user.tfa_secret.as_deref().unwrap_or_default())?;
    let Some(step) = tfa::verify(&secret, code, Utc::now().timestamp().max(0) as u64, user.tfa_last_step) else {
        return Err(bad());
    };
    // the step is recorded atomically: a concurrent login with the same code finds it already used
    let marked = sqlx::query("UPDATE ferro_users SET tfa_last_step = $2 WHERE id = $1 AND (tfa_last_step IS NULL OR tfa_last_step < $2)")
        .bind(user.id)
        .bind(step)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    if marked == 1 { Ok(()) } else { Err(bad()) }
}

/// Re-check the password of a signed-in user before a sensitive change. Users without a password (single sign-on,
/// directory accounts) have none to confirm: their session is the proof.
pub async fn confirm_password(conn: &mut PgConnection, user: Uuid, password: Option<&str>) -> Result<()> {
    let hash: Option<Option<String>> = sqlx::query_scalar("SELECT password_hash FROM ferro_users WHERE id = $1")
        .bind(user)
        .fetch_optional(&mut *conn)
        .await?;
    match hash {
        None => Err(FerroError::not_found("user not found")),
        Some(None) => Ok(()),
        Some(Some(h)) => match password {
            Some(p) if verify_password(p, &h) => Ok(()),
            _ => Err(FerroError::forbidden("the current password is required and was not correct")),
        },
    }
}

pub struct TfaEnrollment {
    pub secret: String,
    pub otpauth_url: String,
    pub qr_svg: String,
}

/// Step 1 of enrolment: a new secret to put in the authenticator app. Nothing is stored yet.
pub async fn tfa_begin(conn: &mut PgConnection, cfg: &AuthConfig, user: Uuid) -> Result<TfaEnrollment> {
    let row: Option<(String, Option<String>)> = sqlx::query_as("SELECT email, tfa_secret FROM ferro_users WHERE id = $1")
        .bind(user)
        .fetch_optional(&mut *conn)
        .await?;
    let Some((email, existing)) = row else { return Err(FerroError::not_found("user not found")) };
    if existing.is_some() {
        return Err(FerroError::Conflict("two-factor authentication is already enabled: disable it first".into()));
    }
    let secret = tfa::generate_secret();
    let otpauth_url = tfa::otpauth_url(&cfg.issuer, &email, &secret);
    let qr_svg = tfa::qr_svg(&otpauth_url)?;
    Ok(TfaEnrollment { secret, otpauth_url, qr_svg })
}

/// Step 2: the user proves the app works by typing a code; the secret is stored (encrypted) and the recovery
/// codes are returned, once.
pub async fn tfa_enable(conn: &mut PgConnection, cfg: &AuthConfig, user: Uuid, secret: &str, otp: &str) -> Result<Vec<String>> {
    let Some(step) = tfa::verify(secret, otp, Utc::now().timestamp().max(0) as u64, None) else {
        return Err(FerroError::AuthCode("INVALID_OTP", "invalid two-factor code".into()));
    };
    if tfa::base32_decode(secret).map(|b| b.len() < 10).unwrap_or(true) {
        return Err(FerroError::invalid("invalid secret"));
    }
    let codes = tfa::generate_recovery_codes(10);
    let hashes: Vec<String> = codes.iter().map(|c| tfa::hash_recovery_code(c)).collect();
    let done = sqlx::query(
        "UPDATE ferro_users SET tfa_secret = $2, tfa_last_step = $3, tfa_recovery = $4 WHERE id = $1 AND tfa_secret IS NULL",
    )
    .bind(user)
    .bind(tfa::seal(&cfg.encryption_key, secret)?)
    .bind(step)
    .bind(serde_json::json!(hashes))
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if done == 0 {
        return Err(FerroError::Conflict("two-factor authentication is already enabled".into()));
    }
    Ok(codes)
}

/// Turn two-factor off after the user proved they still hold the device (or a recovery code).
pub async fn tfa_disable(conn: &mut PgConnection, cfg: &AuthConfig, user: Uuid, otp: &str) -> Result<()> {
    let row: Option<(Option<String>, Option<i64>)> = sqlx::query_as("SELECT tfa_secret, tfa_last_step FROM ferro_users WHERE id = $1")
        .bind(user)
        .fetch_optional(&mut *conn)
        .await?;
    let Some((Some(secret), last)) = row else {
        return Err(FerroError::Conflict("two-factor authentication is not enabled".into()));
    };
    let login_row = LoginRow { id: user, password_hash: None, status: "active".into(), tfa_secret: Some(secret), tfa_last_step: last };
    second_factor(conn, cfg, &login_row, Some(otp)).await?;
    tfa_reset(conn, user).await
}

/// Remove the second factor (also used by administrators when someone lost their device).
pub async fn tfa_reset(conn: &mut PgConnection, user: Uuid) -> Result<()> {
    sqlx::query("UPDATE ferro_users SET tfa_secret = NULL, tfa_last_step = NULL, tfa_recovery = NULL WHERE id = $1")
        .bind(user)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

// ---- single-use tokens: password reset, invitation, single sign-on hand-over -----------------------------------------

pub const TOKEN_RESET: &str = "reset";
pub const TOKEN_INVITE: &str = "invite";
pub const TOKEN_SSO: &str = "sso";

pub const RESET_TTL_SECS: i64 = 3600;
pub const INVITE_TTL_SECS: i64 = 7 * 24 * 3600;
pub const SSO_CODE_TTL_SECS: i64 = 60;

/// Create a token for `user`; only its hash is stored. The plain value goes to the user (e-mail, redirect).
pub async fn issue_token(conn: &mut PgConnection, user: Uuid, kind: &str, ttl_secs: i64) -> Result<String> {
    let token = random_token();
    sqlx::query("INSERT INTO ferro_auth_tokens (token_hash, user_id, kind, expires) VALUES ($1, $2, $3, now() + make_interval(secs => $4))")
        .bind(sha256_hex(token.as_bytes()))
        .bind(user)
        .bind(kind)
        .bind(ttl_secs as f64)
        .execute(&mut *conn)
        .await?;
    // housekeeping: expired tokens are useless
    sqlx::query("DELETE FROM ferro_auth_tokens WHERE expires < now() - interval '1 day'").execute(&mut *conn).await?;
    Ok(token)
}

/// Spend a token: it is deleted as it is read, so it works exactly once even under concurrent requests.
pub async fn consume_token(conn: &mut PgConnection, token: &str, kind: &str) -> Result<Option<Uuid>> {
    Ok(sqlx::query_scalar::<sqlx::Postgres, Uuid>(
        "DELETE FROM ferro_auth_tokens WHERE token_hash = $1 AND kind = $2 AND expires > now() RETURNING user_id",
    )
    .bind(sha256_hex(token.trim().as_bytes()))
    .bind(kind)
    .fetch_optional(&mut *conn)
    .await?)
}

pub async fn purge_tokens(conn: &mut PgConnection, user: Uuid, kind: &str) -> Result<()> {
    sqlx::query("DELETE FROM ferro_auth_tokens WHERE user_id = $1 AND kind = $2").bind(user).bind(kind).execute(&mut *conn).await?;
    Ok(())
}

/// A user who may reset their password this way: active and signing in with a Ferro password (accounts that sign
/// in through an identity provider or a directory have nothing to reset here).
pub struct ResetTarget {
    pub user: Uuid,
    pub email: String,
    pub first_name: Option<String>,
    pub token: String,
}

/// `Ok(None)` for unknown / ineligible addresses: the caller answers the same either way.
pub async fn start_password_reset(conn: &mut PgConnection, email: &str) -> Result<Option<ResetTarget>> {
    let row: Option<(Uuid, String, Option<String>)> = sqlx::query_as(
        "SELECT id, email, first_name FROM ferro_users WHERE lower(email) = lower($1) AND status = 'active' AND provider IS NULL",
    )
    .bind(email.trim())
    .fetch_optional(&mut *conn)
    .await?;
    let Some((user, email, first_name)) = row else { return Ok(None) };
    purge_tokens(conn, user, TOKEN_RESET).await?; // only the latest link works
    let token = issue_token(conn, user, TOKEN_RESET, RESET_TTL_SECS).await?;
    Ok(Some(ResetTarget { user, email, first_name, token }))
}

pub async fn finish_password_reset(pool: &PgPool, token: &str, password: &str) -> Result<Uuid> {
    validate_new_password(password)?;
    let mut tx = pool.begin().await?;
    let Some(user) = consume_token(&mut tx, token, TOKEN_RESET).await? else {
        return Err(FerroError::Unauthorized("this link is invalid or has expired".into()));
    };
    let active: bool = sqlx::query_scalar("SELECT status = 'active' FROM ferro_users WHERE id = $1").bind(user).fetch_one(&mut *tx).await?;
    if !active {
        return Err(FerroError::Unauthorized("this link is invalid or has expired".into()));
    }
    set_password(&mut tx, user, password).await?; // also ends every session
    purge_tokens(&mut tx, user, TOKEN_RESET).await?;
    tx.commit().await?;
    Ok(user)
}

pub struct Invitation {
    pub user: Uuid,
    pub token: String,
    pub created: bool,
}

/// Invite an address: creates the user as `invited` (or renews the invitation of one who has not accepted yet).
/// Active and suspended accounts are not touched.
pub async fn invite_user(conn: &mut PgConnection, email: &str, role: Option<Uuid>) -> Result<Invitation> {
    let email = email.trim();
    if !email.contains('@') || email.len() > 255 || email.contains(char::is_whitespace) {
        return Err(FerroError::invalid("a valid email address is required"));
    }
    let existing: Option<(Uuid, String)> = sqlx::query_as("SELECT id, status FROM ferro_users WHERE lower(email) = lower($1)")
        .bind(email)
        .fetch_optional(&mut *conn)
        .await?;
    let (user, created) = match existing {
        Some((id, status)) if status == "invited" => {
            if role.is_some() {
                sqlx::query("UPDATE ferro_users SET role = $2 WHERE id = $1").bind(id).bind(role).execute(&mut *conn).await?;
            }
            purge_tokens(conn, id, TOKEN_INVITE).await?;
            (id, false)
        }
        Some(_) => return Err(FerroError::Conflict("a user with this email already exists".into())),
        None => {
            let id = Uuid::now_v7();
            sqlx::query("INSERT INTO ferro_users (id, email, role, status) VALUES ($1, $2, $3, 'invited')")
                .bind(id)
                .bind(email)
                .bind(role)
                .execute(&mut *conn)
                .await?;
            (id, true)
        }
    };
    let token = issue_token(conn, user, TOKEN_INVITE, INVITE_TTL_SECS).await?;
    Ok(Invitation { user, token, created })
}

/// The invitee chooses a password: the account becomes active and they are signed in.
pub async fn accept_invitation(pool: &PgPool, cfg: &AuthConfig, token: &str, password: &str, ip: Option<&str>, ua: Option<&str>) -> Result<Tokens> {
    validate_new_password(password)?;
    let mut tx = pool.begin().await?;
    let gone = || FerroError::Unauthorized("this invitation is invalid or has expired".into());
    let user = consume_token(&mut tx, token, TOKEN_INVITE).await?.ok_or_else(gone)?;
    let done = sqlx::query("UPDATE ferro_users SET password_hash = $2, status = 'active' WHERE id = $1 AND status = 'invited'")
        .bind(user)
        .bind(hash_password(password)?)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if done == 0 {
        return Err(gone());
    }
    let tokens = new_session(&mut tx, cfg, user, ip, ua).await?;
    tx.commit().await?;
    Ok(tokens)
}

// ---- accounts that sign in elsewhere (identity provider, directory) ----------------------------------------------------

/// Who the outside world says this person is.
#[derive(Debug, Clone)]
pub struct ExternalProfile {
    /// Name of the provider as configured (`azure`, `ldap`, ...).
    pub provider: String,
    /// Stable identifier at the provider (`sub`, directory GUID/DN): never the email, which can change hands.
    pub identifier: String,
    pub email: String,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExternalPolicy {
    pub default_role: Option<Uuid>,
    /// Create the account on first sign-in when none exists.
    pub allow_registration: bool,
    /// Let an existing local (password) account be taken over by this provider on a matching email.
    pub link_existing: bool,
}

/// Find or create the Ferro user for an externally authenticated person. Never trusts the email alone to reach an
/// account that belongs to someone else: see the cases below.
pub async fn resolve_external_user(conn: &mut PgConnection, p: &ExternalProfile, policy: &ExternalPolicy) -> Result<Uuid> {
    // Two first sign-ins of the same person at the same moment both find nothing and both try to create the account:
    // the second insert is a no-op, and a second pass finds the account the first one made.
    for _ in 0..2 {
        if let Some(id) = resolve_external_user_once(conn, p, policy).await? {
            return Ok(id);
        }
    }
    Err(FerroError::Unauthorized("this account is not allowed to sign in this way".into()))
}

async fn resolve_external_user_once(conn: &mut PgConnection, p: &ExternalProfile, policy: &ExternalPolicy) -> Result<Option<Uuid>> {
    let denied = |why: &str| {
        tracing::warn!(provider = %p.provider, email = %p.email, "external sign-in refused: {why}");
        FerroError::Unauthorized("this account is not allowed to sign in this way".into())
    };
    if !p.email.contains('@') || p.email.len() > 255 || p.identifier.is_empty() || p.identifier.len() > 255 {
        return Err(denied("the provider returned no usable email / identifier"));
    }
    // 1. already known by the provider's identifier
    let bound: Option<(Uuid, String)> = sqlx::query_as("SELECT id, status FROM ferro_users WHERE provider = $1 AND external_identifier = $2")
        .bind(&p.provider)
        .bind(&p.identifier)
        .fetch_optional(&mut *conn)
        .await?;
    if let Some((id, status)) = bound {
        return if status == "active" { Ok(Some(id)) } else { Err(denied("the account is not active")) };
    }
    // 2. known by email
    let by_email: Option<(Uuid, String, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT id, status, provider, external_identifier FROM ferro_users WHERE lower(email) = lower($1)")
            .bind(&p.email)
            .fetch_optional(&mut *conn)
            .await?;
    if let Some((id, status, provider, identifier)) = by_email {
        // The same person arriving twice at once: the first request created the account between our two lookups.
        if provider.as_deref() == Some(p.provider.as_str()) && identifier.as_deref() == Some(p.identifier.as_str()) {
            return if status == "active" { Ok(Some(id)) } else { Err(denied("the account is not active")) };
        }
        let preprovisioned = provider.as_deref() == Some(p.provider.as_str()) && identifier.is_none();
        let invited = provider.is_none() && status == "invited";
        let local_link = provider.is_none() && status == "active" && policy.link_existing;
        if !(preprovisioned || invited || local_link) || status == "suspended" {
            return Err(denied("an account with this email exists but is not bound to this provider"));
        }
        sqlx::query("UPDATE ferro_users SET provider = $2, external_identifier = $3, status = 'active' WHERE id = $1")
            .bind(id)
            .bind(&p.provider)
            .bind(&p.identifier)
            .execute(&mut *conn)
            .await?;
        return Ok(Some(id));
    }
    // 3. first time
    if !policy.allow_registration {
        return Err(denied("no account exists for this email and registration is closed"));
    }
    let id = Uuid::now_v7();
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO ferro_users (id, email, first_name, last_name, role, provider, external_identifier) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(id)
    .bind(&p.email)
    .bind(&p.first_name)
    .bind(&p.last_name)
    .bind(policy.default_role)
    .bind(&p.provider)
    .bind(&p.identifier)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(inserted)
}

/// Sign in a person authenticated by a directory: find or create their account, then apply Ferro's own second factor
/// if they turned it on (unlike single sign-on, the directory login has no multi-factor step of its own).
pub async fn login_external(pool: &PgPool, cfg: &AuthConfig, profile: &ExternalProfile, policy: &ExternalPolicy, otp: Option<&str>, ip: Option<&str>, ua: Option<&str>) -> Result<Tokens> {
    let mut tx = pool.begin().await?;
    let user = resolve_external_user(&mut tx, profile, policy).await?;
    let row: Option<(Option<String>, Option<i64>)> = sqlx::query_as("SELECT tfa_secret, tfa_last_step FROM ferro_users WHERE id = $1").bind(user).fetch_optional(&mut *tx).await?;
    if let Some((Some(secret), last)) = row {
        let login_row = LoginRow { id: user, password_hash: None, status: "active".into(), tfa_secret: Some(secret), tfa_last_step: last };
        second_factor(&mut tx, cfg, &login_row, otp).await?;
    }
    let tokens = new_session(&mut tx, cfg, user, ip, ua).await?;
    tx.commit().await?;
    Ok(tokens)
}

/// Open a session for a user whose identity was established elsewhere (the provider already did any second factor).
pub async fn open_session(pool: &PgPool, cfg: &AuthConfig, user: Uuid, ip: Option<&str>, ua: Option<&str>) -> Result<Tokens> {
    let mut tx = pool.begin().await?;
    let active: Option<bool> = sqlx::query_scalar("SELECT status = 'active' FROM ferro_users WHERE id = $1").bind(user).fetch_optional(&mut *tx).await?;
    if active != Some(true) {
        return Err(FerroError::Unauthorized("user is not active".into()));
    }
    let tokens = new_session(&mut tx, cfg, user, ip, ua).await?;
    tx.commit().await?;
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_roundtrip() {
        let h = hash_password("s3cret!").unwrap();
        assert!(verify_password("s3cret!", &h));
        assert!(!verify_password("nope", &h));
    }

    #[test]
    fn jwt_roundtrip() {
        let cfg = AuthConfig { secret: "x".repeat(32), access_ttl_secs: 60, refresh_ttl_secs: 60, encryption_key: "k".into(), issuer: "Ferro".into() };
        let u = Uuid::now_v7();
        let t = issue_access_token(&cfg, u).unwrap();
        assert_eq!(decode_access_token(&cfg, &t).unwrap(), u);
        let other = AuthConfig { secret: "y".repeat(32), ..cfg };
        assert!(decode_access_token(&other, &t).is_err());
    }
}
