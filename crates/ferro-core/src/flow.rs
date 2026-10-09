//! Flows: declarative automations that are part of the snapshot.
//!
//! A flow has a **trigger** and a chain of **operations**. Operations read and
//! write a *data chain* (a JSON object): `$trigger` (what started the flow),
//! `$last` (result of the previous operation), `$env`, `$accountability`, and one
//! entry per executed operation key. Any string option may contain `{{ path }}`
//! references to the data chain, e.g. `{{ $trigger.payload.title }}` or
//! `{{ fetch_user.data.email }}`; a string that is exactly one reference keeps the
//! referenced JSON type.
//!
//! The same concept covers what Directus splits into webhooks, flows and hooks:
//!
//! | You want | Flow |
//! |---|---|
//! | outbound webhook | `event` trigger + one `request` operation |
//! | hook that validates / rewrites a write | `event` trigger with `blocking: true`, `output` template |
//! | scheduled job | `schedule` trigger |
//! | custom endpoint | `webhook` trigger, `output` template as response |
//! | button in the admin | `manual` trigger |

use std::collections::{BTreeMap, BTreeSet};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::cron::Cron;
use crate::model::{is_default, is_false, is_true, is_valid_identifier, yes, Snapshot};

pub const MAX_STEPS: usize = 100;
pub const MAX_FLOW_DEPTH: u8 = 5;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Flow {
    pub id: Uuid,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub active: bool,
    pub trigger: Trigger,
    /// Whose permissions item operations use. `system` = unrestricted.
    #[serde(default, skip_serializing_if = "is_default")]
    pub run_as: RunAs,
    /// Key of the first operation (default: the first one in the list).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    #[serde(default)]
    pub operations: Vec<Operation>,
    /// Template rendered against the data chain at the end of the flow: the HTTP
    /// response of a `webhook` flow, the new payload of a `blocking` event flow.
    /// Default: `$last` for webhooks, payload unchanged for blocking flows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunAs {
    #[default]
    System,
    /// The user who caused the run (blocking events, webhooks, manual runs only).
    Caller,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookAccess {
    Public,
    #[default]
    Authenticated,
    Admin,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Trigger {
    /// Item events: `pages.items.create`, `*.items.update`, `pages.items.*`, `*`.
    Event {
        events: Vec<String>,
        /// Runs inside the write, before it: may reject it or rewrite the payload.
        #[serde(default, skip_serializing_if = "is_false")]
        blocking: bool,
    },
    /// Five-field cron expression, UTC.
    Schedule { cron: String },
    /// Inbound HTTP: `/hooks/<flow id>` and, with a slug, `/x/<slug>`.
    Webhook {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slug: Option<String>,
        /// Allowed HTTP methods (default: POST).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        methods: Vec<String>,
        #[serde(default, skip_serializing_if = "is_default")]
        access: WebhookAccess,
        /// Name of the environment variable holding a shared secret, expected in
        /// the `X-Ferro-Secret` header. The secret itself never enters the snapshot.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secret_env: Option<String>,
        /// Answer 202 immediately and run in the background.
        #[serde(default, skip_serializing_if = "is_false")]
        run_async: bool,
    },
    /// Started by a user (button in the admin, `POST /flows/<id>/run`).
    Manual {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        collections: Vec<String>,
        /// Roles allowed to run it (empty = every user with app access).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        roles: Vec<Uuid>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpType {
    Log,
    Request,
    ItemCreate,
    ItemUpdate,
    ItemDelete,
    ItemRead,
    Condition,
    Transform,
    TriggerFlow,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Operation {
    pub key: String,
    #[serde(rename = "type")]
    pub ty: OpType,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub options: Value,
    /// Next operation on success (for `condition`: when the rule matches).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolve: Option<String>,
    /// Next operation on failure (for `condition`: when the rule does not match).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reject: Option<String>,
}

// ---- typed options ------------------------------------------------------------

fn get() -> String {
    "GET".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct LogOpts {
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RequestOpts {
    #[serde(default = "get")]
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: Map<String, Value>,
    #[serde(default)]
    pub body: Option<Value>,
    /// Treat non-2xx answers as a failure of the operation.
    #[serde(default)]
    pub fail_on_status: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ItemCreateOpts {
    pub collection: String,
    pub payload: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ItemUpdateOpts {
    pub collection: String,
    /// One id or a list of ids.
    pub ids: Value,
    pub payload: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ItemDeleteOpts {
    pub collection: String,
    pub ids: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ItemReadOpts {
    pub collection: String,
    #[serde(default)]
    pub filter: Option<Value>,
    #[serde(default)]
    pub sort: Option<Value>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConditionOpts {
    /// Filter evaluated against the data chain, e.g.
    /// `{ "$trigger": { "payload": { "status": { "_eq": "published" } } } }`.
    pub rule: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TransformOpts {
    pub value: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TriggerFlowOpts {
    pub flow: Uuid,
    #[serde(default)]
    pub payload: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FailOpts {
    #[serde(default)]
    pub message: Option<String>,
}

pub fn parse_options<T: DeserializeOwned>(op: &Operation) -> Result<T, String> {
    let v = if op.options.is_null() { Value::Object(Map::new()) } else { op.options.clone() };
    serde_json::from_value(v).map_err(|e| format!("operation `{}` ({:?}): invalid options: {e}", op.key, op.ty))
}

// ---- templating -----------------------------------------------------------------

/// Value at a dotted path (`$trigger.payload.title`, `list.0.id`).
pub fn lookup<'a>(data: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = data;
    for seg in path.trim().split('.') {
        if seg.is_empty() {
            return None;
        }
        cur = match cur {
            Value::Object(m) => m.get(seg)?,
            Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn render_string(s: &str, data: &Value) -> Value {
    let t = s.trim();
    if let Some(inner) = t.strip_prefix("{{").and_then(|r| r.strip_suffix("}}")) {
        if !inner.contains("{{") && !inner.contains("}}") {
            return lookup(data, inner).cloned().unwrap_or(Value::Null);
        }
    }
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        match rest[start + 2..].find("}}") {
            Some(end) => {
                let expr = &rest[start + 2..start + 2 + end];
                match lookup(data, expr) {
                    Some(Value::String(x)) => out.push_str(x),
                    Some(Value::Null) | None => {}
                    Some(other) => out.push_str(&other.to_string()),
                }
                rest = &rest[start + 2 + end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    Value::String(out)
}

/// Replace `{{ … }}` references in every string of `v` (object keys are kept).
pub fn render(v: &Value, data: &Value) -> Value {
    match v {
        Value::String(s) => render_string(s, data),
        Value::Array(a) => Value::Array(a.iter().map(|x| render(x, data)).collect()),
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), render(x, data))).collect()),
        other => other.clone(),
    }
}

/// Render a string option to plain text.
pub fn render_text(s: &str, data: &Value) -> String {
    match render_string(s, data) {
        Value::String(x) => x,
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

// ---- events -----------------------------------------------------------------------

pub const ACTIONS: [&str; 3] = ["create", "update", "delete"];

pub fn event_name(collection: &str, action: &str) -> String {
    format!("{collection}.items.{action}")
}

pub fn event_matches(pattern: &str, event: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let p: Vec<&str> = pattern.split('.').collect();
    let e: Vec<&str> = event.split('.').collect();
    p.len() == e.len() && p.iter().zip(e.iter()).all(|(a, b)| *a == "*" || a == b)
}

/// Active event flows whose patterns match `event`.
pub fn matching_event_flows<'a>(flows: &'a [Flow], event: &str, blocking: bool) -> Vec<&'a Flow> {
    flows
        .iter()
        .filter(|f| f.active)
        .filter(|f| match &f.trigger {
            Trigger::Event { events, blocking: b } => *b == blocking && events.iter().any(|p| event_matches(p, event)),
            _ => false,
        })
        .collect()
}

impl Snapshot {
    pub fn flow(&self, id: Uuid) -> Option<&Flow> {
        self.flows.iter().find(|f| f.id == id)
    }
}

// ---- validation -------------------------------------------------------------------

fn valid_slug(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn check_options(op: &Operation) -> Result<(), String> {
    match op.ty {
        OpType::Log => parse_options::<LogOpts>(op).map(|_| ()),
        OpType::Request => parse_options::<RequestOpts>(op).map(|_| ()),
        OpType::ItemCreate => parse_options::<ItemCreateOpts>(op).map(|_| ()),
        OpType::ItemUpdate => parse_options::<ItemUpdateOpts>(op).map(|_| ()),
        OpType::ItemDelete => parse_options::<ItemDeleteOpts>(op).map(|_| ()),
        OpType::ItemRead => parse_options::<ItemReadOpts>(op).map(|_| ()),
        OpType::Condition => parse_options::<ConditionOpts>(op).map(|_| ()),
        OpType::Transform => parse_options::<TransformOpts>(op).map(|_| ()),
        OpType::TriggerFlow => parse_options::<TriggerFlowOpts>(op).map(|_| ()),
        OpType::Fail => parse_options::<FailOpts>(op).map(|_| ()),
    }
}

fn has_loop(flow: &Flow) -> bool {
    let graph: BTreeMap<&str, Vec<&str>> = flow
        .operations
        .iter()
        .map(|o| (o.key.as_str(), o.resolve.iter().chain(o.reject.iter()).map(|s| s.as_str()).collect()))
        .collect();
    // 0 = unvisited, 1 = on the current path, 2 = done
    let mut state: BTreeMap<&str, u8> = BTreeMap::new();
    fn visit<'a>(n: &'a str, graph: &BTreeMap<&'a str, Vec<&'a str>>, state: &mut BTreeMap<&'a str, u8>) -> bool {
        match state.get(n).copied().unwrap_or(0) {
            1 => return true,
            2 => return false,
            _ => {}
        }
        state.insert(n, 1);
        if let Some(next) = graph.get(n) {
            for m in next {
                if graph.contains_key(m) && visit(*m, graph, state) {
                    return true;
                }
            }
        }
        state.insert(n, 2);
        false
    }
    graph.keys().any(|k| visit(*k, &graph, &mut state))
}

/// Structural validation of the flows of a snapshot (empty = valid).
pub fn validate_flows(s: &Snapshot) -> Vec<String> {
    let mut errors = Vec::new();
    let mut ids = BTreeSet::new();
    let mut slugs = BTreeSet::new();
    let known = |name: &str| s.collection(name).is_some();

    for f in &s.flows {
        let ctx = format!("flow `{}` ({})", f.name, f.id);
        if f.name.trim().is_empty() {
            errors.push(format!("{ctx}: name is required"));
        }
        if !ids.insert(f.id) {
            errors.push(format!("{ctx}: duplicated id"));
        }

        match &f.trigger {
            Trigger::Event { events, blocking } => {
                if events.is_empty() {
                    errors.push(format!("{ctx}: an event trigger needs at least one event"));
                }
                for e in events {
                    if e == "*" {
                        continue;
                    }
                    let parts: Vec<&str> = e.split('.').collect();
                    let shape_ok = parts.len() == 3 && parts[1] == "items" && (parts[2] == "*" || ACTIONS.contains(&parts[2]));
                    if !shape_ok {
                        errors.push(format!("{ctx}: event `{e}` must look like <collection>.items.<create|update|delete|*>"));
                    } else if parts[0] != "*" && !known(parts[0]) {
                        errors.push(format!("{ctx}: event `{e}` refers to an unknown collection"));
                    }
                }
                if *blocking && f.run_as == RunAs::System && f.output.is_none() && f.operations.is_empty() {
                    errors.push(format!("{ctx}: a blocking flow without operations does nothing"));
                }
                if !*blocking && f.run_as == RunAs::Caller {
                    errors.push(format!("{ctx}: run_as `caller` needs a blocking event, webhook or manual trigger"));
                }
            }
            Trigger::Schedule { cron } => {
                if let Err(e) = Cron::parse(cron) {
                    errors.push(format!("{ctx}: {e}"));
                }
                if f.run_as == RunAs::Caller {
                    errors.push(format!("{ctx}: run_as `caller` is meaningless for a schedule"));
                }
            }
            Trigger::Webhook { slug, methods, run_async, .. } => {
                if *run_async && f.run_as == RunAs::Caller {
                    errors.push(format!("{ctx}: run_as `caller` cannot be combined with run_async"));
                }
                if let Some(slug) = slug {
                    if !valid_slug(slug) {
                        errors.push(format!("{ctx}: slug `{slug}` must match [a-z0-9][a-z0-9_-]{{0,63}}"));
                    } else if !slugs.insert(slug.clone()) {
                        errors.push(format!("{ctx}: slug `{slug}` is used by another flow"));
                    }
                }
                for m in methods {
                    if !["GET", "POST", "PUT", "PATCH", "DELETE"].contains(&m.as_str()) {
                        errors.push(format!("{ctx}: unsupported method `{m}`"));
                    }
                }
            }
            Trigger::Manual { collections, .. } => {
                for c in collections {
                    if !known(c) {
                        errors.push(format!("{ctx}: unknown collection `{c}`"));
                    }
                }
            }
        }

        let mut keys = BTreeSet::new();
        for op in &f.operations {
            if !is_valid_identifier(&op.key) || op.key.starts_with('$') {
                errors.push(format!("{ctx}: invalid operation key `{}`", op.key));
            }
            if !keys.insert(op.key.as_str()) {
                errors.push(format!("{ctx}: duplicated operation key `{}`", op.key));
            }
            if let Err(e) = check_options(op) {
                errors.push(format!("{ctx}: {e}"));
            }
            if let Some(c) = op.options.get("collection").and_then(|v| v.as_str()) {
                let templated = c.contains("{{");
                let readable_system = op.ty == OpType::ItemRead && c.starts_with("ferro_");
                if !templated && !readable_system && !known(c) {
                    errors.push(format!("{ctx}: operation `{}` refers to unknown collection `{c}`", op.key));
                }
            }
            if op.ty == OpType::TriggerFlow {
                if let Ok(o) = parse_options::<TriggerFlowOpts>(op) {
                    if s.flow(o.flow).is_none() {
                        errors.push(format!("{ctx}: operation `{}` triggers an unknown flow {}", op.key, o.flow));
                    }
                }
            }
        }
        for op in &f.operations {
            for next in op.resolve.iter().chain(op.reject.iter()) {
                if !keys.contains(next.as_str()) {
                    errors.push(format!("{ctx}: operation `{}` points to unknown operation `{next}`", op.key));
                }
            }
        }
        if let Some(start) = &f.start {
            if !keys.contains(start.as_str()) {
                errors.push(format!("{ctx}: start `{start}` is not an operation"));
            }
        }
        if has_loop(f) {
            errors.push(format!("{ctx}: operations form a loop"));
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn data() -> Value {
        json!({
            "$trigger": { "payload": { "title": "Home", "views": 3, "tags": ["a", "b"] }, "key": "p1" },
            "fetch": { "data": { "email": "a@b.c" } },
            "$last": null
        })
    }

    #[test]
    fn templates_keep_types_for_whole_references() {
        let d = data();
        assert_eq!(render(&json!("{{ $trigger.payload.views }}"), &d), json!(3));
        assert_eq!(render(&json!("{{$trigger.payload.tags}}"), &d), json!(["a", "b"]));
        assert_eq!(render(&json!("{{ nope.here }}"), &d), Value::Null);
        assert_eq!(render(&json!("Page {{ $trigger.payload.title }} has {{ $trigger.payload.views }} views"), &d), json!("Page Home has 3 views"));
        assert_eq!(render(&json!("x{{ missing }}y"), &d), json!("xy"));
        assert_eq!(render(&json!("open {{ not closed"), &d), json!("open {{ not closed"));
        assert_eq!(render(&json!("{{ $trigger.payload.tags.1 }}"), &d), json!("b"));
        assert_eq!(
            render(&json!({ "to": "{{ fetch.data.email }}", "n": [1, "{{ $trigger.key }}"], "keep": true }), &d),
            json!({ "to": "a@b.c", "n": [1, "p1"], "keep": true })
        );
        assert_eq!(render_text("{{ $trigger.payload.views }} items", &d), "3 items");
    }

    #[test]
    fn event_patterns() {
        assert!(event_matches("pages.items.create", "pages.items.create"));
        assert!(event_matches("*.items.create", "pages.items.create"));
        assert!(event_matches("pages.items.*", "pages.items.delete"));
        assert!(event_matches("*", "anything.items.update"));
        assert!(!event_matches("pages.items.create", "pages.items.update"));
        assert!(!event_matches("pages.items.create", "posts.items.create"));
        assert_eq!(event_name("pages", "update"), "pages.items.update");
    }

    fn snapshot(flows: &str) -> Snapshot {
        let mut s: Snapshot = serde_yaml::from_str(include_str!("../tests/fixtures/pages.yaml")).unwrap();
        s.flows = serde_yaml::from_str(flows).unwrap();
        s
    }

    const WEBHOOK_FLOW: &str = r#"
- id: 0199a000-0000-7000-8000-0000000a0001
  name: Notify on publish
  trigger: { type: event, events: ["pages.items.update"] }
  operations:
    - key: only_published
      type: condition
      options: { rule: { $trigger: { payload: { status: { _eq: published } } } } }
      resolve: notify
    - key: notify
      type: request
      options:
        method: POST
        url: https://hooks.example.com/publish
        headers: { Authorization: "Bearer {{ $env.HOOK_TOKEN }}" }
        body: { title: "{{ $trigger.payload.title }}", id: "{{ $trigger.key }}" }
- id: 0199a000-0000-7000-8000-0000000a0002
  name: Stats endpoint
  trigger: { type: webhook, slug: stats, methods: [GET], access: public }
  operations:
    - key: pages
      type: item_read
      options: { collection: pages, limit: 5 }
  output: { count: "{{ pages }}" }
"#;

    #[test]
    fn flows_roundtrip_and_validate() {
        let s = snapshot(WEBHOOK_FLOW);
        assert_eq!(validate_flows(&s), Vec::<String>::new());
        let yaml = serde_yaml::to_string(&s.flows).unwrap();
        let back: Vec<Flow> = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back, s.flows);
        assert!(matches!(&s.flows[0].trigger, Trigger::Event { blocking: false, .. }));
        assert_eq!(s.flows[1].operations[0].ty, OpType::ItemRead);

        let found = matching_event_flows(&s.flows, "pages.items.update", false);
        assert_eq!(found.len(), 1);
        assert!(matching_event_flows(&s.flows, "pages.items.update", true).is_empty());
        assert!(matching_event_flows(&s.flows, "pages.items.create", false).is_empty());
    }

    #[test]
    fn detects_broken_flows() {
        let s = snapshot(
            r#"
- id: 0199a000-0000-7000-8000-0000000b0001
  name: Bad
  trigger: { type: event, events: ["nope.items.create", "pages.items.explode"] }
  run_as: caller
  start: ghost
  operations:
    - { key: a, type: transform, options: { value: 1 }, resolve: b }
    - { key: b, type: transform, options: { value: 2 }, resolve: a, reject: missing }
    - { key: c, type: item_create, options: { collection: ghosts, payload: {} } }
    - { key: c, type: request, options: {} }
- id: 0199a000-0000-7000-8000-0000000b0002
  name: Bad schedule
  trigger: { type: schedule, cron: "61 * * * *" }
- id: 0199a000-0000-7000-8000-0000000b0003
  name: Bad slug
  trigger: { type: webhook, slug: "Not Valid", methods: [TRACE] }
"#,
        );
        let errors = validate_flows(&s).join("\n");
        for needle in [
            "unknown collection",
            "must look like",
            "run_as `caller`",
            "start `ghost`",
            "operations form a loop",
            "unknown operation `missing`",
            "duplicated operation key `c`",
            "refers to unknown collection `ghosts`",
            "invalid options",
            "must be within 0-59",
            "slug `Not Valid`",
            "unsupported method `TRACE`",
        ] {
            assert!(errors.contains(needle), "missing `{needle}` in:\n{errors}");
        }
    }
}
