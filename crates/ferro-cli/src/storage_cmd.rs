//! `ferro storage ...`: operate on the file locations of one environment (direct database access).

use ferro_engine::files::Storages;
use sqlx::Row;
use uuid::Uuid;

use crate::StorageCommand;

pub async fn run(command: StorageCommand) -> anyhow::Result<i32> {
    let cfg = ferro_storage::StorageConfig::from_env().map_err(anyhow::Error::msg)?;
    let pool = crate::database().await?;
    match command {
        StorageCommand::List => {
            let counts = sqlx::query("SELECT storage, count(*) AS n, coalesce(sum(filesize), 0)::bigint AS bytes FROM ferro_files GROUP BY storage")
                .fetch_all(&pool)
                .await?;
            for loc in &cfg.locations {
                let (n, bytes) = counts
                    .iter()
                    .find(|r| r.get::<String, _>("storage") == loc.name)
                    .map(|r| (r.get::<i64, _>("n"), r.get::<i64, _>("bytes")))
                    .unwrap_or((0, 0));
                println!(
                    "{:<12} {:<6} {:>6} files {:>12} bytes{}",
                    loc.name,
                    loc.driver.label(),
                    n,
                    bytes,
                    if loc.name == cfg.default { "  (default)" } else { "" }
                );
            }
            for r in &counts {
                let name: String = r.get("storage");
                if !cfg.locations.iter().any(|l| l.name == name) {
                    println!("{name:<12} NOT CONFIGURED: {} files point to it (served from the default location)", r.get::<i64, _>("n"));
                }
            }
            Ok(0)
        }
        StorageCommand::Check { exit_code } => {
            let storages = ferro_storage::build(&cfg).await?;
            println!("all {} location(s) accept writes", cfg.locations.len());
            let rows = sqlx::query("SELECT id, storage FROM ferro_files ORDER BY id").fetch_all(&pool).await?;
            let mut missing = 0;
            for r in &rows {
                let id: Uuid = r.get("id");
                let name: String = r.get("storage");
                if !storages.resolve(Some(&name)).exists(&id.to_string()).await? {
                    missing += 1;
                    println!("MISSING {id} (location {name})");
                }
            }
            println!("{} file(s) checked, {missing} missing", rows.len());
            Ok(if missing > 0 && exit_code { 2 } else { 0 })
        }
        StorageCommand::Migrate { from, to, delete_source, dry_run } => {
            let storages = ferro_storage::build(&cfg).await?;
            migrate(&pool, &storages, &from, &to, delete_source, dry_run).await
        }
    }
}

async fn migrate(pool: &sqlx::PgPool, storages: &Storages, from: &str, to: &str, delete_source: bool, dry_run: bool) -> anyhow::Result<i32> {
    if from == to {
        anyhow::bail!("--from and --to are the same location");
    }
    let src = storages.get(from).ok_or_else(|| anyhow::anyhow!("location `{from}` is not configured"))?;
    let dst = storages.get(to).ok_or_else(|| anyhow::anyhow!("location `{to}` is not configured"))?;
    let rows = sqlx::query("SELECT id, type, checksum FROM ferro_files WHERE storage = $1 ORDER BY id").bind(from).fetch_all(pool).await?;
    println!("{} file(s) in `{from}`{}", rows.len(), if dry_run { " (dry run, nothing is changed)" } else { "" });
    let (mut moved, mut failed) = (0, 0);
    for r in &rows {
        let id: Uuid = r.get("id");
        let key = id.to_string();
        if dry_run {
            continue;
        }
        let result: anyhow::Result<()> = async {
            let data = src.get(&key).await?;
            if let Some(expected) = r.get::<Option<String>, _>("checksum") {
                let actual = ferro_core::hash::sha256_hex(&data);
                anyhow::ensure!(actual == expected, "checksum of the source blob does not match the row");
            }
            let ct: Option<String> = r.get("type");
            dst.put(&key, data, ct.as_deref()).await?;
            anyhow::ensure!(dst.exists(&key).await?, "the copy is not visible in `{to}`");
            // conditional: a concurrent replace that already moved the file elsewhere wins
            let done = sqlx::query("UPDATE ferro_files SET storage = $1 WHERE id = $2 AND storage = $3").bind(to).bind(id).bind(from).execute(pool).await?;
            if done.rows_affected() == 1 && delete_source {
                src.delete(&key).await?;
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => moved += 1,
            Err(e) => {
                failed += 1;
                eprintln!("FAILED {id}: {e:#}");
            }
        }
    }
    println!("moved {moved}, failed {failed}");
    Ok(if failed > 0 { 1 } else { 0 })
}
