//! Typed query rows. Each variant owns exactly the fields it can answer for;
//! nothing falls back from one kind to another.

use std::collections::BTreeMap;
use std::fmt;

/// Scalar cell. Missing extra keys and absent fields are Null.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Str(String),
    Int(i64),
    Float(f64),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Python `str(value)` for a non-None cell; Null is empty (CLI `_cell`).
    pub fn to_display(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Str(s) => s.clone(),
            Value::Int(n) => n.to_string(),
            Value::Float(f) => format_float(*f),
        }
    }

    /// Python `str(left)` used by filter compare for a present value.
    pub fn py_str(&self) -> String {
        self.to_display()
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_display())
    }
}

/// Match Python `str(float)` / `json.dumps` for finite values (`1.0` not `1`).
pub(crate) fn format_float(v: f64) -> String {
    if v.is_nan() {
        return "nan".into();
    }
    if v.is_infinite() {
        return if v.is_sign_positive() {
            "inf".into()
        } else {
            "-inf".into()
        };
    }
    let s = format!("{v}");
    if s.contains('.') || s.contains('e') || s.contains('E') {
        s
    } else {
        format!("{s}.0")
    }
}

/// `nodes.extra` as a YAML mapping. Top-level null is None; every other value
/// renders to text (`extra.k` v0 has no JSON path): scalars as themselves,
/// sequences / mappings in YAML flow style. Non-mapping or invalid YAML is empty.
pub type ExtraMap = BTreeMap<String, Option<String>>;

pub fn parse_extra(extra: &str) -> ExtraMap {
    let mut mapping = BTreeMap::new();
    let Ok(serde_yaml::Value::Mapping(map)) = serde_yaml::from_str::<serde_yaml::Value>(extra)
    else {
        return mapping;
    };
    for (key, value) in map {
        let key = match yaml_text(&key) {
            Some(k) => k,
            None => "null".to_string(),
        };
        mapping.insert(key, yaml_text(&value));
    }
    mapping
}

/// Text form of a YAML value; None only for a top-level null.
fn yaml_text(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::Null => None,
        other => Some(yaml_flow(other)),
    }
}

/// YAML flow rendering shared with the Python twin: `[a, b]`, `{k: v}`.
fn yaml_flow(value: &serde_yaml::Value) -> String {
    match value {
        serde_yaml::Value::Null => "null".into(),
        serde_yaml::Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        serde_yaml::Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => i.to_string(),
            (None, Some(u), _) => u.to_string(),
            (None, None, Some(f)) => format_float(f),
            _ => n.to_string(),
        },
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Sequence(items) => {
            let parts: Vec<String> = items.iter().map(yaml_flow).collect();
            format!("[{}]", parts.join(", "))
        }
        serde_yaml::Value::Mapping(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", yaml_flow(k), yaml_flow(v)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        serde_yaml::Value::Tagged(tagged) => yaml_flow(&tagged.value),
    }
}

/// A `nodes` row.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeView {
    pub id: String,
    pub vault_id: String,
    pub node_type: String,
    pub path: Option<String>,
    pub extra: String,
    pub extra_map: ExtraMap,
}

impl NodeView {
    pub fn extra_get(&self, key: &str) -> Option<&str> {
        self.extra_map.get(key).and_then(|v| v.as_deref())
    }
}

/// An `edges` row.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeView {
    pub id: String,
    pub from_id: String,
    pub to_id: Option<String>,
    pub to_raw: Option<String>,
    pub edge_type: String,
    pub properties: String,
}

/// Warm `desired_states` row. Never carries events.
#[derive(Clone, Debug, PartialEq)]
pub struct DesiredStateView {
    pub id: String,
    pub vault_id: String,
    pub name: String,
    pub state_version: i64,
    pub reconciled_by: Option<String>,
    pub importance: f64,
    pub spec: String,
    pub status: String,
}

