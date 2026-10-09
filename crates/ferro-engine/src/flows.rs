//! Flow runtime: executes the automations defined in the snapshot.
//!
//! * **Event flows (non blocking)** are queued in `ferro_flow_queue` inside the
//!   transaction of the write that caused them, so they exist only if the write
//!   commits. A worker on every instance claims jobs with `FOR UPDATE SKIP LOCKED`:
//!   each job runs exactly once, whichever instance picks it up.
//! * **Blocking event flows** run inside the write, before it, on its connection.
//! * **Schedule flows** are enqueued once per matching minute (unique `dedupe_key`).
//! * **Webhook / manual flows** run synchronously (or queued with `run_async`).
//!
//! A flow run is one transaction: if the flow fails nothing it wrote is kept.
//! Writes made by flows carry the origin `flow` and never trigger other flows;
//! chaining is explicit (`trigger_flow` operation, at most [`MAX_FLOW_DEPTH`] deep).

use std::net::IpAddr;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use ferro_core::cron::Cron;
use ferro_core::fields::FieldTree;
use ferro_core::filter::matches as filter_matches;
use ferro_core::flow::*;
use futures::future::BoxFuture;
use serde_json::{json, Map, Value};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::items::{self, Ctx, ReadArgs};
use crate::permissions::Accountability;
use crate::schema::Schema;
use crate::{FerroError, Result};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_LOGGED_CHARS: usize = 4000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// runtime
// ---------------------------------------------------------------------------

pub struct Runtime {
    http: reqwest::Client,
    allow_private: bool,
    allowed_hosts: Vec<String>,
    /// `$env` of the data chain: variables `FERRO_FLOW_ENV_<NAME>` as `<NAME>`.
    env: Map<String, Value>,
}

impl Runtime {
    pub fn new(allow_private: bool, allowed_hosts: Vec<String>) -> Runtime {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none()) // redirects would bypass the host checks
            .user_agent(concat!("ferro-flows/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        let env = std::env::vars()
            .filter_map(|(k, v)| k.strip_prefix("FERRO_FLOW_ENV_").map(|n| (n.to_string(), Value::String(v))))
            .collect();
        Runtime { http, allow_private, allowed_hosts, env }
    }
}

static RUNTIME: OnceLock<Arc<Runtime>> = OnceLock::new();

/// Called once at startup. Without it flows run with safe defaults.
pub fn init(rt: Runtime) {
    let _ = RUNTIME.set(Arc::new(rt));
}

fn runtime() -> Arc<Runtime> {
    RUNTIME.get_or_init(|| Arc::new(Runtime::new(false, Vec::new()))).clone()
}

fn is_private_host(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    match h.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            let o = ip.octets();
            ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified() || ip.is_broadcast() || (o[0] == 100 && (o[1] & 0xC0) == 64)
        }
        Ok(IpAddr::V6(ip)) => {
            let s = ip.segments();
            ip.is_loopback()
                || ip.is_unspecified()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                || ip.to_ipv4_mapped().map(|v4| is_private_host(&v4.to_string())).unwrap_or(false)
        }
        Err(_) => h == "localhost" || h.ends_with(".localhost") || h.ends_with(".local") || h.ends_with(".internal"),
    }
}

/// Outbound requests are limited to http(s) and, by default, to public hosts.
/// Literal addresses are checked; a name that *resolves* to a private address is
/// not (use `FERRO_FLOW_ALLOWED_HOSTS` to restrict flows to known hosts).
pub fn check_url(raw: &str, allow_private: bool, allowed: &[String]) -> std::result::Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(raw).map_err(|e| format!("invalid url `{raw}`: {e}"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(format!("unsupported url scheme `{}`", url.scheme()));
    }
    let host = url.host_str().ok_or_else(|| "url without host".to_string())?.to_ascii_lowercase();
    if !allowed.is_empty() {
        let ok = allowed.iter().any(|a| host == *a || host.ends_with(&format!(".{a}")));
        return if ok { Ok(url) } else { Err(format!("host `{host}` is not in FERRO_FLOW_ALLOWED_HOSTS")) };
    }
    if !allow_private && is_private_host(&host) {
        return Err(format!("host `{host}` is private (set FERRO_FLOW_ALLOW_PRIVATE=true to allow it)"));
    }
    Ok(url)
}

// ---------------------------------------------------------------------------
// execution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Outcome {
    pub ok: bool,
    pub error: Option<String>,
    pub log: Vec<Value>,
    /// The final data chain.
    pub data: Value,
}

