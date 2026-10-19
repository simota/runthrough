//! GitHub Actions expression language (`${{ ... }}`).
//!
//! Implements the documented semantics: loose equality with numeric coercion,
//! case-insensitive string comparison and property lookup, `&&`/`||` returning
//! operands, object filters (`.*`), and the built-in functions.

use anyhow::{anyhow, bail, Result};
use serde_json::{Map, Value};
use std::path::PathBuf;

/// Everything an expression can observe.
#[derive(Default, Clone)]
pub struct Ctx {
    pub contexts: Map<String, Value>,
    /// Whether a previous step/job in scope has failed.
    pub failed: bool,
    /// Whether an ancestor job ended without succeeding (skipped or
    /// cancelled): `success()` is false, though `failure()` need not be true.
    pub ancestor_unsucceeded: bool,
    pub cancelled: bool,
    /// Workspace root for `hashFiles`.
    pub workspace: Option<PathBuf>,
}

impl Ctx {
    pub fn set(&mut self, name: &str, v: Value) {
        self.contexts.insert(name.to_string(), v);
    }
}

// ---------------------------------------------------------------- lexer

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Ident(String),
    Dot,
    Star,
    LBracket,
    RBracket,
    LParen,
    RParen,
    Comma,
    Not,
    And,
    Or,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

fn lex(src: &str) -> Result<Vec<Tok>> {
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\n' | '\r' => i += 1,
            '.' if !chars.get(i + 1).is_some_and(|d| d.is_ascii_digit())
                || matches!(
                    out.last(),
                    Some(Tok::Ident(_) | Tok::RBracket | Tok::RParen | Tok::Star)
                ) =>
            {
                out.push(Tok::Dot);
                i += 1;
            }
            '*' => {
                out.push(Tok::Star);
                i += 1;
            }
            '[' => {
                out.push(Tok::LBracket);
                i += 1;
            }
            ']' => {
                out.push(Tok::RBracket);
                i += 1;
            }
            '(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            ',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            '!' if chars.get(i + 1) == Some(&'=') => {
                out.push(Tok::Ne);
                i += 2;
            }
            '!' => {
                out.push(Tok::Not);
                i += 1;
            }
            '=' if chars.get(i + 1) == Some(&'=') => {
                out.push(Tok::Eq);
                i += 2;
            }
            '<' if chars.get(i + 1) == Some(&'=') => {
                out.push(Tok::Le);
                i += 2;
            }
            '<' => {
                out.push(Tok::Lt);
                i += 1;
            }
            '>' if chars.get(i + 1) == Some(&'=') => {
                out.push(Tok::Ge);
                i += 2;
            }
            '>' => {
                out.push(Tok::Gt);
                i += 1;
            }
            '&' if chars.get(i + 1) == Some(&'&') => {
                out.push(Tok::And);
                i += 2;
            }
            '|' if chars.get(i + 1) == Some(&'|') => {
                out.push(Tok::Or);
                i += 2;
            }
            '\'' => {
                let mut s = String::new();
                i += 1;
                loop {
                    match chars.get(i) {
                        None => bail!("unterminated string in expression: {src}"),
                        Some('\'') if chars.get(i + 1) == Some(&'\'') => {
                            s.push('\'');
                            i += 2;
                        }
                        Some('\'') => {
                            i += 1;
                            break;
                        }
                        Some(&ch) => {
                            s.push(ch);
                            i += 1;
                        }
                    }
                }
                out.push(Tok::Str(s));
            }
            c if c.is_ascii_digit() || c == '-' || c == '.' => {
                let start = i;
                i += 1;
                while i < chars.len()
                    && (chars[i].is_ascii_alphanumeric()
                        || chars[i] == '.'
                        || ((chars[i] == '-' || chars[i] == '+')
                            && matches!(chars[i - 1], 'e' | 'E')))
                {
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                out.push(Tok::Num(
                    parse_number(&text).ok_or_else(|| anyhow!("invalid number '{text}'"))?,
                ));
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len()
                    && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '-')
                {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                out.push(match word.as_str() {
                    "null" => Tok::Null,
                    "true" => Tok::Bool(true),
                    "false" => Tok::Bool(false),
                    "NaN" => Tok::Num(f64::NAN),
                    "Infinity" => Tok::Num(f64::INFINITY),
                    _ => Tok::Ident(word),
                });
            }
            other => bail!("unexpected character '{other}' in expression: {src}"),
        }
    }
    Ok(out)
}

