use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use ferro_core::diff::diff as diff_snapshots;
use ferro_core::project::{read_snapshot, write_snapshot};
use ferro_core::report::plan_html;
use ferro_core::validate::validate_snapshot;
use ferro_core::Snapshot;
use ferro_engine::ddl::plan_to_sql;
use ferro_engine::schema_store::ApplyResult;
use serde_json::{json, Value};

use crate::project::Project;
use crate::remote::Remote;
use crate::ui;
use crate::ReportArgs;

pub enum Source {
    Env(Remote),
    Path(PathBuf),
}

impl Source {
    pub fn label(&self) -> String {
        match self {
            Source::Env(r) => r.name.clone(),
            Source::Path(p) => p.display().to_string(),
        }
    }
}

pub fn resolve(project: &Project, spec: Option<&str>) -> anyhow::Result<Source> {
    match spec {
        None => Ok(Source::Path(project.schema_dir())),
        Some(s) if project.has_env(s) => Ok(Source::Env(project.env(s)?)),
        Some(s) => {
            let p = PathBuf::from(s);
            if !p.exists() {
                bail!("`{s}` is neither an environment of ferro.toml nor an existing path");
            }
            Ok(Source::Path(p))
        }
    }
}

pub async fn load(src: &Source) -> anyhow::Result<Snapshot> {
    match src {
        Source::Env(r) => r.get::<Snapshot>("/schema/snapshot").await,
        Source::Path(p) => read_snapshot(p).with_context(|| format!("reading schema from {}", p.display())),
    }
}

fn output(out: &ReportArgs, title: &str, res: &ApplyResult) -> anyhow::Result<()> {
    if let Some(path) = &out.report {
        ui::write_report(path, &plan_html(title, &res.plan, &res.sql))?;
    }
    if out.json {
        println!("{}", serde_json::to_string_pretty(res)?);
    }
    Ok(())
}

pub async fn pull(project: &Project, from: &str, out: Option<PathBuf>) -> anyhow::Result<i32> {
    let remote = project.env(from)?;
    let snap: Snapshot = remote.get("/schema/snapshot").await?;
    let out = out.unwrap_or_else(|| project.schema_dir());
    write_snapshot(&out, &snap)?;
    println!(
        "{} collections, {} roles, {} policies from `{}` -> {} (hash {})",
        snap.collections.len(),
        snap.roles.len(),
        snap.policies.len(),
        remote.name,
        out.display(),
        ui::short(&snap.hash())
    );
    Ok(0)
}

pub async fn diff(project: &Project, from: Option<String>, to: &str, exit_code: bool, out: &ReportArgs) -> anyhow::Result<i32> {
    let source = resolve(project, from.as_deref())?;
    let desired = load(&source).await?;
    let target = resolve(project, Some(to))?;
    let res = match &target {
        Source::Env(r) => r.post::<Snapshot, ApplyResult>("/schema/plan", &desired).await?,
        Source::Path(p) => {
            let current = read_snapshot(p)?;
            let plan = diff_snapshots(&current, &desired);
            let sql = plan_to_sql(&plan, &current).unwrap_or_default();
            ApplyResult { plan, sql, applied: false }
        }
    };
    let title = format!("Schema diff: {} -> {}", source.label(), target.label());
    if !out.json {
        ui::print_plan(&title, &res.plan);
    }
    output(out, &title, &res)?;
    Ok(if exit_code && !res.plan.is_empty() { 2 } else { 0 })
}

pub async fn apply(
    project: &Project,
    from: Option<String>,
    to: &str,
    allow_destructive: bool,
    dry_run: bool,
    yes: bool,
    out: &ReportArgs,
) -> anyhow::Result<i32> {
    let source = resolve(project, from.as_deref())?;
    let desired = load(&source).await?;
    let remote = project.env(to)?;
    let planned: ApplyResult = remote.post("/schema/plan", &desired).await?;
    let title = format!("Schema apply: {} -> {}", source.label(), remote.name);
    ui::print_plan(&title, &planned.plan);
    if planned.plan.is_empty() {
        println!("`{}` is already up to date", remote.name);
        return Ok(0);
    }
    if planned.plan.has_destructive() && !allow_destructive {
        bail!("the plan contains destructive changes: review them and re-run with --allow-destructive");
    }
    if !dry_run {
        ui::confirm(&remote, "apply these schema changes", yes)?;
    }
    let body = json!({
        "snapshot": desired,
        "expected_hash": planned.plan.from_hash,
        "allow_destructive": allow_destructive,
        "dry_run": dry_run,
    });
    let res: ApplyResult = remote.post("/schema/apply", &body).await?;
    if dry_run {
        println!("dry run: {} statements executed and rolled back successfully", res.sql.len());
    } else {
        println!("applied {} changes ({} statements) to `{}`", res.plan.changes.len(), res.sql.len(), remote.name);
    }
    output(out, &title, &res)?;
    Ok(0)
}

pub fn import_directus(project: &Project, file: &Path, out: Option<PathBuf>) -> anyhow::Result<i32> {
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let mut value: Value = serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
    if value.get("collections").is_none() {
        if let Some(inner) = value.get_mut("data").map(Value::take) {
            value = inner;
        }
    }
    let converted = ferro_core::directus::convert(&value).map_err(|e| anyhow::anyhow!(e))?;
    let out = out.unwrap_or_else(|| project.schema_dir());
    write_snapshot(&out, &converted.snapshot)?;
    for w in &converted.warnings {
        println!("warning: {w}");
    }
    println!(
        "{} collections converted -> {} (hash {}); roles and permissions are not converted",
        converted.snapshot.collections.len(),
        out.display(),
        ui::short(&converted.snapshot.hash())
    );
    Ok(0)
}

pub fn validate(project: &Project, path: Option<PathBuf>) -> anyhow::Result<i32> {
    let path = path.unwrap_or_else(|| project.schema_dir());
    let snap = read_snapshot(&path)?;
    let errors = validate_snapshot(&snap);
    if errors.is_empty() {
        println!(
            "{}: valid ({} collections, {} roles, {} policies, hash {})",
            path.display(),
            snap.collections.len(),
            snap.roles.len(),
            snap.policies.len(),
            ui::short(&snap.hash())
        );
        Ok(0)
    } else {
        for e in &errors {
            println!("error: {e}");
        }
        Ok(1)
    }
}

pub async fn env_status(project: &Project) -> anyhow::Result<i32> {
    let local = read_snapshot(&project.schema_dir()).ok().map(|s| s.hash());
    if let Some(h) = &local {
        println!("{:<12} {:<45} {}", "local", project.schema_dir().display(), ui::short(h));
    }
    let mut failures = 0;
    for name in project.file.environments.keys() {
        let remote = match project.env(name) {
            Ok(r) => r,
            Err(e) => {
                println!("{name:<12} {e}");
                failures += 1;
                continue;
            }
        };
        match remote.get::<Value>("/server/info").await {
            Ok(info) => {
                let hash = info.get("schema_hash").and_then(|v| v.as_str()).unwrap_or("?").to_string();
                let cmp = match &local {
                    Some(l) if *l == hash => "= local schema",
                    Some(_) => "differs from local schema",
                    None => "",
                };
                println!("{name:<12} {:<45} {} {cmp}", remote.url, ui::short(&hash));
            }
            Err(e) => {
                println!("{name:<12} {:<45} unreachable: {e}", remote.url);
                failures += 1;
            }
        }
    }
    Ok(if failures > 0 { 1 } else { 0 })
}
