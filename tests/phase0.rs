use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hedron_core::{ConditionKind, DesiredState, Edge, Error, Node, Store, CAUSAL_SUPERSEDES};
use serde_yaml::Value;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempStore {
    store: Store,
    path: PathBuf,
}

impl TempStore {
    fn new() -> Self {
        let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("hedron-phase0-{}-{}.db", std::process::id(), n));
        let store = Store::open(&path).expect("open store");
        Self { store, path }
    }
}

impl Drop for TempStore {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[test]
fn recon_loop_option_b_current_state_and_causal_chain() {
    let mut tmp = TempStore::new();
    let store = &mut tmp.store;

    let boot = store
        .bootstrap("demo-vault", "scribe", "agents/scribe")
        .unwrap();
    assert_eq!(boot.agent.htec_path.as_deref(), Some("agents/scribe"));
    let token = store.rotate_token(&boot.token).unwrap();
    assert!(matches!(
        store.put_node(&boot.token, boot.agent.clone()),
        Err(Error::Unauthorized)
    ));

    let spec = DesiredState::docs_eod_spec("2026-08-25", &["alpha", "beta", "gamma"]).unwrap();
    let ds = store
        .put_desired_state(&token, "eod-2026-08-25", spec, 0.8)
        .unwrap();
    assert_eq!(ds.state_version, 1);
    assert_eq!(ds.name, "eod-2026-08-25");

    let observed = store.observe(&token, ds.id).unwrap();
    assert_eq!(observed.state_version, 1);
    assert!(observed
        .status
        .conditions
        .iter()
        .any(|c| c.kind == ConditionKind::Pending));
    assert_eq!(
        observed.status.observed["missing"]
            .as_sequence()
            .unwrap()
            .len(),
        3
    );

    for name in ["alpha", "beta"] {
        let doc = Node::brief_document(boot.vault.id, name, "2026-08-25").unwrap();
        store.put_node(&token, doc).unwrap();
    }

    let (reconciled, event) = store.reconcile(&token, ds.id).unwrap();
    assert_eq!(reconciled.state_version, 2);
    assert_ne!(reconciled.content_hash, observed.content_hash);
    assert_eq!(reconciled.reconciled_by, Some(boot.agent.id));
    assert_eq!(event.reconciles, Some(ds.id));
    assert_eq!(
        event.supersedes.as_deref(),
        Some(format!("{}@1", ds.id).as_str())
    );
    assert_eq!(event.caused_by.len(), 2);
    assert_eq!(
        reconciled.status.observed["present"]
            .as_sequence()
            .unwrap()
            .len(),
        2
    );
    assert!(reconciled
        .status
        .conditions
        .iter()
        .any(|c| c.kind == ConditionKind::Pending));

    let current = store.current_state(&token, ds.id).unwrap();
    assert_eq!(
        current.spec["required_briefs"].as_sequence().unwrap().len(),
        3
    );
    assert_eq!(current.status.observed["missing"][0].as_str(), Some("gamma"));
    assert_eq!(current.state_version, 2);

    let chain = store.causal_chain(&token, ds.id).unwrap();
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].id, event.id);
    assert_eq!(chain[0].caused_by, event.caused_by);
    assert_eq!(chain[0].reconciles, Some(ds.id));
    assert!(chain[0].supersedes.is_some());
    assert!(
        chain[0].data.get("required_briefs").is_none(),
        "causal_chain must not return current spec"
    );
}

#[test]
fn two_named_desired_states_in_one_vault() {
    let mut tmp = TempStore::new();
    let boot = tmp
        .store
        .bootstrap("prod", "deploy", "agents/deploy")
        .unwrap();
    let a = tmp
        .store
        .put_desired_state(
            &boot.token,
            "eod-2026-08-25",
            DesiredState::docs_eod_spec("2026-08-25", &["alpha"]).unwrap(),
            0.5,
        )
        .unwrap();
    let b = tmp
        .store
        .put_desired_state(
            &boot.token,
            "eod-2026-08-26",
            DesiredState::docs_eod_spec("2026-08-26", &["beta"]).unwrap(),
            0.5,
        )
        .unwrap();
    assert_ne!(a.id, b.id);
    assert_eq!(a.vault_id, b.vault_id);
    assert_eq!(
        tmp.store.current_state(&boot.token, a.id).unwrap().name,
        "eod-2026-08-25"
    );
    assert_eq!(
        tmp.store.current_state(&boot.token, b.id).unwrap().name,
        "eod-2026-08-26"
    );
    // Same name in another vault is a different intent, not a collision.
    let other = tmp.store.bootstrap("staging", "ops", "agents/ops").unwrap();
    let c = tmp
        .store
        .put_desired_state(
            &other.token,
            "eod-2026-08-25",
            DesiredState::docs_eod_spec("2026-08-25", &["alpha"]).unwrap(),
            0.5,
        )
        .unwrap();
    assert_ne!(c.id, a.id);
}

