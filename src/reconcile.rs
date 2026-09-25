//! Kind-dispatched reconcilers.
//!
//! `Store` persists, isolates, versions and appends events. A `Reconciler`
//! only turns the vault's documents plus a spec into an observation. Which
//! impl runs is decided by `spec.kind`; a new kind is a new impl here, not a
//! branch inside `Store`.
//!
//! `Observation.missing` is the source of truth. `ConditionKind::Reconciled`
//! means that array is empty (warm). HQL `status=gap` is a derived view over
//! a nonempty `missing` and is not stored here. Jev is not called from this
//! module and must not invent `missing`.

use serde::Serialize;
use serde_yaml::Value;
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::types::{
    Condition, ConditionKind, CurriculumClockSpec, DocsEodSpec, Node, NodeType, Status,
};

/// `spec.kind` handled by [`DocsEod`].
pub const DOCS_EOD_KIND: &str = "docs_eod";

/// `spec.kind` handled by [`CurriculumClock`].
pub const CURRICULUM_CLOCK_KIND: &str = "curriculum_clock";

const ROLES: [&str; 4] = ["quiz_html", "lab_refs", "ship_note", "lesson_md"];

/// What a reconciler saw: the status to persist and the document ids that
/// produced it. `Store::reconcile` projects `caused_by` onto the event row.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub status: Status,
    pub caused_by: Vec<Uuid>,
}

pub trait Reconciler: Sync {
    fn kind(&self) -> &'static str;

    /// Observe `vault_docs` against `spec`. Pure: no store access, no clock.
    fn observe(&self, vault_docs: &[Node], spec: &Value) -> Result<Observation>;
}

/// Phase 0 reconciler: named briefs that should exist for a date.
/// Matches `Document` nodes on `extra.brief` + `extra.date`.
pub struct DocsEod;

impl Reconciler for DocsEod {
    fn kind(&self) -> &'static str {
        DOCS_EOD_KIND
    }

    fn observe(&self, vault_docs: &[Node], spec: &Value) -> Result<Observation> {
        let parsed: DocsEodSpec = serde_yaml::from_value(spec.clone())
            .map_err(|err| Error::Invalid(format!("{DOCS_EOD_KIND} spec: {err}")))?;

        let mut found: Vec<(Uuid, &str)> = Vec::new();
        for doc in vault_docs {
            if doc.node_type != NodeType::Document {
                continue;
            }
            let Some(brief) = doc.extra.get("brief").and_then(Value::as_str) else {
                continue;
            };
            if doc.extra.get("date").and_then(Value::as_str) != Some(parsed.date.as_str()) {
                continue;
            }
            if parsed.required_briefs.iter().any(|name| name == brief) {
                found.push((doc.id, brief));
            }
        }

        let present: Vec<String> = parsed
            .required_briefs
            .iter()
            .filter(|name| found.iter().any(|(_, brief)| brief == name))
            .cloned()
            .collect();
        let missing: Vec<String> = parsed
            .required_briefs
            .iter()
            .filter(|name| !present.contains(name))
            .cloned()
            .collect();

        let (kind, message) = if missing.is_empty() {
            (
                ConditionKind::Reconciled,
                Some("all required briefs are present".into()),
            )
        } else {
            (
                ConditionKind::Pending,
                Some(format!("missing briefs: {}", missing.join(", "))),
            )
        };

        let observed = serde_yaml::to_value(serde_yaml::Mapping::from_iter([
            (
                Value::String("present".into()),
                serde_yaml::to_value(&present)?,
            ),
            (
                Value::String("missing".into()),
                serde_yaml::to_value(&missing)?,
            ),
        ]))?;

        Ok(Observation {
            status: Status {
                observed,
                conditions: vec![Condition { kind, message }],
            },
            caused_by: found.into_iter().map(|(id, _)| id).collect(),
        })
    }
}

static RECONCILERS: &[&dyn Reconciler] = &[&DocsEod, &CurriculumClock];

/// The reconciler for `spec.kind`. Missing or unknown kind is `Invalid`.
pub fn reconciler_for(spec: &Value) -> Result<&'static dyn Reconciler> {
    let kind = spec
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Invalid("desired state spec requires kind: <string>".into()))?;
    RECONCILERS
        .iter()
        .copied()
        .find(|r| r.kind() == kind)
        .ok_or_else(|| Error::Invalid(format!("unknown desired state kind {kind:?}")))
}

