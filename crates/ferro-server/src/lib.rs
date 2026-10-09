//! Ferro HTTP API.

mod error;
mod extract;
mod login_guard;
mod routes;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::extract::DefaultBodyLimit;
use axum::Router;
use ferro_engine::auth::AuthConfig;
use ferro_engine::files::Storages;
use ferro_engine::schema_store::{self, SCHEMA_CHANNEL};
use ferro_engine::{db, Schema, SchemaHolder};
use sqlx::postgres::PgListener;
use sqlx::PgPool;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

pub use error::{ApiError, ApiResult};

/// `base` + `key=value` as a query parameter (inside the fragment for `#/route` style addresses). `value` must be
/// URL-safe already (tokens and error codes are).
pub fn with_param(base: &str, key: &str, value: &str) -> String {
    format!("{base}{}{key}={value}", if base.contains('?') { '&' } else { '?' })
}

pub fn with_token(base: &str, token: &str) -> String {
    with_param(base, "token", token)
}

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub bind: String,
    pub secret: String,
    /// Name of this environment (dev, test, prod...), shown in reports and bundles.
    pub env_name: String,
    pub storage: ferro_storage::StorageConfig,
    /// Failed logins per email before it is locked (the address limit is ten times higher).
    pub login_max_failures: u32,
    pub login_lockout_secs: u64,
    /// Directory of admin extensions (`<name>/manifest.json` + modules), served on `/extensions`.
    pub extensions_dir: Option<PathBuf>,
    /// Allow flows to call private / loopback addresses.
    pub flow_allow_private: bool,
    /// If not empty, flows may only call these hosts (and their subdomains).
    pub flow_allowed_hosts: Vec<String>,
    pub access_ttl_secs: i64,
    pub refresh_ttl_secs: i64,
    pub max_body_bytes: usize,
    /// Maximum `limit` accepted from clients (`-1` = unlimited allowed).
    pub query_limit_max: i64,
    pub admin_email: Option<String>,
    pub admin_password: Option<String>,
    /// Static admin UI build served under `/admin`.
    pub admin_dir: Option<PathBuf>,
    pub cors_origins: Vec<String>,
    pub db_max_connections: u32,
    /// Key for secrets stored in the database (two-factor seeds); defaults to `secret`.
    pub encryption_key: Option<String>,
    /// Name shown in authenticator apps.
    pub tfa_issuer: String,
    /// Public address, outgoing email and (later) single sign-on / directory login.
    pub identity: ferro_identity::IdentityConfig,
    /// Image transformations (`/assets/:id?width=...`).
    pub image_limits: ferro_engine::images::Limits,
    /// Transformations running at the same time (each decodes a whole image in memory).
    pub image_concurrency: usize,
    /// Budget of the per-instance cache of transformed images; 0 disables it.
    pub image_cache_bytes: usize,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string())
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn env_num<T: std::str::FromStr>(key: &str, default: T) -> anyhow::Result<T> {
    match env_opt(key) {
        None => Ok(default),
        Some(v) => v.parse::<T>().map_err(|_| anyhow::anyhow!("{key}: invalid number `{v}`")),
    }
}

