use std::path::Path;

use anyhow::{bail, Context};
use bytes::Bytes;
use ferro_core::bundle::{Bundle, ImportOptions, ImportReport, Selection, Strategy};
use ferro_core::report::import_html;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::project::Project;
use crate::remote::Remote;
use crate::{ui, ImportArgs, ReportArgs, SelectArgs};

fn selections(args: &SelectArgs) -> anyhow::Result<Vec<Selection>> {
    args.select
        .iter()
        .map(|s| {
            Selection::parse(s)
                .map(|mut sel| {
                    sel.with_dependencies = !args.shallow;
                    sel
                })
                .map_err(|e| anyhow::anyhow!(e))
        })
        .collect()
}

fn options(args: &ImportArgs) -> anyhow::Result<ImportOptions> {
    Ok(ImportOptions {
        strategy: args.strategy.parse::<Strategy>().map_err(|e| anyhow::anyhow!(e))?,
        prune: args.prune,
        dry_run: args.dry_run,
        ignore_missing_fields: args.ignore_missing_fields,
        force: args.force,
    })
}

async fn export_bundle(source: &Remote, sels: &[Selection]) -> anyhow::Result<Bundle> {
    let bundle: Bundle = source.post_raw("/sync/export", &json!({ "selections": sels })).await?;
    eprintln!(
        "exported {} items in {} collections and {} files from `{}`",
        bundle.item_count(),
        bundle.items.len(),
        bundle.files.len(),
        source.name
    );
    Ok(bundle)
}

enum Blobs<'a> {
    Dir(&'a Path),
    Remote(&'a Remote),
}

impl Blobs<'_> {
    async fn get(&self, id: Uuid) -> anyhow::Result<Bytes> {
        match self {
            Blobs::Dir(d) => {
                let p = d.join("files").join(id.to_string());
                Ok(Bytes::from(std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?))
            }
            Blobs::Remote(r) => r.get_bytes(&format!("/sync/files/{id}")).await,
        }
    }
}

/// Upload to `target` only the binaries it does not have yet.
async fn transfer_files(target: &Remote, bundle: &Bundle, blobs: Blobs<'_>) -> anyhow::Result<usize> {
    if bundle.files.is_empty() {
        return Ok(0);
    }
    let refs: Vec<Value> = bundle
        .files
        .iter()
        .map(|f| json!({ "id": f.get("id"), "checksum": f.get("checksum") }))
        .collect();
    let missing: Vec<Uuid> = target.post("/sync/missing-files", &refs).await?;
    for id in &missing {
        let data = blobs.get(*id).await?;
        target.put_bytes(&format!("/sync/files/{id}"), data).await?;
    }
    if !missing.is_empty() {
        eprintln!("transferred {} file(s) to `{}`", missing.len(), target.name);
    }
    Ok(missing.len())
}

async fn import_bundle(target: &Remote, bundle: &Bundle, opts: &ImportOptions) -> anyhow::Result<ImportReport> {
    target.post("/sync/import", &json!({ "bundle": bundle, "options": opts })).await
}

fn finish(report: &ImportReport, title: &str, out: &ReportArgs) -> anyhow::Result<()> {
    if out.json {
        println!("{}", serde_json::to_string_pretty(report)?);
    } else {
        ui::print_import(report);
    }
    if let Some(path) = &out.report {
        ui::write_report(path, &import_html(title, report))?;
    }
    Ok(())
}

fn has_changes(r: &ImportReport) -> bool {
    let t = r.totals();
    t.create + t.update + t.delete + t.conflict > 0
}

/// Shared by `import` and `push`: on protected targets a dry run is shown first.
async fn guarded_import(target: &Remote, bundle: &Bundle, args: &ImportArgs, blobs: Blobs<'_>, title: &str, out: &ReportArgs) -> anyhow::Result<i32> {
    let opts = options(args)?;
    if !opts.dry_run && target.protected && !args.yes {
        let preview = import_bundle(target, bundle, &ImportOptions { dry_run: true, ..opts.clone() }).await?;
        ui::print_import(&preview);
        if !preview.ok {
            if let Some(path) = &out.report {
                ui::write_report(path, &import_html(title, &preview))?;
            }
            return Ok(1);
        }
        if !has_changes(&preview) {
            println!("nothing to do");
            return Ok(0);
        }
        ui::confirm(target, "apply these changes", false)?;
    }
    if !opts.dry_run {
        transfer_files(target, bundle, blobs).await?;
    }
    let report = import_bundle(target, bundle, &opts).await?;
    finish(&report, title, out)?;
    Ok(if report.ok { 0 } else { 1 })
}

pub async fn export(project: &Project, from: &str, select: &SelectArgs, out: &Path) -> anyhow::Result<i32> {
    let source = project.env(from)?;
    let bundle = export_bundle(&source, &selections(select)?).await?;
    std::fs::create_dir_all(out.join("files"))?;
    std::fs::write(out.join("bundle.json"), serde_json::to_vec_pretty(&bundle)?)?;
    for id in bundle.file_ids() {
        let data = source.get_bytes(&format!("/sync/files/{id}")).await?;
        std::fs::write(out.join("files").join(id.to_string()), &data)?;
    }
    for (c, items) in &bundle.items {
        println!("  {c:<32} {:>6}", items.len());
    }
    println!("  {:<32} {:>6}", "(files)", bundle.files.len());
    println!("bundle written to {}", out.display());
    Ok(0)
}

fn read_bundle(dir: &Path) -> anyhow::Result<Bundle> {
    let path = if dir.is_file() { dir.to_path_buf() } else { dir.join("bundle.json") };
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

pub async fn import(project: &Project, to: &str, dir: &Path, args: &ImportArgs, out: &ReportArgs) -> anyhow::Result<i32> {
    let target = project.env(to)?;
    let bundle = read_bundle(dir)?;
    let base = if dir.is_file() { dir.parent().unwrap_or(Path::new(".")) } else { dir };
    let title = format!("Import {} -> {}", bundle.source.as_deref().unwrap_or("bundle"), target.name);
    guarded_import(&target, &bundle, args, Blobs::Dir(base), &title, out).await
}

pub async fn diff(project: &Project, from: &str, to: &str, select: &SelectArgs, exit_code: bool, out: &ReportArgs) -> anyhow::Result<i32> {
    let source = project.env(from)?;
    let target = project.env(to)?;
    if source.name == target.name {
        bail!("source and target are the same environment");
    }
    let bundle = export_bundle(&source, &selections(select)?).await?;
    let opts = ImportOptions { strategy: Strategy::Overwrite, prune: true, dry_run: true, ..Default::default() };
    let report = import_bundle(&target, &bundle, &opts).await?;
    finish(&report, &format!("Data diff {} -> {}", source.name, target.name), out)?;
    Ok(if !report.ok {
        1
    } else if exit_code && has_changes(&report) {
        2
    } else {
        0
    })
}

pub async fn push(project: &Project, from: &str, to: &str, select: &SelectArgs, args: &ImportArgs, out: &ReportArgs) -> anyhow::Result<i32> {
    let source = project.env(from)?;
    let target = project.env(to)?;
    if source.name == target.name {
        bail!("source and target are the same environment");
    }
    let bundle = export_bundle(&source, &selections(select)?).await?;
    let title = format!("Push {} -> {}", source.name, target.name);
    guarded_import(&target, &bundle, args, Blobs::Remote(&source), &title, out).await
}
