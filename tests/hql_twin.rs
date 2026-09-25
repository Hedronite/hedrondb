//! Documented comparison: `hedron hql` and `python/hql` agree on fixture pipes.
//!
//! Same pipeline string against the same db must produce the same rows
//! (fields and values), not merely similar text. `cargo test --test hql_twin`
//! is the comparison.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use hedron_core::{DesiredState, Node, NodeType, Store, Tier};
use rusqlite::Connection;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Fixtures execute the crate `schema.sql`, never a private copy.
const SCHEMA: &str = hedron_core::SCHEMA_SQL;

const PIPE_FOUNDRY: &str = r#"vault demo-vault | search "HedronDB" | filter extra.domain == "foundry" | select path, extra.name | limit 20"#;
const PIPE_DANGLING: &str = r#"vault demo-vault | search "lattice edges" | traverse --edge mentions --hops 1 | filter to_id == null | select path, to_raw"#;
const PIPE_RESOLVED: &str = r#"vault demo-vault | filter path ^= "inbox/" && path !^= "inbox/private/" | traverse --edge mentions --hops 1 | filter to_id != null | select from.path, to.path"#;
const PIPE_SESSION: &str =
    "vault prod | agent deploy | state | select path, extra.name, extra.title, state_version, status";
const PIPE_AGENT_STATE: &str = "agent deploy | state | select path, extra.title, state_version, status, id, reconciled_by, importance";
const PIPE_NO_EVENTS: &str =
    "vault prod | agent deploy | state | select path, spec, status, state_version, id";
const PIPE_HISTORY: &str = "vault prod | agent deploy | history";
const PIPE_HISTORY_SELECT: &str =
    "vault prod | agent deploy | history | select id, ts, actor, type, caused_by, reconciles, supersedes, spec, status";
const PIPE_ALL_STATES: &str = "vault prod | state";
const PIPE_ALL_STATES_SELECT: &str = "vault prod | state | select name, state_version, path, id, spec, status, importance";
const PIPE_ONE_STATE: &str = "vault prod | state eod-2026-08-25";
const PIPE_OPS_EXPLICIT: &str = "vault prod | agent ops | state deploy | select path, extra.title, name, id";
const PIPE_OPS_NONE: &str = "vault prod | agent ops | state";
const PIPE_NO_SUBJECT: &str = r#"vault prod | filter path ^= "notes/" | state deploy"#;
const PIPE_NO_SUBJECT_HISTORY: &str = r#"vault prod | filter path ^= "notes/" | history deploy | select id, supersedes, name"#;
const PIPE_STATE_NO_EVENTS: &str = "vault prod | agent deploy | state | select ts, actor, supersedes, name";
const PIPE_YAML_EXTRA: &str = r#"vault demo-vault | filter path == "notes/nested.md" | select path, extra.tags, extra.version, extra.meta, extra.flag, extra.empty, extra.when, extra.ratio, extra.missing"#;
const PIPE_YAML_FILTER: &str = r#"vault demo-vault | filter extra.version == "2" && extra.flag == "true" | select path"#;

struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new(label: &str) -> Self {
        let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hedron-twin-{}-{}-{}.db",
            label,
            std::process::id(),
            n
        ));
        let _ = fs::remove_file(&path);
        Self { path }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn python_hql_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("python/hql")
}

fn build_tiny_db(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    let vault = "11111111-1111-1111-1111-111111111111";
    let foundry = "22222222-2222-2222-2222-222222222222";
    let lattice = "33333333-3333-3333-3333-333333333333";
    let resolved = "44444444-4444-4444-4444-444444444444";
    conn.execute(
        "INSERT INTO nodes (id, vault_id, type, content_hash, path, version, tier, importance, extra) \
         VALUES (?1, ?1, 'Vault', 'h', 'demo-vault', 1, 'cool', 1.0, 'name: demo-vault\n')",
        [vault],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO nodes (id, vault_id, type, content_hash, path, version, tier, importance, extra) \
         VALUES (?1, ?2, 'Document', 'h', 'inbox/hedron-foundry.md', 1, 'warm', 0.5, \
         'name: HedronDB kernel notes\ndomain: foundry\n')",
        [foundry, vault],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO nodes (id, vault_id, type, content_hash, path, version, tier, importance, extra) \
         VALUES (?1, ?2, 'Document', 'h', 'notes/lattice-edges.md', 1, 'warm', 0.5, \
         'name: lattice edges walk\n')",
        [lattice, vault],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO nodes (id, vault_id, type, content_hash, path, version, tier, importance, extra) \
         VALUES (?1, ?2, 'Document', 'h', 'inbox/resolved-mention.md', 1, 'warm', 0.5, \
         'name: resolved mention note\n')",
        [resolved, vault],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO nodes (id, vault_id, type, content_hash, path, version, tier, importance, extra) \
         VALUES ('88888888-8888-8888-8888-888888888888', ?1, 'Document', 'h', 'notes/nested.md', 1, 'warm', 0.5, \
         'tags:\n- a\n- b\nversion: 2\nmeta:\n  k: v\nflag: true\nempty: ~\nwhen: 2026-08-25\nratio: 1.0\n')",
        [vault],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO edges (id, vault_id, from_id, to_id, to_raw, type, properties) \
         VALUES ('55555555-5555-5555-5555-555555555555', ?1, ?2, NULL, 'GhostLink', 'mentions', '{}')",
        [vault, lattice],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO edges (id, vault_id, from_id, to_id, to_raw, type, properties) \
         VALUES ('66666666-6666-6666-6666-666666666666', ?1, ?2, NULL, 'OtherGhost', 'mentions', '{}')",
        [vault, lattice],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO edges (id, vault_id, from_id, to_id, to_raw, type, properties) \
         VALUES ('77777777-7777-7777-7777-777777777777', ?1, ?2, ?3, 'hedron-foundry', 'mentions', '{}')",
        [vault, resolved, foundry],
    )
    .unwrap();
}