fn parse_number(text: &str) -> Option<f64> {
    let t = text.trim();
    if t.is_empty() {
        return Some(0.0);
    }
    let (neg, body) = match t.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let v = if let Some(h) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        i64::from_str_radix(h, 16).ok()? as f64
    } else if let Some(o) = body.strip_prefix("0o") {
        i64::from_str_radix(o, 8).ok()? as f64
    } else if body == "Infinity" {
        f64::INFINITY
    } else {
        if !body
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '-' | '+'))
        {
            return None;
        }
        body.parse::<f64>().ok()?
    };
    Some(if neg { -v } else { v })
}

// ---------------------------------------------------------------- parser

#[derive(Debug, Clone)]
enum Node {
    Lit(Value),
    Ident(String),
    Prop(Box<Node>, String),
    Index(Box<Node>, Box<Node>),
    Filter(Box<Node>),
    Call(String, Vec<Node>),
    Not(Box<Node>),
    Bin(Tok, Box<Node>, Box<Node>),
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        self.pos += 1;
        t
    }
    fn expect(&mut self, t: Tok) -> Result<()> {
        match self.next() {
            Some(ref got) if *got == t => Ok(()),
            got => bail!("expected {t:?}, found {got:?}"),
        }
    }

    fn parse_or(&mut self) -> Result<Node> {
        let mut lhs = self.parse_and()?;
        while self.peek() == Some(&Tok::Or) {
            self.next();
            lhs = Node::Bin(Tok::Or, Box::new(lhs), Box::new(self.parse_and()?));
        }
        Ok(lhs)
    }
    fn parse_and(&mut self) -> Result<Node> {
        let mut lhs = self.parse_eq()?;
        while self.peek() == Some(&Tok::And) {
            self.next();
            lhs = Node::Bin(Tok::And, Box::new(lhs), Box::new(self.parse_eq()?));
        }
        Ok(lhs)
    }
    fn parse_eq(&mut self) -> Result<Node> {
        let mut lhs = self.parse_cmp()?;
        while let Some(op @ (Tok::Eq | Tok::Ne)) = self.peek().cloned() {
            self.next();
            lhs = Node::Bin(op, Box::new(lhs), Box::new(self.parse_cmp()?));
        }
        Ok(lhs)
    }
    fn parse_cmp(&mut self) -> Result<Node> {
        let mut lhs = self.parse_unary()?;
        while let Some(op @ (Tok::Lt | Tok::Le | Tok::Gt | Tok::Ge)) = self.peek().cloned() {
            self.next();
            lhs = Node::Bin(op, Box::new(lhs), Box::new(self.parse_unary()?));
        }
        Ok(lhs)
    }
    fn parse_unary(&mut self) -> Result<Node> {
        if self.peek() == Some(&Tok::Not) {
            self.next();
            return Ok(Node::Not(Box::new(self.parse_unary()?)));
        }
        self.parse_postfix()
    }
    fn parse_postfix(&mut self) -> Result<Node> {
        let mut node = self.parse_primary()?;
        loop {
            match self.peek() {
                Some(Tok::Dot) => {
                    self.next();
                    match self.next() {
                        Some(Tok::Ident(name)) => node = Node::Prop(Box::new(node), name),
                        Some(Tok::Star) => node = Node::Filter(Box::new(node)),
                        // keywords used as property names, e.g. `inputs.null`
                        Some(Tok::Null) => node = Node::Prop(Box::new(node), "null".into()),
                        Some(Tok::Bool(b)) => node = Node::Prop(Box::new(node), b.to_string()),
                        t => bail!("expected property name after '.', found {t:?}"),
                    }
                }
                Some(Tok::LBracket) => {
                    self.next();
                    if self.peek() == Some(&Tok::Star) {
                        self.next();
                        self.expect(Tok::RBracket)?;
                        node = Node::Filter(Box::new(node));
                    } else {
                        let idx = self.parse_or()?;
                        self.expect(Tok::RBracket)?;
                        node = Node::Index(Box::new(node), Box::new(idx));
                    }
                }
                _ => return Ok(node),
            }
        }
    }
    fn parse_primary(&mut self) -> Result<Node> {
        match self.next() {
            Some(Tok::Null) => Ok(Node::Lit(Value::Null)),
            Some(Tok::Bool(b)) => Ok(Node::Lit(Value::Bool(b))),
            Some(Tok::Num(n)) => Ok(Node::Lit(num(n))),
            Some(Tok::Str(s)) => Ok(Node::Lit(Value::String(s))),
            Some(Tok::LParen) => {
                let e = self.parse_or()?;
                self.expect(Tok::RParen)?;
                Ok(e)
            }
            Some(Tok::Ident(name)) => {
                if self.peek() == Some(&Tok::LParen) {
                    self.next();
                    let mut args = Vec::new();
                    if self.peek() != Some(&Tok::RParen) {
                        loop {
                            args.push(self.parse_or()?);
                            if self.peek() == Some(&Tok::Comma) {
                                self.next();
                            } else {
                                break;
                            }
                        }
                    }
                    self.expect(Tok::RParen)?;
                    Ok(Node::Call(name.to_ascii_lowercase(), args))
                } else {
                    Ok(Node::Ident(name))
                }
            }
            t => bail!("unexpected token {t:?}"),
        }
    }
}

