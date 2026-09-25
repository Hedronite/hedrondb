use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempTree {
    path: PathBuf,
}

impl TempTree {
    fn new(label: &str) -> Self {
        let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hedron-import-{}-{}-{}",
            label,
            std::process::id(),
            n
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn import_resolved_and_dangling_mentions() {
    let src = TempTree::new("src");
    fs::write(
        src.path.join("alpha.md"),
        "---\nname: Alpha\n---\nSee [[beta]].\n",
    )
    .unwrap();
    fs::write(
        src.path.join("beta.md"),
        "---\nname: Beta\n---\nDangling [[no-such-note]].\n",
    )
    .unwrap();

    let out = TempTree::new("out");
    let db = out.path.join("store.db");
    let result = Command::new(env!("CARGO_BIN_EXE_hedron-import"))
        .args([
            "--src",
            src.path.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            "--vault",
            "fixture",
            "--agent",
            "importer",
        ])
        .output()
        .expect("run hedron-import");
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.success(), "import failed: {stderr}{stdout}");
    assert!(
        !stdout.contains("hdt_") && !stderr.contains("hdt_"),
        "token leaked: {stdout}{stderr}"
    );
    assert!(stdout.contains("documents: 2"));
    assert!(stdout.contains("mentions_resolved: 1"));
    assert!(stdout.contains("mentions_dangling: 1"));
    assert!(stdout.contains("mode: 0600"));

    let conn = Connection::open(&db).unwrap();
    let nodes: i64 = conn
        .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))
        .unwrap();
    let edges: i64 = conn
        .query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0))
        .unwrap();
    let dangling: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE type = 'mentions' AND to_id IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let resolved: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE type = 'mentions' AND to_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(nodes, 4, "vault + agent + 2 documents");
    assert_eq!(edges, 2);
    assert_eq!(dangling, 1);
    assert_eq!(resolved, 1);

    let mode = fs::metadata(&db).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    let events: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(events, 0, "HAL supersedes must not become Event.supersedes");
}

#[test]
fn import_refuses_existing_db_without_force() {
    let src = TempTree::new("src-refuse");
    fs::write(src.path.join("only.md"), "# hi\n").unwrap();
    let out = TempTree::new("out-refuse");
    let db = out.path.join("store.db");
    fs::write(&db, b"already").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_hedron-import"))
        .args([
            "--src",
            src.path.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            "--vault",
            "fixture",
            "--agent",
            "importer",
        ])
        .output()
        .expect("run hedron-import");
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("refuse existing store"));
}

#[test]
fn import_yaml_fence_after_deprecated_hal_comment() {
    let src = TempTree::new("src-hal-shim");
    fs::write(
        src.path.join("OPERATOR.md"),
        "<!-- hal:authoritative:yaml -->\n\
         ---\n\
         name: Leo OPERATOR\n\
         title: Operator\n\
         domain: foundry\n\
         supersedes: old.md\n\
         token: should-drop\n\
         ---\n\
         Body of Leo.\n",
    )
    .unwrap();

    let out = TempTree::new("out-hal-shim");
    let db = out.path.join("store.db");
    let result = Command::new(env!("CARGO_BIN_EXE_hedron-import"))
        .args([
            "--src",
            src.path.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            "--vault",
            "fixture",
            "--agent",
            "importer",
        ])
        .output()
        .expect("run hedron-import");
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.success(), "import failed: {stderr}{stdout}");

    let conn = Connection::open(&db).unwrap();
    let extra: String = conn
        .query_row(
            "SELECT extra FROM nodes WHERE type = 'Document' AND path = 'OPERATOR.md'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        extra.contains("name: Leo OPERATOR"),
        "name from YAML fence must land in extra: {extra}"
    );
    assert!(
        extra.contains("title: Operator"),
        "title from YAML fence must land in extra: {extra}"
    );
    assert!(
        extra.contains("domain: foundry"),
        "domain from YAML fence must land in extra: {extra}"
    );
    assert!(
        extra.contains("supersedes: old.md"),
        "HAL supersedes stays in extra: {extra}"
    );
    assert!(
        !extra.contains("token"),
        "secret keys must be dropped: {extra}"
    );
    assert!(
        !extra.contains("hal:authoritative"),
        "deprecated html comment must not be stored in extra: {extra}"
    );

    let events: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(events, 0, "HAL supersedes must not become Event.supersedes");
    let event_supersedes: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE supersedes IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(event_supersedes, 0);
}

