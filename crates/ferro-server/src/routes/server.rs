use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::extract::Acc;
use crate::{ApiResult, AppState};

pub async fn health(State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let db_ok = sqlx::query("SELECT 1").execute(&st.pool).await.is_ok();
    Ok(Json(json!({ "status": if db_ok { "ok" } else { "error" }, "database": db_ok })))
}

pub async fn info(State(st): State<AppState>, Acc(acc): Acc) -> ApiResult<Json<Value>> {
    let schema = st.schema.get();
    let mut data = json!({ "project": "ferro", "version": env!("CARGO_PKG_VERSION") });
    if acc.user.is_some() {
        data["environment"] = json!(st.config.env_name);
        data["schema_hash"] = json!(schema.hash);
        data["collections"] = json!(schema.snapshot.collections.len());
        data["user"] = json!({ "id": acc.user, "role": acc.role, "admin": acc.admin, "app": acc.app });
        data["storage"] = json!({ "default": st.storages.default_name(), "locations": st.storages.names() });
    }
    Ok(Json(json!({ "data": data })))
}

/// Admin extensions found in `FERRO_EXTENSIONS_DIR`: `<dir>/<name>/manifest.json`
/// with `{ "name", "version", "description", "entry": "index.js" }`.
pub async fn extensions(State(st): State<AppState>, Acc(acc): Acc) -> ApiResult<Json<Value>> {
    if acc.user.is_none() {
        return Err(ferro_engine::FerroError::Unauthorized("sign in first".into()).into());
    }
    let Some(dir) = &st.config.extensions_dir else {
        return Ok(Json(json!({ "data": [] })));
    };
    let mut out: Vec<Value> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let id = entry.file_name().to_string_lossy().to_string();
            let Ok(text) = std::fs::read_to_string(entry.path().join("manifest.json")) else { continue };
            let Ok(manifest) = serde_json::from_str::<Value>(&text) else {
                tracing::warn!(extension = %id, "invalid manifest.json");
                continue;
            };
            let file = manifest.get("entry").and_then(|v| v.as_str()).unwrap_or("index.js");
            if file.contains("..") || file.starts_with('/') || file.contains('\\') {
                tracing::warn!(extension = %id, "extension entry must be a relative path");
                continue;
            }
            out.push(json!({
                "id": id,
                "name": manifest.get("name").and_then(|v| v.as_str()).unwrap_or(&id),
                "version": manifest.get("version"),
                "description": manifest.get("description"),
                "url": format!("/extensions/{id}/{file}"),
            }));
        }
    }
    out.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Ok(Json(json!({ "data": out })))
}