fn parse(src: &str) -> Result<Node> {
    let mut p = Parser {
        toks: lex(src)?,
        pos: 0,
    };
    let node = p.parse_or()?;
    if p.pos < p.toks.len() {
        bail!("unexpected trailing tokens in expression: {src}");
    }
    Ok(node)
}

// ---------------------------------------------------------------- values

fn num(n: f64) -> Value {
    if n.fract() == 0.0 && n.is_finite() && n.abs() < 9.0e15 {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn to_number(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => *b as i32 as f64,
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => parse_number(s).unwrap_or(f64::NAN),
        _ => f64::NAN,
    }
}

pub fn to_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => {
            let f = n.as_f64().unwrap_or(0.0);
            if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{}", f as i64)
            } else {
                n.to_string()
            }
        }
        Value::String(s) => s.clone(),
        Value::Array(_) => "Array".into(),
        Value::Object(_) => "Object".into(),
    }
}

fn loose_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(x), Value::String(y)) => x.eq_ignore_ascii_case(y),
        // GitHub compares objects and arrays by instance; values here are copies,
        // so two of them are never the same instance.
        (Value::Array(_) | Value::Object(_), _) | (_, Value::Array(_) | Value::Object(_)) => false,
        (Value::Null, Value::Null) => true,
        _ => to_number(a) == to_number(b),
    }
}

fn compare(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::String(x), Value::String(y)) => Some(x.to_lowercase().cmp(&y.to_lowercase())),
        _ => to_number(a).partial_cmp(&to_number(b)),
    }
}

fn get_prop(v: &Value, name: &str) -> Value {
    match v {
        Value::Object(m) => m
            .get(name)
            .or_else(|| {
                m.iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v)
            })
            .cloned()
            .unwrap_or(Value::Null),
        // property access through a filtered array maps over elements
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|i| get_prop(i, name))
                .filter(|v| !v.is_null())
                .collect(),
        ),
        _ => Value::Null,
    }
}

/// `name` in an object: the exact key, else one that differs only in case.
fn prop_ref<'v>(m: &'v Map<String, Value>, name: &str) -> Option<&'v Value> {
    m.get(name).or_else(|| {
        m.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
    })
}

/// A property chain (`github.event.ref`, `a['b']`, `a[0]`) looked up in place,
/// so only the value it ends at is copied, not every context on the way.
/// `None` for any other expression; `Some(None)` when the chain reaches nothing.
fn lookup<'c>(node: &Node, ctx: &'c Ctx) -> Option<Option<&'c Value>> {
    Some(match node {
        Node::Ident(name) => prop_ref(&ctx.contexts, name),
        // As in `eval`: an unfiltered array has no properties.
        Node::Prop(base, name) => match lookup(base, ctx)? {
            Some(Value::Object(m)) => prop_ref(m, name),
            _ => None,
        },
        Node::Index(base, idx) => {
            let Node::Lit(i) = idx.as_ref() else {
                return None;
            };
            match lookup(base, ctx)? {
                Some(Value::Array(a)) => {
                    let n = to_number(i);
                    (n.is_finite() && n >= 0.0)
                        .then(|| a.get(n as usize))
                        .flatten()
                }
                Some(Value::Object(m)) => prop_ref(m, &to_string(i)),
                _ => None,
            }
        }
        _ => return None,
    })
}

