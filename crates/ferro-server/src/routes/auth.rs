use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header::{CACHE_CONTROL, COOKIE, LOCATION, SET_COOKIE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use ferro_engine::{auth, FerroError};
use ferro_identity::mail;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::extract::Acc;
use crate::{ApiError, ApiResult, AppState};

#[derive(Deserialize)]
pub struct LoginBody {
    email: String,
    password: String,
    /// One-time code (or recovery code) when two-factor authentication is on.
    otp: Option<String>,
}

#[derive(Deserialize)]
pub struct RefreshBody {
    refresh_token: String,
}

/// Client address as seen by the proxy in front of Ferro (used for throttling and the audit trail only).
pub fn client_ip(headers: &HeaderMap) -> String {
    let forwarded = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok());
    forwarded.and_then(|v| v.split(',').next()).map(str::trim).filter(|s| !s.is_empty()).unwrap_or("unknown").to_string()
}

fn forwarded(headers: &HeaderMap) -> Option<&str> {
    headers.get("x-forwarded-for").and_then(|v| v.to_str().ok())
}

fn too_many(secs: u64, what: &str) -> ApiError {
    ApiError::new(StatusCode::TOO_MANY_REQUESTS, "TOO_MANY_REQUESTS", format!("too many {what}: try again in {secs} seconds"))
}

pub async fn login(State(st): State<AppState>, headers: HeaderMap, Json(body): Json<LoginBody>) -> ApiResult<Json<Value>> {
    let ip = client_ip(&headers);
    if let Err(secs) = st.login_guard.check(&body.email, &ip) {
        return Err(too_many(secs, "failed attempts"));
    }
    if body.email.contains('\0') || body.password.contains('\0') || body.otp.as_deref().map(|o| o.contains('\0')).unwrap_or(false) {
        st.login_guard.failure(&body.email, &ip);
        return Err(FerroError::Unauthorized("invalid credentials".into()).into());
    }
    let ua = headers.get("user-agent").and_then(|v| v.to_str().ok());
    let attempt = if st.directory.is_some() && uses_directory(&st, &body.email).await? {
        directory_login(&st, &body, forwarded(&headers), ua).await
    } else {
        auth::login(&st.pool, &st.auth, &body.email, &body.password, body.otp.as_deref(), forwarded(&headers), ua).await
    };
    match attempt {
        Ok(tokens) => {
            st.login_guard.success(&body.email);
            Ok(Json(json!({ "data": tokens })))
        }
        Err(e) => {
            // a missing code is not a failure (the client simply has to ask for it); a wrong one is
            if matches!(e, FerroError::Unauthorized(_) | FerroError::AuthCode("INVALID_OTP", _)) {
                st.login_guard.failure(&body.email, &ip);
            }
            Err(e.into())
        }
    }
}

/// Does this sign-in go to the directory? Yes for someone who is bound to it, and for a login Ferro has no account for
/// (a first sign-in); accounts with a Ferro password, or bound elsewhere, never fall through to the directory.
async fn uses_directory(st: &AppState, login: &str) -> ApiResult<bool> {
    let row: Option<Option<String>> = sqlx::query_scalar("SELECT provider FROM ferro_users WHERE lower(email) = lower($1)")
        .bind(login.trim())
        .fetch_optional(&st.pool)
        .await?;
    Ok(match row {
        None => true,
        Some(provider) => provider.as_deref() == Some("ldap"),
    })
}

async fn directory_login(st: &AppState, body: &LoginBody, ip: Option<&str>, ua: Option<&str>) -> ferro_engine::Result<auth::Tokens> {
    use ferro_identity::ldap::LdapError;
    let Some(dir) = &st.directory else { return Err(FerroError::Unauthorized("invalid credentials".into())) };
    match dir.authenticate(&body.email, &body.password).await {
        Ok(profile) => auth::login_external(&st.pool, &st.auth, &profile, &dir.policy(), body.otp.as_deref(), ip, ua).await,
        Err(LdapError::InvalidCredentials) => Err(FerroError::Unauthorized("invalid credentials".into())),
        Err(LdapError::Unavailable(why)) => {
            tracing::error!("directory sign-in unavailable: {why}");
            Err(FerroError::AuthCode("DIRECTORY_UNAVAILABLE", "the directory is not reachable right now: try again later".into()))
        }
    }
}