// --- curriculum_clock -------------------------------------------------------
// Path/count observe for one clock label. Does not open a database and does
// not schedule clocks. A Maghrib day's file is the sibling `maghrib-YYYY-MM-DD.db`
// (mode 0600); never overwrite an `eod-*.db`. This kind stays off `docs_eod`.

/// Curriculum path/count reconciler. Matches vault document paths and embedded
/// LapisObserveEmit / ObserveResult documents. `missing` is computed here.
pub struct CurriculumClock;

impl Reconciler for CurriculumClock {
    fn kind(&self) -> &'static str {
        CURRICULUM_CLOCK_KIND
    }

    fn observe(&self, vault_docs: &[Node], spec: &Value) -> Result<Observation> {
        let parsed = parse_curriculum_clock_spec(spec)?;
        let mut facts = Vec::new();
        for doc in vault_docs {
            if doc.node_type != NodeType::Document {
                continue;
            }
            if looks_like_emit(&doc.extra) {
                reject_nested_secrets(&doc.extra)?;
                if emit_date_ok(&doc.extra, &parsed.date) {
                    for (path, role) in collect_emit_hits(&doc.extra)? {
                        facts.push(Fact {
                            path,
                            role,
                            source: Some(doc.id),
                        });
                    }
                }
                continue;
            }
            if let Some(date) = doc.extra.get("date").and_then(Value::as_str) {
                if date != parsed.date {
                    continue;
                }
            }
            if let Some(path) = doc.path.clone() {
                let role = doc
                    .extra
                    .get("role")
                    .and_then(Value::as_str)
                    .filter(|role| role_ok(role))
                    .map(str::to_string);
                facts.push(Fact {
                    path,
                    role,
                    source: Some(doc.id),
                });
            }
        }
        observe_facts(&parsed, &facts)
    }
}

/// Fold a LapisObserveEmit or a bare ObserveResult into an [`Observation`].
///
/// Present paths and evidence are inputs. `missing` is recomputed against
/// `spec`. Emit `status`, `missing`, and `counts` are not copied.
pub fn adapt_lapis_observe(spec: &Value, emit: &Value) -> Result<Observation> {
    let parsed = parse_curriculum_clock_spec(spec)?;
    reject_nested_secrets(emit)?;
    if !looks_like_emit(emit) {
        return Err(Error::Invalid(format!(
            "{CURRICULUM_CLOCK_KIND} observe input must be a LapisObserveEmit or ObserveResult"
        )));
    }
    if !emit_date_ok(emit, &parsed.date) {
        return Err(Error::Invalid(format!(
            "{CURRICULUM_CLOCK_KIND} emit date does not match spec date {}",
            parsed.date
        )));
    }
    let facts = collect_emit_hits(emit)?
        .into_iter()
        .map(|(path, role)| Fact {
            path,
            role,
            source: None,
        })
        .collect::<Vec<_>>();
    observe_facts(&parsed, &facts)
}

/// Parse and check a `curriculum_clock` spec. `kind` is ignored here.
pub(crate) fn parse_curriculum_clock_spec(spec: &Value) -> Result<CurriculumClockSpec> {
    reject_nested_secrets(spec)?;
    let parsed: CurriculumClockSpec = serde_yaml::from_value(spec.clone())
        .map_err(|err| Error::Invalid(format!("{CURRICULUM_CLOCK_KIND} spec: {err}")))?;
    validate_curriculum_clock_spec(&parsed)?;
    Ok(parsed)
}

struct Fact {
    path: String,
    role: Option<String>,
    source: Option<Uuid>,
}

#[derive(Serialize)]
struct CountPair {
    expected: u64,
    actual: u64,
}

#[derive(Serialize)]
struct ObservedCounts {
    quiz_html: CountPair,
    lab_refs: CountPair,
    ship_note: CountPair,
    lesson_md: CountPair,
}

#[derive(Serialize)]
struct ObservedBody {
    present: Vec<String>,
    missing: Vec<String>,
    counts: ObservedCounts,
}

fn observe_facts(spec: &CurriculumClockSpec, facts: &[Fact]) -> Result<Observation> {
    if spec.required_paths.is_empty() {
        return observe_globs(spec, facts);
    }
    let mut present = Vec::new();
    let mut missing = Vec::new();
    let mut caused_by = Vec::new();
    for req in &spec.required_paths {
        if let Some(hit) = facts.iter().find(|fact| fact.path == req.path) {
            present.push(req.path.clone());
            push_source(&mut caused_by, hit.source);
        } else {
            missing.push(req.path.clone());
        }
    }
    let actuals = role_actuals(spec, &present);
    finish(spec, present, missing, caused_by, actuals)
}

