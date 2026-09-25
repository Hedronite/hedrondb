//! Golden: put curriculum_clock → gap (nonempty missing) → warm when missing:[].
//!
//! Emit `status` / `missing` / `counts` are decoys. Kind-dispatch recomputes
//! `Observation.missing`. HQL `status=gap` is not stored.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use hedron_core::{adapt_lapis_observe, ConditionKind, DesiredState, Node, Store};
use serde_json::{json, Value as Json};

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

const PATHS: &[(&str, &str)] = &[
    (
        "Archmagus-Stack/01-Earth-DevOps/Synthesis-Lessons/2026-09-21-eks/quiz.html",
        "quiz_html",
    ),
    (
        "Archmagus-Stack/Polyglot-Dev/Python/2026-09-21-boto3/quiz.html",
        "quiz_html",
    ),
    (
        "Archmagus-Stack/Cert-Prep/CNCF/2026-09-21-cks/quiz.html",
        "quiz_html",
    ),
    (
        "Archmagus-Stack/01-Earth-DevOps/Synthesis-Lessons/2026-09-21-eks/lab-ref.md",
        "lab_refs",
    ),
    (
        "Archmagus-Stack/Polyglot-Dev/Python/2026-09-21-boto3/lab-ref.md",
        "lab_refs",
    ),
    (
        "Archmagus-Stack/Cert-Prep/CNCF/2026-09-21-cks/lab-ref.md",
        "lab_refs",
    ),
    ("agents/mail_room/Leo/2026-09-21-maghrib.md", "ship_note"),
];

struct TempStore {
    store: Store,
    path: PathBuf,
}

impl TempStore {
    fn new() -> Self {
        let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hedron-curriculum-clock-{}-{}.db",
            std::process::id(),
            n
        ));
        let store = Store::open(&path).expect("open store");
        Self { store, path }
    }
}

impl Drop for TempStore {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn yaml_from_json(value: Json) -> serde_yaml::Value {
    serde_yaml::from_str(&serde_json::to_string(&value).unwrap()).unwrap()
}

fn strings(value: &serde_yaml::Value) -> Vec<&str> {
    value
        .as_sequence()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap())
        .collect()
}

fn count(observed: &serde_yaml::Value, role: &str, field: &str) -> i64 {
    observed["counts"][role][field].as_i64().unwrap()
}

fn evidence(present: bool) -> Json {
    json!(PATHS
        .iter()
        .map(|(path, role)| json!({ "path": path, "role": role, "present": present }))
        .collect::<Vec<_>>())
}

fn emit(status: &str, present: &[&str], present_flag: bool) -> Json {
    json!({
        "kind": "curriculum_clock",
        "name": "maghrib-2026-09-21",
        "date": "2026-09-21",
        "clock": "maghrib",
        "source": "fm",
        "checked_at": "2026-09-21T22:57:05-04:00",
        "observed": {
            "present": present,
            "missing": ["decoy-not-a-path"],
            "counts": {
                "quiz_html": { "expected": 0, "actual": 0 },
                "lab_refs": { "expected": 0, "actual": 0 },
                "ship_note": { "expected": 0, "actual": 0 }
            }
        },
        "observeResult": {
            "kind": "curriculum_clock",
            "subject": { "name": "maghrib-2026-09-21" },
            "status": status,
            "present": present,
            "missing": ["decoy-not-a-path"],
            "evidence": evidence(present_flag),
            "observedAt": "2026-09-21T22:57:05-04:00",
            "remainderEligible": false
        }
    })
}

