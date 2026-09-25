//! Same session/pipe cases as `python/hql/tests`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use hedron_core::hql::{run_pipeline, Query, RoStore, Value};
use hedron_core::{DesiredState, Node, NodeType, Store, Tier};
use rusqlite::Connection;
use uuid::Uuid;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Fixtures execute the crate `schema.sql`, never a private copy.
const SCHEMA: &str = hedron_core::SCHEMA_SQL;

const VAULT_PIPE: &str = "11111111-1111-1111-1111-111111111111";
const FOUNDRY: &str = "22222222-2222-2222-2222-222222222222";
const LATTICE: &str = "33333333-3333-3333-3333-333333333333";
const RESOLVED: &str = "44444444-4444-4444-4444-444444444444";
const EDGE_D1: &str = "55555555-5555-5555-5555-555555555555";
const EDGE_D2: &str = "66666666-6666-6666-6666-666666666666";
const EDGE_R: &str = "77777777-7777-7777-7777-777777777777";
const NESTED: &str = "88888888-8888-8888-8888-888888888888";

const PIPE_FOUNDRY: &str = r#"vault demo-vault | search "HedronDB" | filter extra.domain == "foundry" | select path, extra.name | limit 20"#;
const PIPE_DANGLING: &str = r#"vault demo-vault | search "lattice edges" | traverse --edge mentions --hops 1 | filter to_id == null | select path, to_raw"#;
const PIPE_RESOLVED: &str = r#"vault demo-vault | filter path ^= "inbox/" && path !^= "inbox/private/" | traverse --edge mentions --hops 1 | filter to_id != null | select from.path, to.path"#;

const PIPE_SESSION: &str =
    "vault prod | agent deploy | state | select path, extra.name, extra.title, state_version, status";

struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new(label: &str) -> Self {
        let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hedron-hql-{}-{}-{}.db",
            label,
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        Self { path }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn build_tiny_db(path: &std::path::Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    conn.execute(
        "INSERT INTO nodes (id, vault_id, type, content_hash, path, version, tier, importance, extra) \
         VALUES (?1, ?2, 'Vault', 'h', ?3, 1, 'cool', 1.0, ?4)",
        rusqlite::params![VAULT_PIPE, VAULT_PIPE, "demo-vault", "name: demo-vault\n"],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO nodes (id, vault_id, type, content_hash, path, version, tier, importance, extra) \
         VALUES (?1, ?2, 'Document', 'h', ?3, 1, 'warm', 0.5, ?4)",
        rusqlite::params![
            FOUNDRY,
            VAULT_PIPE,
            "inbox/hedron-foundry.md",
            "name: HedronDB kernel notes\ndomain: foundry\n",
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO nodes (id, vault_id, type, content_hash, path, version, tier, importance, extra) \
         VALUES (?1, ?2, 'Document', 'h', ?3, 1, 'warm', 0.5, ?4)",
        rusqlite::params![
            LATTICE,
            VAULT_PIPE,
            "notes/lattice-edges.md",
            "name: lattice edges walk\n",
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO nodes (id, vault_id, type, content_hash, path, version, tier, importance, extra) \
         VALUES (?1, ?2, 'Document', 'h', ?3, 1, 'warm', 0.5, ?4)",
        rusqlite::params![
            RESOLVED,
            VAULT_PIPE,
            "inbox/resolved-mention.md",
            "name: resolved mention note\n",
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO nodes (id, vault_id, type, content_hash, path, version, tier, importance, extra) \
         VALUES (?1, ?2, 'Document', 'h', ?3, 1, 'warm', 0.5, ?4)",
        rusqlite::params![
            NESTED,
            VAULT_PIPE,
            "notes/nested.md",
            "tags:\n- a\n- b\nversion: 2\nmeta:\n  k: v\nflag: true\nempty: ~\nwhen: 2026-08-25\n",
        ],
    )
    .unwrap();
    for (edge_id, to_raw) in [(EDGE_D1, "GhostLink"), (EDGE_D2, "OtherGhost")] {
        conn.execute(
            "INSERT INTO edges (id, vault_id, from_id, to_id, to_raw, type, properties) \
             VALUES (?1, ?2, ?3, NULL, ?4, 'mentions', '{}')",
            rusqlite::params![edge_id, VAULT_PIPE, LATTICE, to_raw],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO edges (id, vault_id, from_id, to_id, to_raw, type, properties) \
         VALUES (?1, ?2, ?3, ?4, 'hedron-foundry', 'mentions', '{}')",
        rusqlite::params![EDGE_R, VAULT_PIPE, RESOLVED, FOUNDRY],
    )
    .unwrap();
}

/// Ids minted by `Store` while building the session fixture.
struct SessionIds {
    agent: Uuid,
    ds: Uuid,
    ev1: Uuid,
    ev2: Uuid,
    eod: Uuid,
}

/// Warm/cool session fixture built through `Store`: vault `prod`, agent
/// `deploy` (extra.name), a title-only agent `ops`, one document, and TWO
/// named desired states: `deploy` reconciled twice (version 3, two events)
/// and `eod-2026-08-25` reconciled once.
fn build_session_db(path: &std::path::Path) -> SessionIds {
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

    // H.TEC agents often set title and omit name.
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
    let (_, ev1) = store.reconcile(&token, ds.id).unwrap();
    let alpha = Node::brief_document(vault_id, "alpha", "2026-08-25").unwrap();
    store.put_node(&token, alpha).unwrap();
    let (_, ev2) = store.reconcile(&token, ds.id).unwrap();
    let eod = store
        .put_desired_state(
            &token,
            "eod-2026-08-25",
            DesiredState::docs_eod_spec("2026-08-25", &["alpha", "beta"]).unwrap(),
            0.4,
        )
        .unwrap();
    store.reconcile(&token, eod.id).unwrap();

    SessionIds {
        agent: boot.agent.id,
        ds: ds.id,
        ev1: ev1.id,
        ev2: ev2.id,
        eod: eod.id,
    }
}

fn s(row: &hedron_core::hql::Row, field: &str) -> Option<String> {
    match row.get(field) {
        Value::Null => None,
        other => Some(other.to_display()),
    }
}

#[test]
fn pipe_schema_matches_crate() {
    let db = TempDb::new("tiny-schema");
    build_tiny_db(&db.path);
    assert_eq!(
        RoStore::open(&db.path)
            .unwrap()
            .schema_mismatches()
            .unwrap(),
        Vec::<String>::new()
    );
}

#[test]
fn pipe_foundry_hedron() {
    let db = TempDb::new("tiny-foundry");
    build_tiny_db(&db.path);
    let rows = run_pipeline(&db.path, PIPE_FOUNDRY).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        s(&rows[0], "path").as_deref(),
        Some("inbox/hedron-foundry.md")
    );
    assert!(s(&rows[0], "extra.name")
        .unwrap_or_default()
        .contains("HedronDB"));
}

#[test]
fn pipe_dangling_lattice_mentions() {
    let db = TempDb::new("tiny-dangling");
    build_tiny_db(&db.path);
    let rows = run_pipeline(&db.path, PIPE_DANGLING).unwrap();
    let raws: std::collections::HashSet<String> = rows
        .iter()
        .map(|row| s(row, "to_raw").unwrap_or_default())
        .collect();
    assert_eq!(
        raws,
        ["GhostLink".to_string(), "OtherGhost".to_string()]
            .into_iter()
            .collect()
    );
    assert!(rows
        .iter()
        .all(|row| s(row, "path").as_deref() == Some("notes/lattice-edges.md")));
}

#[test]
fn pipe_resolved_inbox_mention() {
    let db = TempDb::new("tiny-resolved");
    build_tiny_db(&db.path);
    let rows = run_pipeline(&db.path, PIPE_RESOLVED).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        s(&rows[0], "from.path").as_deref(),
        Some("inbox/resolved-mention.md")
    );
    assert_eq!(
        s(&rows[0], "to.path").as_deref(),
        Some("inbox/hedron-foundry.md")
    );
}

#[test]
fn missing_domain_dropped_by_filter_kept_by_search() {
    let db = TempDb::new("tiny-domain");
    build_tiny_db(&db.path);
    let kept = Query::open(&db.path)
        .unwrap()
        .vault("demo-vault")
        .search("lattice edges")
        .select(["path", "extra.domain"])
        .run()
        .unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(
        s(&kept[0], "path").as_deref(),
        Some("notes/lattice-edges.md")
    );
    assert_eq!(kept[0].get("extra.domain"), Value::Null);

    let dropped = Query::open(&db.path)
        .unwrap()
        .vault("demo-vault")
        .search("lattice edges")
        .filter(r#"extra.domain == "foundry""#)
        .run()
        .unwrap();
    assert!(dropped.is_empty());
}

#[test]
fn fluent_matches_foundry_pipe() {
    let db = TempDb::new("tiny-fluent");
    build_tiny_db(&db.path);
    let rows = Query::open(&db.path)
        .unwrap()
        .vault("demo-vault")
        .search("HedronDB")
        .filter(r#"extra.domain == "foundry""#)
        .select(["path", "extra.name"])
        .limit(20)
        .run()
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| s(row, "path").unwrap_or_default())
            .collect::<Vec<_>>(),
        vec!["inbox/hedron-foundry.md".to_string()]
    );
}

#[test]
fn session_schema_matches_crate() {
    let db = TempDb::new("session-schema");
    build_session_db(&db.path);
    assert_eq!(
        RoStore::open(&db.path)
            .unwrap()
            .schema_mismatches()
            .unwrap(),
        Vec::<String>::new()
    );
}

#[test]
fn agent_state_pipe_returns_current_version() {
    let db = TempDb::new("session-current");
    let ids = build_session_db(&db.path);
    let rows = run_pipeline(&db.path, "agent deploy | state").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("state_version"), Value::Int(3));
    let status = s(&rows[0], "status").unwrap_or_default();
    assert!(status.contains("Reconciled"));
    assert!(!status.contains("Pending"));
    assert_eq!(s(&rows[0], "id"), Some(ids.ds.to_string()));
    assert_eq!(s(&rows[0], "reconciled_by"), Some(ids.agent.to_string()));
    assert_eq!(rows[0].get("importance"), Value::Float(0.8));
}

#[test]
fn session_pipe_selects_agent_name_and_title() {
    let db = TempDb::new("session-title");
    build_session_db(&db.path);
    let rows = run_pipeline(&db.path, PIPE_SESSION).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(s(&rows[0], "path").as_deref(), Some("deploy"));
    assert_eq!(s(&rows[0], "extra.name").as_deref(), Some("deploy"));
    assert_eq!(rows[0].get("extra.title"), Value::Null);
    assert_eq!(rows[0].get("state_version"), Value::Int(3));
    assert!(s(&rows[0], "status")
        .unwrap_or_default()
        .contains("Reconciled"));

    // Title-only agent: matched by extra.title, name stays null.
    let ops = run_pipeline(
        &db.path,
        "vault prod | agent ops | select path, extra.name, extra.title",
    )
    .unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(s(&ops[0], "path").as_deref(), Some("agents/ops"));
    assert_eq!(ops[0].get("extra.name"), Value::Null);
    assert_eq!(s(&ops[0], "extra.title").as_deref(), Some("ops"));
}

#[test]
fn fluent_matches_session_pipe() {
    let db = TempDb::new("session-fluent");
    build_session_db(&db.path);
    let rows = Query::open(&db.path)
        .unwrap()
        .vault("prod")
        .agent("deploy")
        .state()
        .select([
            "path",
            "extra.name",
            "extra.title",
            "state_version",
            "status",
        ])
        .run()
        .unwrap();
    let piped = run_pipeline(&db.path, PIPE_SESSION).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(s(&rows[0], "path"), s(&piped[0], "path"));
    assert_eq!(rows[0].get("extra.name"), piped[0].get("extra.name"));
    assert_eq!(s(&rows[0], "extra.title"), s(&piped[0], "extra.title"));
    assert_eq!(rows[0].get("state_version"), piped[0].get("state_version"));
    assert_eq!(s(&rows[0], "status"), s(&piped[0], "status"));
}

#[test]
fn agent_stays_inside_vault_slice() {
    let db = TempDb::new("session-vault");
    build_session_db(&db.path);
    let inside = run_pipeline(&db.path, "vault prod | agent deploy").unwrap();
    assert_eq!(inside.len(), 1);
    let outside = run_pipeline(&db.path, "vault no-such-vault | agent deploy").unwrap();
    assert!(outside.is_empty());
    let global_hit = run_pipeline(&db.path, "agent deploy").unwrap();
    assert_eq!(global_hit.len(), 1);
}

#[test]
fn agent_prefers_exact_name_and_accepts_substring() {
    let db = TempDb::new("session-substr");
    build_session_db(&db.path);
    let exact = run_pipeline(&db.path, "agent deploy").unwrap();
    assert_eq!(exact.len(), 1);
    assert_eq!(s(&exact[0], "extra.name").as_deref(), Some("deploy"));
    let substr = run_pipeline(&db.path, "agent dep").unwrap();
    assert_eq!(substr.len(), 1);
    assert_eq!(s(&substr[0], "path").as_deref(), Some("deploy"));
    let title_substr = run_pipeline(&db.path, "agent op").unwrap();
    assert_eq!(title_substr.len(), 1);
    assert_eq!(s(&title_substr[0], "path").as_deref(), Some("agents/ops"));
}

#[test]
fn agent_does_not_match_document() {
    let db = TempDb::new("session-doc");
    build_session_db(&db.path);
    assert!(run_pipeline(&db.path, "agent hello").unwrap().is_empty());
}

#[test]
fn state_does_not_pull_event_log_or_tokens() {
    let db = TempDb::new("session-no-events");
    let ids = build_session_db(&db.path);
    let rows = run_pipeline(
        &db.path,
        "vault prod | agent deploy | state | select path, spec, status, state_version, id",
    )
    .unwrap();
    assert_eq!(rows.len(), 1);
    let blob = rows[0]
        .as_dict(None)
        .into_iter()
        .map(|(_, v)| v.to_display())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(!blob.contains("hdt_"));
    assert!(!blob.contains(&ids.ev1.to_string()));
    assert!(!blob.contains(&ids.ev2.to_string()));
}

#[test]
fn default_columns_hide_spec_and_status() {
    let db = TempDb::new("session-hide");
    build_session_db(&db.path);
    let rows = run_pipeline(&db.path, "agent deploy | state").unwrap();
    let fields = hedron_core::hql::fields_of(&rows);
    assert!(!fields.iter().any(|f| f == "spec" || f == "status"));
    assert!(s(&rows[0], "status")
        .unwrap_or_default()
        .contains("Reconciled"));
}

#[test]
fn causal_operator_is_out_of_scope() {
    let db = TempDb::new("session-causal");
    build_session_db(&db.path);
    let err = run_pipeline(&db.path, "agent deploy | causal").unwrap_err();
    assert!(err.to_string().contains("unknown operator"));
}

#[test]
fn history_shows_every_superseded_version_of_one_id() {
    let db = TempDb::new("session-history");
    let ids = build_session_db(&db.path);
    let state = run_pipeline(&db.path, "vault prod | agent deploy | state").unwrap();
    assert_eq!(state.len(), 1);
    assert_eq!(state[0].get("state_version"), Value::Int(3));
    assert_eq!(s(&state[0], "id"), Some(ids.ds.to_string()));

    let rows = run_pipeline(&db.path, "vault prod | agent deploy | history").unwrap();
    assert_eq!(rows.len(), 2);
    let event_ids: Vec<String> = rows
        .iter()
        .map(|row| s(row, "id").unwrap_or_default())
        .collect();
    assert_eq!(event_ids, vec![ids.ev1.to_string(), ids.ev2.to_string()]);
    let supersedes: Vec<String> = rows
        .iter()
        .map(|row| s(row, "supersedes").unwrap_or_default())
        .collect();
    assert_eq!(
        supersedes,
        vec![format!("{}@1", ids.ds), format!("{}@2", ids.ds)]
    );
    assert!(rows
        .iter()
        .all(|row| s(row, "reconciles") == Some(ids.ds.to_string())));
}

#[test]
fn history_does_not_bleed_spec_status_or_payloads() {
    let db = TempDb::new("session-history-hygiene");
    build_session_db(&db.path);
    let rows = run_pipeline(
        &db.path,
        "vault prod | agent deploy | history | select id, spec, status, state_version, name, data, reconciles, supersedes",
    )
    .unwrap();
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row.get("name"), Value::Null);
        assert_eq!(row.get("spec"), Value::Null);
        assert_eq!(row.get("status"), Value::Null);
        assert_eq!(row.get("state_version"), Value::Null);
        assert_eq!(row.get("data"), Value::Null);
        let blob = row
            .as_dict(None)
            .into_iter()
            .map(|(_, v)| v.to_display())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!blob.contains("hdt_"));
        assert!(!blob.contains("present"));
        assert!(!blob.contains("missing"));
        assert!(!blob.contains("Pending"));
        assert!(!blob.contains("conditions:"));
    }
}