impl Config {
    pub fn from_env() -> anyhow::Result<Config> {
        let secret = env_opt("FERRO_SECRET").context("FERRO_SECRET is required (at least 32 random characters)")?;
        if secret.len() < 32 {
            anyhow::bail!("FERRO_SECRET must be at least 32 characters");
        }
        Ok(Config {
            database_url: env_opt("FERRO_DATABASE_URL").context("FERRO_DATABASE_URL is required")?,
            bind: env_or("FERRO_BIND", "0.0.0.0:8055"),
            secret,
            env_name: env_or("FERRO_ENV", "local"),
            storage: ferro_storage::StorageConfig::from_env().map_err(anyhow::Error::msg)?,
            login_max_failures: env_num("FERRO_LOGIN_MAX_FAILURES", 10)?,
            login_lockout_secs: env_num("FERRO_LOGIN_LOCKOUT_SECS", 900)?,
            extensions_dir: env_opt("FERRO_EXTENSIONS_DIR").map(PathBuf::from),
            flow_allow_private: env_or("FERRO_FLOW_ALLOW_PRIVATE", "false") == "true",
            flow_allowed_hosts: env_or("FERRO_FLOW_ALLOWED_HOSTS", "")
                .split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
            access_ttl_secs: env_num("FERRO_ACCESS_TOKEN_TTL", 900)?,
            refresh_ttl_secs: env_num("FERRO_REFRESH_TOKEN_TTL", 7 * 24 * 3600)?,
            max_body_bytes: env_num::<usize>("FERRO_MAX_BODY_MB", 256)? * 1024 * 1024,
            query_limit_max: env_num("FERRO_QUERY_LIMIT_MAX", -1)?,
            admin_email: env_opt("FERRO_ADMIN_EMAIL"),
            admin_password: env_opt("FERRO_ADMIN_PASSWORD"),
            admin_dir: env_opt("FERRO_ADMIN_DIR").map(PathBuf::from),
            cors_origins: env_or("FERRO_CORS_ORIGINS", "")
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            db_max_connections: env_num("FERRO_DB_MAX_CONNECTIONS", 10)?,
            encryption_key: env_opt("FERRO_ENCRYPTION_KEY"),
            identity: ferro_identity::IdentityConfig::from_env().map_err(anyhow::Error::msg)?,
            tfa_issuer: env_opt("FERRO_TFA_ISSUER").unwrap_or_else(|| match env_or("FERRO_ENV", "local").as_str() {
                "local" | "prod" | "production" => "Ferro".to_string(),
                other => format!("Ferro ({other})"),
            }),
            image_limits: ferro_engine::images::Limits {
                max_dimension: env_num("FERRO_IMAGE_MAX_DIMENSION", 4096)?,
                max_input_pixels: env_num::<u64>("FERRO_IMAGE_MAX_INPUT_MEGAPIXELS", 64)? * 1_000_000,
            },
            image_concurrency: env_num("FERRO_IMAGE_CONCURRENCY", std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2))?,
            image_cache_bytes: env_num::<usize>("FERRO_IMAGE_CACHE_MB", 64)? * 1024 * 1024,
        })
    }
}

pub struct Inner {
    pub pool: PgPool,
    /// Fan-out of change notifications to the WebSocket sessions of this instance.
    pub hub: Arc<ferro_engine::realtime::Hub>,
    /// Open WebSocket connections.
    pub ws_active: std::sync::atomic::AtomicUsize,
    pub login_guard: login_guard::LoginGuard,
    pub schema: SchemaHolder,
    pub config: Config,
    pub auth: AuthConfig,
    pub storages: Storages,
    pub image_gate: tokio::sync::Semaphore,
    pub mailer: ferro_identity::Mailer,
    pub oauth: ferro_identity::OAuth,
    /// Directory (LDAP) login, when configured.
    pub directory: Option<ferro_identity::ldap::Directory>,
    /// Throttles `POST /auth/password/request`: it can make Ferro send mail to anybody.
    pub reset_guard: login_guard::LoginGuard,
    pub image_cache: ferro_engine::images::TransformCache,
}

#[derive(Clone)]
pub struct AppState(pub Arc<Inner>);

impl std::ops::Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl AppState {
    /// Can Ferro put a working link in front of a user (outgoing email + a public address to point it at)?
    pub fn can_send_links(&self) -> bool {
        self.mailer.enabled() && self.config.identity.public_url.is_some()
    }

    /// Base address of a link sent to a user. A caller-supplied one must be an allowed origin; otherwise the link
    /// is built from `FERRO_PUBLIC_URL` (never from the request's Host header: that would allow poisoned links).
    pub fn link_base(&self, explicit: Option<&str>, default_path: &str) -> ApiResult<String> {
        if let Some(u) = explicit.filter(|u| !u.is_empty()) {
            if !self.config.identity.redirect_allowed(u) {
                return Err(ApiError::bad_request("that address is not allowed (see FERRO_PUBLIC_URL and FERRO_AUTH_REDIRECT_ALLOWLIST)"));
            }
            return Ok(u.to_string());
        }
        match &self.config.identity.public_url {
            Some(p) => Ok(format!("{}{}", p.as_str().trim_end_matches('/'), default_path)),
            None => Err(ApiError::bad_request("FERRO_PUBLIC_URL is not set on this server: pass an explicit address or configure it")),
        }
    }

    pub async fn reload_schema(&self) -> ferro_engine::Result<Arc<Schema>> {
        let schema = schema_store::load_schema(&self.pool).await?;
        self.schema.set(schema);
        Ok(self.schema.get())
    }
}

