use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};

pub const CAUSAL_CAUSED_BY: &str = "caused_by";
pub const CAUSAL_RECONCILES: &str = "reconciles";
pub const CAUSAL_SUPERSEDES: &str = "supersedes";
pub const EDGE_GRANT: &str = "grant";

const SECRET_KEYS: &[&str] = &["token", "api_key", "secret", "password", "authorization"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeType {
    Agent,
    Document,
    Vault,
}

impl NodeType {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeType::Agent => "Agent",
            NodeType::Document => "Document",
            NodeType::Vault => "Vault",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "Agent" => Ok(NodeType::Agent),
            "Document" => Ok(NodeType::Document),
            "Vault" => Ok(NodeType::Vault),
            other => Err(Error::Invalid(format!("unknown node type {other}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Hot,
    Warm,
    Cool,
    Cold,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Hot => "hot",
            Tier::Warm => "warm",
            Tier::Cool => "cool",
            Tier::Cold => "cold",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "hot" => Ok(Tier::Hot),
            "warm" => Ok(Tier::Warm),
            "cool" => Ok(Tier::Cool),
            "cold" => Ok(Tier::Cold),
            other => Err(Error::Invalid(format!("unknown tier {other}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: Uuid,
    pub vault_id: Uuid,
    pub node_type: NodeType,
    pub content_hash: String,
    pub path: Option<String>,
    pub version: u64,
    pub tier: Tier,
    pub importance: f64,
    pub htec_path: Option<String>,
    pub extra: serde_yaml::Value,
}

impl Node {
    pub fn document(vault_id: Uuid, path: Option<&str>, extra: serde_yaml::Value) -> Result<Self> {
        reject_secrets(&extra)?;
        let node_type = NodeType::Document;
        let content_hash = content_hash(node_type, path, None, &extra);
        Ok(Self {
            id: Uuid::new_v4(),
            vault_id,
            node_type,
            content_hash,
            path: path.map(str::to_string),
            version: 1,
            tier: Tier::Warm,
            importance: 0.5,
            htec_path: None,
            extra,
        })
    }

    pub fn brief_document(vault_id: Uuid, name: &str, date: &str) -> Result<Self> {
        let extra = serde_yaml::to_value(serde_yaml::Mapping::from_iter([
            (
                serde_yaml::Value::String("brief".into()),
                serde_yaml::Value::String(name.into()),
            ),
            (
                serde_yaml::Value::String("date".into()),
                serde_yaml::Value::String(date.into()),
            ),
        ]))?;
        Self::document(vault_id, Some(&format!("briefs/{date}/{name}")), extra)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub id: Uuid,
    pub vault_id: Uuid,
    pub from: Uuid,
    pub to_id: Option<Uuid>,
    pub to_raw: Option<String>,
    pub edge_type: String,
    pub properties: serde_yaml::Value,
}

impl Edge {
    pub fn new(
        vault_id: Uuid,
        from: Uuid,
        to_id: Option<Uuid>,
        to_raw: Option<String>,
        edge_type: impl Into<String>,
    ) -> Result<Self> {
        let to_raw = to_raw.filter(|s| !s.is_empty());
        if to_id.is_none() && to_raw.is_none() {
            return Err(Error::Invalid(
                "edge target needs a dest UUID, a raw string, or both".into(),
            ));
        }
        Ok(Self {
            id: Uuid::new_v4(),
            vault_id,
            from,
            to_id,
            to_raw,
            edge_type: edge_type.into(),
            properties: serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConditionKind {
    Reconciled,
    Pending,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    #[serde(rename = "type")]
    pub kind: ConditionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub observed: serde_yaml::Value,
    pub conditions: Vec<Condition>,
}

impl Status {
    pub fn empty() -> Self {
        Self {
            observed: serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
            conditions: Vec::new(),
        }
    }
}

/// `kind: docs_eod` spec body: named briefs that should exist for a date.
/// `kind` itself is read by the dispatcher, not by this struct.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocsEodSpec {
    pub date: String,
    pub required_briefs: Vec<String>,
}

/// Expected path counts for `kind: curriculum_clock`.
/// Freeze key is `ship_note` (LapisObserveEmit / ObserveResult).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CurriculumExpected {
    pub quiz_html: u64,
    pub lab_refs: u64,
    pub ship_note: u64,
}

/// One vault-relative path the clock requires, with its count role.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CurriculumPath {
    pub path: String,
    pub role: String,
}

/// Optional globs, one pattern per role. `*` stays in a segment; `**` crosses `/`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CurriculumGlobs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiz_html: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lab_refs: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ship_note: Option<String>,
}

impl CurriculumGlobs {
    pub fn is_empty(&self) -> bool {
        self.quiz_html.is_none() && self.lab_refs.is_none() && self.ship_note.is_none()
    }
}

/// `kind: curriculum_clock` spec body. `kind` is read by the dispatcher.
///
/// `check_at` is stored for a later derived pending-vs-gap view. Observe does
/// not read the wall clock. Sibling store file for a Maghrib day is
/// `maghrib-YYYY-MM-DD.db` (mode 0600); never overwrite an `eod-*.db`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CurriculumClockSpec {
    pub date: String,
    pub clock: String,
    pub expected: CurriculumExpected,
    #[serde(default)]
    pub required_paths: Vec<CurriculumPath>,
    #[serde(default, skip_serializing_if = "CurriculumGlobs::is_empty")]
    pub globs: CurriculumGlobs,
    /// ISO date-time. Not compared to the host clock in observe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DesiredState {
    pub id: Uuid,
    pub vault_id: Uuid,
    /// Unique per vault; the id is stable across spec replacements.
    pub name: String,
    pub state_version: u64,
    pub content_hash: String,
    pub last_reconciled: Option<i64>,
    pub reconciled_by: Option<Uuid>,
    pub importance: f64,
    pub spec: serde_yaml::Value,
    pub status: Status,
}

impl DesiredState {
    /// A `kind: docs_eod` spec for `date` requiring `required_briefs`.
    pub fn docs_eod_spec(date: &str, required_briefs: &[&str]) -> Result<serde_yaml::Value> {
        let body = DocsEodSpec {
            date: date.to_string(),
            required_briefs: required_briefs.iter().map(|s| (*s).to_string()).collect(),
        };
        let mut spec = serde_yaml::to_value(body)?;
        if let Some(map) = spec.as_mapping_mut() {
            map.insert(
                serde_yaml::Value::String("kind".into()),
                serde_yaml::Value::String(crate::reconcile::DOCS_EOD_KIND.into()),
            );
        }
        Ok(spec)
    }

    /// A `kind: curriculum_clock` spec. Path/count only — no secrets, no grades.
    /// `required_paths` entries are `(path, role)` with role
    /// `quiz_html` | `lab_refs` | `ship_note`. Role counts must match `expected`.
    pub fn curriculum_clock_spec(
        date: &str,
        clock: &str,
        quiz_html: u64,
        lab_refs: u64,
        ship_note: u64,
        required_paths: &[(&str, &str)],
        check_at: Option<&str>,
    ) -> Result<serde_yaml::Value> {
        let body = CurriculumClockSpec {
            date: date.to_string(),
            clock: clock.to_string(),
            expected: CurriculumExpected {
                quiz_html,
                lab_refs,
                ship_note,
            },
            required_paths: required_paths
                .iter()
                .map(|(path, role)| CurriculumPath {
                    path: (*path).to_string(),
                    role: (*role).to_string(),
                })
                .collect(),
            globs: CurriculumGlobs::default(),
            check_at: check_at.map(str::to_string),
        };
        let mut spec = serde_yaml::to_value(body)?;
        if let Some(map) = spec.as_mapping_mut() {
            map.insert(
                serde_yaml::Value::String("kind".into()),
                serde_yaml::Value::String(crate::reconcile::CURRICULUM_CLOCK_KIND.into()),
            );
        }
        crate::reconcile::parse_curriculum_clock_spec(&spec)?;
        Ok(spec)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: Uuid,
    pub vault_id: Uuid,
    pub ts: i64,
    pub actor: Uuid,
    pub event_type: String,
    pub data: serde_yaml::Value,
    pub caused_by: Vec<Uuid>,
    pub reconciles: Option<Uuid>,
    pub supersedes: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Bootstrap {
    pub vault: Node,
    pub agent: Node,
    pub token: String,
}

pub(crate) fn is_causal_type(edge_type: &str) -> bool {
    matches!(
        edge_type,
        CAUSAL_CAUSED_BY | CAUSAL_RECONCILES | CAUSAL_SUPERSEDES
    )
}

pub(crate) fn reject_secrets(extra: &serde_yaml::Value) -> Result<()> {
    let Some(map) = extra.as_mapping() else {
        return Ok(());
    };
    for key in map.keys() {
        let Some(name) = key.as_str() else {
            continue;
        };
        if SECRET_KEYS
            .iter()
            .any(|forbidden| name.eq_ignore_ascii_case(forbidden))
        {
            return Err(Error::Invalid(
                "secrets must not be stored in YAML/frontmatter".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn content_hash(
    node_type: NodeType,
    path: Option<&str>,
    htec_path: Option<&str>,
    extra: &serde_yaml::Value,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(node_type.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(path.unwrap_or("").as_bytes());
    hasher.update(b"\0");
    hasher.update(htec_path.unwrap_or("").as_bytes());
    hasher.update(b"\0");
    if let Ok(blob) = serde_yaml::to_string(extra) {
        hasher.update(blob.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

pub(crate) fn desired_state_hash(
    spec: &serde_yaml::Value,
    status: &Status,
    state_version: u64,
) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(serde_yaml::to_string(spec)?.as_bytes());
    hasher.update(b"\0");
    hasher.update(serde_yaml::to_string(status)?.as_bytes());
    hasher.update(b"\0");
    hasher.update(state_version.to_string().as_bytes());
    Ok(hasher.finalize().to_hex().to_string())
}

pub(crate) fn version_ref(id: Uuid, state_version: u64) -> String {
    format!("{id}@{state_version}")
}

pub(crate) fn validate_importance(importance: f64) -> Result<()> {
    if (0.0..=1.0).contains(&importance) {
        Ok(())
    } else {
        Err(Error::Invalid("importance must be in 0..=1".into()))
    }
}