/// Cool `events` row. Drops `data`; never carries spec / status.
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryEventView {
    pub id: String,
    pub vault_id: String,
    pub ts: i64,
    pub actor: String,
    pub event_type: String,
    pub caused_by: Option<String>,
    pub reconciles: Option<String>,
    pub supersedes: Option<String>,
}

/// One pipeline row.
#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    Node(NodeView),
    /// One traverse step: `from` node, the edge, and the resolved `to` node.
    Walk {
        from: NodeView,
        edge: EdgeView,
        to: Option<NodeView>,
    },
    /// Warm overlay: a named desired state, optionally under the Agent / Vault
    /// node that selected it. `subject` is None for `state <name>` over a slice
    /// with no such node.
    State {
        subject: Option<NodeView>,
        ds: DesiredStateView,
    },
    /// Cool: one causal event.
    History { event: HistoryEventView },
    /// Output of `select`: fixed columns over `source`; later stages still see
    /// the source node for traverse / search.
    Selected {
        columns: Vec<(String, Value)>,
        source: Box<Row>,
    },
}

impl Row {
    /// The node this row stands on, if any: the node itself, a walk's `from`,
    /// a state's subject. History rows stand on nothing.
    pub fn node(&self) -> Option<&NodeView> {
        match self {
            Row::Node(node) => Some(node),
            Row::Walk { from, .. } => Some(from),
            Row::State { subject, .. } => subject.as_ref(),
            Row::History { .. } => None,
            Row::Selected { source, .. } => source.node(),
        }
    }

    pub fn node_id(&self) -> Option<&str> {
        self.node().map(|n| n.id.as_str())
    }

    pub fn node_type(&self) -> Option<&str> {
        self.node().map(|n| n.node_type.as_str())
    }

    /// Vault the row belongs to.
    pub fn vault_id(&self) -> Option<&str> {
        match self {
            Row::State { ds, .. } => Some(&ds.vault_id),
            Row::History { event } => Some(&event.vault_id),
            Row::Selected { source, .. } => source.vault_id(),
            _ => self.node().map(|n| n.vault_id.as_str()),
        }
    }

    pub fn to_id(&self) -> Option<&str> {
        match self {
            Row::Walk { edge, .. } => edge.to_id.as_deref(),
            Row::Selected { source, .. } => source.to_id(),
            _ => None,
        }
    }

    pub fn from_id(&self) -> Option<&str> {
        match self {
            Row::Walk { edge, .. } => Some(&edge.from_id),
            Row::Selected { source, .. } => source.from_id(),
            _ => None,
        }
    }

    pub fn selected_fields(&self) -> Option<Vec<String>> {
        match self {
            Row::Selected { columns, .. } => Some(columns.iter().map(|(k, _)| k.clone()).collect()),
            _ => None,
        }
    }

    /// Cell for `field`. A selected row answers only for its columns.
    pub fn get(&self, field: &str) -> Value {
        match self {
            Row::Selected { columns, .. } => columns
                .iter()
                .find(|(k, _)| k == field)
                .map(|(_, v)| v.clone())
                .unwrap_or(Value::Null),
            Row::Node(node) => node_field(node, field),
            Row::Walk { from, edge, to } => match field {
                "from_id" => Value::Str(edge.from_id.clone()),
                "from.path" => opt_str(&from.path),
                "to_id" => opt_str(&edge.to_id),
                "to_raw" => opt_str(&edge.to_raw),
                "to.path" => match to {
                    Some(dest) => opt_str(&dest.path),
                    None => Value::Null,
                },
                "type" => Value::Str(edge.edge_type.clone()),
                _ => node_field(from, field),
            },
            Row::State { subject, ds } => match field {
                "id" => Value::Str(ds.id.clone()),
                "vault_id" => Value::Str(ds.vault_id.clone()),
                "name" => Value::Str(ds.name.clone()),
                "state_version" => Value::Int(ds.state_version),
                "reconciled_by" => opt_str(&ds.reconciled_by),
                "importance" => Value::Float(ds.importance),
                "spec" => Value::Str(ds.spec.clone()),
                "status" => Value::Str(ds.status.clone()),
                _ => match subject {
                    Some(node) => node_field(node, field),
                    None => Value::Null,
                },
            },
            Row::History { event } => match field {
                "id" => Value::Str(event.id.clone()),
                "vault_id" => Value::Str(event.vault_id.clone()),
                "ts" => Value::Int(event.ts),
                "actor" => Value::Str(event.actor.clone()),
                "type" => Value::Str(event.event_type.clone()),
                "caused_by" => opt_str(&event.caused_by),
                "reconciles" => opt_str(&event.reconciles),
                "supersedes" => opt_str(&event.supersedes),
                _ => Value::Null,
            },
        }
    }