fn observe_globs(spec: &CurriculumClockSpec, facts: &[Fact]) -> Result<Observation> {
    let mut present = Vec::new();
    let mut caused_by = Vec::new();
    let mut actuals = [0u64; ROLES.len()];
    for fact in facts {
        let role = fact
            .role
            .as_deref()
            .filter(|role| role_ok(role))
            .or_else(|| glob_role(spec, &fact.path));
        let Some(role) = role else {
            continue;
        };
        if present.iter().any(|path| path == &fact.path) {
            continue;
        }
        present.push(fact.path.clone());
        push_source(&mut caused_by, fact.source);
        actuals[role_index(role)] += 1;
    }
    let mut missing = Vec::new();
    for (idx, role) in ROLES.iter().enumerate() {
        let expected = expected_of(spec, role);
        if actuals[idx] < expected {
            let label = glob_label(spec, role).unwrap_or(role);
            for _ in 0..(expected - actuals[idx]) {
                missing.push(label.to_string());
            }
        }
    }
    finish(spec, present, missing, caused_by, actuals)
}

fn finish(
    spec: &CurriculumClockSpec,
    present: Vec<String>,
    missing: Vec<String>,
    caused_by: Vec<Uuid>,
    actuals: [u64; ROLES.len()],
) -> Result<Observation> {
    let (kind, message) = if missing.is_empty() {
        (
            ConditionKind::Reconciled,
            Some("all required paths are present".into()),
        )
    } else {
        (
            ConditionKind::Pending,
            Some(format!("missing paths: {}", missing.join(", "))),
        )
    };
    let observed = serde_yaml::to_value(ObservedBody {
        present,
        missing,
        counts: ObservedCounts {
            quiz_html: count_pair(spec, &actuals, ROLES[0]),
            lab_refs: count_pair(spec, &actuals, ROLES[1]),
            ship_note: count_pair(spec, &actuals, ROLES[2]),
            lesson_md: count_pair(spec, &actuals, ROLES[3]),
        },
    })?;
    Ok(Observation {
        status: Status {
            observed,
            conditions: vec![Condition { kind, message }],
        },
        caused_by,
    })
}

fn validate_curriculum_clock_spec(spec: &CurriculumClockSpec) -> Result<()> {
    if !is_iso_date(&spec.date) {
        return Err(Error::Invalid(format!(
            "{CURRICULUM_CLOCK_KIND} spec: date must be YYYY-MM-DD"
        )));
    }
    if spec.clock.trim().is_empty() {
        return Err(Error::Invalid(format!(
            "{CURRICULUM_CLOCK_KIND} spec: clock is required"
        )));
    }
    if let Some(check_at) = &spec.check_at {
        if check_at.len() < 16 || !check_at.contains('T') {
            return Err(Error::Invalid(format!(
                "{CURRICULUM_CLOCK_KIND} spec: check_at must be an ISO date-time"
            )));
        }
    }
    if spec.required_paths.is_empty() && spec.globs.is_empty() {
        return Err(Error::Invalid(format!(
            "{CURRICULUM_CLOCK_KIND} spec: required_paths or globs is required"
        )));
    }
    let mut seen = Vec::new();
    for req in &spec.required_paths {
        if req.path.is_empty() {
            return Err(Error::Invalid(format!(
                "{CURRICULUM_CLOCK_KIND} spec: required path is empty"
            )));
        }
        if !role_ok(&req.role) {
            return Err(Error::Invalid(format!(
                "{CURRICULUM_CLOCK_KIND} spec: unknown role {}",
                req.role
            )));
        }
        if seen.iter().any(|path| *path == &req.path) {
            return Err(Error::Invalid(format!(
                "{CURRICULUM_CLOCK_KIND} spec: duplicate path {}",
                req.path
            )));
        }
        seen.push(&req.path);
    }
    if !spec.required_paths.is_empty() {
        for role in ROLES {
            let got = spec
                .required_paths
                .iter()
                .filter(|path| path.role == role)
                .count() as u64;
            let expected = expected_of(spec, role);
            if got != expected {
                return Err(Error::Invalid(format!(
                    "{CURRICULUM_CLOCK_KIND} spec: expected.{role} is {expected} but required_paths has {got}"
                )));
            }
        }
    }
    for (label, pat) in [
        ("quiz_html", spec.globs.quiz_html.as_deref()),
        ("lab_refs", spec.globs.lab_refs.as_deref()),
        ("ship_note", spec.globs.ship_note.as_deref()),
        ("lesson_md", spec.globs.lesson_md.as_deref()),
    ] {
        if let Some(pat) = pat {
            if pat.is_empty() {
                return Err(Error::Invalid(format!(
                    "{CURRICULUM_CLOCK_KIND} spec: globs.{label} is empty"
                )));
            }
        }
    }
    Ok(())
}