#[test]
fn fluent_matches_history_pipe() {
    let db = TempDb::new("session-history-fluent");
    build_session_db(&db.path);
    let rows = Query::open(&db.path)
        .unwrap()
        .vault("prod")
        .agent("deploy")
        .history()
        .select(["id", "ts", "reconciles", "supersedes"])
        .run()
        .unwrap();
    let piped = run_pipeline(
        &db.path,
        "vault prod | agent deploy | history | select id, ts, reconciles, supersedes",
    )
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(s(&rows[0], "id"), s(&piped[0], "id"));
    assert_eq!(rows[0].get("ts"), piped[0].get("ts"));
    assert_eq!(s(&rows[1], "id"), s(&piped[1], "id"));
}

#[test]
fn history_matches_store_causal_chain() {
    let db = TempDb::new("chain");
    let mut store = Store::open(&db.path).unwrap();
    let boot = store.bootstrap("prod", "deploy", "agents/deploy").unwrap();
    let spec = DesiredState::docs_eod_spec("2026-08-25", &["alpha"]).unwrap();
    let ds = store
        .put_desired_state(&boot.token, "deploy", spec, 0.5)
        .unwrap();
    let doc = Node::brief_document(boot.vault.id, "alpha", "2026-08-25").unwrap();
    store.put_node(&boot.token, doc).unwrap();
    let (_, ev1) = store.reconcile(&boot.token, ds.id).unwrap();
    let (_, ev2) = store.reconcile(&boot.token, ds.id).unwrap();
    let chain = store.causal_chain(&boot.token, ds.id).unwrap();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0].id, ev1.id);
    assert_eq!(chain[1].id, ev2.id);
    drop(store);

    let state = run_pipeline(&db.path, "vault prod | agent deploy | state").unwrap();
    assert_eq!(state.len(), 1);
    assert_eq!(state[0].get("state_version"), Value::Int(3));

    let history = run_pipeline(&db.path, "vault prod | agent deploy | history").unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(s(&history[0], "id"), Some(ev1.id.to_string()));
    assert_eq!(s(&history[1], "id"), Some(ev2.id.to_string()));
    assert_eq!(s(&history[0], "supersedes"), Some(format!("{}@1", ds.id)));
    assert_eq!(s(&history[1], "supersedes"), Some(format!("{}@2", ds.id)));
    for row in &history {
        assert_eq!(row.get("spec"), Value::Null);
        assert_eq!(row.get("status"), Value::Null);
    }
}

