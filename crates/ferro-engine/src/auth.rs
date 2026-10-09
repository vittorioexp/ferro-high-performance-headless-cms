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
use crate::schema::Schema;
use crate::{FerroError, Result};

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub secret: String,
    pub access_ttl_secs: i64,
    pub refresh_ttl_secs: i64,
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

pub async fn login(pool: &PgPool, cfg: &AuthConfig, email: &str, password: &str, ip: Option<&str>, ua: Option<&str>) -> Result<Tokens> {
    let mut conn = pool.acquire().await?;
    let row: Option<(Uuid, Option<String>, String)> =
        sqlx::query_as("SELECT id, password_hash, status FROM ferro_users WHERE lower(email) = lower($1)")
            .bind(email)
            .fetch_optional(&mut *conn)
            .await?;
    let invalid = || FerroError::Unauthorized("invalid credentials".into());
    let Some((id, hash, status)) = row else {
        // constant-ish time: hash anyway
        let _ = hash_password(password);
        return Err(invalid());
    };
    if status != "active" {
        return Err(invalid());
    }
    match hash {
        Some(h) if verify_password(password, &h) => {}
        _ => return Err(invalid()),
    }
    new_session(&mut conn, cfg, id, ip, ua).await
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
        Some(p) => Some(hash_password(p)?),
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
        let cfg = AuthConfig { secret: "x".repeat(32), access_ttl_secs: 60, refresh_ttl_secs: 60 };
        let u = Uuid::now_v7();
        let t = issue_access_token(&cfg, u).unwrap();
        assert_eq!(decode_access_token(&cfg, &t).unwrap(), u);
        let other = AuthConfig { secret: "y".repeat(32), ..cfg };
        assert!(decode_access_token(&other, &t).is_err());
    }
}
