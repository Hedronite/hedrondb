//! Golden G1–G9 for the read-only lesson-ship lattice source.
//!
//! Fixtures are built in a temp directory. Nothing here opens a live lattice.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use hedron_core::{
    parse_manifest, parse_timestamp, reconcile_lesson_ships, walk_watermark, ConditionKind,
    DesiredState, Error, LaneDue, LaneReport, LatticeSource, Node, SourceConfig, SCOPE_START,
};
use rusqlite::{Connection, OpenFlags};
use serde_yaml::Value;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

const DDL: &str = "
CREATE TABLE documents (
  doc_id INTEGER PRIMARY KEY,
  path TEXT UNIQUE NOT NULL,
  doc_type TEXT,
  frontmatter_json TEXT,
  mtime REAL,
  content_hash TEXT,
  indexed_at TEXT NOT NULL
);
CREATE TABLE index_state (
  id INTEGER PRIMARY KEY,
  last_reconcile_at TEXT,
  last_indexer_at TEXT,
  last_full_pass_at TEXT,
  document_count INTEGER
);
";

struct LatticeFixture {
    dir: PathBuf,
    path: PathBuf,
}

struct StoreFixture {
    store: hedron_core::Store,
    path: PathBuf,
    token: String,
}

struct Doc<'a> {
    path: &'a str,
    frontmatter: Option<&'a str>,
    indexed_at: &'a str,
}

impl LatticeFixture {
    fn build(docs: &[Doc<'_>], reconcile_at: &str, indexer_at: &str, full_pass_at: &str) -> Self {
        let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hedron-lattice-{}-{}", std::process::id(), n));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lattice.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(DDL).unwrap();
        for doc in docs {
            conn.execute(
                "INSERT INTO documents (path, doc_type, frontmatter_json, mtime, content_hash, indexed_at)
                 VALUES (?1, 'synthesis-lesson', ?2, 1.0, 'abc', ?3)",
                (doc.path, doc.frontmatter, doc.indexed_at),
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO index_state (id, last_reconcile_at, last_indexer_at, last_full_pass_at, document_count)
             VALUES (1, ?1, ?2, ?3, ?4)",
            (reconcile_at, indexer_at, full_pass_at, docs.len() as i64),
        )
        .unwrap();
        drop(conn);
        Self { dir, path }
    }

    fn open(&self) -> LatticeSource {
        LatticeSource::open(&SourceConfig::new(&self.path)).unwrap()
    }
}

impl Drop for LatticeFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

impl StoreFixture {
    fn new() -> Self {
        let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hedron-lesson-ship-{}-{}.db",
            std::process::id(),
            n
        ));
        let mut store = hedron_core::Store::open(&path).unwrap();
        let boot = store
            .bootstrap("lessons", "scribe", "agents/scribe")
            .unwrap();
        let token = boot.token;
        Self { store, path, token }
    }
}

impl Drop for StoreFixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn lane(date: &str, name: &str, check_at: Option<&str>, glob: Option<&str>) -> LaneDue {
    LaneDue {
        date: date.to_string(),
        lane: name.to_string(),
        check_at: check_at.map(str::to_string),
        lesson_glob: glob.map(str::to_string),
    }
}

fn report<'a>(reports: &'a [LaneReport], name: &str) -> &'a LaneReport {
    reports
        .iter()
        .find(|item| item.lane == name)
        .unwrap_or_else(|| panic!("missing lane {name}"))
}

fn scalar_count(path: &Path, sql: &str) -> i64 {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

fn status_of(path: &Path, name: &str) -> Value {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let raw: String = conn
        .query_row(
            "SELECT status FROM desired_states WHERE name = ?1",
            [name],
            |row| row.get(0),
        )
        .unwrap();
    serde_yaml::from_str(&raw).unwrap()
}

fn event_row(path: &Path) -> (String, String) {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    conn.query_row("SELECT type, caused_by FROM events", [], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })
    .unwrap()
}