struct Step {
    value: Value,
    /// `false` = follow `reject` (a condition that did not match).
    pass: bool,
}

fn shorten(v: &Value) -> Value {
    let s = v.to_string();
    if s.chars().count() > MAX_LOGGED_CHARS {
        Value::String(format!("{}…", s.chars().take(MAX_LOGGED_CHARS).collect::<String>()))
    } else {
        v.clone()
    }
}

pub fn flow_accountability(flow: &Flow, caller: &Accountability) -> Accountability {
    match flow.run_as {
        RunAs::System => Accountability::system(),
        RunAs::Caller => caller.clone(),
    }
}

fn set_data(data: &mut Value, key: &str, v: Value) {
    if let Some(m) = data.as_object_mut() {
        m.insert(key.to_string(), v.clone());
        m.insert("$last".to_string(), v);
    }
}

/// Run `flow` on `conn`. Never fails: errors are reported in the [`Outcome`].
pub fn execute_flow<'a>(
    conn: &'a mut PgConnection,
    schema: Arc<Schema>,
    flow: &'a Flow,
    trigger: Value,
    acc: Accountability,
    depth: u8,
) -> BoxFuture<'a, Outcome> {
    Box::pin(async move {
        let rt = runtime();
        let ctx = Ctx { schema: schema.clone(), acc: acc.clone(), origin: "flow" };
        let mut data = json!({
            "$trigger": trigger,
            "$last": Value::Null,
            "$env": Value::Object(rt.env.clone()),
            "$accountability": { "user": acc.user, "role": acc.role, "admin": acc.admin },
            "$flow": { "id": flow.id, "name": flow.name },
            "$depth": depth,
        });
        let mut log: Vec<Value> = Vec::new();
        let mut error: Option<String> = None;
        let mut key = flow.start.clone().or_else(|| flow.operations.first().map(|o| o.key.clone()));
        let mut steps = 0usize;

        while let Some(k) = key.take() {
            steps += 1;
            if steps > MAX_STEPS {
                error = Some("too many steps (loop?)".to_string());
                break;
            }
            let Some(op) = flow.operations.iter().find(|o| o.key == k) else {
                error = Some(format!("unknown operation `{k}`"));
                break;
            };
            let started = Instant::now();
            match run_op(&rt, &ctx, &mut *conn, &schema, flow, op, &data, depth).await {
                Ok(step) => {
                    log.push(json!({ "key": k, "type": op.ty, "ok": true, "pass": step.pass, "ms": started.elapsed().as_millis() as u64, "result": shorten(&step.value) }));
                    set_data(&mut data, &k, step.value);
                    key = if step.pass { op.resolve.clone() } else { op.reject.clone() };
                }
                Err(msg) => {
                    log.push(json!({ "key": k, "type": op.ty, "ok": false, "ms": started.elapsed().as_millis() as u64, "error": msg }));
                    set_data(&mut data, &k, json!({ "error": msg }));
                    match &op.reject {
                        Some(next) => key = Some(next.clone()),
                        None => {
                            error = Some(format!("operation `{k}` failed: {msg}"));
                            break;
                        }
                    }
                }
            }
        }
        Outcome { ok: error.is_none(), error, log, data }
    })
}

fn uuids(v: Value) -> std::result::Result<Vec<Uuid>, String> {
    let items = match v {
        Value::Array(a) => a,
        Value::Null => Vec::new(),
        other => vec![other],
    };
    items
        .iter()
        .map(|x| x.as_str().and_then(|s| Uuid::parse_str(s).ok()).ok_or_else(|| format!("`{x}` is not a valid id")))
        .collect()
}

fn text(v: &str, data: &Value) -> String {
    render_text(v, data)
}