// ---------------------------------------------------------------- eval

fn eval(node: &Node, ctx: &Ctx) -> Result<Value> {
    if let Some(v) = lookup(node, ctx) {
        return Ok(v.cloned().unwrap_or(Value::Null));
    }
    Ok(match node {
        Node::Lit(v) => v.clone(),
        Node::Ident(name) => prop_ref(&ctx.contexts, name)
            .cloned()
            .unwrap_or(Value::Null),
        Node::Prop(base, name) => {
            let b = eval(base, ctx)?;
            match &b {
                Value::Array(_) if !is_filtered(base) => Value::Null,
                _ => get_prop(&b, name),
            }
        }
        Node::Index(base, idx) => {
            let b = eval(base, ctx)?;
            let i = eval(idx, ctx)?;
            match (&b, &i) {
                (Value::Array(a), _) => {
                    let n = to_number(&i);
                    if n.is_finite() && n >= 0.0 {
                        a.get(n as usize).cloned().unwrap_or(Value::Null)
                    } else {
                        Value::Null
                    }
                }
                (Value::Object(_), _) => get_prop(&b, &to_string(&i)),
                _ => Value::Null,
            }
        }
        Node::Filter(base) => match eval(base, ctx)? {
            Value::Array(a) => Value::Array(a),
            Value::Object(m) => Value::Array(m.into_iter().map(|(_, v)| v).collect()),
            _ => Value::Array(vec![]),
        },
        Node::Not(e) => Value::Bool(!truthy(&eval(e, ctx)?)),
        Node::Bin(op, l, r) => {
            let lv = eval(l, ctx)?;
            match op {
                Tok::And => return if truthy(&lv) { eval(r, ctx) } else { Ok(lv) },
                Tok::Or => return if truthy(&lv) { Ok(lv) } else { eval(r, ctx) },
                _ => {}
            }
            let rv = eval(r, ctx)?;
            use std::cmp::Ordering::*;
            Value::Bool(match op {
                Tok::Eq => loose_eq(&lv, &rv),
                Tok::Ne => !loose_eq(&lv, &rv),
                Tok::Lt => compare(&lv, &rv) == Some(Less),
                Tok::Le => matches!(compare(&lv, &rv), Some(Less | Equal)),
                Tok::Gt => compare(&lv, &rv) == Some(Greater),
                Tok::Ge => matches!(compare(&lv, &rv), Some(Greater | Equal)),
                _ => unreachable!(),
            })
        }
        Node::Call(name, args) => call(name, args, ctx)?,
    })
}

/// True when a property chain passes through an object filter (`.*`).
fn is_filtered(node: &Node) -> bool {
    match node {
        Node::Filter(_) => true,
        Node::Prop(base, _) => is_filtered(base),
        _ => false,
    }
}

fn call(name: &str, args: &[Node], ctx: &Ctx) -> Result<Value> {
    let argc = |min: usize, max: usize| -> Result<()> {
        if args.len() < min || args.len() > max {
            bail!(
                "{name}() expects {min}..{max} arguments, got {}",
                args.len()
            );
        }
        Ok(())
    };
    match name {
        "success" => {
            argc(0, 0)?;
            Ok(Value::Bool(succeeded(ctx)))
        }
        "failure" => {
            argc(0, 0)?;
            Ok(Value::Bool(ctx.failed && !ctx.cancelled))
        }
        "always" => {
            argc(0, 0)?;
            Ok(Value::Bool(true))
        }
        "cancelled" => {
            argc(0, 0)?;
            Ok(Value::Bool(ctx.cancelled))
        }
        // case(pred1, val1, ..., default): the value of the first true predicate;
        // only the predicates up to it and the chosen value are evaluated.
        "case" => {
            if args.len() < 3 || args.len().is_multiple_of(2) {
                bail!(
                    "case() expects predicate/value pairs and a default (an odd number of 3 or more arguments), got {}",
                    args.len()
                );
            }
            let (default, pairs) = args.split_last().expect("at least 3 arguments");
            for pair in pairs.chunks(2) {
                if truthy(&eval(&pair[0], ctx)?) {
                    return eval(&pair[1], ctx);
                }
            }
            eval(default, ctx)
        }
        _ => {
            let vals = args
                .iter()
                .map(|a| eval(a, ctx))
                .collect::<Result<Vec<_>>>()?;
            builtin(name, &vals, ctx)
        }
    }
}