fn fingerprint(path: &Path) -> (blake3::Hash, SystemTime) {
    let bytes = fs::read(path).unwrap();
    let modified = fs::metadata(path).unwrap().modified().unwrap();
    (blake3::hash(&bytes), modified)
}

fn seed_untouched(store: &mut StoreFixture) {
    let spec = DesiredState::docs_eod_spec("2026-09-25", &["brief"]).unwrap();
    store
        .store
        .put_desired_state(&store.token, "preexisting", spec, 0.5)
        .unwrap();
}

fn assert_seed_untouched(path: &Path) {
    assert_eq!(scalar_count(path, "SELECT count(*) FROM events"), 0);
    assert_eq!(scalar_count(path, "SELECT count(*) FROM desired_states"), 1);
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let version: i64 = conn
        .query_row(
            "SELECT state_version FROM desired_states WHERE name = 'preexisting'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, 1);
}

#[test]
fn g1_warm_two_lessons_and_close_note() {
    let rust = "Archmagus-Stack/Polyglot-Dev/Rust/2026-09-25-alpha/lesson.md";
    let nix = "Archmagus-Stack/Polyglot-Dev/Nix/2026-09-25-beta/lesson.md";
    let note = "agents/mail_room/Leo/2026-09-25-duha.md";
    let manifest = r#"
- date: 2026-09-25
  lane: duha
  registered_by: "2026-09-25T14:00:00Z"
  dev:
    source: Polyglot-Dev/Rust/2026-09-25-alpha/lesson.html
- date: 2026-09-25
  lane: duha
  registered_by: "2026-09-25T14:05:00Z"
  dev:
    source: Polyglot-Dev/Nix/2026-09-25-beta/lesson.html
"#;
    let lattice = LatticeFixture::build(
        &[
            Doc {
                path: rust,
                frontmatter: None,
                indexed_at: "2026-09-25T14:10:00Z",
            },
            Doc {
                path: nix,
                frontmatter: None,
                indexed_at: "2026-09-25T14:11:00Z",
            },
            Doc {
                path: note,
                frontmatter: Some(r#"{"landed_at":"2026-09-25T14:12:00Z"}"#),
                indexed_at: "2026-09-25T14:12:00Z",
            },
        ],
        "2026-09-25T15:00:00Z",
        "2026-09-25T12:00:00Z",
        "2026-06-07T13:55:36Z",
    );
    let before = fingerprint(&lattice.path);
    let mut hedron = StoreFixture::new();
    let source = lattice.open();
    let reports = reconcile_lesson_ships(
        &mut hedron.store,
        &hedron.token,
        Ok(&source),
        manifest,
        &[
            lane("2026-09-25", "duha", None, None),
            lane("2026-09-24", "duha", None, None),
        ],
        None,
    )
    .unwrap();
    let warm = report(&reports, "duha");
    let skipped = reports
        .iter()
        .find(|item| item.date == "2026-09-24")
        .unwrap();
    assert_eq!(warm.status, "warm");
    assert!(warm.missing.is_empty());
    assert!(warm.stale.is_empty());
    assert_eq!(warm.shipped.len(), 3);
    assert_eq!(skipped.status, "not_evaluated");
    assert!(skipped.missing.is_empty());
    assert_eq!(scalar_count(&hedron.path, "SELECT count(*) FROM events"), 1);
    assert_eq!(scalar_count(&hedron.path, "SELECT count(*) FROM nodes"), 2);
    let status = status_of(&hedron.path, "duha-2026-09-25");
    assert_eq!(status["conditions"][0]["type"].as_str(), Some("Reconciled"));
    assert!(status["observed"]["missing"]
        .as_sequence()
        .unwrap()
        .is_empty());
    let (event_type, caused_by) = event_row(&hedron.path);
    assert_eq!(event_type, "Reconciled");
    assert_eq!(caused_by.trim(), "[]");
    assert_eq!(fingerprint(&lattice.path), before);
    let _ = ConditionKind::Reconciled;
    let _ = SCOPE_START;
}

#[test]
fn g2_trusted_miss_is_gap() {
    let nix = "Archmagus-Stack/Polyglot-Dev/Nix/2026-09-25-nix/lesson.md";
    let bend = "Archmagus-Stack/Polyglot-Dev/Bend/2026-09-25-bend/lesson.md";
    let note = "agents/mail_room/Leo/2026-09-25-asr.md";
    let manifest = r#"
- date: 2026-09-25
  lane: asr
  registered_by: "2026-09-25T20:00:00Z"
  dev:
    source: Polyglot-Dev/Nix/2026-09-25-nix/lesson.html
- date: 2026-09-25
  lane: asr
  registered_by: "2026-09-25T20:00:00Z"
  dev:
    source: Polyglot-Dev/Bend/2026-09-25-bend/lesson.html
"#;
    let lattice = LatticeFixture::build(
        &[
            Doc {
                path: nix,
                frontmatter: None,
                indexed_at: "2026-09-25T20:10:00Z",
            },
            Doc {
                path: note,
                frontmatter: None,
                indexed_at: "2026-09-25T20:11:00Z",
            },
        ],
        "2026-09-25T20:30:00Z",
        "2026-09-25T12:00:00Z",
        "2026-06-07T13:55:36Z",
    );
    let mut hedron = StoreFixture::new();
    let source = lattice.open();
    let reports = reconcile_lesson_ships(
        &mut hedron.store,
        &hedron.token,
        Ok(&source),
        manifest,
        &[lane("2026-09-25", "asr", None, None)],
        None,
    )
    .unwrap();
    let gap = report(&reports, "asr");
    assert_eq!(gap.status, "gap");
    assert_eq!(gap.missing, vec![bend]);
    assert_eq!(gap.reasons.len(), 1);
    assert_eq!(gap.reasons[0].reason, "missing_path");
    assert!(gap.stale.is_empty());
    let status = status_of(&hedron.path, "asr-2026-09-25");
    assert_eq!(status["conditions"][0]["type"].as_str(), Some("Pending"));
    assert_eq!(scalar_count(&hedron.path, "SELECT count(*) FROM events"), 1);
    assert_eq!(scalar_count(&hedron.path, "SELECT count(*) FROM nodes"), 2);
}

#[test]
fn g3_indexer_kick_is_stale_not_missing() {
    let lesson = "Archmagus-Stack/Polyglot-Dev/Rust/2026-09-25-asr/lesson.md";
    let note = "agents/mail_room/Leo/2026-09-25-asr.md";
    let manifest = r#"
- date: 2026-09-25
  lane: asr
  registered_by: "2026-09-25T20:28:00Z"
  dev:
    source: Polyglot-Dev/Rust/2026-09-25-asr/lesson.html
"#;
    let lattice = LatticeFixture::build(
        &[],
        "2026-09-25T19:31:00Z",
        "2026-09-25T21:56:00Z",
        "2026-06-07T13:55:36Z",
    );
    let source = lattice.open();
    let fresh = source.freshness().unwrap();
    assert_eq!(
        fresh.watermark_unix,
        parse_timestamp("2026-09-25T19:31:00Z")
    );
    assert_ne!(
        fresh.watermark_unix,
        parse_timestamp(fresh.last_indexer_at.as_deref().unwrap())
    );
    assert_eq!(
        walk_watermark(Some("2026-09-25T19:31:00Z"), Some("2026-06-07T13:55:36Z")),
        fresh.watermark_unix
    );
    let mut hedron = StoreFixture::new();
    let reports = reconcile_lesson_ships(
        &mut hedron.store,
        &hedron.token,
        Ok(&source),
        manifest,
        &[lane("2026-09-25", "asr", None, None)],
        None,
    )
    .unwrap();
    let stale = report(&reports, "asr");
    assert_eq!(stale.status, "stale");
    assert!(stale.missing.is_empty(), "untrusted absence is not missing");
    assert!(stale.stale.contains(&lesson.to_string()));
    assert!(stale.stale.contains(&note.to_string()));
    assert_eq!(stale.watermark_utc.as_deref(), Some("2026-09-25T19:31:00Z"));
    assert_eq!(
        scalar_count(&hedron.path, "SELECT count(*) FROM desired_states"),
        0
    );
    assert_eq!(scalar_count(&hedron.path, "SELECT count(*) FROM events"), 0);
}

#[test]
fn g4_superseded_ghost_is_not_counted() {
    let rust = "Archmagus-Stack/Polyglot-Dev/Rust/2026-09-25-owned/lesson.md";
    let ghost = "Archmagus-Stack/Polyglot-Dev/Python/_superseded/2026-09-25-groupby/lesson.md";
    let note = "agents/mail_room/Leo/2026-09-25-asr.md";
    let manifest = r#"
- date: 2026-09-25
  lane: asr
  registered_by: "2026-09-25T18:00:00Z"
  dev:
    source: Polyglot-Dev/Rust/2026-09-25-owned/lesson.html
"#;
    let lattice = LatticeFixture::build(
        &[
            Doc {
                path: ghost,
                frontmatter: None,
                indexed_at: "2026-09-25T18:05:00Z",
            },
            Doc {
                path: note,
                frontmatter: None,
                indexed_at: "2026-09-25T18:06:00Z",
            },
        ],
        "2026-09-25T19:00:00Z",
        "2026-09-25T12:00:00Z",
        "2026-06-07T13:55:36Z",
    );
    let source = lattice.open();
    let globbed = source
        .glob_rows("Archmagus-Stack/Polyglot-Dev/*/2026-09-25-*/lesson.md")
        .unwrap();
    assert!(
        globbed.iter().all(|row| row.path != ghost),
        "superseded ghost must not be returned"
    );
    let mut hedron = StoreFixture::new();
    let reports = reconcile_lesson_ships(
        &mut hedron.store,
        &hedron.token,
        Ok(&source),
        manifest,
        &[lane("2026-09-25", "asr", None, None)],
        None,
    )
    .unwrap();
    let gap = report(&reports, "asr");
    assert_eq!(gap.status, "gap");
    assert_eq!(gap.missing, vec![rust]);
    assert!(!gap.shipped.iter().any(|path| path == ghost));
    assert!(!gap.missing.iter().any(|path| path == ghost));
}

#[test]
fn g5_missing_manifest_row_uses_glob_label() {
    let note = "agents/mail_room/Leo/2026-09-25-asr.md";
    let glob = "Archmagus-Stack/Polyglot-Dev/Nix/2026-09-25-*/lesson.md";
    let lattice = LatticeFixture::build(
        &[Doc {
            path: note,
            frontmatter: None,
            indexed_at: "2026-09-25T21:10:00Z",
        }],
        "2026-09-25T22:00:00Z",
        "2026-09-25T12:00:00Z",
        "2026-06-07T13:55:36Z",
    );
    let mut hedron = StoreFixture::new();
    let source = lattice.open();
    let reports = reconcile_lesson_ships(
        &mut hedron.store,
        &hedron.token,
        Ok(&source),
        "[]\n",
        &[lane(
            "2026-09-25",
            "asr",
            Some("2026-09-25T21:00:00Z"),
            Some(glob),
        )],
        None,
    )
    .unwrap();
    let gap = report(&reports, "asr");
    assert_eq!(gap.status, "gap");
    assert_eq!(gap.missing, vec![glob]);
    assert_eq!(gap.reasons[0].path, glob);
    assert_eq!(gap.reasons[0].reason, "manifest_row_missing");
    assert_eq!(scalar_count(&hedron.path, "SELECT count(*) FROM events"), 1);
}

#[test]
fn g6_fail_closed_cannot_tell_leaves_desired_state() {
    let lanes = [lane("2026-09-25", "asr", None, None)];
    let manifest = "[]\n";

    let missing = std::env::temp_dir().join(format!(
        "hedron-lattice-missing-{}-{}.db",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let mut hedron = StoreFixture::new();
    seed_untouched(&mut hedron);
    let opened = LatticeSource::open(&SourceConfig::new(&missing));
    assert!(matches!(opened, Err(Error::SourceMissing(_))));
    assert!(!missing.exists());
    let reports = reconcile_lesson_ships(
        &mut hedron.store,
        &hedron.token,
        opened.as_ref(),
        manifest,
        &lanes,
        None,
    )
    .unwrap();
    assert_eq!(report(&reports, "asr").status, "cannot_tell");
    assert!(report(&reports, "asr").missing.is_empty());
    assert_seed_untouched(&hedron.path);

    let locked = LatticeFixture::build(
        &[],
        "2026-09-25T19:31:00Z",
        "2026-09-25T21:56:00Z",
        "2026-06-07T13:55:36Z",
    );
    let writer = Connection::open(&locked.path).unwrap();
    writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let opened = LatticeSource::open(&SourceConfig {
        lattice_path: locked.path.clone(),
        busy_timeout_ms: 50,
    });
    let lock_err = opened
        .as_ref()
        .err()
        .expect("exclusive lock must fail closed");
    assert!(matches!(lock_err, Error::SourceLocked(_)), "{lock_err}");
    drop(writer);
    let mut hedron = StoreFixture::new();
    seed_untouched(&mut hedron);
    let reports = reconcile_lesson_ships(
        &mut hedron.store,
        &hedron.token,
        opened.as_ref(),
        manifest,
        &lanes,
        None,
    )
    .unwrap();
    assert_eq!(report(&reports, "asr").status, "cannot_tell");
    assert!(report(&reports, "asr").missing.is_empty());
    assert_seed_untouched(&hedron.path);

    let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("hedron-lattice-drift-{}-{}", std::process::id(), n));
    fs::create_dir_all(&dir).unwrap();
    let drifted = dir.join("lattice.db");
    let conn = Connection::open(&drifted).unwrap();
    conn.execute_batch(
        "CREATE TABLE documents (
            path TEXT, frontmatter_json TEXT, mtime REAL, content_hash TEXT, doc_type TEXT
         );
         CREATE TABLE index_state (
            id INTEGER PRIMARY KEY,
            last_reconcile_at TEXT,
            last_indexer_at TEXT,
            last_full_pass_at TEXT,
            document_count INTEGER
         );",
    )
    .unwrap();
    drop(conn);
    let opened = LatticeSource::open(&SourceConfig::new(&drifted));
    let drift_err = opened
        .as_ref()
        .err()
        .expect("drifted schema must fail closed");
    assert!(
        matches!(drift_err, Error::SourceSchemaDrift(_)),
        "{drift_err}"
    );
    let mut hedron = StoreFixture::new();
    seed_untouched(&mut hedron);
    let reports = reconcile_lesson_ships(
        &mut hedron.store,
        &hedron.token,
        opened.as_ref(),
        manifest,
        &lanes,
        None,
    )
    .unwrap();
    assert_eq!(report(&reports, "asr").status, "cannot_tell");
    assert!(report(&reports, "asr").missing.is_empty());
    assert_seed_untouched(&hedron.path);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn g7_write_is_rejected_and_bytes_stay_put() {
    let lesson = "Archmagus-Stack/Polyglot-Dev/Rust/2026-09-25-alpha/lesson.md";
    let lattice = LatticeFixture::build(
        &[Doc {
            path: lesson,
            frontmatter: None,
            indexed_at: "2026-09-25T14:10:00Z",
        }],
        "2026-09-25T15:00:00Z",
        "2026-09-25T21:56:00Z",
        "2026-06-07T13:55:36Z",
    );
    let before = fingerprint(&lattice.path);
    let source = lattice.open();
    let _ = source.rows_for(&[lesson.to_string()]).unwrap();
    let _ = source.freshness().unwrap();
    let rejected = source.rejected_write().unwrap();
    assert_eq!(
        rejected.sqlite_code & 0xFF,
        rusqlite::ffi::SQLITE_READONLY,
        "{}",
        rejected.message
    );
    assert_eq!(fingerprint(&lattice.path), before);
    let conn =
        Connection::open_with_flags(&lattice.path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let probes: i64 = conn
        .query_row(
            "SELECT count(*) FROM documents WHERE path = '__hedron_ro_probe__'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(probes, 0);
}

#[test]
fn g8_reconcile_observed_ignores_decoy_missing_and_vault_docs() {
    let lesson = "Archmagus-Stack/Polyglot-Dev/Rust/2026-09-25-alpha/lesson.md";
    let mut hedron = StoreFixture::new();
    let spec = DesiredState::curriculum_clock_spec(
        "2026-09-25",
        "asr",
        0,
        0,
        1,
        1,
        &[
            (lesson, "lesson_md"),
            ("agents/mail_room/Leo/2026-09-25-asr.md", "ship_note"),
        ],
        None,
    )
    .unwrap();
    let ds = hedron
        .store
        .put_desired_state(&hedron.token, "asr-2026-09-25", spec, 0.5)
        .unwrap();
    let decoy = Node::document(
        ds.vault_id,
        Some("decoy-from-vault"),
        serde_yaml::from_str("path: decoy-from-vault\n").unwrap(),
    )
    .unwrap();
    hedron.store.put_node(&hedron.token, decoy).unwrap();
    let emit = serde_yaml::from_str(&serde_json::to_string(&serde_json::json!({
        "kind": "curriculum_clock",
        "date": "2026-09-25",
        "status": "gap",
        "subject": { "name": "asr-2026-09-25" },
        "present": [lesson, "agents/mail_room/Leo/2026-09-25-asr.md"],
        "missing": ["decoy-not-a-path"],
        "evidence": [
            {"path": lesson, "role": "lesson_md", "present": true},
            {"path": "agents/mail_room/Leo/2026-09-25-asr.md", "role": "ship_note", "present": true}
        ],
        "observed": {
            "present": [],
            "missing": ["decoy-not-a-path"],
            "counts": {}
        }
    })).unwrap()).unwrap();
    let (warm, event) = hedron
        .store
        .reconcile_observed(&hedron.token, ds.id, &emit)
        .unwrap();
    assert_eq!(warm.status.conditions[0].kind, ConditionKind::Reconciled);
    assert!(warm.status.observed["missing"]
        .as_sequence()
        .unwrap()
        .is_empty());
    let missing = serde_yaml::to_string(&warm.status.observed["missing"]).unwrap();
    assert!(!missing.contains("decoy-not-a-path"));
    assert!(!missing.contains("decoy-from-vault"));
    assert!(event.caused_by.is_empty());
    assert_eq!(event.event_type, "Reconciled");
    assert_eq!(scalar_count(&hedron.path, "SELECT count(*) FROM nodes"), 3);
    let conn = Connection::open_with_flags(&hedron.path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let copied: i64 = conn
        .query_row(
            "SELECT count(*) FROM nodes WHERE path = ?1",
            [lesson],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(copied, 0, "lattice path must not be ingested");
}

#[test]
fn g9_space_and_t_timestamps_are_one_instant() {
    let space = parse_timestamp("2026-09-25 16:21:00-04:00").unwrap();
    let tee = parse_timestamp("2026-09-25T16:21:00-04:00").unwrap();
    let zulu = parse_timestamp("2026-09-25T20:21:00Z").unwrap();
    assert_eq!(space, tee);
    assert_eq!(space, zulu);
    assert_eq!(hedron_core::format_unix_utc(space), "2026-09-25T20:21:00Z");
    assert_eq!(
        parse_timestamp("2026-09-25 20:28:00Z"),
        parse_timestamp("2026-09-25T20:28:00Z")
    );
    assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(
        hedron_core::format_unix_utc(parse_timestamp("2026-09-25 19:31:34").unwrap()),
        "2026-09-25T19:31:34Z"
    );

    let lesson = "Archmagus-Stack/Polyglot-Dev/Rust/2026-09-25-alpha/lesson.md";
    let note_space = "agents/mail_room/Leo/2026-09-25-duha.md";
    let note_tee = "agents/mail_room/Leo/2026-09-25-asr.md";
    let lattice = LatticeFixture::build(
        &[
            Doc {
                path: note_space,
                frontmatter: Some(r#"{"landed_at":"2026-09-25 16:21:00-04:00"}"#),
                indexed_at: "2026-09-25T20:21:00Z",
            },
            Doc {
                path: note_tee,
                frontmatter: Some(
                    r#"{"landed_at":"2026-09-25T16:21:00-04:00","redo_landed_at":"2026-09-25T20:21:00Z"}"#,
                ),
                indexed_at: "2026-09-25T20:21:00Z",
            },
            Doc {
                path: lesson,
                frontmatter: None,
                indexed_at: "2026-09-25T20:21:00Z",
            },
        ],
        "2026-09-25T20:21:00Z",
        "2026-09-25T21:56:00Z",
        "2026-06-07T13:55:36Z",
    );
    let source = lattice.open();
    let rows = source
        .rows_for(&[note_space.to_string(), note_tee.to_string()])
        .unwrap();
    let left = rows.iter().find(|row| row.path == note_space).unwrap();
    let right = rows.iter().find(|row| row.path == note_tee).unwrap();
    assert_eq!(
        parse_timestamp(left.landed_at.as_deref().unwrap()),
        parse_timestamp(right.landed_at.as_deref().unwrap())
    );
    assert_eq!(
        parse_timestamp(right.redo_landed_at.as_deref().unwrap()),
        Some(zulu)
    );

    let hint = zulu;
    let trusted = hedron_core::judge_freshness(
        &[hedron_core::AbsenceCheck {
            path: lesson.to_string(),
            present: false,
            landed_hint: Some(hint),
            check_at: None,
        }],
        Some(hint),
    );
    let stale = hedron_core::judge_freshness(
        &[hedron_core::AbsenceCheck {
            path: lesson.to_string(),
            present: false,
            landed_hint: Some(hint),
            check_at: None,
        }],
        Some(hint - 1),
    );
    assert_eq!(trusted, hedron_core::GuardVerdict::Trusted);
    match stale {
        hedron_core::GuardVerdict::Stale { untrusted } => assert_eq!(untrusted, vec![lesson]),
        hedron_core::GuardVerdict::Trusted => panic!("watermark before the hint must be stale"),
    }
}

#[test]
fn manifest_maps_html_source_to_sibling_lesson_md() {
    let yaml = r#"
- date: 2026-09-25
  registered_by: "2026-09-25T20:00:00Z"
  revised: "2026-09-25T20:05:00Z"
  ops:
    source: 01-Earth-DevOps/Synthesis-Lessons/2026-09-25-rust-ops/lesson.html
  dev:
    source: Polyglot-Dev/Rust/2026-09-25-pyo3/lesson.html
  cert:
    source: Cert-Prep/CNCF/2026-09-25-cks/lesson.html
- date: 2026-09-25
  lane: duha
  registered_by: "2026-09-25T12:00:00Z"
  dev:
    source: Polyglot-Dev/Nix/2026-09-25-nix-pills/lesson.html
- date: 2026-09-25
  lane: asr
  era: pre-manifest-backfill
  dev:
    source: Polyglot-Dev/Bend/2026-09-25-bend/lesson.html
- date: 2026-06-10
  lane: duha
  dev:
    source: Polyglot-Dev/Old/lesson.html
- date: 2026-09-25
  lane: asr
  dev:
    source: Polyglot-Dev/Python/_superseded/2026-09-25-groupby/lesson.html
"#;
    let bundles = parse_manifest(yaml).unwrap();
    let lanes: Vec<&str> = bundles.iter().map(|bundle| bundle.lane.as_str()).collect();
    assert_eq!(lanes, ["maghrib", "maghrib", "maghrib", "duha"]);
    assert_eq!(
        bundles[0].lesson_md_path,
        "Archmagus-Stack/01-Earth-DevOps/Synthesis-Lessons/2026-09-25-rust-ops/lesson.md"
    );
    assert_eq!(bundles[0].seat, "ops");
    assert_eq!(
        bundles[3].lesson_md_path,
        "Archmagus-Stack/Polyglot-Dev/Nix/2026-09-25-nix-pills/lesson.md"
    );
    assert!(bundles
        .iter()
        .all(|bundle| bundle.date.as_str() >= "2026-06-11"));
    assert!(bundles
        .iter()
        .all(|bundle| !bundle.lesson_md_path.contains("_superseded")));
}

#[test]
fn maghrib_bundle_requires_sibling_lab_ref() {
    let lesson = "Archmagus-Stack/Polyglot-Dev/Rust/2026-09-25-pyo3/lesson.md";
    let lab = "Archmagus-Stack/Polyglot-Dev/Rust/2026-09-25-pyo3/lab-ref.md";
    let note = "agents/mail_room/Leo/2026-09-25-maghrib.md";
    let manifest = r#"
- date: 2026-09-25
  registered_by: "2026-09-25T22:00:00Z"
  dev:
    source: Polyglot-Dev/Rust/2026-09-25-pyo3/lesson.html
"#;
    let lattice = LatticeFixture::build(
        &[
            Doc {
                path: lesson,
                frontmatter: None,
                indexed_at: "2026-09-25T22:05:00Z",
            },
            Doc {
                path: note,
                frontmatter: None,
                indexed_at: "2026-09-25T22:06:00Z",
            },
        ],
        "2026-09-25T23:00:00Z",
        "2026-09-25T12:00:00Z",
        "2026-06-07T13:55:36Z",
    );
    let mut hedron = StoreFixture::new();
    let source = lattice.open();
    let reports = reconcile_lesson_ships(
        &mut hedron.store,
        &hedron.token,
        Ok(&source),
        manifest,
        &[lane("2026-09-25", "maghrib", None, None)],
        None,
    )
    .unwrap();
    let gap = report(&reports, "maghrib");
    assert_eq!(gap.status, "gap");
    assert_eq!(gap.missing, vec![lab]);
    assert_eq!(gap.reasons[0].reason, "missing_path");
}

#[test]
fn pending_before_check_at_does_not_write() {
    let lattice = LatticeFixture::build(
        &[],
        "2026-09-25T23:00:00Z",
        "2026-09-25T23:30:00Z",
        "2026-06-07T13:55:36Z",
    );
    let mut hedron = StoreFixture::new();
    let source = lattice.open();
    let reports = reconcile_lesson_ships(
        &mut hedron.store,
        &hedron.token,
        Ok(&source),
        "[]\n",
        &[lane(
            "2026-09-25",
            "asr",
            Some("2099-01-01T00:00:00Z"),
            Some("Archmagus-Stack/Polyglot-Dev/Nix/2026-09-25-*/lesson.md"),
        )],
        parse_timestamp("2026-09-25T12:00:00Z"),
    )
    .unwrap();
    let pending = report(&reports, "asr");
    assert_eq!(pending.status, "pending");
    assert!(pending.missing.is_empty());
    assert_eq!(scalar_count(&hedron.path, "SELECT count(*) FROM events"), 0);
    assert_eq!(
        scalar_count(&hedron.path, "SELECT count(*) FROM desired_states"),
        0
    );
}
