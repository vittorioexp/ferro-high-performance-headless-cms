use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use ferro_engine::{auth, FerroError};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{ApiError, ApiResult, AppState};

#[derive(Deserialize)]
pub struct LoginBody {
    email: String,
    password: String,
}

#[derive(Deserialize)]
pub struct RefreshBody {
    refresh_token: String,
}

pub async fn login(State(st): State<AppState>, headers: HeaderMap, Json(body): Json<LoginBody>) -> ApiResult<Json<Value>> {
    let forwarded = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok());
    let ip = forwarded.and_then(|v| v.split(',').next()).map(str::trim).filter(|s| !s.is_empty()).unwrap_or("unknown");
    if let Err(secs) = st.login_guard.check(&body.email, ip) {
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "TOO_MANY_REQUESTS", format!("too many failed attempts: try again in {secs} seconds")));
    }
    let ua = headers.get("user-agent").and_then(|v| v.to_str().ok());
    match auth::login(&st.pool, &st.auth, &body.email, &body.password, forwarded, ua).await {
        Ok(tokens) => {
            st.login_guard.success(&body.email);
            Ok(Json(json!({ "data": tokens })))
        }
        Err(e) => {
            if matches!(e, FerroError::Unauthorized(_)) {
                st.login_guard.failure(&body.email, ip);
            }
            Err(e.into())
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