#[allow(clippy::too_many_arguments)]
async fn run_op(
    rt: &Runtime,
    ctx: &Ctx,
    conn: &mut PgConnection,
    schema: &Arc<Schema>,
    flow: &Flow,
    op: &Operation,
    data: &Value,
    depth: u8,
) -> std::result::Result<Step, String> {
    let ok = |value: Value| -> std::result::Result<Step, String> { Ok(Step { value, pass: true }) };
    let fe = |e: FerroError| e.to_string();
    match op.ty {
        OpType::Log => {
            let o: LogOpts = parse_options(op)?;
            let message = text(&o.message, data);
            tracing::info!(flow = %flow.name, operation = %op.key, "{message}");
            ok(Value::String(message))
        }
        OpType::Request => {
            let o: RequestOpts = parse_options(op)?;
            let url = check_url(&text(&o.url, data), rt.allow_private, &rt.allowed_hosts)?;
            let method = reqwest::Method::from_bytes(o.method.to_ascii_uppercase().as_bytes()).map_err(|_| format!("invalid method `{}`", o.method))?;
            let mut req = rt.http.request(method, url);
            for (k, v) in &o.headers {
                req = req.header(k.as_str(), text(v.as_str().unwrap_or_default(), data));
            }
            if let Some(body) = &o.body {
                req = req.json(&render(body, data));
            }
            let resp = req.send().await.map_err(|e| format!("request failed: {e}"))?;
            let status = resp.status();
            let headers: Map<String, Value> = resp
                .headers()
                .iter()
                .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str().to_string(), Value::String(s.to_string()))))
                .collect();
            let bytes = resp.bytes().await.map_err(|e| format!("reading the response failed: {e}"))?;
            let slice = &bytes[..bytes.len().min(MAX_RESPONSE_BYTES)];
            let body = serde_json::from_slice::<Value>(slice).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(slice).into_owned()));
            if o.fail_on_status && !status.is_success() {
                return Err(format!("HTTP {}", status.as_u16()));
            }
            ok(json!({ "status": status.as_u16(), "ok": status.is_success(), "headers": headers, "data": body }))
        }
        OpType::ItemCreate => {
            let o: ItemCreateOpts = parse_options(op)?;
            let id = items::create_one(ctx, conn, &text(&o.collection, data), render(&o.payload, data)).await.map_err(fe)?;
            ok(json!({ "id": id }))
        }
        OpType::ItemUpdate => {
            let o: ItemUpdateOpts = parse_options(op)?;
            let collection = text(&o.collection, data);
            let ids = uuids(render(&o.ids, data))?;
            let payload = render(&o.payload, data);
            for id in &ids {
                items::update_one(ctx, conn, &collection, *id, payload.clone()).await.map_err(fe)?;
            }
            ok(json!({ "ids": ids }))
        }
        OpType::ItemDelete => {
            let o: ItemDeleteOpts = parse_options(op)?;
            let ids = uuids(render(&o.ids, data))?;
            items::delete_many(ctx, conn, &text(&o.collection, data), ids.clone()).await.map_err(fe)?;
            ok(json!({ "ids": ids }))
        }
        OpType::ItemRead => {
            let o: ItemReadOpts = parse_options(op)?;
            let sort: Vec<String> = match o.sort.as_ref().map(|s| render(s, data)) {
                Some(Value::String(s)) => s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
                Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
                _ => Vec::new(),
            };
            let args = ReadArgs {
                fields: FieldTree::star(),
                filter: o.filter.as_ref().map(|f| render(f, data)),
                sort,
                limit: Some(o.limit.unwrap_or(100)),
                ..Default::default()
            };
            let rows = items::read_many(ctx, conn, &text(&o.collection, data), args).await.map_err(fe)?;
            ok(Value::Array(rows))
        }
        OpType::Condition => {
            let o: ConditionOpts = parse_options(op)?;
            let pass = filter_matches(&render(&o.rule, data), data);
            Ok(Step { value: Value::Bool(pass), pass })
        }
        OpType::Transform => {
            let o: TransformOpts = parse_options(op)?;
            ok(render(&o.value, data))
        }
        OpType::TriggerFlow => {
            let o: TriggerFlowOpts = parse_options(op)?;
            if depth >= MAX_FLOW_DEPTH {
                return Err(format!("flows are chained more than {MAX_FLOW_DEPTH} levels deep"));
            }
            let target = schema.snapshot.flow(o.flow).ok_or_else(|| format!("flow {} does not exist", o.flow))?;
            if !target.active {
                return Err(format!("flow `{}` is not active", target.name));
            }
            let payload = o.payload.as_ref().map(|p| render(p, data)).unwrap_or(Value::Null);
            let trigger = json!({ "event": "flow", "payload": payload, "parent": flow.id });
            let out = execute_flow(conn, schema.clone(), target, trigger, ctx.acc.clone(), depth + 1).await;
            if let Some(e) = out.error {
                return Err(format!("flow `{}` failed: {e}", target.name));
            }
            ok(out.data.get("$last").cloned().unwrap_or(Value::Null))
        }
        OpType::Fail => {
            let o: FailOpts = parse_options(op)?;
            Err(o.message.map(|m| text(&m, data)).unwrap_or_else(|| "the flow was stopped".to_string()))
        }
    }
}