/// Session fixture built through `Store` (same shape as `tests/hql.rs`):
/// one desired state named `deploy`, reconciled twice.
fn build_session_db(path: &Path) {
    let mut store = Store::open(path).unwrap();
    let boot = store.bootstrap("prod", "deploy", "agents/deploy").unwrap();
    let token = boot.token;
    let vault_id = boot.vault.id;
    let hello = Node::document(
        vault_id,
        Some("notes/hello.md"),
        serde_yaml::from_str("name: hello\n").unwrap(),
    )
    .unwrap();
    store.put_node(&token, hello).unwrap();
    let ops = Node {
        node_type: NodeType::Agent,
        tier: Tier::Hot,
        importance: 1.0,
        htec_path: Some("agents/ops".into()),
        ..Node::document(
            vault_id,
            Some("agents/ops"),
            serde_yaml::from_str("title: ops\n").unwrap(),
        )
        .unwrap()
    };
    store.put_node(&token, ops).unwrap();
    let spec = DesiredState::docs_eod_spec("2026-08-25", &["alpha"]).unwrap();
    let ds = store.put_desired_state(&token, "deploy", spec, 0.8).unwrap();
    store.reconcile(&token, ds.id).unwrap();
    let alpha = Node::brief_document(vault_id, "alpha", "2026-08-25").unwrap();
    store.put_node(&token, alpha).unwrap();
    store.reconcile(&token, ds.id).unwrap();
    let eod = store
        .put_desired_state(
            &token,
            "eod-2026-08-25",
            DesiredState::docs_eod_spec("2026-08-25", &["alpha", "beta"]).unwrap(),
            0.4,
        )
        .unwrap();
    store.reconcile(&token, eod.id).unwrap();
}

fn run_hedron_hql(db: &Path, pipeline: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_hedron"))
        .args([
            "hql",
            "--db",
            db.to_str().unwrap(),
            "--format",
            "json",
            pipeline,
        ])
        .output()
        .expect("run hedron hql");
    assert!(
        out.status.success(),
        "hedron hql failed: {}{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn run_python_hql(db: &Path, pipeline: &str) -> String {
    let out = Command::new("python3")
        .args([
            "-m",
            "hql",
            "--db",
            db.to_str().unwrap(),
            "--format",
            "json",
            pipeline,
        ])
        .current_dir(python_hql_dir())
        .output()
        .expect("run python hql");
    assert!(
        out.status.success(),
        "python hql failed: {}{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn json_values_equal(rust_json: &str, python_json: &str) -> Result<(), String> {
    let dir = std::env::temp_dir().join(format!(
        "hedron-twin-cmp-{}-{}",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    let rust_path = dir.join("rust.json");
    let py_path = dir.join("python.json");
    fs::write(&rust_path, rust_json).unwrap();
    fs::write(&py_path, python_json).unwrap();
    let script = r#"
import json, sys
a = json.load(open(sys.argv[1], encoding="utf-8"))
b = json.load(open(sys.argv[2], encoding="utf-8"))
if a != b:
    sys.stderr.write("rust=%r\npython=%r\n" % (a, b))
    sys.exit(1)
"#;
    let out = Command::new("python3")
        .args([
            "-c",
            script,
            rust_path.to_str().unwrap(),
            py_path.to_str().unwrap(),
        ])
        .output()
        .expect("compare json");
    let _ = fs::remove_dir_all(&dir);
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

fn assert_twins(db: &Path, pipeline: &str) {
    let rust_json = run_hedron_hql(db, pipeline);
    let python_json = run_python_hql(db, pipeline);
    json_values_equal(&rust_json, &python_json).unwrap_or_else(|err| {
        panic!("twin mismatch for {pipeline:?}\nrust={rust_json}\npython={python_json}\n{err}")
    });
}

#[test]
fn rust_and_python_agree_on_demo_vault_pipes() {
    let db = TempDb::new("pipes");
    build_tiny_db(&db.path);
    for pipeline in [
        PIPE_FOUNDRY,
        PIPE_DANGLING,
        PIPE_RESOLVED,
        PIPE_YAML_EXTRA,
        PIPE_YAML_FILTER,
    ] {
        assert_twins(&db.path, pipeline);
    }
}

#[test]
fn rust_and_python_agree_on_session_pipes() {
    let db = TempDb::new("session");
    build_session_db(&db.path);
    for pipeline in [
        PIPE_SESSION,
        PIPE_AGENT_STATE,
        PIPE_NO_EVENTS,
        PIPE_HISTORY,
        PIPE_HISTORY_SELECT,
        PIPE_ALL_STATES,
        PIPE_ALL_STATES_SELECT,
        PIPE_ONE_STATE,
        PIPE_OPS_EXPLICIT,
        PIPE_OPS_NONE,
        PIPE_NO_SUBJECT,
        PIPE_NO_SUBJECT_HISTORY,
        PIPE_STATE_NO_EVENTS,
    ] {
        assert_twins(&db.path, pipeline);
    }
}

#[test]
fn rust_and_python_agree_agent_stays_in_vault() {
    let db = TempDb::new("vault-slice");
    build_session_db(&db.path);
    assert_twins(
        &db.path,
        "vault prod | agent deploy | select path, extra.title",
    );
    assert_twins(
        &db.path,
        "vault no-such-vault | agent deploy | select path, extra.title",
    );
}