pub async fn refresh(State(st): State<AppState>, Json(body): Json<RefreshBody>) -> ApiResult<Json<Value>> {
    let tokens = auth::refresh(&st.pool, &st.auth, &body.refresh_token).await?;
    Ok(Json(json!({ "data": tokens })))
}

pub async fn logout(State(st): State<AppState>, Json(body): Json<RefreshBody>) -> ApiResult<StatusCode> {
    auth::logout(&st.pool, &body.refresh_token).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// What the sign-in page may offer (public).
pub async fn methods(State(st): State<AppState>) -> Json<Value> {
    Json(json!({ "data": {
        "password": true,
        "password_reset": st.can_send_links(),
        "invitations": st.can_send_links(),
        "providers": st.oauth.list(),
        "directory": st.directory.is_some(),
    } }))
}

// ---- password reset ------------------------------------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ResetRequest {
    email: String,
    /// Where the link in the email should point (must be an allowed origin); default: the admin app.
    reset_url: Option<String>,
}

#[derive(Deserialize)]
pub struct ResetBody {
    token: String,
    password: String,
}

/// Always answers 202, whether or not the address belongs to an account, and does the work in the background so that
/// the response time tells nothing either.
pub async fn password_request(State(st): State<AppState>, headers: HeaderMap, Json(body): Json<ResetRequest>) -> ApiResult<(StatusCode, Json<Value>)> {
    if !st.can_send_links() {
        return Err(ApiError::bad_request("password reset by email is not configured on this server"));
    }
    let base = st.link_base(body.reset_url.as_deref(), "/admin/#/reset")?;
    let ip = client_ip(&headers);
    if let Err(secs) = st.reset_guard.check(&body.email, &ip) {
        return Err(too_many(secs, "reset requests"));
    }
    st.reset_guard.failure(&body.email, &ip); // every request counts: this endpoint can send mail to anybody
    let state = st.clone();
    let email = body.email.clone();
    tokio::spawn(async move {
        let result: ferro_engine::Result<()> = async {
            let mut conn = state.pool.acquire().await?;
            let Some(target) = auth::start_password_reset(&mut conn, &email).await? else { return Ok(()) };
            drop(conn);
            let link = crate::with_token(&base, &target.token);
            let m = mail::password_reset_message(&state.config.tfa_issuer, target.first_name.as_deref(), &link);
            state.mailer.send(&target.email, &m.subject, &m.text, &m.html).await
        }
        .await;
        if let Err(e) = result {
            tracing::error!(error = %e, "password reset email failed");
        }
    });
    Ok((StatusCode::ACCEPTED, Json(json!({ "data": { "message": "if the address belongs to an account, an email is on its way" } }))))
}

pub async fn password_reset(State(st): State<AppState>, Json(body): Json<ResetBody>) -> ApiResult<StatusCode> {
    let user = auth::finish_password_reset(&st.pool, &body.token, &body.password).await?;
    tracing::info!(target: "ferro::audit", %user, "password reset completed (all sessions ended)");
    Ok(StatusCode::NO_CONTENT)
}

// ---- invitations --------------------------------------------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct AcceptBody {
    token: String,
    password: String,
}

pub async fn invite_accept(State(st): State<AppState>, headers: HeaderMap, Json(body): Json<AcceptBody>) -> ApiResult<Json<Value>> {
    let ua = headers.get("user-agent").and_then(|v| v.to_str().ok());
    let tokens = auth::accept_invitation(&st.pool, &st.auth, &body.token, &body.password, forwarded(&headers), ua).await?;
    tracing::info!(target: "ferro::audit", "invitation accepted");
    Ok(Json(json!({ "data": tokens })))
}

// ---- two-factor authentication ----------------------------------------------------------------------------------------------------

fn user_of(acc: &ferro_engine::Accountability) -> ApiResult<uuid::Uuid> {
    acc.user.ok_or_else(|| FerroError::Unauthorized("sign in first".into()).into())
}

#[derive(Deserialize, Default)]
pub struct PasswordBody {
    password: Option<String>,
}