#[test]
fn hql_opens_read_only() {
    let db = TempDb::new("ro");
    build_tiny_db(&db.path);
    let before = std::fs::read(&db.path).unwrap();
    run_pipeline(&db.path, PIPE_RESOLVED).unwrap();
    run_pipeline(&db.path, "vault demo-vault | state").unwrap();
    assert_eq!(std::fs::read(&db.path).unwrap(), before, "HQL must not touch the file");
    let _store = RoStore::open(&db.path).unwrap();
    let write = Connection::open_with_flags(&db.path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY);
    let conn = write.unwrap();
    let failed = conn.execute("UPDATE nodes SET extra = 'x' WHERE 1=1", []);
    assert!(failed.is_err());
}

#[test]
fn vault_state_lists_all_named_desired_states() {
    let db = TempDb::new("session-all-states");
    let ids = build_session_db(&db.path);
    let rows = run_pipeline(&db.path, "vault prod | state").unwrap();
    let names: Vec<String> = rows
        .iter()
        .map(|row| s(row, "name").unwrap_or_default())
        .collect();
    assert_eq!(names, vec!["deploy".to_string(), "eod-2026-08-25".to_string()]);
    assert_eq!(s(&rows[0], "id"), Some(ids.ds.to_string()));
    assert_eq!(s(&rows[1], "id"), Some(ids.eod.to_string()));
    assert_eq!(rows[0].get("state_version"), Value::Int(3));
    assert_eq!(rows[1].get("state_version"), Value::Int(2));
    // Subject is the vault node.
    assert!(rows.iter().all(|row| s(row, "path").as_deref() == Some("prod")));
    let fields = hedron_core::hql::fields_of(&rows);
    assert!(fields.iter().any(|f| f == "name"));
    assert!(!fields.iter().any(|f| f == "spec" || f == "status"));

    let one = run_pipeline(&db.path, "vault prod | state eod-2026-08-25").unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(s(&one[0], "id"), Some(ids.eod.to_string()));
    assert!(run_pipeline(&db.path, "vault prod | state no-such")
        .unwrap()
        .is_empty());
}