#[test]
fn same_name_put_keeps_id_replaces_spec_and_appends_no_event() {
    let mut tmp = TempStore::new();
    let boot = tmp
        .store
        .bootstrap("prod", "deploy", "agents/deploy")
        .unwrap();
    let token = boot.token;
    let first = tmp
        .store
        .put_desired_state(
            &token,
            "deploy",
            DesiredState::docs_eod_spec("2026-08-25", &["alpha"]).unwrap(),
            0.5,
        )
        .unwrap();
    let (reconciled, _) = tmp.store.reconcile(&token, first.id).unwrap();
    assert_eq!(reconciled.state_version, 2);

    let again = tmp
        .store
        .put_desired_state(
            &token,
            "deploy",
            DesiredState::docs_eod_spec("2026-08-25", &["alpha", "beta"]).unwrap(),
            0.9,
        )
        .unwrap();
    assert_eq!(again.id, first.id, "same (vault, name) must keep its id");
    assert_eq!(again.state_version, 2, "put does not bump the version");
    assert_eq!(again.status, reconciled.status, "put does not touch status");
    assert_eq!(again.reconciled_by, reconciled.reconciled_by);
    assert_ne!(again.content_hash, reconciled.content_hash);
    assert_eq!(again.importance, 0.9);
    assert_eq!(
        again.spec["required_briefs"].as_sequence().unwrap().len(),
        2
    );
    assert_eq!(
        tmp.store.causal_chain(&token, first.id).unwrap().len(),
        1,
        "spec replacement appends no event"
    );

    let (third, event) = tmp.store.reconcile(&token, first.id).unwrap();
    assert_eq!(third.id, first.id);
    assert_eq!(third.state_version, 3);
    assert_eq!(event.supersedes.as_deref(), Some(format!("{}@2", first.id).as_str()));
    assert_eq!(tmp.store.causal_chain(&token, first.id).unwrap().len(), 2);
}

#[test]
fn missing_or_unknown_kind_is_invalid() {
    let mut tmp = TempStore::new();
    let boot = tmp
        .store
        .bootstrap("prod", "deploy", "agents/deploy")
        .unwrap();
    let no_kind: Value =
        serde_yaml::from_str("date: 2026-08-25\nrequired_briefs:\n- alpha\n").unwrap();
    assert!(matches!(
        tmp.store.put_desired_state(&boot.token, "eod", no_kind, 0.5),
        Err(Error::Invalid(_))
    ));
    let unknown: Value =
        serde_yaml::from_str("kind: cluster_ready\nreplicas: 3\n").unwrap();
    assert!(matches!(
        tmp.store.put_desired_state(&boot.token, "eod", unknown, 0.5),
        Err(Error::Invalid(_))
    ));
    let empty_name = DesiredState::docs_eod_spec("2026-08-25", &["alpha"]).unwrap();
    assert!(matches!(
        tmp.store.put_desired_state(&boot.token, "", empty_name, 0.5),
        Err(Error::Invalid(_))
    ));
    let ok = DesiredState::docs_eod_spec("2026-08-25", &["alpha"]).unwrap();
    assert_eq!(ok["kind"].as_str(), Some("docs_eod"));
    assert!(tmp
        .store
        .put_desired_state(&boot.token, "eod", ok, 0.5)
        .is_ok());
}

#[test]
fn outer_transaction_rolls_back_nested_reconcile() {
    let mut tmp = TempStore::new();
    let boot = tmp
        .store
        .bootstrap("prod", "deploy", "agents/deploy")
        .unwrap();
    let token = boot.token;

    tmp.store.begin().unwrap();
    let ds = tmp
        .store
        .put_desired_state(
            &token,
            "eod",
            DesiredState::docs_eod_spec("2026-08-25", &["alpha"]).unwrap(),
            0.5,
        )
        .unwrap();
    let (reconciled, _) = tmp.store.reconcile(&token, ds.id).unwrap();
    assert_eq!(reconciled.state_version, 2, "savepoint nests inside BEGIN");
    tmp.store.rollback().unwrap();
    assert!(
        matches!(
            tmp.store.current_state(&token, ds.id),
            Err(Error::NotFound(_))
        ),
        "rollback must leave no desired state"
    );
    assert!(tmp.store.causal_chain(&token, ds.id).is_err());

    tmp.store.begin().unwrap();
    let ds = tmp
        .store
        .put_desired_state(
            &token,
            "eod",
            DesiredState::docs_eod_spec("2026-08-25", &["alpha"]).unwrap(),
            0.5,
        )
        .unwrap();
    tmp.store.reconcile(&token, ds.id).unwrap();
    tmp.store.commit().unwrap();
    assert_eq!(tmp.store.current_state(&token, ds.id).unwrap().state_version, 2);
    assert_eq!(tmp.store.causal_chain(&token, ds.id).unwrap().len(), 1);
}

