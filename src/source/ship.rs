//! Intended schedule versus lattice hits.
//!
//! `missing` is copied from `adapt_lapis_observe`. This module does not build
//! a second missing list. Stale and cannot-tell lanes are not reconciled.

use serde::Serialize;
use serde_yaml::Value;

use crate::error::{Error, Result};
use crate::reconcile::adapt_lapis_observe;
use crate::source::guard::{judge_freshness, AbsenceCheck, GuardVerdict};
use crate::source::lattice::{IndexFreshness, LatticeSource};
use crate::source::manifest::{
    close_note_path, is_superseded_path, lab_ref_path, parse_manifest, IntendedBundle,
};
use crate::source::register_log::RegisterLog;
use crate::source::time::{format_unix_utc, parse_timestamp, SCOPE_START};
use crate::store::Store;
use crate::types::{DesiredState, LessonClockSpec};

const REASON_MISSING_PATH: &str = "missing_path";
const REASON_CLOSE_NOTE: &str = "close_note_missing";
const REASON_MANIFEST_ROW: &str = "manifest_row_missing";
const LESSON_SHIP_IMPORTANCE: f64 = 0.5;

/// One due lane. Slice 2 will fill this from Fire Watch; slice 1 takes it as data.
#[derive(Debug, Clone)]
pub struct LaneDue {
    pub date: String,
    pub lane: String,
    pub check_at: Option<String>,
    /// SQLite GLOB used only when the manifest has no row for this lane-day.
    pub lesson_glob: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShipRequirement {
    pub path: String,
    pub role: String,
    pub reason: String,
    /// Lesson and lab paths only. A close note ignores this and uses `check_at`.
    pub landed_hint: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PathReason {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LaneReport {
    pub date: String,
    pub lane: String,
    pub subject: String,
    pub status: String,
    pub shipped: Vec<String>,
    pub missing: Vec<String>,
    pub stale: Vec<String>,
    pub cannot_tell: Option<String>,
    pub reasons: Vec<PathReason>,
    pub watermark_utc: Option<String>,
    pub counts: Option<serde_json::Value>,
}

struct ReconcilePlan {
    spec: Value,
    emit: Value,
}

#[derive(Serialize)]
pub struct ObserveBatch {
    pub watermark_utc: Option<String>,
    pub last_reconcile_at: Option<String>,
    pub last_full_pass_at: Option<String>,
    pub last_indexer_at: Option<String>,
    pub document_count: Option<i64>,
    pub live_count: Option<i64>,
    pub lanes: Vec<LaneReport>,
    #[serde(skip)]
    plans: Vec<Option<ReconcilePlan>>,
}

impl ObserveBatch {
    /// Persist warm and gap lanes. Stale, pending, cannot-tell, and
    /// out-of-scope lanes are left untouched.
    pub fn persist(&self, store: &mut Store, token: &str) -> Result<()> {
        for (report, plan) in self.lanes.iter().zip(self.plans.iter()) {
            let Some(plan) = plan else {
                continue;
            };
            if report.status != "warm" && report.status != "gap" {
                continue;
            }
            let ds = store.put_desired_state(
                token,
                &report.subject,
                plan.spec.clone(),
                LESSON_SHIP_IMPORTANCE,
            )?;
            store.reconcile_observed(token, ds.id, &plan.emit)?;
        }
        Ok(())
    }
}

pub fn cannot_tell_batch(lanes: &[LaneDue], err: &Error) -> ObserveBatch {
    let detail = err.to_string();
    let mut reports = Vec::with_capacity(lanes.len());
    for lane in lanes {
        if in_scope(&lane.date) {
            reports.push(cannot_tell_report(lane, &detail, None));
        } else {
            reports.push(not_evaluated(lane));
        }
    }
    ObserveBatch {
        watermark_utc: None,
        last_reconcile_at: None,
        last_full_pass_at: None,
        last_indexer_at: None,
        document_count: None,
        live_count: None,
        lanes: reports,
        plans: std::iter::repeat_with(|| None).take(lanes.len()).collect(),
    }
}

pub fn observe_lesson_ships(
    source: &LatticeSource,
    manifest_yaml: &str,
    lanes: &[LaneDue],
    as_of_unix: Option<i64>,
    register_log: Option<&str>,
) -> Result<ObserveBatch> {
    let freshness = source.freshness()?;
    let bundles = parse_manifest(manifest_yaml)?;
    let register_log = RegisterLog::parse(register_log.unwrap_or(""));
    let watermark_utc = freshness.watermark_unix.map(format_unix_utc);
    let mut prepared = Vec::with_capacity(lanes.len());
    let mut exact_paths = Vec::new();
    let mut glob_hits = Vec::new();
    for lane in lanes {
        if !in_scope(&lane.date) {
            prepared.push(None);
            continue;
        }
        let (reqs, extra) = requirements_for(source, lane, &bundles, &register_log)?;
        for req in &reqs {
            if is_exact_path(&req.path) {
                exact_paths.push(req.path.clone());
            }
        }
        glob_hits.extend(extra);
        prepared.push(Some(reqs));
    }
    let mut hits = source.rows_for(&exact_paths)?;
    hits.extend(glob_hits);
    let present = hits.iter().map(|hit| hit.path.clone()).collect::<Vec<_>>();
    let mut reports = Vec::with_capacity(lanes.len());
    let mut plans = Vec::with_capacity(lanes.len());
    for (lane, reqs) in lanes.iter().zip(prepared) {
        let Some(reqs) = reqs else {
            reports.push(not_evaluated(lane));
            plans.push(None);
            continue;
        };
        match decide(lane, &reqs, &present, None, &freshness, as_of_unix) {
            Ok(decided) => {
                let mut report = decided.report;
                report.watermark_utc.clone_from(&watermark_utc);
                reports.push(report);
                plans.push(decided.plan);
            }
            Err(err) => {
                reports.push(cannot_tell_report(
                    lane,
                    &err.to_string(),
                    watermark_utc.clone(),
                ));
                plans.push(None);
            }
        }
    }
    Ok(ObserveBatch {
        watermark_utc,
        last_reconcile_at: freshness.last_reconcile_at,
        last_full_pass_at: freshness.last_full_pass_at,
        last_indexer_at: freshness.last_indexer_at,
        document_count: freshness.document_count,
        live_count: Some(freshness.live_count),
        lanes: reports,
        plans,
    })
}

pub fn reconcile_lesson_ships(
    store: &mut Store,
    token: &str,
    opened: std::result::Result<&LatticeSource, &Error>,
    manifest_yaml: &str,
    lanes: &[LaneDue],
    as_of_unix: Option<i64>,
    register_log: Option<&str>,
) -> Result<Vec<LaneReport>> {
    let batch = match opened {
        Ok(source) => {
            match observe_lesson_ships(source, manifest_yaml, lanes, as_of_unix, register_log) {
                Ok(batch) => batch,
                Err(err) => return Ok(cannot_tell_batch(lanes, &err).lanes),
            }
        }
        Err(err) => return Ok(cannot_tell_batch(lanes, err).lanes),
    };
    batch.persist(store, token)?;
    Ok(batch.lanes)
}

/// Pure diff for one lane after hits are known. `missing` comes from
/// `adapt_lapis_observe` when the watermark trusts every absence.
pub fn evaluate_requirements(
    lane: &LaneDue,
    requirements: &[ShipRequirement],
    present_paths: &[String],
    fill_hint: Option<i64>,
    watermark_unix: Option<i64>,
    as_of_unix: Option<i64>,
) -> Result<LaneReport> {
    let freshness = IndexFreshness {
        last_reconcile_at: None,
        last_full_pass_at: None,
        last_indexer_at: None,
        document_count: None,
        live_count: 0,
        watermark_unix,
    };
    Ok(decide(
        lane,
        requirements,
        present_paths,
        fill_hint,
        &freshness,
        as_of_unix,
    )?
    .report)
}

struct Decided {
    report: LaneReport,
    plan: Option<ReconcilePlan>,
}

fn decide(
    lane: &LaneDue,
    requirements: &[ShipRequirement],
    present_paths: &[String],
    fill_hint: Option<i64>,
    freshness: &IndexFreshness,
    as_of_unix: Option<i64>,
) -> Result<Decided> {
    let watermark_utc = freshness.watermark_unix.map(format_unix_utc);
    if !in_scope(&lane.date) {
        return Ok(Decided {
            report: not_evaluated(lane),
            plan: None,
        });
    }
    let check_at = lane.check_at.as_deref().and_then(parse_timestamp);
    if let (Some(as_of), Some(check_at)) = (as_of_unix, check_at) {
        if as_of < check_at {
            return Ok(Decided {
                report: LaneReport {
                    date: lane.date.clone(),
                    lane: lane.lane.clone(),
                    subject: subject_name(&lane.lane, &lane.date),
                    status: "pending".into(),
                    shipped: shipped_paths(requirements, present_paths),
                    missing: Vec::new(),
                    stale: Vec::new(),
                    cannot_tell: None,
                    reasons: Vec::new(),
                    watermark_utc,
                    counts: None,
                },
                plan: None,
            });
        }
    }
    let checks: Vec<AbsenceCheck> = requirements
        .iter()
        .map(|req| AbsenceCheck {
            path: req.path.clone(),
            present: path_present(&req.path, present_paths),
            landed_hint: path_hint(req, fill_hint),
            check_at,
        })
        .collect();
    if let GuardVerdict::Stale { untrusted } = judge_freshness(&checks, freshness.watermark_unix) {
        return Ok(Decided {
            report: LaneReport {
                date: lane.date.clone(),
                lane: lane.lane.clone(),
                subject: subject_name(&lane.lane, &lane.date),
                status: "stale".into(),
                shipped: shipped_paths(requirements, present_paths),
                missing: Vec::new(),
                stale: untrusted,
                cannot_tell: None,
                reasons: Vec::new(),
                watermark_utc,
                counts: None,
            },
            plan: None,
        });
    }
    let (spec, emit) = spec_and_emit(lane, requirements, present_paths)?;
    let observation = adapt_lapis_observe(&spec, &emit)?;
    let missing = yaml_strings(observation.status.observed.get("missing"))?;
    let shipped = yaml_strings(observation.status.observed.get("present"))?;
    let counts = serde_json::to_value(observation.status.observed.get("counts"))?;
    let reasons = missing
        .iter()
        .map(|path| PathReason {
            path: path.clone(),
            reason: reason_for(requirements, path).to_string(),
        })
        .collect();
    let status = if missing.is_empty() { "warm" } else { "gap" };
    Ok(Decided {
        report: LaneReport {
            date: lane.date.clone(),
            lane: lane.lane.clone(),
            subject: subject_name(&lane.lane, &lane.date),
            status: status.into(),
            shipped,
            missing,
            stale: Vec::new(),
            cannot_tell: None,
            reasons,
            watermark_utc,
            counts: Some(counts),
        },
        plan: Some(ReconcilePlan { spec, emit }),
    })
}

fn requirements_for(
    source: &LatticeSource,
    lane: &LaneDue,
    bundles: &[IntendedBundle],
    register_log: &RegisterLog,
) -> Result<(Vec<ShipRequirement>, Vec<crate::source::LatticeRow>)> {
    let matched: Vec<&IntendedBundle> = bundles
        .iter()
        .filter(|bundle| bundle.date == lane.date && bundle.lane == lane.lane)
        .collect();
    let mut reqs = Vec::new();
    let mut extra = Vec::new();
    if matched.is_empty() {
        if let Some(glob) = &lane.lesson_glob {
            let found = source.glob_rows(glob)?;
            if found.is_empty() {
                reqs.push(requirement(glob, "lesson_md", REASON_MANIFEST_ROW, None));
            } else {
                for row in &found {
                    reqs.push(requirement(
                        &row.path,
                        "lesson_md",
                        REASON_MISSING_PATH,
                        None,
                    ));
                }
                extra = found;
            }
        } else {
            let label = format!("manifest-row-missing:{}:{}", lane.lane, lane.date);
            reqs.push(requirement(label, "lesson_md", REASON_MANIFEST_ROW, None));
        }
    } else {
        for bundle in &matched {
            if is_superseded_path(&bundle.lesson_md_path) {
                continue;
            }
            let hint = bundle_landed_hint(bundle, register_log);
            reqs.push(requirement(
                bundle.lesson_md_path.clone(),
                "lesson_md",
                REASON_MISSING_PATH,
                hint,
            ));
            if lane.lane == "maghrib" {
                reqs.push(requirement(
                    lab_ref_path(&bundle.lesson_md_path),
                    "lab_refs",
                    REASON_MISSING_PATH,
                    hint,
                ));
            }
        }
    }
    reqs.push(requirement(
        close_note_path(&lane.date, &lane.lane),
        "ship_note",
        REASON_CLOSE_NOTE,
        None,
    ));
    dedupe_reqs(&mut reqs);
    Ok((reqs, extra))
}

fn requirement(
    path: impl AsRef<str>,
    role: &str,
    reason: &str,
    landed_hint: Option<i64>,
) -> ShipRequirement {
    ShipRequirement {
        path: path.as_ref().to_string(),
        role: role.to_string(),
        reason: reason.to_string(),
        landed_hint,
    }
}

/// Close notes use the lane `check_at`, never a lesson registration stamp.
fn path_hint(req: &ShipRequirement, fill_hint: Option<i64>) -> Option<i64> {
    if req.role == "ship_note" {
        None
    } else {
        req.landed_hint.or(fill_hint)
    }
}

fn bundle_landed_hint(bundle: &IntendedBundle, register_log: &RegisterLog) -> Option<i64> {
    if bundle.revised {
        register_log.latest_for(&bundle.date, &bundle.lane)
    } else {
        bundle.registered_by.as_deref().and_then(parse_timestamp)
    }
}

fn dedupe_reqs(reqs: &mut Vec<ShipRequirement>) {
    let mut seen = Vec::new();
    reqs.retain(|req| {
        if seen.contains(&req.path) {
            false
        } else {
            seen.push(req.path.clone());
            true
        }
    });
}

fn spec_and_emit(
    lane: &LaneDue,
    requirements: &[ShipRequirement],
    present_paths: &[String],
) -> Result<(Value, Value)> {
    let pairs: Vec<(String, String)> = requirements
        .iter()
        .map(|req| (req.path.clone(), req.role.clone()))
        .collect();
    let refs: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(path, role)| (path.as_str(), role.as_str()))
        .collect();
    let check_at = lane
        .check_at
        .as_deref()
        .filter(|value| value.contains('T') && value.len() >= 16);
    let spec = DesiredState::lesson_clock(LessonClockSpec {
        date: &lane.date,
        clock: &lane.lane,
        quiz_html: count_role(requirements, "quiz_html"),
        lab_refs: count_role(requirements, "lab_refs"),
        ship_note: count_role(requirements, "ship_note"),
        lesson_md: count_role(requirements, "lesson_md"),
        required_paths: &refs,
        check_at,
    })?;
    let mut evidence = Vec::new();
    let mut present = Vec::new();
    for req in requirements {
        if path_present(&req.path, present_paths) {
            present.push(serde_json::Value::String(req.path.clone()));
            evidence.push(serde_json::json!({
                "path": req.path,
                "role": req.role,
                "present": true,
            }));
        }
    }
    // `adapt_lapis_observe` accepts evidence hits only when the value also
    // looks like an emit (`present` + `subject`, or `observed` / `observeResult`).
    let emit_json = serde_json::json!({
        "kind": "curriculum_clock",
        "date": lane.date,
        "subject": { "name": subject_name(&lane.lane, &lane.date) },
        "present": present,
        "evidence": evidence,
    });
    let emit: Value = serde_yaml::from_str(&serde_json::to_string(&emit_json)?)?;
    Ok((spec, emit))
}

fn count_role(requirements: &[ShipRequirement], role: &str) -> u64 {
    requirements.iter().filter(|req| req.role == role).count() as u64
}

fn path_present(path: &str, present_paths: &[String]) -> bool {
    !is_superseded_path(path) && present_paths.iter().any(|seen| seen == path)
}

fn shipped_paths(requirements: &[ShipRequirement], present_paths: &[String]) -> Vec<String> {
    requirements
        .iter()
        .filter(|req| path_present(&req.path, present_paths))
        .map(|req| req.path.clone())
        .collect()
}

fn reason_for<'a>(requirements: &'a [ShipRequirement], path: &str) -> &'a str {
    requirements
        .iter()
        .find(|req| req.path == path)
        .map(|req| req.reason.as_str())
        .unwrap_or(REASON_MISSING_PATH)
}