/// Step 1: a secret, its `otpauth://` address and a QR code. Nothing is stored until `enable`.
pub async fn tfa_generate(State(st): State<AppState>, Acc(acc): Acc, Json(body): Json<PasswordBody>) -> ApiResult<Json<Value>> {
    let user = user_of(&acc)?;
    let mut conn = st.pool.acquire().await?;
    auth::confirm_password(&mut conn, user, body.password.as_deref()).await?;
    let e = auth::tfa_begin(&mut conn, &st.auth, user).await?;
    Ok(Json(json!({ "data": { "secret": e.secret, "otpauth_url": e.otpauth_url, "qr_svg": e.qr_svg } })))
}

#[derive(Deserialize)]
pub struct EnableBody {
    secret: String,
    otp: String,
}

/// Step 2: prove the app works. Returns the recovery codes, this once.
pub async fn tfa_enable(State(st): State<AppState>, headers: HeaderMap, Acc(acc): Acc, Json(body): Json<EnableBody>) -> ApiResult<Json<Value>> {
    let user = user_of(&acc)?;
    let key = format!("tfa:{user}");
    let ip = client_ip(&headers);
    if let Err(secs) = st.login_guard.check(&key, &ip) {
        return Err(too_many(secs, "failed attempts"));
    }
    let mut conn = st.pool.acquire().await?;
    match auth::tfa_enable(&mut conn, &st.auth, user, &body.secret, &body.otp).await {
        Ok(codes) => {
            tracing::info!(target: "ferro::audit", %user, "two-factor authentication enabled");
            Ok(Json(json!({ "data": { "recovery_codes": codes } })))
        }
        Err(e) => {
            if matches!(e, FerroError::AuthCode("INVALID_OTP", _)) {
                st.login_guard.failure(&key, &ip);
            }
            Err(e.into())
        }
    }
}

#[derive(Deserialize)]
pub struct DisableBody {
    otp: String,
    password: Option<String>,
}

pub async fn tfa_disable(State(st): State<AppState>, headers: HeaderMap, Acc(acc): Acc, Json(body): Json<DisableBody>) -> ApiResult<StatusCode> {
    let user = user_of(&acc)?;
    let key = format!("tfa:{user}");
    let ip = client_ip(&headers);
    if let Err(secs) = st.login_guard.check(&key, &ip) {
        return Err(too_many(secs, "failed attempts"));
    }
    let mut tx = st.pool.begin().await?;
    auth::confirm_password(&mut tx, user, body.password.as_deref()).await?;
    match auth::tfa_disable(&mut tx, &st.auth, user, &body.otp).await {
        Ok(()) => {
            tx.commit().await?;
            tracing::info!(target: "ferro::audit", %user, "two-factor authentication disabled by the user");
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) => {
            if matches!(e, FerroError::AuthCode("INVALID_OTP", _)) {
                st.login_guard.failure(&key, &ip);
            }
            tx.rollback().await.ok();
            Err(e.into())
        }
    }
}

// ---- single sign-on ------------------------------------------------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct StartQuery {
    /// Where to send the user when the sign-in is over (an allowed origin); default: the admin app.
    redirect: Option<String>,
}

fn cookie_header(st: &AppState, value: &str, max_age: i64) -> String {
    format!(
        "{}={value}; HttpOnly; SameSite=Lax; Path=/auth/oauth; Max-Age={max_age}{}",
        ferro_identity::oauth::STATE_COOKIE,
        if st.oauth.secure_cookie() { "; Secure" } else { "" }
    )
}

fn read_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

fn redirect(location: &str, set_cookie: Option<String>) -> ApiResult<Response> {
    let mut b = Response::builder().status(StatusCode::FOUND).header(LOCATION, location).header(CACHE_CONTROL, "no-store");
    if let Some(c) = set_cookie {
        b = b.header(SET_COOKIE, c);
    }
    b.body(Body::empty()).map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL", e.to_string()))
}