fn builtin(name: &str, a: &[Value], ctx: &Ctx) -> Result<Value> {
    let need = |n: usize| -> Result<()> {
        if a.len() < n {
            bail!("{name}() expects at least {n} arguments")
        } else {
            Ok(())
        }
    };
    Ok(match name {
        "contains" => {
            need(2)?;
            Value::Bool(match &a[0] {
                Value::Array(items) => items.iter().any(|i| loose_eq(i, &a[1])),
                other => to_string(other)
                    .to_lowercase()
                    .contains(&to_string(&a[1]).to_lowercase()),
            })
        }
        "startswith" => {
            need(2)?;
            Value::Bool(
                to_string(&a[0])
                    .to_lowercase()
                    .starts_with(&to_string(&a[1]).to_lowercase()),
            )
        }
        "endswith" => {
            need(2)?;
            Value::Bool(
                to_string(&a[0])
                    .to_lowercase()
                    .ends_with(&to_string(&a[1]).to_lowercase()),
            )
        }
        "format" => {
            need(1)?;
            Value::String(format_str(&to_string(&a[0]), &a[1..])?)
        }
        "join" => {
            need(1)?;
            let sep = a.get(1).map(to_string).unwrap_or_else(|| ",".into());
            match &a[0] {
                Value::Array(items) => {
                    Value::String(items.iter().map(to_string).collect::<Vec<_>>().join(&sep))
                }
                other => Value::String(to_string(other)),
            }
        }
        "tojson" => {
            need(1)?;
            Value::String(serde_json::to_string_pretty(&a[0])?)
        }
        "fromjson" => {
            need(1)?;
            serde_json::from_str(&to_string(&a[0]))
                .map_err(|e| anyhow!("fromJSON: invalid JSON '{}': {e}", to_string(&a[0])))?
        }
        "hashfiles" => Value::String(hash_files(ctx, a)?),
        other => bail!("unknown function '{other}'"),
    })
}