#[test]
fn agent_state_selects_desired_state_named_after_agent() {
    let db = TempDb::new("session-agent-state");
    let ids = build_session_db(&db.path);
    let rows = run_pipeline(&db.path, "vault prod | agent deploy | state").unwrap();
    assert_eq!(rows.len(), 1, "agent deploy selects only the DS named deploy");
    assert_eq!(s(&rows[0], "name").as_deref(), Some("deploy"));
    assert_eq!(s(&rows[0], "id"), Some(ids.ds.to_string()));
    assert_eq!(s(&rows[0], "path").as_deref(), Some("deploy"));

    // No desired state is named after the title-only agent.
    assert!(run_pipeline(&db.path, "vault prod | agent ops | state")
        .unwrap()
        .is_empty());
    // ...but an explicit name still works under that agent.
    let explicit = run_pipeline(&db.path, "vault prod | agent ops | state deploy").unwrap();
    assert_eq!(explicit.len(), 1);
    assert_eq!(s(&explicit[0], "path").as_deref(), Some("agents/ops"));
    assert_eq!(s(&explicit[0], "id"), Some(ids.ds.to_string()));
}

#[test]
fn state_name_works_without_agent_or_vault_node() {
    let db = TempDb::new("session-state-no-subject");
    let ids = build_session_db(&db.path);
    let rows = run_pipeline(
        &db.path,
        r#"vault prod | filter path ^= "notes/" | state deploy"#,
    )
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("path"), Value::Null, "no subject node");
    assert_eq!(s(&rows[0], "name").as_deref(), Some("deploy"));
    assert_eq!(s(&rows[0], "id"), Some(ids.ds.to_string()));
    assert!(matches!(rows[0], hedron_core::hql::Row::State { subject: None, .. }));

    let history = run_pipeline(
        &db.path,
        r#"vault prod | filter path ^= "notes/" | history deploy"#,
    )
    .unwrap();
    let event_ids: Vec<String> = history
        .iter()
        .map(|row| s(row, "id").unwrap_or_default())
        .collect();
    assert_eq!(event_ids, vec![ids.ev1.to_string(), ids.ev2.to_string()]);
}