// ---------------------------------------------------------------------------
// hooks called by the item service
// ---------------------------------------------------------------------------

fn trigger_json(ctx: &Ctx, event: &str, collection: &str, keys: &[Uuid], payload: Option<&Map<String, Value>>) -> Value {
    json!({
        "event": event,
        "collection": collection,
        "keys": keys,
        "key": keys.first(),
        "payload": payload,
        "accountability": { "user": ctx.acc.user, "role": ctx.acc.role },
        "timestamp": Utc::now().to_rfc3339(),
    })
}

/// Blocking event flows: may rewrite the payload (via the flow `output`) or reject
/// the write (a failing operation, e.g. `fail`).
pub async fn before_write(
    ctx: &Ctx,
    conn: &mut PgConnection,
    collection: &str,
    action: &str,
    keys: &[Uuid],
    payload: Option<Map<String, Value>>,
) -> Result<Option<Map<String, Value>>> {
    if ctx.origin == "flow" {
        return Ok(payload);
    }
    let event = event_name(collection, action);
    let flows = matching_event_flows(&ctx.schema.snapshot.flows, &event, true);
    if flows.is_empty() {
        return Ok(payload);
    }
    let mut payload = payload;
    for flow in flows {
        let trigger = trigger_json(ctx, &event, collection, keys, payload.as_ref());
        let acc = flow_accountability(flow, &ctx.acc);
        let out = execute_flow(&mut *conn, ctx.schema.clone(), flow, trigger, acc, 0).await;
        if let Some(e) = out.error {
            return Err(FerroError::invalid(format!("Rejected by flow \"{}\": {e}", flow.name)));
        }
        if let Some(template) = &flow.output {
            match render(template, &out.data) {
                Value::Object(m) => payload = Some(m),
                Value::Null => {}
                _ => return Err(FerroError::invalid(format!("Flow \"{}\": the output of a blocking flow must be an object", flow.name))),
            }
        }
    }
    Ok(payload)
}

/// Queue the non-blocking event flows (called inside the transaction of the write).
pub async fn after_write(
    ctx: &Ctx,
    conn: &mut PgConnection,
    collection: &str,
    action: &str,
    keys: &[Uuid],
    payload: Option<&Map<String, Value>>,
) -> Result<()> {
    if ctx.origin == "flow" {
        return Ok(());
    }
    let event = event_name(collection, action);
    let flows = matching_event_flows(&ctx.schema.snapshot.flows, &event, false);
    if flows.is_empty() {
        return Ok(());
    }
    let mut trigger = trigger_json(ctx, &event, collection, keys, payload);
    if action != "delete" && keys.len() == 1 {
        let info = ctx.schema.collection(collection)?;
        if let Some(item) = items::raw_get(&mut *conn, &info, keys[0]).await? {
            trigger["item"] = Value::Object(item);
        }
    }
    for flow in flows {
        enqueue(&mut *conn, flow.id, &trigger, None).await?;
    }
    Ok(())
}

async fn enqueue(conn: &mut PgConnection, flow_id: Uuid, trigger: &Value, dedupe_key: Option<&str>) -> Result<bool> {
    let n = sqlx::query("INSERT INTO ferro_flow_queue (id, flow_id, trigger, dedupe_key) VALUES ($1, $2, $3, $4) ON CONFLICT (dedupe_key) DO NOTHING")
        .bind(Uuid::now_v7())
        .bind(flow_id)
        .bind(trigger)
        .bind(dedupe_key)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    Ok(n > 0)
}

// ---------------------------------------------------------------------------
// synchronous runs (webhook, manual) and run history
// ---------------------------------------------------------------------------

async fn record_run(pool: &PgPool, flow: &Flow, source: &str, trigger: &Value, outcome: &Outcome, started: DateTime<Utc>) {
    let res = sqlx::query(
        "INSERT INTO ferro_flow_runs (id, flow_id, flow_name, source, status, started, finished, trigger, log, error) \
         VALUES ($1, $2, $3, $4, $5, $6, now(), $7, $8, $9)",
    )
    .bind(Uuid::now_v7())
    .bind(flow.id)
    .bind(flow.name.clone())
    .bind(source)
    .bind(if outcome.ok { "ok" } else { "failed" })
    .bind(started)
    .bind(shorten(trigger))
    .bind(Value::Array(outcome.log.clone()))
    .bind(outcome.error.as_deref())
    .execute(pool)
    .await;
    if let Err(e) = res {
        tracing::warn!(error = %e, flow = %flow.name, "cannot record the flow run");
    }
}

