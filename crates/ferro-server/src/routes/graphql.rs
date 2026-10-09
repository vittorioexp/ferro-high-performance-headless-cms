//! GraphQL endpoint. Same authentication and permissions as REST.
//!
//! * `POST /graphql`         `{ "query", "variables", "operationName" }`
//! * `GET  /graphql?query=…` queries only (mutations over GET are refused)
//! * `GET  /graphql/schema`  the schema in SDL (authenticated users)
//!
//! Errors in a query are reported in the `errors` member with HTTP 200, as the
//! GraphQL-over-HTTP convention requires; authentication failures are 401.

use axum::extract::{Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum::Json;
use ferro_core::sdl::sdl;
use ferro_engine::graphql::{self, Options, Request};
use ferro_engine::items::Ctx;
use ferro_engine::{Accountability, FerroError};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::extract::Acc;
use crate::{ApiResult, AppState};

#[derive(Deserialize)]
pub struct PostBody {
    query: String,
    #[serde(default)]
    variables: Option<Value>,
    #[serde(default, rename = "operationName")]
    operation_name: Option<String>,
}

#[derive(Deserialize)]
pub struct GetQuery {
    query: String,
    /// JSON object, URL-encoded.
    variables: Option<String>,
    #[serde(rename = "operationName")]
    operation_name: Option<String>,
}

async fn run(st: &AppState, acc: Accountability, req: Request, allow_mutations: bool) -> Json<Value> {
    let ctx = Ctx::new(st.schema.get(), acc);
    let opts = Options { limit_max: st.config.query_limit_max, allow_mutations };
    Json(graphql::execute(&st.pool, &ctx, req, opts).await)
}

pub async fn post(State(st): State<AppState>, Acc(acc): Acc, Json(body): Json<PostBody>) -> Json<Value> {
    let variables = match body.variables {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    run(&st, acc, Request { query: body.query, variables, operation_name: body.operation_name }, true).await
}

pub async fn get(State(st): State<AppState>, Acc(acc): Acc, Query(q): Query<GetQuery>) -> Json<Value> {
    let variables = match q.variables.as_deref().map(serde_json::from_str::<Value>) {
        Some(Ok(Value::Object(m))) => m,
        _ => Map::new(),
    };
    run(&st, acc, Request { query: q.query, variables, operation_name: q.operation_name }, false).await
}

pub async fn schema_sdl(State(st): State<AppState>, Acc(acc): Acc) -> ApiResult<Response> {
    if acc.user.is_none() {
        return Err(FerroError::Unauthorized("sign in to read the GraphQL schema".into()).into());
    }
    let text = sdl(&st.schema.get().snapshot);
    Ok(([(CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response())
}