#[test]
fn state_rows_carry_no_event_fields() {
    let db = TempDb::new("session-state-no-events");
    build_session_db(&db.path);
    let rows = run_pipeline(
        &db.path,
        "vault prod | agent deploy | state | select ts, actor, caused_by, supersedes, reconciles, name",
    )
    .unwrap();
    assert_eq!(rows.len(), 1);
    for field in ["ts", "actor", "caused_by", "supersedes", "reconciles"] {
        assert_eq!(rows[0].get(field), Value::Null, "{field} is cool-only");
    }
    assert_eq!(s(&rows[0], "name").as_deref(), Some("deploy"));
}

#[test]
fn extra_is_real_yaml() {
    let db = TempDb::new("tiny-yaml");
    build_tiny_db(&db.path);
    let rows = run_pipeline(
        &db.path,
        r#"vault demo-vault | filter path == "notes/nested.md" | select extra.tags, extra.version, extra.meta, extra.flag, extra.empty, extra.when, extra.missing"#,
    )
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(s(&rows[0], "extra.tags").as_deref(), Some("[a, b]"));
    assert_eq!(s(&rows[0], "extra.version").as_deref(), Some("2"));
    assert_eq!(s(&rows[0], "extra.meta").as_deref(), Some("{k: v}"));
    assert_eq!(s(&rows[0], "extra.flag").as_deref(), Some("true"));
    assert_eq!(rows[0].get("extra.empty"), Value::Null);
    assert_eq!(s(&rows[0], "extra.when").as_deref(), Some("2026-08-25"));
    assert_eq!(rows[0].get("extra.missing"), Value::Null);
}