#[test]
fn vault_isolation_denies_cross_vault_read() {
    let mut tmp = TempStore::new();
    let a = tmp.store.bootstrap("alpha", "agent-a", "agents/a").unwrap();
    let b = tmp.store.bootstrap("beta", "agent-b", "agents/b").unwrap();
    let spec = DesiredState::docs_eod_spec("2026-08-25", &["alpha"]).unwrap();
    let ds = tmp
        .store
        .put_desired_state(&a.token, "eod", spec, 0.5)
        .unwrap();

    match tmp.store.current_state(&b.token, ds.id) {
        Err(Error::VaultDenied { vault_id }) => assert_eq!(vault_id, a.vault.id),
        other => panic!("expected VaultDenied, got {other:?}"),
    }
    match tmp.store.causal_chain(&b.token, ds.id) {
        Err(Error::VaultDenied { vault_id }) => assert_eq!(vault_id, a.vault.id),
        other => panic!("expected VaultDenied, got {other:?}"),
    }
}

#[test]
fn hivemind_and_shared_are_deny_by_default_until_grant() {
    let mut tmp = TempStore::new();
    let hive = tmp
        .store
        .bootstrap("hivemind", "hive-keeper", "agents/hive")
        .unwrap();
    let shared = tmp
        .store
        .bootstrap("shared", "shared-keeper", "agents/shared")
        .unwrap();
    let outsider = tmp
        .store
        .bootstrap("solo", "solo-agent", "agents/solo")
        .unwrap();

    let hive_ds = tmp
        .store
        .put_desired_state(
            &hive.token,
            "eod",
            DesiredState::docs_eod_spec("2026-08-25", &["alpha"]).unwrap(),
            0.4,
        )
        .unwrap();
    assert!(matches!(
        tmp.store.current_state(&outsider.token, hive_ds.id),
        Err(Error::VaultDenied { .. })
    ));

    tmp.store
        .grant_access(&hive.token, outsider.agent.id, hive.vault.id)
        .unwrap();
    let granted = tmp
        .store
        .current_state(&outsider.token, hive_ds.id)
        .unwrap();
    assert_eq!(granted.id, hive_ds.id);

    let shared_ds = tmp
        .store
        .put_desired_state(
            &shared.token,
            "eod",
            DesiredState::docs_eod_spec("2026-08-25", &["beta"]).unwrap(),
            0.4,
        )
        .unwrap();
    assert!(
        matches!(
            tmp.store.current_state(&outsider.token, shared_ds.id),
            Err(Error::VaultDenied { .. })
        ),
        "shared is not open just because of the vault name"
    );
}

#[test]
fn unresolved_edge_target_allowed() {
    let mut tmp = TempStore::new();
    let boot = tmp
        .store
        .bootstrap("demo-vault", "scribe", "agents/scribe")
        .unwrap();
    let edge = Edge::new(
        boot.vault.id,
        boot.agent.id,
        None,
        Some("briefs/2026-08-25/gamma".into()),
        "mentions",
    )
    .unwrap();
    let stored = tmp.store.put_edge(&boot.token, edge).unwrap();
    assert!(stored.to_id.is_none());
    assert_eq!(stored.to_raw.as_deref(), Some("briefs/2026-08-25/gamma"));

    let causal = Edge::new(
        boot.vault.id,
        boot.agent.id,
        None,
        Some(format!("{}@1", boot.vault.id)),
        CAUSAL_SUPERSEDES,
    )
    .unwrap();
    assert!(matches!(
        tmp.store.put_edge(&boot.token, causal),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn store_file_is_mode_0600() {
    let tmp = TempStore::new();
    let mode = fs::metadata(&tmp.path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

/// Every `.rs` under `src/`, recursing into `hql/` and `bin/`.
fn crate_source_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(Path::new("src"), &mut out);
    out.sort();
    out
}

#[test]
fn crate_sources_stay_sync_and_local() {
    let files = crate_source_files();
    // The walk must reach the nested modules or the hold is hollow.
    for must_see in ["src/hql/mod.rs", "src/bin/hedron.rs"] {
        assert!(
            files.iter().any(|p| p == Path::new(must_see)),
            "source walk missed {must_see}"
        );
    }
    for path in files {
        let src = fs::read_to_string(&path).unwrap();
        for part in ["tok", "pyo", "TcpL", "TcpS", "std::n"] {
            let needle = match part {
                "tok" => "tokio",
                "pyo" => "pyo3",
                "TcpL" => "TcpListener",
                "TcpS" => "TcpStream",
                "std::n" => "std::net",
                _ => part,
            };
            assert!(
                !src.contains(needle),
                "{} must not contain {needle}",
                path.display()
            );
        }
    }
}

#[test]
fn crate_sources_stay_under_1000_lines() {
    for path in crate_source_files() {
        let lines = fs::read_to_string(&path).unwrap().lines().count();
        assert!(
            lines <= 1000,
            "{} is {lines} lines; split it (limit 1000)",
            path.display()
        );
    }
}

/// The line-splitting extra parser is gone from both engines.
#[test]
fn legacy_extra_parser_is_gone() {
    let needle = ["parse_extra", "_map"].concat();
    let mut files = crate_source_files();
    for entry in fs::read_dir("python/hql/hql").unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) == Some("py") {
            files.push(path);
        }
    }
    assert!(files.iter().any(|p| p.ends_with("row.py")), "python engine not scanned");
    for path in files {
        let src = fs::read_to_string(&path).unwrap();
        assert!(
            !src.contains(&needle),
            "{} still carries {needle}",
            path.display()
        );
    }
}