fn yaml_strings(value: Option<&Value>) -> Result<Vec<String>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Some(seq) = value.as_sequence() else {
        return Err(Error::Invalid(
            "curriculum_clock observation list must be a sequence".into(),
        ));
    };
    let mut out = Vec::with_capacity(seq.len());
    for item in seq {
        let Some(text) = item.as_str() else {
            return Err(Error::Invalid(
                "curriculum_clock observation entry must be a string".into(),
            ));
        };
        out.push(text.to_string());
    }
    Ok(out)
}

fn cannot_tell_report(lane: &LaneDue, detail: &str, watermark_utc: Option<String>) -> LaneReport {
    LaneReport {
        date: lane.date.clone(),
        lane: lane.lane.clone(),
        subject: subject_name(&lane.lane, &lane.date),
        status: "cannot_tell".into(),
        shipped: Vec::new(),
        missing: Vec::new(),
        stale: Vec::new(),
        cannot_tell: Some(detail.to_string()),
        reasons: Vec::new(),
        watermark_utc,
        counts: None,
    }
}

fn not_evaluated(lane: &LaneDue) -> LaneReport {
    LaneReport {
        date: lane.date.clone(),
        lane: lane.lane.clone(),
        subject: subject_name(&lane.lane, &lane.date),
        status: "not_evaluated".into(),
        shipped: Vec::new(),
        missing: Vec::new(),
        stale: Vec::new(),
        cannot_tell: None,
        reasons: Vec::new(),
        watermark_utc: None,
        counts: None,
    }
}

fn subject_name(lane: &str, date: &str) -> String {
    format!("{lane}-{date}")
}

fn in_scope(date: &str) -> bool {
    let bytes = date.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(idx, byte)| idx == 4 || idx == 7 || byte.is_ascii_digit())
        && date >= SCOPE_START
}

fn is_exact_path(path: &str) -> bool {
    !path.contains('*') && !path.contains('?')
}