#[test]
fn import_hal_comment_without_yaml_fence_leaves_extra_empty() {
    let src = TempTree::new("src-hal-empty");
    fs::write(
        src.path.join("Leo-OPERATOR.md"),
        "<!-- hal:authoritative:yaml -->\n\
         name: Leo OPERATOR\n\
         domain: foundry\n\
         supersedes: old.md\n\
         Body without a fence.\n",
    )
    .unwrap();

    let out = TempTree::new("out-hal-empty");
    let db = out.path.join("store.db");
    let result = Command::new(env!("CARGO_BIN_EXE_hedron-import"))
        .args([
            "--src",
            src.path.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            "--vault",
            "fixture",
            "--agent",
            "importer",
        ])
        .output()
        .expect("run hedron-import");
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.success(), "import failed: {stderr}{stdout}");

    let conn = Connection::open(&db).unwrap();
    let extra: String = conn
        .query_row(
            "SELECT extra FROM nodes WHERE type = 'Document' AND path = 'Leo-OPERATOR.md'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        !extra.contains("Leo OPERATOR")
            && !extra.contains("foundry")
            && !extra.contains("old.md")
            && !extra.contains("hal:authoritative"),
        "html comment is not frontmatter; extra must stay empty of those keys: {extra}"
    );
    assert!(
        !extra.contains("name:") && !extra.contains("domain:"),
        "do not invent extra.name or extra.domain from the path: {extra}"
    );

    let events: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(events, 0, "HAL supersedes must not become Event.supersedes");
}

#[test]
fn import_rolls_back_leaving_no_half_graph() {
    let src = TempTree::new("src-rollback");
    fs::write(
        src.path.join("alpha.md"),
        "---\nname: Alpha\n---\nSee [[beta]].\n",
    )
    .unwrap();
    fs::write(
        src.path.join("beta.md"),
        "---\nname: Beta\n---\nBody.\n",
    )
    .unwrap();

    let out = TempTree::new("out-rollback");
    let db = out.path.join("store.db");
    let result = Command::new(env!("CARGO_BIN_EXE_hedron-import"))
        .env("HEDRON_IMPORT_TEST_ROLLBACK", "1")
        .args([
            "--src",
            src.path.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            "--vault",
            "fixture",
            "--agent",
            "importer",
        ])
        .output()
        .expect("run hedron-import");

    assert!(
        !result.status.success(),
        "import should fail on rollback test hook"
    );

    let conn = Connection::open(&db).unwrap();
    let nodes: i64 = conn
        .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))
        .unwrap();
    let edges: i64 = conn
        .query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0))
        .unwrap();
    assert_eq!(nodes, 0, "rolled-back import must leave no nodes");
    assert_eq!(edges, 0, "rolled-back import must leave no edges");
}

#[test]
fn import_briefs_date_writes_docs_eod_named_state() {
    let src = TempTree::new("src-briefs");
    fs::create_dir_all(src.path.join("inbox")).unwrap();
    fs::write(
        src.path.join("inbox/alpha.md"),
        "---\nbrief: alpha\ndate: 2026-09-11\n---\nBody.\n",
    )
    .unwrap();

    let out = TempTree::new("out-briefs");
    let db = out.path.join("store.db");
    let result = Command::new(env!("CARGO_BIN_EXE_hedron-import"))
        .args([
            "--src",
            src.path.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            "--vault",
            "fixture",
            "--agent",
            "importer",
            "--briefs-date",
            "2026-09-11",
        ])
        .output()
        .expect("run hedron-import");
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.success(), "import failed: {stderr}{stdout}");

    let conn = Connection::open(&db).unwrap();
    let (name, spec): (String, String) = conn
        .query_row(
            "SELECT name, spec FROM desired_states LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(name, "eod-2026-09-11");
    assert!(
        spec.contains("kind: docs_eod"),
        "spec must include kind docs_eod: {spec}"
    );

    let events: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(events, 1, "reconcile-only import should append one event");
}
