//! HQL v0 read-only pipes. Opens the store read-only; never writes.
//!
//! This module is the `Query` builder and pipeline parser. Operators live in
//! `ops`, typed rows in `row`, the CLI and writers in `cli`.

pub mod cli;
mod expr;
mod ops;
mod row;
mod store;

use std::collections::HashMap;
use std::path::Path;

pub use cli::{fields_of, run_cli, write_output, OutputFormat, HQL_HELP};
pub use expr::{parse_filter, Comparison, FilterExpr};
pub use row::{
    default_history_fields, default_output_fields, default_state_fields, parse_extra,
    DesiredStateView, EdgeView, ExtraMap, HistoryEventView, NodeView, Row, Value,
};
pub use store::RoStore;

use crate::error::{Error, Result};

#[derive(Clone, Debug)]
enum Op {
    Vault(String),
    Agent(String),
    /// `state` (all named desired states in scope) or `state NAME`.
    State(Option<String>),
    /// `history` / `history NAME`: causal events of the same selection.
    History(Option<String>),
    Search(String),
    Traverse { edge: String, hops: i64 },
    Filter(String),
    Select(Vec<String>),
    Limit(i64),
}

pub struct Query {
    store: RoStore,
    ops: Vec<Op>,
}

impl Query {
    pub fn open(db: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            store: RoStore::open(db)?,
            ops: Vec::new(),
        })
    }

    pub fn vault(mut self, name: impl Into<String>) -> Self {
        self.ops.push(Op::Vault(name.into()));
        self
    }

    pub fn agent(mut self, name: impl Into<String>) -> Self {
        self.ops.push(Op::Agent(name.into()));
        self
    }

    /// Every named desired state in scope (see `state_named` for one).
    pub fn state(mut self) -> Self {
        self.ops.push(Op::State(None));
        self
    }

    pub fn state_named(mut self, name: impl Into<String>) -> Self {
        self.ops.push(Op::State(Some(name.into())));
        self
    }

    pub fn history(mut self) -> Self {
        self.ops.push(Op::History(None));
        self
    }

    pub fn history_named(mut self, name: impl Into<String>) -> Self {
        self.ops.push(Op::History(Some(name.into())));
        self
    }

    pub fn search(mut self, text: impl Into<String>) -> Self {
        self.ops.push(Op::Search(text.into()));
        self
    }

    pub fn traverse(mut self, edge: impl Into<String>, hops: i64) -> Self {
        self.ops.push(Op::Traverse {
            edge: edge.into(),
            hops,
        });
        self
    }

    pub fn filter(mut self, expr: impl Into<String>) -> Self {
        self.ops.push(Op::Filter(expr.into()));
        self
    }

    pub fn select<I, S>(mut self, fields: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut parsed = Vec::new();
        for field in fields {
            parsed.extend(split_fields(field.as_ref()));
        }
        self.ops.push(Op::Select(parsed));
        self
    }

    pub fn limit(mut self, n: i64) -> Self {
        self.ops.push(Op::Limit(n));
        self
    }

    pub fn pipe(mut self, pipeline: &str) -> Result<Self> {
        for stage in split_pipeline(pipeline) {
            self.apply_stage(&stage)?;
        }
        Ok(self)
    }

    pub fn run(self) -> Result<Vec<Row>> {
        let nodes = self.store.nodes()?;
        let by_id: HashMap<String, NodeView> =
            nodes.iter().map(|n| (n.id.clone(), n.clone())).collect();
        let mut outgoing: HashMap<String, Vec<EdgeView>> = HashMap::new();
        for edge in self.store.edges()? {
            outgoing.entry(edge.from_id.clone()).or_default().push(edge);
        }
        let graph = ops::Graph {
            by_id: &by_id,
            outgoing: &outgoing,
        };

        let mut rows: Vec<Row> = nodes.into_iter().map(Row::Node).collect();
        for op in &self.ops {
            match op {
                Op::Vault(name) => {
                    let vault_ids = self.store.vault_ids_named(name)?;
                    rows.retain(|row| {
                        row.vault_id()
                            .map(|id| vault_ids.iter().any(|v| v == id))
                            .unwrap_or(false)
                    });
                }
                Op::Agent(name) => rows = ops::filter_agents(rows, name),
                Op::State(name) => {
                    rows = ops::apply_state(&rows, name.as_deref(), &self.store)?;
                }
                Op::History(name) => {
                    rows = ops::apply_history(&rows, name.as_deref(), &self.store)?;
                }
                Op::Search(needle) => rows.retain(|row| ops::search_hit(row, needle, &graph)),
                Op::Traverse { edge, hops } => rows = ops::traverse(rows, edge, *hops, &graph)?,
                Op::Filter(expr) => {
                    let expr = parse_filter(expr)?;
                    rows.retain(|row| expr.eval(row));
                }
                Op::Select(fields) => {
                    rows = rows.iter().map(|row| row.project(fields)).collect();
                }
                Op::Limit(n) => ops::apply_limit(&mut rows, *n),
            }
        }
        Ok(rows)
    }

    fn apply_stage(&mut self, stage: &str) -> Result<()> {
        let stage = stage.trim();
        if stage.is_empty() {
            return Ok(());
        }
        let (name, rest) = match stage.split_once(' ') {
            Some((name, rest)) => (name, rest.trim()),
            None => (stage, ""),
        };
        match name {
            "vault" => self.ops.push(Op::Vault(unquote_arg(rest))),
            "agent" => {
                if rest.is_empty() {
                    return Err(Error::Invalid("agent requires a name".into()));
                }
                self.ops.push(Op::Agent(unquote_arg(rest)));
            }
            "state" => self.ops.push(Op::State(optional_name(rest))),
            "history" => self.ops.push(Op::History(optional_name(rest))),
            "search" => self.ops.push(Op::Search(unquote_arg(rest))),
            "traverse" => {
                let (edge, hops) = parse_traverse(rest)?;
                self.ops.push(Op::Traverse { edge, hops });
            }
            "filter" => self.ops.push(Op::Filter(rest.to_string())),
            "select" => self.ops.push(Op::Select(split_fields(rest))),
            "limit" => {
                let n = rest
                    .parse::<i64>()
                    .map_err(|_| Error::Invalid(format!("limit needs an integer, got {rest:?}")))?;
                self.ops.push(Op::Limit(n));
            }
            other => return Err(Error::Invalid(format!("unknown operator {other:?}"))),
        }
        Ok(())
    }
}