/// `GET /auth/oauth/:name` — send the browser to the identity provider.
pub async fn oauth_start(State(st): State<AppState>, Path(name): Path<String>, Query(q): Query<StartQuery>) -> ApiResult<Response> {
    if !st.oauth.has(&name) {
        return Err(FerroError::not_found("unknown sign-in provider").into());
    }
    let target = match q.redirect.as_deref().filter(|r| !r.is_empty()) {
        Some(r) if st.config.identity.redirect_allowed(r) => r.to_string(),
        Some(_) => return Err(ApiError::bad_request("that redirect address is not allowed (see FERRO_PUBLIC_URL and FERRO_AUTH_REDIRECT_ALLOWLIST)")),
        None => st.link_base(None, "/admin/#/sso")?,
    };
    let begin = st.oauth.begin(&name, &target).await.map_err(|e| {
        tracing::error!(error = %e, provider = %name, "cannot start single sign-on");
        ApiError::new(StatusCode::BAD_GATEWAY, "PROVIDER_UNAVAILABLE", "the identity provider is not reachable")
    })?;
    redirect(&begin.location, Some(cookie_header(&st, &begin.cookie_value, begin.cookie_max_age)))
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    /// Set by the provider when the user declined or something went wrong there.
    error: Option<String>,
}

/// `GET /auth/oauth/:name/callback` — the provider sends the browser back here.
pub async fn oauth_callback(State(st): State<AppState>, headers: HeaderMap, Path(name): Path<String>, Query(q): Query<CallbackQuery>) -> ApiResult<Response> {
    let clear = Some(cookie_header(&st, "", 0));
    let cookie = read_cookie(&headers, ferro_identity::oauth::STATE_COOKIE);
    let finished = if q.error.is_some() {
        Err(ferro_identity::oauth::Failure { code: "denied", redirect: st.oauth.redirect_of(cookie.as_deref()) })
    } else {
        st.oauth.finish(&name, q.code.as_deref().unwrap_or(""), q.state.as_deref().unwrap_or(""), cookie.as_deref()).await
    };
    let fin = match finished {
        Ok(f) => f,
        // nowhere safe to send the user: say so plainly
        Err(f) if f.redirect.is_none() => return Err(ApiError::new(StatusCode::BAD_REQUEST, "SSO_FAILED", format!("sign-in failed ({}): start again from the sign-in page", f.code))),
        Err(f) => return redirect(&crate::with_param(f.redirect.as_deref().unwrap_or_default(), "error", f.code), clear),
    };
    let outcome: ferro_engine::Result<String> = async {
        let mut tx = st.pool.begin().await?;
        let user = auth::resolve_external_user(&mut tx, &fin.profile, &fin.policy).await?;
        let code = auth::issue_token(&mut tx, user, auth::TOKEN_SSO, auth::SSO_CODE_TTL_SECS).await?;
        tx.commit().await?;
        Ok(code)
    }
    .await;
    match outcome {
        Ok(code) => {
            tracing::info!(target: "ferro::audit", provider = %name, email = %fin.profile.email, "single sign-on accepted");
            redirect(&crate::with_param(&fin.redirect, "code", &code), clear)
        }
        Err(FerroError::Unauthorized(_)) => redirect(&crate::with_param(&fin.redirect, "error", "denied"), clear),
        Err(e) => {
            tracing::error!(error = %e, "single sign-on: cannot resolve the user");
            redirect(&crate::with_param(&fin.redirect, "error", "server"), clear)
        }
    }
}

#[derive(Deserialize)]
pub struct ExchangeBody {
    code: String,
}

/// The browser trades the one-time code it was redirected with for real tokens (so tokens never travel in a URL).
pub async fn oauth_exchange(State(st): State<AppState>, headers: HeaderMap, Json(body): Json<ExchangeBody>) -> ApiResult<Json<Value>> {
    let mut conn = st.pool.acquire().await?;
    let Some(user) = auth::consume_token(&mut conn, &body.code, auth::TOKEN_SSO).await? else {
        return Err(FerroError::Unauthorized("this sign-in code is invalid or has expired".into()).into());
    };
    drop(conn);
    let ua = headers.get("user-agent").and_then(|v| v.to_str().ok());
    let tokens = auth::open_session(&st.pool, &st.auth, user, forwarded(&headers), ua).await?;
    Ok(Json(json!({ "data": tokens })))
}
