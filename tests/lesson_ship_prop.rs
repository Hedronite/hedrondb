//! Diff properties: missing is the intended set minus observed hits, in spec order.
//! An untrusted absence stays out of `missing`.

use hedron_core::{
    evaluate_requirements, DesiredState, LaneDue, LessonClockSpec, ShipRequirement, SCOPE_START,
};
use proptest::prelude::*;

fn lesson_paths(n: usize) -> Vec<String> {
    (0..n)
        .map(|idx| format!("Archmagus-Stack/Polyglot-Dev/Rust/2026-09-25-p{idx}/lesson.md"))
        .collect()
}

fn requirements(paths: &[String]) -> Vec<ShipRequirement> {
    paths
        .iter()
        .map(|path| ShipRequirement {
            path: path.clone(),
            role: "lesson_md".into(),
            reason: "missing_path".into(),
            landed_hint: None,
        })
        .collect()
}

fn lane() -> LaneDue {
    LaneDue {
        date: SCOPE_START.to_string(),
        lane: "asr".into(),
        check_at: None,
        lesson_glob: None,
    }
}

fn present_for(paths: &[String], mask: &[bool]) -> Vec<String> {
    paths
        .iter()
        .zip(mask.iter())
        .filter(|(_, on)| **on)
        .map(|(path, _)| path.clone())
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn trusted_missing_is_intended_minus_observed(mask in prop::collection::vec(any::<bool>(), 1..6)) {
        let paths = lesson_paths(mask.len());
        let present = present_for(&paths, &mask);
        let reqs = requirements(&paths);
        let report = evaluate_requirements(
            &lane(),
            &reqs,
            &present,
            Some(1_000),
            Some(1_000),
            None,
        )
        .unwrap();
        let expected: Vec<String> = paths
            .iter()
            .zip(mask.iter())
            .filter(|(_, on)| !**on)
            .map(|(path, _)| path.clone())
            .collect();
        prop_assert_eq!(&report.missing, &expected);
        prop_assert!(report.shipped.iter().all(|path| !report.missing.contains(path)));
        prop_assert_eq!(report.shipped.len() + report.missing.len(), paths.len());
        prop_assert_eq!(report.status == "warm", report.missing.is_empty());
        prop_assert!(report.stale.is_empty());
        let actual = report.counts.as_ref().unwrap()["lesson_md"]["actual"].as_u64().unwrap();
        let expected_count = report.counts.as_ref().unwrap()["lesson_md"]["expected"].as_u64().unwrap();
        prop_assert_eq!(actual, present.len() as u64);
        prop_assert_eq!(expected_count, paths.len() as u64);
        prop_assert_eq!(actual, report.shipped.len() as u64);

        let pairs: Vec<(&str, &str)> = paths.iter().map(|path| (path.as_str(), "lesson_md")).collect();
        let spec = DesiredState::lesson_clock(LessonClockSpec {
            date: SCOPE_START,
            clock: "asr",
            quiz_html: 0,
            lab_refs: 0,
            ship_note: 0,
            lesson_md: paths.len() as u64,
            required_paths: &pairs,
            check_at: None,
        })
        .unwrap();
        let evidence: Vec<serde_json::Value> = present
            .iter()
            .map(|path| serde_json::json!({"path": path, "role": "lesson_md", "present": true}))
            .collect();
        let emit = serde_yaml::from_str(&serde_json::to_string(&serde_json::json!({
            "kind": "curriculum_clock",
            "date": SCOPE_START,
            "status": "gap",
            "subject": {"name": "asr-2026-09-25"},
            "missing": ["decoy-not-a-path"],
            "present": present,
            "evidence": evidence,
        })).unwrap()).unwrap();
        let observation = hedron_core::adapt_lapis_observe(&spec, &emit).unwrap();
        let adapted: Vec<String> = observation.status.observed["missing"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|item| item.as_str().unwrap().to_string())
            .collect();
        prop_assert!(!adapted.iter().any(|path| path == "decoy-not-a-path"));
        prop_assert_eq!(adapted, report.missing);
    }

    #[test]
    fn adding_an_observed_row_never_grows_missing(mask in prop::collection::vec(any::<bool>(), 1..6)) {
        let paths = lesson_paths(mask.len());
        let present = present_for(&paths, &mask);
        let reqs = requirements(&paths);
        let before = evaluate_requirements(&lane(), &reqs, &present, Some(10), Some(10), None).unwrap();
        if let Some(extra) = paths.iter().find(|path| !present.iter().any(|seen| seen == *path)) {
            let mut more = present.clone();
            more.push(extra.clone());
            let after = evaluate_requirements(&lane(), &reqs, &more, Some(10), Some(10), None).unwrap();
            prop_assert!(after.missing.len() <= before.missing.len());
            prop_assert!(after.missing.iter().all(|path| before.missing.contains(path)));
        }
    }

    #[test]
    fn untrusted_absence_never_appears_in_missing(n in 1usize..6) {
        let paths = lesson_paths(n);
        let reqs = requirements(&paths);
        // Keep at least one path absent so the watermark can refuse the miss.
        let present: Vec<String> = if n == 1 {
            Vec::new()
        } else {
            vec![paths[0].clone()]
        };
        let report = evaluate_requirements(
            &lane(),
            &reqs,
            &present,
            Some(5_000),
            Some(1_000),
            None,
        )
        .unwrap();
        prop_assert_eq!(report.status, "stale");
        prop_assert!(report.missing.is_empty());
        for path in &paths {
            if present.iter().any(|seen| seen == path) {
                prop_assert!(!report.stale.contains(path));
            } else {
                prop_assert!(report.stale.contains(path));
                prop_assert!(!report.missing.contains(path));
            }
        }
    }
}