pub fn router(state: AppState) -> Router {
    let cors = if state.config.cors_origins.iter().any(|o| o == "*") {
        CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any)
    } else {
        let origins: Vec<axum::http::HeaderValue> =
            state.config.cors_origins.iter().filter_map(|o| o.parse().ok()).collect();
        CorsLayer::new().allow_origin(AllowOrigin::list(origins)).allow_methods(Any).allow_headers(Any)
    };
    let mut app = routes::router().layer(DefaultBodyLimit::max(state.config.max_body_bytes));
    if let Some(dir) = &state.config.admin_dir {
        app = app
            .nest_service("/admin", ServeDir::new(dir))
            .route("/", axum::routing::get(|| async { axum::response::Redirect::to("/admin/") }));
    }
    if let Some(dir) = &state.config.extensions_dir {
        app = app.nest_service("/extensions", ServeDir::new(dir));
    }
    app.layer(cors).layer(TraceLayer::new_for_http()).with_state(state)
}

/// Keep the in-memory schema in sync when another instance applies a change.
async fn listen_schema_changes(state: AppState) {
    loop {
        match PgListener::connect_with(&state.pool).await {
            Ok(mut listener) => {
                if let Err(e) = listener.listen(SCHEMA_CHANNEL).await {
                    tracing::warn!(error = %e, "schema listener: LISTEN failed");
                } else {
                    while let Ok(n) = listener.recv().await {
                        if n.payload() != state.schema.get().hash {
                            match state.reload_schema().await {
                                Ok(s) => tracing::info!(hash = %s.hash, "schema reloaded"),
                                Err(e) => tracing::error!(error = %e, "schema reload failed"),
                            }
                        }
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "schema listener: connection failed"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}

pub async fn build_state(config: Config) -> anyhow::Result<AppState> {
    ferro_identity::install_crypto_provider();
    let pool = db::connect(&config.database_url, config.db_max_connections).await.context("database connection")?;
    db::migrate_system(&pool).await.context("system migrations")?;
    schema_store::bootstrap(&pool, config.admin_email.as_deref(), config.admin_password.as_deref())
        .await
        .context("bootstrap")?;
    let schema = schema_store::load_schema(&pool).await?;
    let storages = ferro_storage::build(&config.storage).await.context("storage")?;
    let mailer = match &config.identity.smtp {
        Some(smtp) => ferro_identity::Mailer::new(smtp).context("outgoing email")?,
        None => ferro_identity::Mailer::disabled(),
    };
    let oauth = ferro_identity::OAuth::new(&config.identity, &config.secret).context("single sign-on")?;
    let directory = config.identity.ldap.clone().map(ferro_identity::ldap::Directory::new);
    let (config_image_concurrency, config_image_cache_bytes) = (config.image_concurrency, config.image_cache_bytes);
    let auth = AuthConfig {
        secret: config.secret.clone(),
        access_ttl_secs: config.access_ttl_secs,
        refresh_ttl_secs: config.refresh_ttl_secs,
        encryption_key: config.encryption_key.clone().unwrap_or_else(|| config.secret.clone()),
        issuer: config.tfa_issuer.clone(),
    };
    Ok(AppState(Arc::new(Inner {
        pool,
        hub: Arc::new(ferro_engine::realtime::Hub::new()),
        ws_active: std::sync::atomic::AtomicUsize::new(0),
        login_guard: login_guard::LoginGuard::new(config.login_max_failures, Duration::from_secs(config.login_lockout_secs)),
        schema: SchemaHolder::new(schema),
        config,
        auth,
        storages,
        image_gate: tokio::sync::Semaphore::new(config_image_concurrency.max(1)),
        mailer,
        oauth,
        directory,
        reset_guard: login_guard::LoginGuard::new(5, Duration::from_secs(3600)),
        image_cache: ferro_engine::images::TransformCache::new(config_image_cache_bytes),
    })))
}

pub async fn run(config: Config) -> anyhow::Result<()> {
    let bind = config.bind.clone();
    let state = build_state(config).await?;
    tracing::info!(env = %state.config.env_name, hash = %state.schema.get().hash, "schema loaded");
    tokio::spawn(listen_schema_changes(state.clone()));
    tokio::spawn(ferro_engine::realtime::run_listener(state.pool.clone(), state.hub.clone()));
    ferro_engine::flows::init(ferro_engine::flows::Runtime::new(state.config.flow_allow_private, state.config.flow_allowed_hosts.clone()));
    let for_worker = state.clone();
    tokio::spawn(ferro_engine::flows::run_worker(state.pool.clone(), Arc::new(move || for_worker.schema.get())));
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(&bind).await.with_context(|| format!("bind {bind}"))?;
    tracing::info!("Ferro listening on http://{bind}");
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await?;
    Ok(())
}