pub fn run_pipeline(db: impl AsRef<Path>, pipeline: &str) -> Result<Vec<Row>> {
    Query::open(db)?.pipe(pipeline)?.run()
}

/// Quote-aware `|` split. The shell is not the parser.
pub fn split_pipeline(pipeline: &str) -> Vec<String> {
    let mut stages = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = pipeline.chars().collect();
    let mut i = 0;
    let mut quote: Option<char> = None;
    while i < chars.len() {
        let ch = chars[i];
        if quote.is_some() {
            buf.push(ch);
            if Some(ch) == quote {
                quote = None;
            } else if ch == '\\' && i + 1 < chars.len() {
                buf.push(chars[i + 1]);
                i += 1;
            }
            i += 1;
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            buf.push(ch);
            i += 1;
            continue;
        }
        if ch == '|' {
            stages.push(buf.trim().to_string());
            buf.clear();
            i += 1;
            continue;
        }
        buf.push(ch);
        i += 1;
    }
    let tail = buf.trim();
    if !tail.is_empty() {
        stages.push(tail.to_string());
    }
    stages
}

fn optional_name(rest: &str) -> Option<String> {
    if rest.is_empty() {
        None
    } else {
        Some(unquote_arg(rest))
    }
}

fn parse_traverse(rest: &str) -> Result<(String, i64)> {
    let tokens = tokenize_args(rest);
    let mut edge = None;
    let mut hops = 1i64;
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i].as_str();
        if tok == "--edge" && i + 1 < tokens.len() {
            edge = Some(unquote_arg(&tokens[i + 1]));
            i += 2;
            continue;
        }
        if tok == "--hops" && i + 1 < tokens.len() {
            hops = tokens[i + 1]
                .parse::<i64>()
                .map_err(|_| Error::Invalid(format!("invalid --hops {}", tokens[i + 1])))?;
            i += 2;
            continue;
        }
        return Err(Error::Invalid(format!("unknown traverse argument {tok:?}")));
    }
    let edge = edge.ok_or_else(|| Error::Invalid("traverse requires --edge".into()))?;
    Ok((edge, hops))
}

fn tokenize_args(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut buf = String::new();
    let mut quote: Option<char> = None;
    for ch in text.chars() {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                buf.push(ch);
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            continue;
        }
        if ch.is_whitespace() {
            if !buf.is_empty() {
                tokens.push(std::mem::take(&mut buf));
            }
            continue;
        }
        buf.push(ch);
    }
    if !buf.is_empty() {
        tokens.push(buf);
    }
    tokens
}

fn unquote_arg(text: &str) -> String {
    let text = text.trim();
    if text.len() >= 2 {
        let bytes = text.as_bytes();
        let first = bytes[0];
        if (first == b'\'' || first == b'"') && bytes[text.len() - 1] == first {
            return text[1..text.len() - 1].to_string();
        }
    }
    text.to_string()
}

fn split_fields(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}
