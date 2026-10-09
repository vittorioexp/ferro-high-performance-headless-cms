use std::io::{IsTerminal, Write};
use std::path::Path;

use anyhow::bail;
use ferro_core::bundle::{ImportReport, ItemAction};
use ferro_core::diff::Plan;

use crate::remote::Remote;

/// Protected environments require `--yes` (CI) or typing their name (interactive).
pub fn confirm(remote: &Remote, what: &str, yes: bool) -> anyhow::Result<()> {
    if !remote.protected || yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!("`{}` is protected: pass --yes to {what}", remote.name);
    }
    print!("`{}` is a protected environment. Type its name to {what}: ", remote.name);
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    if line.trim() != remote.name {
        bail!("aborted");
    }
    Ok(())
}

pub fn write_report(path: &Path, html: &str) -> anyhow::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, html)?;
    eprintln!("report written to {}", path.display());
    Ok(())
}

pub fn short(hash: &str) -> &str {
    &hash[..12.min(hash.len())]
}

pub fn print_plan(title: &str, plan: &Plan) {
    println!("{title}");
    println!("  from {} -> to {}", short(&plan.from_hash), short(&plan.to_hash));
    if plan.is_empty() {
        println!("  no differences");
    }
    for line in plan.summary_lines() {
        println!("  {line}");
    }
    for w in &plan.warnings {
        println!("  warning: {w}");
    }
}

pub fn print_import(report: &ImportReport) {
    let mode = if report.dry_run {
        "DRY RUN — nothing written"
    } else if report.committed {
        "APPLIED"
    } else {
        "NOT APPLIED"
    };
    println!(
        "{} -> {}   {mode}",
        report.source.as_deref().unwrap_or("bundle"),
        report.target.as_deref().unwrap_or("target")
    );
    println!("  {:<32} {:>7} {:>7} {:>7} {:>8} {:>9} {:>5}", "collection", "create", "update", "delete", "conflict", "unchanged", "skip");
    for (c, s) in &report.summary {
        println!(
            "  {:<32} {:>7} {:>7} {:>7} {:>8} {:>9} {:>5}",
            c, s.create, s.update, s.delete, s.conflict, s.unchanged, s.skip
        );
    }
    let conflicts: Vec<_> = report.items.iter().filter(|i| i.action == ItemAction::Conflict).collect();
    for c in conflicts.iter().take(20) {
        println!(
            "  conflict {}/{} {}: {}",
            c.collection,
            c.id,
            c.label.as_deref().unwrap_or(""),
            c.message.as_deref().unwrap_or("")
        );
    }
    if conflicts.len() > 20 {
        println!("  ... {} more conflicts (see --report)", conflicts.len() - 20);
    }
    for w in report.warnings.iter().take(30) {
        println!("  warning: {w}");
    }
    if report.warnings.len() > 30 {
        println!("  ... {} more warnings", report.warnings.len() - 30);
    }
    for e in &report.errors {
        println!("  error: {e}");
    }
}