#[test]
fn curriculum_clock_put_gap_then_warm() {
    let mut tmp = TempStore::new();
    let store = &mut tmp.store;
    let boot = store
        .bootstrap("curriculum", "scribe", "agents/scribe")
        .unwrap();
    let spec = DesiredState::curriculum_clock_spec(
        "2026-09-21",
        "maghrib",
        3,
        3,
        1,
        0,
        PATHS,
        Some("2026-09-21T19:45:00-04:00"),
    )
    .unwrap();
    assert_eq!(spec["kind"].as_str(), Some("curriculum_clock"));

    let ds = store
        .put_desired_state(&boot.token, "maghrib-2026-09-21", spec.clone(), 0.5)
        .unwrap();
    assert_eq!(ds.state_version, 1);
    assert!(ds.status.conditions.is_empty(), "put is not observe");

    let expected_missing: Vec<&str> = PATHS.iter().map(|(path, _)| *path).collect();

    let (gap, gap_event) = store.reconcile(&boot.token, ds.id).unwrap();
    assert_eq!(gap.state_version, 2);
    assert_eq!(gap.status.conditions.len(), 1);
    assert_eq!(gap.status.conditions[0].kind, ConditionKind::Pending);
    assert_eq!(strings(&gap.status.observed["missing"]), expected_missing);
    assert!(strings(&gap.status.observed["present"]).is_empty());
    assert!(gap_event.caused_by.is_empty());
    assert_eq!(count(&gap.status.observed, "quiz_html", "expected"), 3);
    assert_eq!(count(&gap.status.observed, "quiz_html", "actual"), 0);
    assert_eq!(count(&gap.status.observed, "lab_refs", "actual"), 0);
    assert_eq!(count(&gap.status.observed, "ship_note", "actual"), 0);
    assert!(!strings(&gap.status.observed["missing"])
        .iter()
        .any(|path| *path == "decoy-not-a-path"));

    let gap_emit = yaml_from_json(emit("warm", &[], false));
    let adapted_gap = adapt_lapis_observe(&spec, &gap_emit).unwrap();
    assert_eq!(
        strings(&adapted_gap.status.observed["missing"]),
        expected_missing,
        "adapter must recompute missing and ignore emit status/missing"
    );
    assert_eq!(
        adapted_gap.status.conditions[0].kind,
        ConditionKind::Pending
    );

    let warm_paths: Vec<&str> = expected_missing.clone();
    let warm_emit = yaml_from_json(emit("gap", &warm_paths, true));
    let doc = Node::document(
        boot.vault.id,
        Some("foundry/glue/examples/maghrib-2026-09-21.lapis-observe-emit.json"),
        warm_emit.clone(),
    )
    .unwrap();
    let doc_id = doc.id;
    store.put_node(&boot.token, doc).unwrap();

    let (warm, warm_event) = store.reconcile(&boot.token, ds.id).unwrap();
    assert_eq!(warm.id, ds.id);
    assert_eq!(warm.state_version, 3);
    assert_eq!(
        warm.spec["check_at"].as_str(),
        Some("2026-09-21T19:45:00-04:00")
    );
    assert!(strings(&warm.status.observed["missing"]).is_empty());
    assert_eq!(strings(&warm.status.observed["present"]), expected_missing);
    assert_eq!(warm.status.conditions.len(), 1);
    assert_eq!(warm.status.conditions[0].kind, ConditionKind::Reconciled);
    assert_eq!(warm_event.caused_by, vec![doc_id]);
    assert_eq!(count(&warm.status.observed, "quiz_html", "actual"), 3);
    assert_eq!(count(&warm.status.observed, "lab_refs", "actual"), 3);
    assert_eq!(count(&warm.status.observed, "ship_note", "expected"), 1);
    assert_eq!(count(&warm.status.observed, "ship_note", "actual"), 1);
    assert_eq!(
        strings(&warm_event.data["missing"]),
        Vec::<&str>::new(),
        "reconcile event carries recomputed missing, not the emit decoy"
    );

    let adapted_warm = adapt_lapis_observe(&spec, &warm_emit).unwrap();
    assert_eq!(
        adapted_warm.status.observed["missing"],
        warm.status.observed["missing"]
    );
    assert_eq!(
        adapted_warm.status.observed["present"],
        warm.status.observed["present"]
    );
    assert_eq!(
        adapted_warm.status.observed["counts"],
        warm.status.observed["counts"]
    );
    assert_eq!(
        adapted_warm.status.conditions[0].kind,
        ConditionKind::Reconciled
    );

    let current = store.current_state(&boot.token, ds.id).unwrap();
    assert!(strings(&current.status.observed["missing"]).is_empty());
    assert_eq!(current.spec["kind"].as_str(), Some("curriculum_clock"));
}

#[test]
fn old_curriculum_clock_spec_without_lesson_md_still_parses() {
    let mut tmp = TempStore::new();
    let boot = tmp
        .store
        .bootstrap("curriculum", "scribe", "agents/scribe")
        .unwrap();
    let spec: serde_yaml::Value = serde_yaml::from_str(
        r#"
kind: curriculum_clock
date: "2026-09-21"
clock: maghrib
expected:
  quiz_html: 0
  lab_refs: 0
  ship_note: 1
required_paths:
  - path: agents/mail_room/Leo/2026-09-21-maghrib.md
    role: ship_note
"#,
    )
    .unwrap();
    let ds = tmp
        .store
        .put_desired_state(&boot.token, "maghrib-2026-09-21", spec, 0.5)
        .unwrap();
    assert!(ds.spec["expected"].get("lesson_md").is_none());
}