/// Run a flow now, in its own transaction. Returns the outcome and the response
/// (`output` template, or the last result).
pub async fn run_sync(pool: &PgPool, schema: Arc<Schema>, flow: &Flow, source: &str, trigger: Value, caller: &Accountability) -> Result<(Outcome, Value)> {
    let started = Utc::now();
    let mut tx = pool.begin().await?;
    let acc = flow_accountability(flow, caller);
    let outcome = execute_flow(&mut tx, schema, flow, trigger.clone(), acc, 0).await;
    if outcome.ok {
        tx.commit().await?;
    } else {
        tx.rollback().await?;
    }
    record_run(pool, flow, source, &trigger, &outcome, started).await;
    let response = match &flow.output {
        Some(template) => render(template, &outcome.data),
        None => outcome.data.get("$last").cloned().unwrap_or(Value::Null),
    };
    Ok((outcome, response))
}

/// Queue a flow for the background worker (webhooks with `run_async`).
pub async fn enqueue_flow(pool: &PgPool, flow: &Flow, trigger: &Value) -> Result<()> {
    let mut conn = pool.acquire().await?;
    enqueue(&mut conn, flow.id, trigger, None).await?;
    Ok(())
}

pub async fn list_runs(pool: &PgPool, flow_id: Uuid, limit: i64) -> Result<Value> {
    let v: Value = sqlx::query_scalar(
        "SELECT COALESCE(jsonb_agg(r ORDER BY r.started DESC), '[]'::jsonb) FROM \
         (SELECT id, flow_name, source, status, started, finished, trigger, log, error FROM ferro_flow_runs WHERE flow_id = $1 ORDER BY started DESC LIMIT $2) r",
    )
    .bind(flow_id)
    .bind(limit.clamp(1, 200))
    .fetch_one(pool)
    .await?;
    Ok(v)
}

// ---------------------------------------------------------------------------
// background worker
// ---------------------------------------------------------------------------