fn collect_emit_hits(emit: &Value) -> Result<Vec<(String, Option<String>)>> {
    let mut denied = Vec::new();
    let mut hits = Vec::new();
    if let Some(items) = evidence_items(emit) {
        for item in items {
            let Some(path) = item.get("path").and_then(Value::as_str) else {
                return Err(Error::Invalid(format!(
                    "{CURRICULUM_CLOCK_KIND} evidence path must be a string"
                )));
            };
            if path.is_empty() {
                return Err(Error::Invalid(format!(
                    "{CURRICULUM_CLOCK_KIND} evidence path must be a string"
                )));
            }
            let is_present = item.get("present").and_then(Value::as_bool).unwrap_or(true);
            if !is_present {
                push_unique(&mut denied, path);
                continue;
            }
            let role = item
                .get("role")
                .and_then(Value::as_str)
                .filter(|role| role_ok(role))
                .map(str::to_string);
            if !denied.iter().any(|item| item == path) {
                push_hit(&mut hits, path, role);
            }
        }
    }
    for seq in present_sequences(emit) {
        for item in seq {
            let Some(path) = item.as_str() else {
                return Err(Error::Invalid(format!(
                    "{CURRICULUM_CLOCK_KIND} present entries must be strings"
                )));
            };
            if path.is_empty() || denied.iter().any(|item| item == path) {
                continue;
            }
            push_hit(&mut hits, path, None);
        }
    }
    Ok(hits)
}

fn looks_like_emit(value: &Value) -> bool {
    if value.get("observed").and_then(Value::as_mapping).is_some() {
        return true;
    }
    if value
        .get("observeResult")
        .and_then(Value::as_mapping)
        .is_some()
    {
        return true;
    }
    value.get("present").and_then(Value::as_sequence).is_some()
        && (value.get("subject").is_some() || value.get("status").is_some())
}

fn emit_date_ok(emit: &Value, spec_date: &str) -> bool {
    match emit.get("date").and_then(Value::as_str) {
        Some(date) => date == spec_date,
        None => true,
    }
}

fn evidence_items(emit: &Value) -> Option<&serde_yaml::Sequence> {
    if let Some(items) = emit.get("evidence").and_then(Value::as_sequence) {
        return Some(items);
    }
    emit.get("observeResult")
        .and_then(|value| value.get("evidence"))
        .and_then(Value::as_sequence)
}

fn present_sequences(emit: &Value) -> Vec<&serde_yaml::Sequence> {
    let mut out = Vec::new();
    if let Some(observed) = emit.get("observed") {
        if let Some(seq) = observed.get("present").and_then(Value::as_sequence) {
            out.push(seq);
        }
    }
    if let Some(result) = emit.get("observeResult") {
        if let Some(seq) = result.get("present").and_then(Value::as_sequence) {
            out.push(seq);
        }
    }
    if emit.get("observed").is_none() {
        if let Some(seq) = emit.get("present").and_then(Value::as_sequence) {
            out.push(seq);
        }
    }
    out
}

fn push_hit(hits: &mut Vec<(String, Option<String>)>, path: &str, role: Option<String>) {
    if let Some(existing) = hits.iter_mut().find(|(seen, _)| seen == path) {
        if existing.1.is_none() {
            existing.1 = role;
        }
        return;
    }
    hits.push((path.to_string(), role));
}

fn push_unique(out: &mut Vec<String>, path: &str) {
    if !out.iter().any(|seen| seen == path) {
        out.push(path.to_string());
    }
}

fn push_source(out: &mut Vec<Uuid>, source: Option<Uuid>) {
    if let Some(id) = source {
        if !out.contains(&id) {
            out.push(id);
        }
    }
}

fn count_role(spec: &CurriculumClockSpec, present: &[String], role: &str) -> u64 {
    spec.required_paths
        .iter()
        .filter(|path| path.role == role && present.iter().any(|seen| seen == &path.path))
        .count() as u64
}