fn format_str(fmt: &str, args: &[Value]) -> Result<String> {
    let chars: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '{' if chars.get(i + 1) == Some(&'{') => {
                out.push('{');
                i += 2;
            }
            '}' if chars.get(i + 1) == Some(&'}') => {
                out.push('}');
                i += 2;
            }
            '{' => {
                let end = chars[i..]
                    .iter()
                    .position(|&c| c == '}')
                    .ok_or_else(|| anyhow!("format: unclosed '{{' in '{fmt}'"))?;
                let idx: String = chars[i + 1..i + end].iter().collect();
                let n: usize = idx
                    .trim()
                    .parse()
                    .map_err(|_| anyhow!("format: bad placeholder '{{{idx}}}'"))?;
                out.push_str(&to_string(
                    args.get(n)
                        .ok_or_else(|| anyhow!("format: missing argument {n}"))?,
                ));
                i += end + 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

fn hash_files(ctx: &Ctx, patterns: &[Value]) -> Result<String> {
    use sha2::{Digest, Sha256};
    let Some(root) = &ctx.workspace else {
        return Ok(String::new());
    };
    let mut includes = Vec::new();
    let mut excludes = Vec::new();
    for p in patterns.iter().map(to_string) {
        match p.strip_prefix('!') {
            Some(ex) => excludes.push(glob::Pattern::new(ex)?),
            None => includes.push(p),
        }
    }
    let mut files = std::collections::BTreeSet::new();
    for pat in includes {
        let full = root.join(&pat);
        for entry in glob::glob(&full.to_string_lossy())?.flatten() {
            if !entry.is_file() {
                continue;
            }
            let rel = entry.strip_prefix(root).unwrap_or(&entry).to_path_buf();
            if !excludes.iter().any(|e| e.matches_path(&rel)) {
                files.insert(entry);
            }
        }
    }
    if files.is_empty() {
        return Ok(String::new());
    }
    let mut outer = Sha256::new();
    for f in files {
        outer.update(Sha256::digest(std::fs::read(&f)?));
    }
    Ok(hex::encode(outer.finalize()))
}

// ---------------------------------------------------------------- public API

/// Evaluate a bare expression (no `${{ }}`).
pub fn evaluate(src: &str, ctx: &Ctx) -> Result<Value> {
    eval(&parse(src)?, ctx).map_err(|e| anyhow!("{e} (in expression `{}`)", src.trim()))
}

/// Split a template into literal and expression segments.
fn segments(s: &str) -> Result<Vec<(bool, String)>> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find("${{") {
        out.push((false, rest[..start].to_string()));
        let body = &rest[start + 3..];
        // find the closing `}}` outside of string literals
        let bytes = body.as_bytes();
        let (mut i, mut in_str, mut end) = (0, false, None);
        while i < bytes.len() {
            match bytes[i] {
                b'\'' => in_str = !in_str,
                b'}' if !in_str && bytes.get(i + 1) == Some(&b'}') => {
                    end = Some(i);
                    break;
                }
                _ => {}
            }
            i += 1;
        }
        let end = end.ok_or_else(|| anyhow!("unterminated '${{{{' in: {s}"))?;
        out.push((true, body[..end].to_string()));
        rest = &body[end + 2..];
    }
    out.push((false, rest.to_string()));
    Ok(out)
}

/// Substitute every `${{ expr }}` in a string.
pub fn interpolate(s: &str, ctx: &Ctx) -> Result<String> {
    if !s.contains("${{") {
        return Ok(s.to_string());
    }
    let mut out = String::new();
    for (is_expr, text) in segments(s)? {
        if is_expr {
            out.push_str(&to_string(&evaluate(&text, ctx)?));
        } else {
            out.push_str(&text);
        }
    }
    Ok(out)
}

/// Like `interpolate`, but a string that is exactly one expression keeps its typed value.
pub fn interpolate_value(s: &str, ctx: &Ctx) -> Result<Value> {
    let segs = segments(s)?;
    if let [(false, a), (true, e), (false, b)] = segs.as_slice() {
        if a.trim().is_empty() && b.trim().is_empty() {
            return evaluate(e, ctx);
        }
    }
    Ok(Value::String(interpolate(s, ctx)?))
}

/// Recursively interpolate every string inside a JSON value.
pub fn interpolate_json(v: &Value, ctx: &Ctx) -> Result<Value> {
    interpolate_tree(v, ctx, false)
}

/// Like `interpolate_json`, for `strategy.matrix`: a list item that is one
/// expression yielding a list is spliced into the surrounding list (as act does).
pub fn interpolate_matrix(v: &Value, ctx: &Ctx) -> Result<Value> {
    interpolate_tree(v, ctx, true)
}

/// `${{ insert }}` as a mapping key merges the (object) value into the mapping.
fn is_insert(key: &str) -> bool {
    key.trim()
        .strip_prefix("${{")
        .and_then(|k| k.strip_suffix("}}"))
        .is_some_and(|k| k.trim() == "insert")
}

fn interpolate_tree(v: &Value, ctx: &Ctx, splice: bool) -> Result<Value> {
    Ok(match v {
        Value::String(s) => interpolate_value(s, ctx)?,
        Value::Array(a) => {
            let mut out = Vec::with_capacity(a.len());
            for x in a {
                let lone_expr = matches!(x, Value::String(s) if segments(s).is_ok_and(|g| {
                    g.iter().filter(|(e, _)| *e).count() == 1
                        && g.iter().all(|(e, t)| *e || t.trim().is_empty())
                }));
                match interpolate_tree(x, ctx, splice)? {
                    Value::Array(inner) if splice && lone_expr => out.extend(inner),
                    y => out.push(y),
                }
            }
            Value::Array(out)
        }
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, x) in m {
                let val = interpolate_tree(x, ctx, splice)?;
                if is_insert(k) {
                    match val {
                        Value::Object(inner) => out.extend(inner),
                        Value::Null => {}
                        other => bail!("`${{{{ insert }}}}` needs an object, got {other}"),
                    }
                } else {
                    out.insert(interpolate(k, ctx)?, val);
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    })
}

/// Whether `node` calls a status function; one named only inside a string
/// literal (`contains(msg, 'always(')`) does not count.
fn calls_status(node: &Node) -> bool {
    match node {
        Node::Call(name, args) => {
            ["success", "failure", "always", "cancelled"].contains(&name.as_str())
                || args.iter().any(calls_status)
        }
        Node::Prop(n, _) | Node::Filter(n) | Node::Not(n) => calls_status(n),
        Node::Index(a, b) | Node::Bin(_, a, b) => calls_status(a) || calls_status(b),
        Node::Lit(_) | Node::Ident(_) => false,
    }
}

fn succeeded(ctx: &Ctx) -> bool {
    !ctx.failed && !ctx.ancestor_unsucceeded && !ctx.cancelled
}

/// Evaluate an `if:` condition. Without a status function it is implicitly `success() && (...)`.
pub fn evaluate_if(cond: Option<&str>, ctx: &Ctx) -> Result<bool> {
    let raw = cond.map(str::trim).unwrap_or("");
    if raw.is_empty() {
        return Ok(succeeded(ctx));
    }
    let body = match raw.strip_prefix("${{").and_then(|r| r.strip_suffix("}}")) {
        Some(inner) if !inner.contains("${{") => inner.to_string(),
        _ if raw.contains("${{") => format!("'{}'", interpolate(raw, ctx)?.replace('\'', "''")),
        _ => raw.to_string(),
    };
    let node = parse(&body).map_err(|e| anyhow!("{e} (in expression `{}`)", body.trim()))?;
    let full = if calls_status(&node) {
        body
    } else {
        format!("success() && ({body})")
    };
    Ok(truthy(&evaluate(&full, ctx)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> Ctx {
        let mut c = Ctx::default();
        c.set(
            "github",
            json!({"event_name": "push", "ref": "refs/heads/main"}),
        );
        c.set("matrix", json!({"os": "ubuntu", "node": 18}));
        c.set(
            "steps",
            json!({"build-step": {"outputs": {"v": "1.2"}, "outcome": "success"}}),
        );
        c.set(
            "needs",
            json!({"a": {"result": "success"}, "b": {"result": "failure"}}),
        );
        c
    }

    #[test]
    fn literals_and_ops() {
        let c = ctx();
        assert_eq!(evaluate("1 == '1'", &c).unwrap(), json!(true));
        assert_eq!(evaluate("'ABC' == 'abc'", &c).unwrap(), json!(true));
        assert_eq!(evaluate("null == 0", &c).unwrap(), json!(true));
        assert_eq!(evaluate("0x10 > 15", &c).unwrap(), json!(true));
        assert_eq!(evaluate("'' || 'fallback'", &c).unwrap(), json!("fallback"));
        assert_eq!(evaluate("'a' && 'b'", &c).unwrap(), json!("b"));
        assert_eq!(evaluate("!(1 < 2)", &c).unwrap(), json!(false));
        assert_eq!(evaluate("-1.5 < 0", &c).unwrap(), json!(true));
        assert_eq!(evaluate("'it''s'", &c).unwrap(), json!("it's"));
    }

    #[test]
    fn contexts() {
        let c = ctx();
        assert_eq!(evaluate("github.event_name", &c).unwrap(), json!("push"));
        assert_eq!(evaluate("GITHUB.EVENT_NAME", &c).unwrap(), json!("push"));
        assert_eq!(evaluate("matrix['os']", &c).unwrap(), json!("ubuntu"));
        assert_eq!(
            evaluate("steps.build-step.outputs.v", &c).unwrap(),
            json!("1.2")
        );
        assert_eq!(evaluate("github.missing.deep", &c).unwrap(), json!(null));
        assert_eq!(
            evaluate("needs.*.result", &c).unwrap(),
            json!(["success", "failure"])
        );
        assert_eq!(
            evaluate("contains(needs.*.result, 'failure')", &c).unwrap(),
            json!(true)
        );
    }

    /// Bytes this thread allocated, so a test can see what an evaluation copies.
    struct Counting;
    thread_local! {
        static ALLOCATED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }
    unsafe impl std::alloc::GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
            let _ = ALLOCATED.try_with(|a| a.set(a.get() + layout.size()));
            std::alloc::System.alloc(layout)
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
            std::alloc::System.dealloc(ptr, layout)
        }
    }
    #[global_allocator]
    static COUNTING: Counting = Counting;

    fn allocated_by(f: impl FnOnce()) -> usize {
        let before = ALLOCATED.with(|a| a.get());
        f();
        ALLOCATED.with(|a| a.get()) - before
    }

    #[test]
    fn property_chains_copy_only_their_value() {
        let mut c = ctx();
        let big: Vec<Value> = (0..20_000).map(|n| json!({"n": n})).collect();
        c.contexts["github"]["event"] = json!({"commits": big, "ref": "refs/heads/x"});
        let bytes = allocated_by(|| {
            for src in [
                "github.event_name",
                "github.event.ref",
                "github['ref']",
                "matrix.node",
            ] {
                evaluate(src, &c).unwrap();
            }
        });
        // Copying the event even once would allocate well over a megabyte.
        assert!(bytes < 16 * 1024, "{bytes} bytes allocated");
        assert_eq!(evaluate("github.event.commits[1].n", &c).unwrap(), json!(1));
        assert_eq!(evaluate("github.event.commits.n", &c).unwrap(), json!(null));
        assert_eq!(
            evaluate("github.event.commits[-1]", &c).unwrap(),
            json!(null)
        );
        assert_eq!(
            evaluate("github.EVENT.Ref", &c).unwrap(),
            json!("refs/heads/x")
        );
        assert_eq!(
            evaluate("github.event.commits.*.n", &c)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            20_000
        );
    }

    #[test]
    fn case_returns_the_first_matching_value() {
        let c = ctx();
        let ev = |e: &str| evaluate(e, &c).unwrap();
        assert_eq!(
            ev("case(github.ref == 'refs/heads/main', 'production', 'development')"),
            json!("production")
        );
        assert_eq!(
            ev("case(matrix.os == 'macos', 'mac', matrix.node > 16, matrix.node, 'other')"),
            json!(18)
        );
        assert_eq!(ev("case(false, 'a', '', 'b', 'default')"), json!("default"));
        // Values after the match are not evaluated.
        assert_eq!(
            ev("case(true, 'ok', true, fromJSON('not json'), 'd')"),
            json!("ok")
        );
        for bad in [
            "case(true, 'a')",
            "case(true)",
            "case(true, 'a', false, 'b')",
        ] {
            assert!(evaluate(bad, &c).is_err(), "{bad}");
        }
    }

    #[test]
    fn functions() {
        let c = ctx();
        assert_eq!(
            evaluate("format('{0}-{1} {{x}}', 'a', 1)", &c).unwrap(),
            json!("a-1 {x}")
        );
        assert_eq!(
            evaluate("startsWith(github.ref, 'refs/HEADS')", &c).unwrap(),
            json!(true)
        );
        assert_eq!(
            evaluate("join(fromJSON('[1,2,3]'), '+')", &c).unwrap(),
            json!("1+2+3")
        );
        assert_eq!(
            evaluate("fromJSON('{\"a\":[1]}').a[0]", &c).unwrap(),
            json!(1)
        );
        assert_eq!(evaluate("toJSON(matrix.node)", &c).unwrap(), json!("18"));
    }

    #[test]
    fn templates() {
        let c = ctx();
        assert_eq!(
            interpolate("os=${{ matrix.os }} n=${{ matrix.node }}", &c).unwrap(),
            "os=ubuntu n=18"
        );
        assert_eq!(interpolate("${{ '}}' }}", &c).unwrap(), "}}");
        assert_eq!(
            interpolate_value("${{ matrix.node }}", &c).unwrap(),
            json!(18)
        );
    }

    #[test]
    fn if_conditions() {
        let mut c = ctx();
        assert!(evaluate_if(None, &c).unwrap());
        assert!(evaluate_if(Some("github.event_name == 'push'"), &c).unwrap());
        assert!(evaluate_if(Some("${{ matrix.os == 'ubuntu' }}"), &c).unwrap());
        c.failed = true;
        assert!(!evaluate_if(Some("true"), &c).unwrap());
        assert!(evaluate_if(Some("failure()"), &c).unwrap());
        assert!(evaluate_if(Some("always()"), &c).unwrap());
        assert!(evaluate_if(Some("ALWAYS() && true"), &c).unwrap());
        // A status function named in a string is no call: `success() &&` still applies.
        assert!(!evaluate_if(Some("contains('x always() y', 'always(')"), &c).unwrap());
        assert!(!evaluate_if(Some("'failure()' != ''"), &c).unwrap());
    }
}