async fn claim(pool: &PgPool) -> Result<Option<(Uuid, Uuid, Value)>> {
    let row: Option<(Uuid, Uuid, Value)> = sqlx::query_as(
        "UPDATE ferro_flow_queue SET status = 'running', started = now(), attempts = attempts + 1 \
         WHERE id = (SELECT id FROM ferro_flow_queue WHERE status = 'pending' AND run_at <= now() \
                     ORDER BY created FOR UPDATE SKIP LOCKED LIMIT 1) \
         RETURNING id, flow_id, trigger",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

async fn process(pool: &PgPool, schema: Arc<Schema>, job: (Uuid, Uuid, Value)) {
    let (job_id, flow_id, trigger) = job;
    let source = trigger.get("event").and_then(|e| e.as_str()).map(|e| if e == "schedule" { "schedule" } else { "event" }).unwrap_or("event");
    if let Some(flow) = schema.snapshot.flow(flow_id).filter(|f| f.active) {
        let started = Utc::now();
        let outcome = async {
            let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
            let out = execute_flow(&mut tx, schema.clone(), flow, trigger.clone(), Accountability::system(), 0).await;
            if out.ok {
                tx.commit().await.map_err(|e| e.to_string())?;
            } else {
                tx.rollback().await.map_err(|e| e.to_string())?;
            }
            Ok::<Outcome, String>(out)
        }
        .await;
        let outcome = outcome.unwrap_or_else(|e| Outcome { ok: false, error: Some(e), log: Vec::new(), data: Value::Null });
        if !outcome.ok {
            tracing::warn!(flow = %flow.name, error = ?outcome.error, "flow failed");
        }
        record_run(pool, flow, source, &trigger, &outcome, started).await;
    }
    // scheduled jobs stay (as 'done') for a while so that a slower instance cannot enqueue the same minute again
    let done = sqlx::query("UPDATE ferro_flow_queue SET status = 'done' WHERE id = $1 AND dedupe_key IS NOT NULL").bind(job_id).execute(pool).await;
    let removed = sqlx::query("DELETE FROM ferro_flow_queue WHERE id = $1 AND dedupe_key IS NULL").bind(job_id).execute(pool).await;
    if let Err(e) = done.and(removed) {
        tracing::warn!(error = %e, "cannot remove a finished flow job");
    }
}

async fn enqueue_scheduled(pool: &PgPool, schema: &Schema, now: DateTime<Utc>) -> Result<()> {
    let minute = now.timestamp() / 60;
    let mut conn = pool.acquire().await?;
    for flow in schema.snapshot.flows.iter().filter(|f| f.active) {
        let Trigger::Schedule { cron } = &flow.trigger else { continue };
        let Ok(cron) = Cron::parse(cron) else { continue };
        if cron.matches(&now) {
            let trigger = json!({ "event": "schedule", "timestamp": now.to_rfc3339() });
            enqueue(&mut conn, flow.id, &trigger, Some(&format!("{}:{minute}", flow.id))).await?;
        }
    }
    Ok(())
}

async fn housekeeping(pool: &PgPool) -> Result<()> {
    // jobs left 'running' by a crashed instance
    sqlx::query("UPDATE ferro_flow_queue SET status = 'pending' WHERE status = 'running' AND started < now() - interval '10 minutes' AND attempts < 3")
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM ferro_flow_queue WHERE status = 'running' AND started < now() - interval '10 minutes' AND attempts >= 3")
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM ferro_flow_queue WHERE status = 'done' AND started < now() - interval '1 hour'").execute(pool).await?;
    sqlx::query("DELETE FROM ferro_flow_runs WHERE started < now() - interval '30 days'").execute(pool).await?;
    Ok(())
}

/// Poll the queue forever. Run it on every instance: jobs are claimed atomically.
/// Scheduled flows missed while all instances were down are skipped.
pub async fn run_worker(pool: PgPool, schema: Arc<dyn Fn() -> Arc<Schema> + Send + Sync>) {
    let mut last_minute: Option<i64> = None;
    let mut last_housekeeping: Option<Instant> = None;
    loop {
        let now = Utc::now();
        let minute = now.timestamp() / 60;
        if last_minute != Some(minute) {
            last_minute = Some(minute);
            if let Err(e) = enqueue_scheduled(&pool, &schema(), now).await {
                tracing::warn!(error = %e, "cannot queue scheduled flows");
            }
        }
        if last_housekeeping.map(|t| t.elapsed() > Duration::from_secs(600)).unwrap_or(true) {
            last_housekeeping = Some(Instant::now());
            if let Err(e) = housekeeping(&pool).await {
                tracing::warn!(error = %e, "flow housekeeping failed");
            }
        }
        loop {
            match claim(&pool).await {
                Ok(Some(job)) => process(&pool, schema(), job).await,
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "cannot claim a flow job");
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_policy() {
        assert!(check_url("https://hooks.example.com/x", false, &[]).is_ok());
        for bad in [
            "http://localhost:8055/",
            "http://127.0.0.1/",
            "http://10.1.2.3/",
            "http://192.168.0.1/",
            "http://169.254.169.254/latest/meta-data",
            "http://[::1]/",
            "http://[fd00::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://db.internal/",
            "http://2130706433/", // 127.0.0.1 as a single number
            "ftp://example.com/",
            "file:///etc/passwd",
        ] {
            assert!(check_url(bad, false, &[]).is_err(), "{bad} must be refused");
        }
        assert!(check_url("http://localhost/", true, &[]).is_ok(), "explicitly allowed");
        let allowed = vec!["example.com".to_string()];
        assert!(check_url("https://hooks.example.com/", false, &allowed).is_ok());
        assert!(check_url("https://evil.com/", false, &allowed).is_err());
        assert!(check_url("https://notexample.com/", false, &allowed).is_err());
    }

    #[test]
    fn id_lists() {
        let a = Uuid::now_v7();
        assert_eq!(uuids(json!(a.to_string())).unwrap(), vec![a]);
        assert_eq!(uuids(json!([a.to_string(), a.to_string()])).unwrap().len(), 2);
        assert!(uuids(json!(["nope"])).is_err());
        assert!(uuids(Value::Null).unwrap().is_empty());
    }

    #[test]
    fn long_results_are_shortened_in_logs() {
        let big = Value::String("x".repeat(10_000));
        let short = shorten(&big);
        assert!(short.as_str().unwrap().chars().count() <= MAX_LOGGED_CHARS + 1);
        assert_eq!(shorten(&json!({"a": 1})), json!({"a": 1}));
    }
}
