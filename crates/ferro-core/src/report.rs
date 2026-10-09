//! Self-contained HTML reports (open in a browser, attach to a CI build).

use crate::bundle::{ImportReport, ItemAction};
use crate::diff::{Plan, Risk};

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

const STYLE: &str = r#"
body{font-family:system-ui,-apple-system,Segoe UI,sans-serif;margin:2rem;color:#1d2433;background:#fafbfc}
h1{font-size:1.4rem}h2{font-size:1.1rem;margin-top:2rem}
table{border-collapse:collapse;width:100%;background:#fff;margin:.5rem 0}
th,td{border:1px solid #dde1e6;padding:.35rem .6rem;text-align:left;vertical-align:top;font-size:.9rem}
th{background:#f0f2f5}
code,pre{font-family:ui-monospace,Consolas,monospace;font-size:.82rem;white-space:pre-wrap;word-break:break-all;margin:0}
.tag{display:inline-block;padding:0 .45rem;border-radius:.6rem;font-size:.75rem;font-weight:600}
.safe,.create{background:#e3f5e8;color:#1c6b35}.risky,.update{background:#fff4d6;color:#7a5600}
.destructive,.delete,.conflict{background:#fde4e4;color:#9b1c1c}.skip,.unchanged{background:#eef0f3;color:#555}
.muted{color:#6b7280}.before{background:#fff1f1}.after{background:#effaf2}
"#;

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"it\"><head><meta charset=\"utf-8\"><title>{t}</title><style>{STYLE}</style></head><body><h1>{t}</h1>{body}</body></html>",
        t = escape(title)
    )
}

fn risk_tag(r: Risk) -> &'static str {
    match r {
        Risk::Safe => "<span class=\"tag safe\">safe</span>",
        Risk::Risky => "<span class=\"tag risky\">risky</span>",
        Risk::Destructive => "<span class=\"tag destructive\">destructive</span>",
    }
}

pub fn plan_html(title: &str, plan: &Plan, sql: &[String]) -> String {
    let mut body = format!(
        "<p class=\"muted\">from <code>{}</code> → to <code>{}</code> · {} changes · max risk {}</p>",
        &plan.from_hash[..12.min(plan.from_hash.len())],
        &plan.to_hash[..12.min(plan.to_hash.len())],
        plan.changes.len(),
        risk_tag(plan.max_risk())
    );
    if plan.is_empty() {
        body.push_str("<p>No differences: the environments are aligned.</p>");
    } else {
        body.push_str("<table><tr><th>#</th><th>Risk</th><th>Change</th><th>Detail</th></tr>");
        for (i, c) in plan.changes.iter().enumerate() {
            let detail = serde_json::to_string_pretty(c).unwrap_or_default();
            body.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td><details><summary>json</summary><pre>{}</pre></details></td></tr>",
                i + 1,
                risk_tag(c.risk()),
                escape(&c.summary()),
                escape(&detail)
            ));
        }
        body.push_str("</table>");
    }
    if !plan.warnings.is_empty() {
        body.push_str("<h2>Warnings</h2><ul>");
        for w in &plan.warnings {
            body.push_str(&format!("<li>{}</li>", escape(w)));
        }
        body.push_str("</ul>");
    }
    if !sql.is_empty() {
        body.push_str("<h2>SQL</h2><pre>");
        for s in sql {
            body.push_str(&escape(s));
            body.push_str(";\n");
        }
        body.push_str("</pre>");
    }
    page(title, &body)
}

fn action_tag(a: ItemAction) -> String {
    let name = match a {
        ItemAction::Create => "create",
        ItemAction::Update => "update",
        ItemAction::Unchanged => "unchanged",
        ItemAction::Skip => "skip",
        ItemAction::Conflict => "conflict",
        ItemAction::Delete => "delete",
    };
    format!("<span class=\"tag {name}\">{name}</span>")
}

fn short_json(v: &serde_json::Value) -> String {
    let s = match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if s.chars().count() > 400 {
        format!("{}…", s.chars().take(400).collect::<String>())
    } else {
        s
    }
}

pub fn import_html(title: &str, report: &ImportReport) -> String {
    let t = report.totals();
    let mut body = format!(
        "<p class=\"muted\">{} → {} · {} · create {} · update {} · delete {} · conflict {} · unchanged {} · skip {}</p>",
        escape(report.source.as_deref().unwrap_or("bundle")),
        escape(report.target.as_deref().unwrap_or("target")),
        if report.dry_run { "DRY RUN (nothing written)" } else if report.committed { "applied" } else { "NOT applied" },
        t.create, t.update, t.delete, t.conflict, t.unchanged, t.skip
    );
    for (title, list) in [("Errors", &report.errors), ("Warnings", &report.warnings)] {
        if !list.is_empty() {
            body.push_str(&format!("<h2>{title}</h2><ul>"));
            for e in list {
                body.push_str(&format!("<li>{}</li>", escape(e)));
            }
            body.push_str("</ul>");
        }
    }
    body.push_str("<h2>Per collection</h2><table><tr><th>Collection</th><th>create</th><th>update</th><th>delete</th><th>conflict</th><th>unchanged</th><th>skip</th></tr>");
    for (c, s) in &report.summary {
        body.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape(c), s.create, s.update, s.delete, s.conflict, s.unchanged, s.skip
        ));
    }
    body.push_str("</table><h2>Items</h2>");
    if report.items.is_empty() {
        body.push_str("<p>No item differences.</p>");
    } else {
        body.push_str("<table><tr><th>Action</th><th>Collection</th><th>Item</th><th>Changes</th></tr>");
        for it in &report.items {
            let mut changes = String::new();
            if !it.changes.is_empty() {
                changes.push_str("<table><tr><th>field</th><th>before</th><th>after</th></tr>");
                for c in &it.changes {
                    changes.push_str(&format!(
                        "<tr><td><code>{}</code></td><td class=\"before\"><code>{}</code></td><td class=\"after\"><code>{}</code></td></tr>",
                        escape(&c.field),
                        escape(&short_json(&c.before)),
                        escape(&short_json(&c.after))
                    ));
                }
                changes.push_str("</table>");
            }
            if let Some(m) = &it.message {
                changes.push_str(&format!("<p class=\"muted\">{}</p>", escape(m)));
            }
            let mut ident = format!("<code>{}</code>", it.id);
            if let Some(l) = &it.label {
                ident = format!("{}<br>{}", escape(l), ident);
            }
            if let Some(src) = &it.source_id {
                ident.push_str(&format!("<br><span class=\"muted\">source id {src}</span>"));
            }
            body.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                action_tag(it.action),
                escape(&it.collection),
                ident,
                changes
            ));
        }
        body.push_str("</table>");
    }
    page(title, &body)
}
