//! GraphQL query language: lexer, parser and the pure helpers needed to execute
//! a document (variables, directives, fragment flattening).
//!
//! Supported: queries and mutations, aliases, arguments, variables with default
//! values, named and inline fragments (`... on block_text { body }`), `@skip` /
//! `@include`, comments, block strings. Not supported: subscriptions, schema
//! definitions and introspection (the SDL is served separately, see [`crate::sdl`]).

use std::collections::BTreeMap;

use serde_json::{Map, Number, Value};

/// Maximum nesting of selection sets, values and fragments.
pub const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq)]
pub enum InputValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Enum(String),
    List(Vec<InputValue>),
    Object(Vec<(String, InputValue)>),
    Variable(String),
}

impl InputValue {
    /// Literal or variable -> JSON. A variable that was not provided is `null`.
    pub fn resolve(&self, vars: &Map<String, Value>) -> Result<Value, String> {
        Ok(match self {
            InputValue::Null => Value::Null,
            InputValue::Bool(b) => Value::Bool(*b),
            InputValue::Int(i) => Value::from(*i),
            InputValue::Float(f) => Number::from_f64(*f).map(Value::Number).ok_or_else(|| "invalid float".to_string())?,
            InputValue::Str(s) | InputValue::Enum(s) => Value::String(s.clone()),
            InputValue::List(items) => Value::Array(items.iter().map(|v| v.resolve(vars)).collect::<Result<Vec<_>, _>>()?),
            InputValue::Object(fields) => {
                let mut m = Map::new();
                for (k, v) in fields {
                    m.insert(k.clone(), v.resolve(vars)?);
                }
                Value::Object(m)
            }
            InputValue::Variable(name) => vars.get(name).cloned().unwrap_or(Value::Null),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Directive {
    pub name: String,
    pub args: Vec<(String, InputValue)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldNode {
    pub alias: Option<String>,
    pub name: String,
    pub args: Vec<(String, InputValue)>,
    pub directives: Vec<Directive>,
    pub selection: Vec<Selection>,
}

impl FieldNode {
    /// Key under which the result is returned.
    pub fn response_key(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }

    pub fn arg(&self, name: &str, vars: &Map<String, Value>) -> Result<Option<Value>, String> {
        match self.args.iter().find(|(k, _)| k.as_str() == name) {
            None => Ok(None),
            Some((_, v)) => match v.resolve(vars)? {
                Value::Null => Ok(None),
                other => Ok(Some(other)),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Selection {
    Field(FieldNode),
    Spread { name: String, directives: Vec<Directive> },
    Inline { type_cond: Option<String>, directives: Vec<Directive>, selection: Vec<Selection> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Query,
    Mutation,
    Subscription,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VarDef {
    pub name: String,
    pub default: Option<InputValue>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Operation {
    pub kind: OpKind,
    pub name: Option<String>,
    pub variables: Vec<VarDef>,
    pub selection: Vec<Selection>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fragment {
    pub name: String,
    pub type_cond: String,
    pub selection: Vec<Selection>,
}

#[derive(Debug, Default, Clone)]
pub struct Document {
    pub operations: Vec<Operation>,
    pub fragments: BTreeMap<String, Fragment>,
}

impl Document {
    pub fn operation(&self, name: Option<&str>) -> Result<&Operation, String> {
        match name {
            Some(n) => self
                .operations
                .iter()
                .find(|o| o.name.as_deref() == Some(n))
                .ok_or_else(|| format!("unknown operation \"{n}\"")),
            None if self.operations.len() == 1 => Ok(&self.operations[0]),
            None => Err("operationName is required when the document has several operations".to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// lexer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Punct(char),
    Spread,
    Name(String),
    Int(i64),
    Float(f64),
    Str(String),
    Eof,
}

impl Tok {
    fn describe(&self) -> String {
        match self {
            Tok::Punct(c) => format!("'{c}'"),
            Tok::Spread => "'...'".into(),
            Tok::Name(n) => format!("`{n}`"),
            Tok::Int(i) => i.to_string(),
            Tok::Float(f) => f.to_string(),
            Tok::Str(_) => "a string".into(),
            Tok::Eof => "end of query".into(),
        }
    }
}

fn dedent(raw: &str) -> String {
    let lines: Vec<&str> = raw.lines().collect();
    let indent = lines
        .iter()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let mut out: Vec<String> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| if i == 0 { l.to_string() } else { l.chars().skip(indent).collect() })
        .collect();
    while out.first().map(|l| l.trim().is_empty()).unwrap_or(false) {
        out.remove(0);
    }
    while out.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        out.pop();
    }
    out.join("\n")
}

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let c: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < c.len() {
        let ch = c[i];
        match ch {
            ' ' | '\t' | '\n' | '\r' | ',' | '\u{feff}' => i += 1,
            '#' => {
                while i < c.len() && c[i] != '\n' && c[i] != '\r' {
                    i += 1;
                }
            }
            '!' | '$' | '&' | '(' | ')' | '[' | ']' | '{' | '}' | ':' | '=' | '@' | '|' => {
                out.push(Tok::Punct(ch));
                i += 1;
            }
            '.' => {
                if c.get(i..i + 3) == Some(&['.', '.', '.'][..]) {
                    out.push(Tok::Spread);
                    i += 3;
                } else {
                    return Err("unexpected '.'".to_string());
                }
            }
            '"' => {
                let triple = c.get(i + 1).copied() == Some('"') && c.get(i + 2).copied() == Some('"');
                if triple {
                    i += 3;
                    let mut raw = String::new();
                    loop {
                        if i >= c.len() {
                            return Err("unterminated block string".to_string());
                        }
                        let closing = c[i] == '"' && c.get(i + 1).copied() == Some('"') && c.get(i + 2).copied() == Some('"');
                        if closing {
                            i += 3;
                            break;
                        }
                        let escaped = c[i] == '\\'
                            && c.get(i + 1).copied() == Some('"')
                            && c.get(i + 2).copied() == Some('"')
                            && c.get(i + 3).copied() == Some('"');
                        if escaped {
                            raw.push_str("\"\"\"");
                            i += 4;
                            continue;
                        }
                        raw.push(c[i]);
                        i += 1;
                    }
                    out.push(Tok::Str(dedent(&raw)));
                } else {
                    i += 1;
                    let mut s = String::new();
                    loop {
                        let Some(&d) = c.get(i) else { return Err("unterminated string".to_string()) };
                        i += 1;
                        match d {
                            '"' => break,
                            '\n' | '\r' => return Err("unterminated string".to_string()),
                            '\\' => {
                                let Some(&e) = c.get(i) else { return Err("unterminated string".to_string()) };
                                i += 1;
                                match e {
                                    '"' => s.push('"'),
                                    '\\' => s.push('\\'),
                                    '/' => s.push('/'),
                                    'b' => s.push('\u{8}'),
                                    'f' => s.push('\u{c}'),
                                    'n' => s.push('\n'),
                                    'r' => s.push('\r'),
                                    't' => s.push('\t'),
                                    'u' => {
                                        let hex: String = c.get(i..i + 4).ok_or("bad unicode escape")?.iter().collect();
                                        let code = u32::from_str_radix(&hex, 16).map_err(|_| "bad unicode escape".to_string())?;
                                        s.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                                        i += 4;
                                    }
                                    other => return Err(format!("invalid escape \\{other}")),
                                }
                            }
                            other => s.push(other),
                        }
                    }
                    out.push(Tok::Str(s));
                }
            }
            '-' | '0'..='9' => {
                let start = i;
                if c[i] == '-' {
                    i += 1;
                }
                while i < c.len() && c[i].is_ascii_digit() {
                    i += 1;
                }
                let mut float = false;
                if c.get(i).copied() == Some('.') && c.get(i + 1).map(|d| d.is_ascii_digit()).unwrap_or(false) {
                    float = true;
                    i += 1;
                    while i < c.len() && c[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                if matches!(c.get(i).copied(), Some('e') | Some('E')) {
                    float = true;
                    i += 1;
                    if matches!(c.get(i).copied(), Some('+') | Some('-')) {
                        i += 1;
                    }
                    while i < c.len() && c[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                let text: String = c[start..i].iter().collect();
                if float {
                    out.push(Tok::Float(text.parse().map_err(|_| format!("invalid number {text}"))?));
                } else {
                    out.push(Tok::Int(text.parse().map_err(|_| format!("invalid number {text}"))?));
                }
            }
            _ if ch.is_ascii_alphabetic() || ch == '_' => {
                let start = i;
                while i < c.len() && (c[i].is_ascii_alphanumeric() || c[i] == '_') {
                    i += 1;
                }
                out.push(Tok::Name(c[start..i].iter().collect()));
            }
            _ => return Err(format!("unexpected character '{ch}'")),
        }
    }
    out.push(Tok::Eof);
    Ok(out)
}

// ---------------------------------------------------------------------------
// parser
// ---------------------------------------------------------------------------

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    depth: usize,
    vdepth: usize,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos]
    }

    fn next(&mut self) -> Tok {
        let t = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }

    fn eat_punct(&mut self, ch: char) -> bool {
        if *self.peek() == Tok::Punct(ch) {
            self.next();
            true
        } else {
            false
        }
    }

    fn expect_punct(&mut self, ch: char) -> Result<(), String> {
        if self.eat_punct(ch) {
            Ok(())
        } else {
            Err(format!("expected '{ch}', found {}", self.peek().describe()))
        }
    }

    fn name(&mut self) -> Result<String, String> {
        match self.next() {
            Tok::Name(n) => Ok(n),
            t => Err(format!("expected a name, found {}", t.describe())),
        }
    }

    fn is_eof(&self) -> bool {
        *self.peek() == Tok::Eof
    }

    fn selection_set(&mut self) -> Result<Vec<Selection>, String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("query is nested too deeply".to_string());
        }
        self.expect_punct('{')?;
        let mut out = Vec::new();
        while !self.eat_punct('}') {
            if self.is_eof() {
                return Err("unexpected end of query, expected '}'".to_string());
            }
            out.push(self.selection()?);
        }
        if out.is_empty() {
            return Err("empty selection set".to_string());
        }
        self.depth -= 1;
        Ok(out)
    }

    fn selection(&mut self) -> Result<Selection, String> {
        if *self.peek() == Tok::Spread {
            self.next();
            if let Tok::Name(n) = self.peek().clone() {
                if n != "on" {
                    self.next();
                    let directives = self.directives()?;
                    return Ok(Selection::Spread { name: n, directives });
                }
            }
            let type_cond = if matches!(self.peek(), Tok::Name(n) if n.as_str() == "on") {
                self.next();
                Some(self.name()?)
            } else {
                None
            };
            let directives = self.directives()?;
            let selection = self.selection_set()?;
            return Ok(Selection::Inline { type_cond, directives, selection });
        }
        let first = self.name()?;
        let (alias, name) = if self.eat_punct(':') { (Some(first), self.name()?) } else { (None, first) };
        let args = if *self.peek() == Tok::Punct('(') { self.arguments()? } else { Vec::new() };
        let directives = self.directives()?;
        let selection = if *self.peek() == Tok::Punct('{') { self.selection_set()? } else { Vec::new() };
        Ok(Selection::Field(FieldNode { alias, name, args, directives, selection }))
    }

    fn arguments(&mut self) -> Result<Vec<(String, InputValue)>, String> {
        self.expect_punct('(')?;
        let mut args = Vec::new();
        while !self.eat_punct(')') {
            if self.is_eof() {
                return Err("unexpected end of query, expected ')'".to_string());
            }
            let k = self.name()?;
            self.expect_punct(':')?;
            args.push((k, self.value()?));
        }
        Ok(args)
    }

    fn directives(&mut self) -> Result<Vec<Directive>, String> {
        let mut out = Vec::new();
        while self.eat_punct('@') {
            let name = self.name()?;
            let args = if *self.peek() == Tok::Punct('(') { self.arguments()? } else { Vec::new() };
            out.push(Directive { name, args });
        }
        Ok(out)
    }

    fn value(&mut self) -> Result<InputValue, String> {
        self.vdepth += 1;
        if self.vdepth > MAX_DEPTH {
            return Err("value is nested too deeply".to_string());
        }
        let v = match self.next() {
            Tok::Punct('$') => InputValue::Variable(self.name()?),
            Tok::Int(i) => InputValue::Int(i),
            Tok::Float(f) => InputValue::Float(f),
            Tok::Str(s) => InputValue::Str(s),
            Tok::Name(n) => match n.as_str() {
                "true" => InputValue::Bool(true),
                "false" => InputValue::Bool(false),
                "null" => InputValue::Null,
                _ => InputValue::Enum(n),
            },
            Tok::Punct('[') => {
                let mut items = Vec::new();
                while !self.eat_punct(']') {
                    if self.is_eof() {
                        return Err("unexpected end of query, expected ']'".to_string());
                    }
                    items.push(self.value()?);
                }
                InputValue::List(items)
            }
            Tok::Punct('{') => {
                let mut fields = Vec::new();
                while !self.eat_punct('}') {
                    if self.is_eof() {
                        return Err("unexpected end of query, expected '}'".to_string());
                    }
                    let k = self.name()?;
                    self.expect_punct(':')?;
                    fields.push((k, self.value()?));
                }
                InputValue::Object(fields)
            }
            t => return Err(format!("unexpected {} in a value", t.describe())),
        };
        self.vdepth -= 1;
        Ok(v)
    }

    /// Variable type: parsed and ignored (`[Int!]!`).
    fn skip_type(&mut self) -> Result<(), String> {
        if self.eat_punct('[') {
            self.skip_type()?;
            self.expect_punct(']')?;
        } else {
            self.name()?;
        }
        self.eat_punct('!');
        Ok(())
    }

    fn operation(&mut self, kind: OpKind) -> Result<Operation, String> {
        let name = if matches!(self.peek(), Tok::Name(_)) { Some(self.name()?) } else { None };
        let mut variables = Vec::new();
        if self.eat_punct('(') {
            while !self.eat_punct(')') {
                if self.is_eof() {
                    return Err("unexpected end of query, expected ')'".to_string());
                }
                self.expect_punct('$')?;
                let vname = self.name()?;
                self.expect_punct(':')?;
                self.skip_type()?;
                let default = if self.eat_punct('=') { Some(self.value()?) } else { None };
                self.directives()?;
                variables.push(VarDef { name: vname, default });
            }
        }
        self.directives()?;
        let selection = self.selection_set()?;
        Ok(Operation { kind, name, variables, selection })
    }
}

pub fn parse_document(src: &str) -> Result<Document, String> {
    let toks = lex(src)?;
    let mut p = Parser { toks, pos: 0, depth: 0, vdepth: 0 };
    let mut doc = Document::default();
    while !p.is_eof() {
        match p.peek().clone() {
            Tok::Punct('{') => {
                let selection = p.selection_set()?;
                doc.operations.push(Operation { kind: OpKind::Query, name: None, variables: Vec::new(), selection });
            }
            Tok::Name(n) if n == "query" || n == "mutation" || n == "subscription" => {
                p.next();
                let kind = match n.as_str() {
                    "query" => OpKind::Query,
                    "mutation" => OpKind::Mutation,
                    _ => OpKind::Subscription,
                };
                doc.operations.push(p.operation(kind)?);
            }
            Tok::Name(n) if n == "fragment" => {
                p.next();
                let name = p.name()?;
                if p.name()? != "on" {
                    return Err("expected `on` after the fragment name".to_string());
                }
                let type_cond = p.name()?;
                p.directives()?;
                let selection = p.selection_set()?;
                doc.fragments.insert(name.clone(), Fragment { name, type_cond, selection });
            }
            other => return Err(format!("unexpected {}", other.describe())),
        }
    }
    if doc.operations.is_empty() {
        return Err("the document contains no operation".to_string());
    }
    Ok(doc)
}

// ---------------------------------------------------------------------------
// execution helpers
// ---------------------------------------------------------------------------

fn directive_if(d: &Directive, vars: &Map<String, Value>) -> Result<bool, String> {
    let arg = d
        .args
        .iter()
        .find(|(k, _)| k.as_str() == "if")
        .ok_or_else(|| format!("@{} needs an \"if\" argument", d.name))?;
    match arg.1.resolve(vars)? {
        Value::Bool(b) => Ok(b),
        _ => Err(format!("the \"if\" argument of @{} must be a boolean", d.name)),
    }
}

/// `@skip(if: …)` / `@include(if: …)`.
pub fn is_included(directives: &[Directive], vars: &Map<String, Value>) -> Result<bool, String> {
    for d in directives {
        match d.name.as_str() {
            "skip" => {
                if directive_if(d, vars)? {
                    return Ok(false);
                }
            }
            "include" => {
                if !directive_if(d, vars)? {
                    return Ok(false);
                }
            }
            _ => {}
        }
    }
    Ok(true)
}

fn type_ok(actual: Option<&str>, cond: Option<&str>) -> bool {
    match (actual, cond) {
        (Some(a), Some(c)) => a == c,
        _ => true,
    }
}

/// Flatten fragments into the list of fields that apply to `type_name`
/// (`None` = no type filtering: the union of every branch).
pub fn collect_fields<'a>(
    sel: &'a [Selection],
    doc: &'a Document,
    vars: &Map<String, Value>,
    type_name: Option<&str>,
    out: &mut Vec<&'a FieldNode>,
    depth: usize,
) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err("fragments are nested too deeply (cycle?)".to_string());
    }
    for s in sel {
        match s {
            Selection::Field(f) => {
                if is_included(&f.directives, vars)? {
                    out.push(f);
                }
            }
            Selection::Spread { name, directives } => {
                if !is_included(directives, vars)? {
                    continue;
                }
                let frag = doc.fragments.get(name).ok_or_else(|| format!("unknown fragment \"{name}\""))?;
                if type_ok(type_name, Some(frag.type_cond.as_str())) {
                    collect_fields(&frag.selection, doc, vars, type_name, out, depth + 1)?;
                }
            }
            Selection::Inline { type_cond, directives, selection } => {
                if !is_included(directives, vars)? {
                    continue;
                }
                if type_ok(type_name, type_cond.as_deref()) {
                    collect_fields(selection, doc, vars, type_name, out, depth + 1)?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vars(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn parses_operations_arguments_and_variables() {
        let doc = parse_document(
            r#"
            # list published pages
            query Pages($status: String = "published", $n: Int) {
              items: pages(filter: { status: { _eq: $status } }, sort: ["-date_created"], limit: $n, page: 2) {
                id
                title
                category { code }
              }
            }
            "#,
        )
        .unwrap();
        let op = doc.operation(None).unwrap();
        assert_eq!(op.kind, OpKind::Query);
        assert_eq!(op.name.as_deref(), Some("Pages"));
        assert_eq!(op.variables.len(), 2);
        assert_eq!(op.variables[0].default, Some(InputValue::Str("published".into())));
        let Selection::Field(f) = &op.selection[0] else { panic!("field expected") };
        assert_eq!(f.response_key(), "items");
        assert_eq!(f.name, "pages");
        assert_eq!(f.selection.len(), 3);

        let provided = vars(json!({ "status": "draft" }));
        assert_eq!(f.arg("filter", &provided).unwrap(), Some(json!({ "status": { "_eq": "draft" } })));
        assert_eq!(f.arg("sort", &provided).unwrap(), Some(json!(["-date_created"])));
        assert_eq!(f.arg("page", &provided).unwrap(), Some(json!(2)));
        assert_eq!(f.arg("limit", &provided).unwrap(), None, "missing variable = absent argument");
    }

    #[test]
    fn shorthand_query_and_mutation() {
        let doc = parse_document("{ pages { id } }").unwrap();
        assert_eq!(doc.operations[0].kind, OpKind::Query);
        let doc = parse_document(r#"mutation { create_pages_item(data: { title: "x", views: 3, ok: true, none: null, ratio: 1.5e1 }) { id } }"#).unwrap();
        let Selection::Field(f) = &doc.operations[0].selection[0] else { panic!() };
        assert_eq!(
            f.arg("data", &Map::new()).unwrap(),
            Some(json!({ "title": "x", "views": 3, "ok": true, "none": null, "ratio": 15.0 }))
        );
    }

    #[test]
    fn fragments_and_type_conditions() {
        let doc = parse_document(
            r#"
            query {
              pages {
                blocks {
                  collection
                  item {
                    __typename
                    ... on block_text { body }
                    ... on block_cards { title ...cardFields }
                  }
                }
              }
            }
            fragment cardFields on block_cards { cards { title } }
            "#,
        )
        .unwrap();
        let op = doc.operation(None).unwrap();
        let Selection::Field(pages) = &op.selection[0] else { panic!() };
        let Selection::Field(blocks) = &pages.selection[0] else { panic!() };
        let Selection::Field(item) = &blocks.selection[1] else { panic!() };
        let names = |t: Option<&str>| {
            let mut out = Vec::new();
            collect_fields(&item.selection, &doc, &Map::new(), t, &mut out, 0).unwrap();
            out.iter().map(|f| f.name.clone()).collect::<Vec<_>>()
        };
        assert_eq!(names(Some("block_text")), vec!["__typename", "body"]);
        assert_eq!(names(Some("block_cards")), vec!["__typename", "title", "cards"]);
        assert_eq!(names(None), vec!["__typename", "body", "title", "cards"]);
    }

    #[test]
    fn skip_and_include() {
        let doc = parse_document("query($yes: Boolean!) { a @include(if: $yes) b @skip(if: $yes) c }").unwrap();
        let op = doc.operation(None).unwrap();
        let run = |yes: bool| {
            let mut out = Vec::new();
            collect_fields(&op.selection, &doc, &vars(json!({ "yes": yes })), None, &mut out, 0).unwrap();
            out.iter().map(|f| f.name.clone()).collect::<Vec<_>>()
        };
        assert_eq!(run(true), vec!["a", "c"]);
        assert_eq!(run(false), vec!["b", "c"]);
    }

    #[test]
    fn strings() {
        let doc = parse_document(r#"{ f(a: "line\n\"q\" è", b: """
            block
              indented
            """) }"#)
        .unwrap();
        let Selection::Field(f) = &doc.operations[0].selection[0] else { panic!() };
        assert_eq!(f.arg("a", &Map::new()).unwrap(), Some(json!("line\n\"q\" è")));
        assert_eq!(f.arg("b", &Map::new()).unwrap(), Some(json!("block\n  indented")));
    }

    #[test]
    fn rejects_bad_documents() {
        assert!(parse_document("").is_err());
        assert!(parse_document("{ }").is_err());
        assert!(parse_document("{ a ").is_err());
        assert!(parse_document("{ a(x: ) }").is_err());
        assert!(parse_document("query { a } query { b }").unwrap().operation(None).is_err());
        let deep = format!("{}a{}", "{ a ".repeat(40), "}".repeat(40));
        assert!(parse_document(&deep).unwrap_err().contains("deeply"));
        // unknown fragment is reported when executing
        let doc = parse_document("{ a { ...nope } }").unwrap();
        let Selection::Field(a) = &doc.operations[0].selection[0] else { panic!() };
        assert!(collect_fields(&a.selection, &doc, &Map::new(), None, &mut Vec::new(), 0).is_err());
    }

    #[test]
    fn fragment_cycles_are_cut() {
        let doc = parse_document("{ a { ...x } } fragment x on T { ...x }").unwrap();
        let Selection::Field(a) = &doc.operations[0].selection[0] else { panic!() };
        assert!(collect_fields(&a.selection, &doc, &Map::new(), None, &mut Vec::new(), 0).is_err());
    }
}
