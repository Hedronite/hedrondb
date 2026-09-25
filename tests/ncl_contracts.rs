use hedron_core::{DesiredState, Error, Status, Store};
use serde_yaml::Value;

struct TempStore {
    store: Store,
    path: std::path::PathBuf,
}

impl TempStore {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "hedron-ncl-contracts-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Store::open(&path).expect("open store");
        Self { store, path }
    }
}

impl Drop for TempStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[test]
fn bad_docs_eod_missing_date_is_invalid_at_put() {
    let mut tmp = TempStore::new();
    let boot = tmp
        .store
        .bootstrap("prod", "deploy", "agents/deploy")
        .unwrap();
    let bad: Value = serde_yaml::from_str(
        "kind: docs_eod\nrequired_briefs:\n- alpha\n",
    )
    .unwrap();
    let err = tmp
        .store
        .put_desired_state(&boot.token, "eod", bad, 0.5)
        .expect_err("missing date should fail at put");
    assert!(matches!(err, Error::Invalid(_)));
    assert!(
        err.to_string().contains("date"),
        "expected date-related Invalid, got: {err}"
    );
}

#[test]
fn valid_docs_eod_put_keeps_frozen_spec_hash() {
    let mut tmp = TempStore::new();
    let boot = tmp
        .store
        .bootstrap("prod", "deploy", "agents/deploy")
        .unwrap();
    let spec = DesiredState::docs_eod_spec("2026-08-25", &["alpha"]).unwrap();
    let first = tmp
        .store
        .put_desired_state(&boot.token, "eod", spec.clone(), 0.5)
        .unwrap();
    assert_eq!(first.state_version, 1);
    assert_eq!(first.status, Status::empty());
    assert!(!first.content_hash.is_empty());

    let again = tmp
        .store
        .put_desired_state(&boot.token, "eod", spec.clone(), 0.5)
        .unwrap();
    assert_eq!(again.id, first.id);
    assert_eq!(again.content_hash, first.content_hash);
    assert_eq!(again.spec, spec);
}