fn expected_of(spec: &CurriculumClockSpec, role: &str) -> u64 {
    match role {
        "quiz_html" => spec.expected.quiz_html,
        "lab_refs" => spec.expected.lab_refs,
        "ship_note" => spec.expected.ship_note,
        "lesson_md" => spec.expected.lesson_md,
        _ => 0,
    }
}

fn role_actuals(spec: &CurriculumClockSpec, present: &[String]) -> [u64; ROLES.len()] {
    let mut actuals = [0u64; ROLES.len()];
    for (idx, role) in ROLES.iter().enumerate() {
        actuals[idx] = count_role(spec, present, role);
    }
    actuals
}

fn count_pair(spec: &CurriculumClockSpec, actuals: &[u64], role: &str) -> CountPair {
    CountPair {
        expected: expected_of(spec, role),
        actual: actuals[role_index(role)],
    }
}

fn role_ok(role: &str) -> bool {
    ROLES.contains(&role)
}

fn role_index(role: &str) -> usize {
    ROLES.iter().position(|item| *item == role).unwrap_or(0)
}

fn glob_role<'a>(spec: &'a CurriculumClockSpec, path: &str) -> Option<&'a str> {
    let pairs = [
        (spec.globs.quiz_html.as_deref(), ROLES[0]),
        (spec.globs.lab_refs.as_deref(), ROLES[1]),
        (spec.globs.ship_note.as_deref(), ROLES[2]),
        (spec.globs.lesson_md.as_deref(), ROLES[3]),
    ];
    pairs.into_iter().find_map(|(pat, role)| {
        pat.filter(|pattern| glob_match(pattern, path))
            .map(|_| role)
    })
}

fn glob_label<'a>(spec: &'a CurriculumClockSpec, role: &str) -> Option<&'a str> {
    match role {
        "quiz_html" => spec.globs.quiz_html.as_deref(),
        "lab_refs" => spec.globs.lab_refs.as_deref(),
        "ship_note" => spec.globs.ship_note.as_deref(),
        "lesson_md" => spec.globs.lesson_md.as_deref(),
        _ => None,
    }
}

fn is_iso_date(date: &str) -> bool {
    let bytes = date.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(idx, byte)| idx == 4 || idx == 7 || byte.is_ascii_digit())
}

fn glob_match(pattern: &str, text: &str) -> bool {
    glob_rec(pattern.as_bytes(), text.as_bytes())
}

fn glob_rec(pat: &[u8], text: &[u8]) -> bool {
    let mut pi = 0;
    let mut ti = 0;
    while pi < pat.len() {
        if pat[pi] == b'*' {
            let double = pi + 1 < pat.len() && pat[pi + 1] == b'*';
            if double {
                pi += 2;
                if pi < pat.len() && pat[pi] == b'/' {
                    pi += 1;
                }
                if pi == pat.len() {
                    return true;
                }
                let rest = &pat[pi..];
                for skip in 0..=(text.len() - ti) {
                    if glob_rec(rest, &text[ti + skip..]) {
                        return true;
                    }
                }
                return false;
            }
            pi += 1;
            if pi == pat.len() {
                return !text[ti..].contains(&b'/');
            }
            let rest = &pat[pi..];
            for skip in 0..=(text.len() - ti) {
                if skip > 0 && text[ti + skip - 1] == b'/' {
                    break;
                }
                if glob_rec(rest, &text[ti + skip..]) {
                    return true;
                }
            }
            return false;
        }
        if ti >= text.len() || pat[pi] != text[ti] {
            return false;
        }
        pi += 1;
        ti += 1;
    }
    ti == text.len()
}

fn reject_nested_secrets(value: &Value) -> Result<()> {
    match value {
        Value::Mapping(map) => {
            for (key, child) in map {
                if let Some(name) = key.as_str() {
                    if is_secret_key(name) {
                        return Err(Error::Invalid(
                            "secrets must not be stored in YAML/frontmatter".into(),
                        ));
                    }
                }
                reject_nested_secrets(child)?;
            }
            Ok(())
        }
        Value::Sequence(items) => {
            for child in items {
                reject_nested_secrets(child)?;
            }
            Ok(())
        }
        Value::Tagged(tagged) => reject_nested_secrets(&tagged.value),
        _ => Ok(()),
    }
}

fn is_secret_key(name: &str) -> bool {
    const SECRET_KEYS: &[&str] = &["token", "api_key", "secret", "password", "authorization"];
    SECRET_KEYS
        .iter()
        .any(|forbidden| name.eq_ignore_ascii_case(forbidden))
}