    pub fn project(&self, fields: &[String]) -> Row {
        let source = match self {
            Row::Selected { source, .. } => source.as_ref(),
            other => other,
        };
        let columns = fields
            .iter()
            .map(|field| (field.clone(), source.get(field)))
            .collect();
        Row::Selected {
            columns,
            source: Box::new(source.clone()),
        }
    }

    /// Columns shown when nothing was selected.
    pub fn default_fields(&self) -> &'static [&'static str] {
        match self {
            Row::History { .. } => default_history_fields(),
            Row::State { .. } => default_state_fields(),
            Row::Selected { source, .. } => source.default_fields(),
            _ => default_output_fields(),
        }
    }

    pub fn as_dict(&self, fields: Option<&[String]>) -> Vec<(String, Value)> {
        if let Row::Selected { columns, .. } = self {
            return columns.clone();
        }
        let owned: Vec<String> = match fields {
            Some(f) => f.to_vec(),
            None => self.default_fields().iter().map(|s| (*s).to_string()).collect(),
        };
        owned
            .into_iter()
            .map(|field| {
                let value = self.get(&field);
                (field, value)
            })
            .collect()
    }

    /// What `search` scans: the node's path + extra, plus a walk's edge text.
    pub fn searchable_text(&self) -> String {
        let (to_raw, properties) = match self {
            Row::Walk { edge, .. } => (edge.to_raw.as_deref().unwrap_or(""), edge.properties.as_str()),
            Row::Selected { source, .. } => return source.searchable_text(),
            _ => ("", ""),
        };
        let (path, extra) = match self.node() {
            Some(node) => (node.path.as_deref().unwrap_or(""), node.extra.as_str()),
            None => ("", ""),
        };
        format!("{path}\n{extra}\n{to_raw}\n{properties}")
    }
}

fn node_field(node: &NodeView, field: &str) -> Value {
    if let Some(key) = field.strip_prefix("extra.") {
        return match node.extra_map.get(key) {
            Some(Some(s)) => Value::Str(s.clone()),
            Some(None) | None => Value::Null,
        };
    }
    match field {
        "path" => opt_str(&node.path),
        "id" => Value::Str(node.id.clone()),
        "vault_id" => Value::Str(node.vault_id.clone()),
        "node_type" => Value::Str(node.node_type.clone()),
        _ => Value::Null,
    }
}

pub fn default_output_fields() -> &'static [&'static str] {
    &["path", "from.path", "to.path", "to_id", "to_raw", "from_id"]
}

/// Warm default columns. Identity + version; spec / status only on request.
pub fn default_state_fields() -> &'static [&'static str] {
    &[
        "path",
        "name",
        "state_version",
        "reconciled_by",
        "importance",
        "id",
    ]
}

/// Cool-path default columns. Event metadata only; no spec/status/data.
pub fn default_history_fields() -> &'static [&'static str] {
    &[
        "id",
        "ts",
        "actor",
        "type",
        "caused_by",
        "reconciles",
        "supersedes",
    ]
}

fn opt_str(value: &Option<String>) -> Value {
    match value {
        Some(s) => Value::Str(s.clone()),
        None => Value::Null,
    }
}
