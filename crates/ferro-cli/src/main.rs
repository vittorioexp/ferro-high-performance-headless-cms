//! `ferro` — server and environment tool.
//!
//! ```text
//! ferro serve                                   start the API server (env: FERRO_*)
//! ferro init                                    create ferro.toml + schema/
//! ferro env status                              ping every environment, compare schema hashes
//! ferro schema pull  --from dev                 environment -> schema/ (git)
//! ferro schema diff  --from schema --to prod    what would change in prod
//! ferro schema apply --from schema --to prod    apply (transactional, destructive changes gated)
//! ferro data export  --from dev --select pages:{"slug":{"_eq":"home"}} --out bundle/
//! ferro data import  --to prod --bundle bundle/ --dry-run --report report.html
//! ferro data diff    --from dev --to prod --select pages
//! ferro data push    --from dev --to prod --select pages --strategy safe --prune
//! ferro storage list                            configured locations and how many files each holds
//! ferro storage check                           write probe on every location + every file present
//! ferro storage migrate --from local --to assets [--delete-source] [--dry-run]
//! ferro user reset-tfa --email ...                remove the second factor of a user (direct DB)
//! ferro user create  --email admin@example.com --password ... --admin   (direct DB)
//! ```

mod data_cmd;
mod project;
mod remote;
mod schema_cmd;
mod storage_cmd;
mod ui;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ferro", version, about = "Ferro CMS — headless CMS with schema-as-code and environment promotion")]
struct Cli {
    /// Project file with the environments.
    #[arg(long, global = true, default_value = "ferro.toml", env = "FERRO_PROJECT")]
    project: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the HTTP API server (configuration from FERRO_* variables).
    Serve,
    /// Create ferro.toml and the schema directory.
    Init {
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// Environments.
    Env {
        #[command(subcommand)]
        command: EnvCommand,
    },
    /// Data model and access control (schema as code).
    Schema {
        #[command(subcommand)]
        command: SchemaCommand,
    },
    /// Content promotion between environments.
    Data {
        #[command(subcommand)]
        command: DataCommand,
    },
    /// Users (direct database access through FERRO_DATABASE_URL).
    User {
        #[command(subcommand)]
        command: UserCommand,
    },
    /// File storage locations (FERRO_STORAGE_* and FERRO_DATABASE_URL).
    Storage {
        #[command(subcommand)]
        command: StorageCommand,
    },
    /// Create/upgrade the system tables (FERRO_DATABASE_URL).
    Migrate,
}

#[derive(Subcommand)]
enum EnvCommand {
    /// List environments of ferro.toml.
    List,
    /// Reachability, schema hash of each environment, alignment with the local schema.
    Status,
}

#[derive(Args, Clone)]
pub struct ReportArgs {
    /// Write a standalone HTML report.
    #[arg(long)]
    pub report: Option<PathBuf>,
    /// Print the machine-readable result as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand)]
enum SchemaCommand {
    /// Download the snapshot of an environment into files.
    Pull {
        #[arg(long)]
        from: String,
        /// Directory (one file per object) or a .yaml/.json file. Default: project schema dir.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Show the changes needed to make `to` equal to `from`.
    Diff {
        /// Environment name or path (default: project schema dir).
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: String,
        /// Exit with code 2 when there are differences (CI drift detection).
        #[arg(long)]
        exit_code: bool,
        #[command(flatten)]
        out: ReportArgs,
    },
    /// Apply a snapshot (path or environment) to an environment.
    Apply {
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: String,
        #[arg(long)]
        allow_destructive: bool,
        /// Run the DDL in a transaction and roll back.
        #[arg(long)]
        dry_run: bool,
        /// Do not ask for confirmation on protected environments.
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        out: ReportArgs,
    },
    /// Convert a Directus schema snapshot (JSON) into a Ferro schema.
    ImportDirectus {
        /// Output of `directus schema snapshot` (or the API response).
        #[arg(long)]
        file: PathBuf,
        /// Directory or .yaml/.json file. Default: project schema dir.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Validate a local snapshot.
    Validate {
        #[arg(long)]
        path: Option<PathBuf>,
    },
}

#[derive(Args, Clone)]
pub struct SelectArgs {
    /// `collection`, `collection:id=<uuid>,<uuid>` or `collection:{"field":{"_eq":"x"}}` (repeatable).
    #[arg(long = "select", required = true)]
    pub select: Vec<String>,
    /// Export only the selected items, without dependencies.
    #[arg(long)]
    pub shallow: bool,
}

#[derive(Args, Clone)]
pub struct ImportArgs {
    /// overwrite | safe | insert-only
    #[arg(long, default_value = "overwrite")]
    pub strategy: String,
    /// Delete/detach children that no longer exist in the source.
    #[arg(long)]
    pub prune: bool,
    #[arg(long)]
    pub dry_run: bool,
    /// Apply even with conflicts (strategy safe).
    #[arg(long)]
    pub force: bool,
    /// Skip fields missing in the target schema instead of failing.
    #[arg(long)]
    pub ignore_missing_fields: bool,
    #[arg(long)]
    pub yes: bool,
}

#[derive(Subcommand)]
enum DataCommand {
    /// Export a bundle (items + dependencies + files) to a directory.
    Export {
        #[arg(long)]
        from: String,
        #[command(flatten)]
        select: SelectArgs,
        #[arg(long)]
        out: PathBuf,
    },
    /// Import a bundle directory into an environment.
    Import {
        #[arg(long)]
        to: String,
        #[arg(long)]
        bundle: PathBuf,
        #[command(flatten)]
        import: ImportArgs,
        #[command(flatten)]
        out: ReportArgs,
    },
    /// Field-level diff of the selection between two environments (nothing is written).
    Diff {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[command(flatten)]
        select: SelectArgs,
        #[arg(long)]
        exit_code: bool,
        #[command(flatten)]
        out: ReportArgs,
    },
    /// Export from `from` and import into `to` in one step (files included).
    Push {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[command(flatten)]
        select: SelectArgs,
        #[command(flatten)]
        import: ImportArgs,
        #[command(flatten)]
        out: ReportArgs,
    },
}

#[derive(Subcommand)]
pub enum StorageCommand {
    /// Configured locations, the default one, and the number of files in each.
    List,
    /// Probe every location and verify that the binary of every file exists where its row says.
    Check {
        /// Exit with code 2 when a binary is missing.
        #[arg(long)]
        exit_code: bool,
    },
    /// Move the binaries of one location to another and update the file rows.
    Migrate {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        /// Delete each source blob once the copy is verified and the row updated.
        #[arg(long)]
        delete_source: bool,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum UserCommand {
    /// Create a user.
    Create {
        #[arg(long)]
        email: String,
        #[arg(long, env = "FERRO_USER_PASSWORD")]
        password: Option<String>,
        /// Give the Administrator role.
        #[arg(long)]
        admin: bool,
        /// Role id.
        #[arg(long)]
        role: Option<uuid::Uuid>,
    },
    /// Generate a static API token for a user (printed once).
    Token {
        #[arg(long)]
        email: String,
    },
    /// Remove a user's two-factor authentication (lost phone, and no other administrator to do it).
    ResetTfa {
        #[arg(long)]
        email: String,
    },
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("FERRO_LOG").unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn,tower_http=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn database() -> anyhow::Result<ferro_engine::db::PgPool> {
    let url = std::env::var("FERRO_DATABASE_URL").map_err(|_| anyhow::anyhow!("FERRO_DATABASE_URL is required"))?;
    let pool = ferro_engine::db::connect(&url, 2).await?;
    ferro_engine::db::migrate_system(&pool).await?;
    Ok(pool)
}

async fn run(cli: Cli) -> anyhow::Result<i32> {
    let project = project::Project::load(&cli.project)?;
    match cli.command {
        Command::Serve => {
            init_tracing();
            let config = ferro_server::Config::from_env()?;
            ferro_server::run(config).await?;
            Ok(0)
        }
        Command::Migrate => {
            init_tracing();
            let pool = database().await?;
            ferro_engine::schema_store::bootstrap(&pool, None, None).await?;
            println!("system tables are up to date");
            Ok(0)
        }
        Command::Storage { command } => {
            init_tracing();
            storage_cmd::run(command).await
        }
        Command::Init { dir } => {
            std::fs::create_dir_all(dir.join("schema/collections"))?;
            std::fs::create_dir_all(dir.join("schema/roles"))?;
            std::fs::create_dir_all(dir.join("schema/policies"))?;
            let file = dir.join("ferro.toml");
            if file.exists() {
                println!("{} already exists, left untouched", file.display());
            } else {
                std::fs::write(&file, project::TEMPLATE)?;
                println!("created {}", file.display());
            }
            println!("next: `ferro schema pull --from dev` to start from an existing environment");
            Ok(0)
        }
        Command::Env { command } => match command {
            EnvCommand::List => {
                for (name, e) in &project.file.environments {
                    println!("{name:<12} {}{}", e.url, if e.protected { "  (protected)" } else { "" });
                }
                Ok(0)
            }
            EnvCommand::Status => schema_cmd::env_status(&project).await,
        },
        Command::Schema { command } => match command {
            SchemaCommand::Pull { from, out } => schema_cmd::pull(&project, &from, out).await,
            SchemaCommand::Diff { from, to, exit_code, out } => schema_cmd::diff(&project, from, &to, exit_code, &out).await,
            SchemaCommand::Apply { from, to, allow_destructive, dry_run, yes, out } => {
                schema_cmd::apply(&project, from, &to, allow_destructive, dry_run, yes, &out).await
            }
            SchemaCommand::ImportDirectus { file, out } => schema_cmd::import_directus(&project, &file, out),
            SchemaCommand::Validate { path } => schema_cmd::validate(&project, path),
        },
        Command::Data { command } => match command {
            DataCommand::Export { from, select, out } => data_cmd::export(&project, &from, &select, &out).await,
            DataCommand::Import { to, bundle, import, out } => data_cmd::import(&project, &to, &bundle, &import, &out).await,
            DataCommand::Diff { from, to, select, exit_code, out } => data_cmd::diff(&project, &from, &to, &select, exit_code, &out).await,
            DataCommand::Push { from, to, select, import, out } => data_cmd::push(&project, &from, &to, &select, &import, &out).await,
        },
        Command::User { command } => {
            let pool = database().await?;
            ferro_engine::schema_store::bootstrap(&pool, None, None).await?;
            let mut conn = pool.acquire().await?;
            match command {
                UserCommand::Create { email, password, admin, role } => {
                    let role = if admin { Some(ferro_core::ADMIN_ROLE_ID) } else { role };
                    let id = ferro_engine::auth::create_user(
                        &mut conn,
                        ferro_engine::auth::NewUser {
                            email: &email,
                            password: password.as_deref(),
                            role,
                            first_name: None,
                            last_name: None,
                        },
                    )
                    .await?;
                    println!("created user {email} ({id})");
                }
                UserCommand::ResetTfa { email } => {
                    let Some(id) = ferro_engine::auth::user_id_by_email(&mut conn, &email).await? else { anyhow::bail!("user {email} not found") };
                    ferro_engine::auth::tfa_reset(&mut conn, id).await?;
                    println!("two-factor authentication removed for {email}");
                }
                UserCommand::Token { email } => {
                    let id: Option<uuid::Uuid> = ferro_engine::auth::user_id_by_email(&mut conn, &email).await?;
                    let Some(id) = id else { anyhow::bail!("user {email} not found") };
                    let token = ferro_engine::auth::rotate_static_token(&mut conn, id).await?;
                    println!("{token}");
                    eprintln!("store it in the variable referenced by token_env in ferro.toml; it will not be shown again");
                }
            }
            Ok(0)
        }
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}
