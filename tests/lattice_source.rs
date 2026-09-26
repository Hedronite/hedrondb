//! Golden G1–G9 and G15–G20 for the read-only lesson-ship lattice source.
//!
//! Fixtures are built in a temp directory. Nothing here opens a live lattice.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use hedron_core::{
    default_lane_check_at, parse_manifest, parse_timestamp, reconcile_lesson_ships,
    reconcile_lesson_ships_with, walk_watermark, BadRow, ConditionKind, DesiredState, Error,
    HookAction, LaneDue, LaneReport, LatticeSource, ManifestHook, Node, ObserveBatch,
    QuarantineOnBadRow, QuarantineStore, SourceConfig, SCOPE_START,
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

fn report_on<'a>(reports: &'a [LaneReport], date: &str, name: &str) -> &'a LaneReport {
    reports
        .iter()
        .find(|item| item.date == date && item.lane == name)
        .unwrap_or_else(|| panic!("missing lane {name} on {date}"))
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
    // The INSERT rejection lives in `g7_insert_is_sqlite_readonly` (crate unit test).
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
    let spec = DesiredState::lesson_clock(hedron_core::LessonClockSpec {
        date: "2026-09-25",
        clock: "asr",
        quiz_html: 0,
        lab_refs: 0,
        ship_note: 1,
        lesson_md: 1,
        required_paths: &[
            (lesson, "lesson_md"),
            ("agents/mail_room/Leo/2026-09-25-asr.md", "ship_note"),
        ],
        check_at: None,
    })
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
    assert!(bundles[0].revised);
    assert!(!bundles[3].revised);
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
        None,
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

#[test]
fn legacy_duplicate_key_still_evaluates_scope_row() {
    let legacy = r#"
- date: 2026-06-23
  lane: duha
  cert:
    source: Polyglot-Dev/Old/2026-06-23-a/lesson.html
  cert:
    source: Polyglot-Dev/Old/2026-06-23-b/lesson.html
"#;
    assert!(
        serde_yaml::from_str::<Value>(legacy).is_err(),
        "the legacy row alone is not valid YAML"
    );
    let lesson = "Archmagus-Stack/Polyglot-Dev/Rust/2026-09-25-alpha/lesson.md";
    let note = "agents/mail_room/Leo/2026-09-25-duha.md";
    let manifest = format!(
        "{legacy}\
- date: 2026-09-25
  lane: duha
  registered_by: \"2026-09-25T14:00:00Z\"
  dev:
    source: Polyglot-Dev/Rust/2026-09-25-alpha/lesson.html
"
    );
    assert!(serde_yaml::from_str::<Value>(&manifest).is_err());
    let bundles = parse_manifest(&manifest).unwrap();
    assert_eq!(bundles.len(), 1);
    assert_eq!(bundles[0].date, "2026-09-25");
    assert_eq!(bundles[0].lesson_md_path, lesson);
    let lattice = LatticeFixture::build(
        &[
            Doc {
                path: lesson,
                frontmatter: None,
                indexed_at: "2026-09-25T14:10:00Z",
            },
            Doc {
                path: note,
                frontmatter: None,
                indexed_at: "2026-09-25T14:12:00Z",
            },
        ],
        "2026-09-25T15:00:00Z",
        "2026-09-25T12:00:00Z",
        "2026-06-07T13:55:36Z",
    );
    let mut hedron = StoreFixture::new();
    let source = lattice.open();
    let reports = reconcile_lesson_ships(
        &mut hedron.store,
        &hedron.token,
        Ok(&source),
        &manifest,
        &[lane("2026-09-25", "duha", None, None)],
        None,
        None,
    )
    .unwrap();
    let warm = report(&reports, "duha");
    assert_eq!(warm.status, "warm");
    assert!(warm.cannot_tell.is_none());
    assert!(warm.missing.is_empty());
}

#[test]
fn in_scope_duplicate_key_fails_closed() {
    let manifest = r#"
- date: 2026-09-25
  lane: duha
  dev:
    source: Polyglot-Dev/Rust/2026-09-25-a/lesson.html
  dev:
    source: Polyglot-Dev/Rust/2026-09-25-b/lesson.html
"#;
    let err = parse_manifest(manifest).unwrap_err();
    assert!(err.to_string().contains("2026-09-25"), "{err}");
    let lattice = LatticeFixture::build(
        &[],
        "2026-09-25T15:00:00Z",
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
        &[lane("2026-09-25", "duha", None, None)],
        None,
        None,
    )
    .unwrap();
    let blocked = report(&reports, "duha");
    assert_eq!(blocked.status, "quarantined");
    assert!(blocked.missing.is_empty());
    assert!(blocked.quarantine.is_some());
    assert_eq!(scalar_count(&hedron.path, "SELECT count(*) FROM events"), 0);
}

#[test]
fn manifest_rejects_top_level_mapping() {
    let err = parse_manifest("rows:\n  - date: 2026-09-25\n    lane: duha\n").unwrap_err();
    assert!(err.to_string().contains("YAML list"), "{err}");
}

#[test]
fn prefixed_registered_by_without_seconds() {
    let stamped = parse_timestamp("_tools/register.py 2026-09-25T14:40Z").unwrap();
    assert_eq!(stamped, parse_timestamp("2026-09-25T14:40:00Z").unwrap());
    let lesson = "Archmagus-Stack/Polyglot-Dev/Rust/2026-09-25-alpha/lesson.md";
    let note = "agents/mail_room/Leo/2026-09-25-duha.md";
    let manifest = r#"
- date: 2026-09-25
  lane: duha
  registered_by: "_tools/register.py 2026-09-25T14:40Z"
  dev:
    source: Polyglot-Dev/Rust/2026-09-25-alpha/lesson.html
"#;
    let trusted = ship_absent_lesson(manifest, note, "2026-09-25T14:40:00Z", None);
    assert_eq!(trusted.status, "gap");
    assert_eq!(trusted.missing, vec![lesson]);
    let early = ship_absent_lesson(manifest, note, "2026-09-25T14:39:00Z", None);
    assert_eq!(early.status, "stale");
    assert!(early.missing.is_empty());
    assert!(early.stale.contains(&lesson.to_string()));
}

#[test]
fn revised_row_uses_latest_register_log() {
    let lesson = "Archmagus-Stack/Polyglot-Dev/Rust/2026-09-25-trio/lesson.md";
    let note = "agents/mail_room/Leo/2026-09-25-maghrib.md";
    let manifest = r#"
- date: 2026-09-25
  revised: "2026-09-25T19:00:00Z"
  registered_by: "_tools/register.py 2026-09-25T17:31Z"
  dev:
    source: Polyglot-Dev/Rust/2026-09-25-trio/lesson.html
"#;
    let log = "\
_tools/register.py 2026-09-25T19:54Z date=2026-09-25 trio=True ok
_tools/register.py 2026-09-25T19:55Z date=2026-09-25 trio=True ok
_tools/register.py 2026-09-25T23:01Z date=2026-09-25 trio=True ok
_tools/register.py 2026-09-25T23:02Z date=2026-09-25 trio=True ok
";
    let early = ship_absent_lesson(manifest, note, "2026-09-25T19:31:00Z", Some(log));
    assert_eq!(early.status, "stale");
    assert!(
        early.missing.is_empty(),
        "watermark before the latest registration must not be missing"
    );
    assert!(early.stale.iter().any(|path| path == lesson));
    let late = ship_absent_lesson(manifest, note, "2026-09-25T23:30:00Z", Some(log));
    assert_eq!(late.status, "gap");
    assert!(late.missing.iter().any(|path| path == lesson));
}

#[test]
fn close_note_does_not_inherit_registration_time() {
    let lesson = "Archmagus-Stack/Polyglot-Dev/Rust/2026-09-25-alpha/lesson.md";
    let note = "agents/mail_room/Leo/2026-09-25-duha.md";
    let manifest = r#"
- date: 2026-09-25
  lane: duha
  registered_by: "2026-09-25T14:00:00Z"
  dev:
    source: Polyglot-Dev/Rust/2026-09-25-alpha/lesson.html
"#;
    let lattice = LatticeFixture::build(
        &[Doc {
            path: lesson,
            frontmatter: None,
            indexed_at: "2026-09-25T14:10:00Z",
        }],
        "2026-09-25T18:00:00Z",
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
        &[lane(
            "2026-09-25",
            "duha",
            Some("2026-09-25T21:00:00Z"),
            None,
        )],
        None,
        None,
    )
    .unwrap();
    let stale = report(&reports, "duha");
    assert_eq!(stale.status, "stale");
    assert!(stale.missing.is_empty());
    assert_eq!(stale.stale, vec![note]);
    assert!(stale
        .reasons
        .iter()
        .all(|reason| reason.reason != "close_note_missing"));

    let later = LatticeFixture::build(
        &[Doc {
            path: lesson,
            frontmatter: None,
            indexed_at: "2026-09-25T14:10:00Z",
        }],
        "2026-09-25T21:00:00Z",
        "2026-09-25T12:00:00Z",
        "2026-06-07T13:55:36Z",
    );
    let source = later.open();
    let reports = reconcile_lesson_ships(
        &mut hedron.store,
        &hedron.token,
        Ok(&source),
        manifest,
        &[lane(
            "2026-09-25",
            "duha",
            Some("2026-09-25T21:00:00Z"),
            None,
        )],
        None,
        None,
    )
    .unwrap();
    let gap = report(&reports, "duha");
    assert_eq!(gap.status, "gap");
    assert_eq!(gap.missing, vec![note]);
    assert_eq!(gap.reasons[0].reason, "close_note_missing");
}

fn ship_absent_lesson(
    manifest: &str,
    note: &str,
    reconcile_at: &str,
    register_log: Option<&str>,
) -> LaneReport {
    let lattice = LatticeFixture::build(
        &[Doc {
            path: note,
            frontmatter: None,
            indexed_at: reconcile_at,
        }],
        reconcile_at,
        "2026-09-25T12:00:00Z",
        "2026-06-07T13:55:36Z",
    );
    let mut hedron = StoreFixture::new();
    let source = lattice.open();
    let lane_name = if manifest.contains("lane:") {
        "duha"
    } else {
        "maghrib"
    };
    let reports = reconcile_lesson_ships(
        &mut hedron.store,
        &hedron.token,
        Ok(&source),
        manifest,
        &[lane(SCOPE_START, lane_name, None, None)],
        None,
        register_log,
    )
    .unwrap();
    report(&reports, lane_name).clone()
}

struct AbortHook;

impl ManifestHook for AbortHook {
    fn on_bad_row(&self, _bad: &BadRow) -> HookAction {
        HookAction::Abort
    }
}

fn lattice_with(paths: &[&str], indexed_at: &str, reconcile_at: &str) -> LatticeFixture {
    let docs: Vec<Doc<'_>> = paths
        .iter()
        .copied()
        .map(|path| Doc {
            path,
            frontmatter: None,
            indexed_at,
        })
        .collect();
    LatticeFixture::build(
        &docs,
        reconcile_at,
        "2026-09-26T12:00:00Z",
        "2026-06-07T13:55:36Z",
    )
}

fn pass(
    hedron: &mut StoreFixture,
    lattice: &LatticeFixture,
    manifest: &str,
    lanes: &[LaneDue],
    now: &str,
    hook: &dyn ManifestHook,
) -> ObserveBatch {
    let source = lattice.open();
    reconcile_lesson_ships_with(
        &mut hedron.store,
        &hedron.token,
        Ok(&source),
        manifest,
        lanes,
        None,
        None,
        hook,
        parse_timestamp(now),
    )
    .unwrap()
}

#[test]
fn g15_bad_row_quarantines_only_its_date() {
    let d_prev = "2026-09-25";
    let d = "2026-09-26";
    let d_next = "2026-09-27";
    let lesson_prev = "Archmagus-Stack/Polyglot-Dev/Rust/2026-09-25-alpha/lesson.md";
    let note_prev = "agents/mail_room/Leo/2026-09-25-duha.md";
    let lesson_next = "Archmagus-Stack/Polyglot-Dev/Rust/2026-09-27-alpha/lesson.md";
    let note_next = "agents/mail_room/Leo/2026-09-27-duha.md";
    let manifest = r#"
- date: 2026-09-25
  lane: duha
  registered_by: "2026-09-25T14:00:00Z"
  dev:
    source: Polyglot-Dev/Rust/2026-09-25-alpha/lesson.html
- date: 2026-09-26
  lane: duha
  dev:
    source: Polyglot-Dev/Rust/2026-09-26-a/lesson.html
  dev:
    source: Polyglot-Dev/Rust/2026-09-26-b/lesson.html
- date: 2026-09-27
  lane: duha
  registered_by: "2026-09-27T14:00:00Z"
  dev:
    source: Polyglot-Dev/Rust/2026-09-27-alpha/lesson.html
"#;
    let paths = [lesson_prev, note_prev, lesson_next, note_next];
    let lattice = lattice_with(&paths, "2026-09-27T16:00:00Z", "2026-09-27T18:00:00Z");
    let before = fingerprint(&lattice.path);
    let mut hedron = StoreFixture::new();
    let lanes = [
        lane(d_prev, "duha", None, None),
        lane(d, "duha", None, None),
        lane(d_next, "duha", None, None),
    ];
    let batch = pass(
        &mut hedron,
        &lattice,
        manifest,
        &lanes,
        "2026-09-27T18:00:00Z",
        &QuarantineOnBadRow,
    );
    assert_eq!(report_on(&batch.lanes, d_prev, "duha").status, "warm");
    assert_eq!(report_on(&batch.lanes, d_next, "duha").status, "warm");
    let held = report_on(&batch.lanes, d, "duha");
    assert_eq!(held.status, "quarantined");
    assert!(held.missing.is_empty());
    assert!(held.shipped.is_empty());
    let mark = held.quarantine.as_ref().unwrap();
    assert!(mark.row_span[0] >= 1 && mark.row_span[1] >= mark.row_span[0]);
    assert_eq!(mark.row_hash.len(), 64);
    assert!(mark.reason.contains(d), "{}", mark.reason);
    assert_eq!(mark.first_seen_at, "2026-09-27T18:00:00Z");
    assert_eq!(batch.quarantine.len(), 1);
    assert_eq!(batch.quarantine[0].date.as_deref(), Some(d));
    assert_eq!(batch.quarantine[0].lanes, vec!["duha".to_string()]);
    assert!(batch.quarantine[0].is_active());
    assert_eq!(scalar_count(&hedron.path, "SELECT count(*) FROM events"), 2);
    assert_eq!(fingerprint(&lattice.path), before);
}

#[test]
fn g16_bad_lane_inside_a_row_holds_only_that_lane() {
    let lesson = "Archmagus-Stack/Polyglot-Dev/Rust/2026-09-26-alpha/lesson.md";
    let note = "agents/mail_room/Leo/2026-09-26-duha.md";
    let manifest = r#"
- date: 2026-09-26
  lane: duha
  registered_by: "2026-09-26T14:00:00Z"
  dev:
    source: Polyglot-Dev/Rust/2026-09-26-alpha/lesson.html
  lane: asr
  dev:
    source: Polyglot-Dev/Rust/2026-09-26-a/lesson.html
  dev:
    source: Polyglot-Dev/Rust/2026-09-26-b/lesson.html
"#;
    let paths = [lesson, note];
    let lattice = lattice_with(&paths, "2026-09-26T16:00:00Z", "2026-09-26T18:00:00Z");
    let mut hedron = StoreFixture::new();
    let lanes = [
        lane("2026-09-26", "duha", None, None),
        lane("2026-09-26", "asr", None, None),
        lane(
            "2026-09-26",
            "maghrib",
            Some("2026-09-26T12:00:00Z"),
            None,
        ),
    ];
    let batch = pass(
        &mut hedron,
        &lattice,
        manifest,
        &lanes,
        "2026-09-26T18:00:00Z",
        &QuarantineOnBadRow,
    );
    assert_eq!(report_on(&batch.lanes, "2026-09-26", "duha").status, "warm");
    let asr = report_on(&batch.lanes, "2026-09-26", "asr");
    assert_eq!(asr.status, "quarantined");
    assert!(asr.missing.is_empty());
    let maghrib = report_on(&batch.lanes, "2026-09-26", "maghrib");
    assert_eq!(maghrib.status, "gap");
    assert_eq!(batch.quarantine.len(), 1);
    assert_eq!(batch.quarantine[0].lanes, vec!["asr".to_string()]);
}

#[test]
fn g17_undated_bad_row_holds_only_dates_without_a_good_row() {
    let lesson_prev = "Archmagus-Stack/Polyglot-Dev/Rust/2026-09-25-alpha/lesson.md";
    let note_prev = "agents/mail_room/Leo/2026-09-25-duha.md";
    let lesson_next = "Archmagus-Stack/Polyglot-Dev/Rust/2026-09-27-alpha/lesson.md";
    let note_next = "agents/mail_room/Leo/2026-09-27-duha.md";
    let manifest = r#"
- date: 2026-09-25
  lane: duha
  registered_by: "2026-09-25T14:00:00Z"
  dev:
    source: Polyglot-Dev/Rust/2026-09-25-alpha/lesson.html
- [not a mapping
- date: 2026-09-27
  lane: duha
  registered_by: "2026-09-27T14:00:00Z"
  dev:
    source: Polyglot-Dev/Rust/2026-09-27-alpha/lesson.html
"#;
    let paths = [lesson_prev, note_prev, lesson_next, note_next];
    let lattice = lattice_with(&paths, "2026-09-27T16:00:00Z", "2026-09-27T18:00:00Z");
    let mut hedron = StoreFixture::new();
    let lanes = [
        lane("2026-09-25", "duha", None, None),
        lane(
            "2026-09-25",
            "asr",
            Some("2026-09-25T12:00:00Z"),
            None,
        ),
        lane("2026-09-26", "duha", None, None),
        lane("2026-09-27", "duha", None, None),
    ];
    let batch = pass(
        &mut hedron,
        &lattice,
        manifest,
        &lanes,
        "2026-09-27T18:00:00Z",
        &QuarantineOnBadRow,
    );
    assert_eq!(report_on(&batch.lanes, "2026-09-25", "duha").status, "warm");
    let asr = report_on(&batch.lanes, "2026-09-25", "asr");
    assert_ne!(asr.status, "quarantined");
    assert_eq!(asr.status, "gap");
    let held = report_on(&batch.lanes, "2026-09-26", "duha");
    assert_eq!(held.status, "quarantined");
    assert!(held.missing.is_empty());
    assert!(held.quarantine.as_ref().unwrap().reason.contains("undated"));
    assert_eq!(report_on(&batch.lanes, "2026-09-27", "duha").status, "warm");
    assert_eq!(batch.quarantine.len(), 1);
    assert!(batch.quarantine[0].date.is_none());
}

#[test]
fn g18_flag_persists_then_auto_clears_when_the_row_parses() {
    let bad = r#"
- date: 2026-09-26
  lane: duha
  dev:
    source: Polyglot-Dev/Rust/2026-09-26-a/lesson.html
  dev:
    source: Polyglot-Dev/Rust/2026-09-26-b/lesson.html
"#;
    let fixed = r#"
- date: 2026-09-26
  lane: duha
  registered_by: "2026-09-26T14:00:00Z"
  dev:
    source: Polyglot-Dev/Rust/2026-09-26-alpha/lesson.html
"#;
    let lesson = "Archmagus-Stack/Polyglot-Dev/Rust/2026-09-26-alpha/lesson.md";
    let note = "agents/mail_room/Leo/2026-09-26-duha.md";
    let paths = [lesson, note];
    let lattice = lattice_with(&paths, "2026-09-26T16:00:00Z", "2026-09-26T18:00:00Z");
    let mut hedron = StoreFixture::new();
    let lanes = [lane("2026-09-26", "duha", None, None)];
    let first = pass(
        &mut hedron,
        &lattice,
        bad,
        &lanes,
        "2026-09-26T18:00:00Z",
        &QuarantineOnBadRow,
    );
    assert_eq!(first.lanes[0].status, "quarantined");
    let hash = first.lanes[0].quarantine.as_ref().unwrap().row_hash.clone();
    assert_eq!(
        first.lanes[0].quarantine.as_ref().unwrap().first_seen_at,
        "2026-09-26T18:00:00Z"
    );
    let second = pass(
        &mut hedron,
        &lattice,
        bad,
        &lanes,
        "2026-09-26T19:00:00Z",
        &QuarantineOnBadRow,
    );
    assert_eq!(second.lanes[0].status, "quarantined");
    let mark = second.lanes[0].quarantine.as_ref().unwrap();
    assert_eq!(mark.first_seen_at, "2026-09-26T18:00:00Z");
    assert_eq!(mark.row_hash, hash);
    let stored = QuarantineStore::list(&hedron.store).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].first_seen_at, "2026-09-26T18:00:00Z");
    assert_eq!(stored[0].last_seen_at, "2026-09-26T19:00:00Z");
    assert!(stored[0].cleared_at.is_none());
    let third = pass(
        &mut hedron,
        &lattice,
        fixed,
        &lanes,
        "2026-09-26T20:00:00Z",
        &QuarantineOnBadRow,
    );
    assert_eq!(third.lanes[0].status, "warm");
    assert!(third.quarantine.is_empty());
    let stored = QuarantineStore::list(&hedron.store).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].row_hash, hash);
    assert_eq!(stored[0].cleared_at.as_deref(), Some("2026-09-26T20:00:00Z"));
    assert_eq!(scalar_count(&hedron.path, "SELECT count(*) FROM events"), 1);
}

#[test]
fn g19_abort_hook_locks_every_in_scope_lane() {
    let manifest = r#"
- date: 2026-09-25
  lane: duha
  registered_by: "2026-09-25T14:00:00Z"
  dev:
    source: Polyglot-Dev/Rust/2026-09-25-alpha/lesson.html
- date: 2026-09-26
  lane: duha
  dev:
    source: Polyglot-Dev/Rust/2026-09-26-a/lesson.html
  dev:
    source: Polyglot-Dev/Rust/2026-09-26-b/lesson.html
"#;
    let lesson = "Archmagus-Stack/Polyglot-Dev/Rust/2026-09-25-alpha/lesson.md";
    let note = "agents/mail_room/Leo/2026-09-25-duha.md";
    let paths = [lesson, note];
    let lattice = lattice_with(&paths, "2026-09-26T16:00:00Z", "2026-09-26T18:00:00Z");
    let mut hedron = StoreFixture::new();
    seed_untouched(&mut hedron);
    let lanes = [
        lane("2026-09-24", "duha", None, None),
        lane("2026-09-25", "duha", None, None),
        lane("2026-09-26", "asr", None, None),
    ];
    let batch = pass(
        &mut hedron,
        &lattice,
        manifest,
        &lanes,
        "2026-09-26T18:00:00Z",
        &AbortHook,
    );
    assert_eq!(report_on(&batch.lanes, "2026-09-24", "duha").status, "not_evaluated");
    assert_eq!(report_on(&batch.lanes, "2026-09-25", "duha").status, "cannot_tell");
    assert_eq!(report_on(&batch.lanes, "2026-09-26", "asr").status, "cannot_tell");
    assert!(batch.quarantine.is_empty());
    assert_seed_untouched(&hedron.path);
}

#[test]
fn g20_legacy_row_is_preacked_and_does_not_block() {
    let legacy = r#"
- date: 2026-06-23
  lane: duha
  cert:
    source: Polyglot-Dev/Old/2026-06-23-a/lesson.html
  cert:
    source: Polyglot-Dev/Old/2026-06-23-b/lesson.html
"#;
    let lesson = "Archmagus-Stack/Polyglot-Dev/Rust/2026-09-25-alpha/lesson.md";
    let note = "agents/mail_room/Leo/2026-09-25-duha.md";
    let manifest = format!(
        "{legacy}\
- date: 2026-09-25
  lane: duha
  registered_by: \"2026-09-25T14:00:00Z\"
  dev:
    source: Polyglot-Dev/Rust/2026-09-25-alpha/lesson.html
"
    );
    let paths = [lesson, note];
    let lattice = lattice_with(&paths, "2026-09-25T16:00:00Z", "2026-09-25T18:00:00Z");
    let mut hedron = StoreFixture::new();
    let lanes = [lane("2026-09-25", "duha", None, None)];
    let batch = pass(
        &mut hedron,
        &lattice,
        &manifest,
        &lanes,
        "2026-09-25T18:00:00Z",
        &QuarantineOnBadRow,
    );
    assert_eq!(batch.lanes[0].status, "warm");
    assert!(batch.quarantine.is_empty());
    let stored = QuarantineStore::list(&hedron.store).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].reason, "legacy");
    assert_eq!(stored[0].acked_by.as_deref(), Some("legacy"));
    assert!(stored[0].cleared_at.is_some());
    assert!(!stored[0].is_active());
    let again = pass(
        &mut hedron,
        &lattice,
        &manifest,
        &lanes,
        "2026-09-25T19:00:00Z",
        &QuarantineOnBadRow,
    );
    assert_eq!(again.lanes[0].status, "warm");
    let stored = QuarantineStore::list(&hedron.store).unwrap();
    assert_eq!(stored.len(), 1, "a legacy row is logged once");
    assert_eq!(stored[0].first_seen_at, "2026-09-25T18:00:00Z");
}

#[test]
fn acked_flag_releases_the_lane() {
    let manifest = r#"
- date: 2026-09-26
  lane: duha
  dev:
    source: Polyglot-Dev/Rust/2026-09-26-a/lesson.html
  dev:
    source: Polyglot-Dev/Rust/2026-09-26-b/lesson.html
"#;
    let lattice = LatticeFixture::build(
        &[],
        "2026-09-26T18:00:00Z",
        "2026-09-26T12:00:00Z",
        "2026-06-07T13:55:36Z",
    );
    let mut hedron = StoreFixture::new();
    let lanes = [lane(
        "2026-09-26",
        "duha",
        Some("2026-09-26T12:00:00Z"),
        None,
    )];
    let first = pass(
        &mut hedron,
        &lattice,
        manifest,
        &lanes,
        "2026-09-26T18:00:00Z",
        &QuarantineOnBadRow,
    );
    assert_eq!(first.lanes[0].status, "quarantined");
    let hash = first.lanes[0].quarantine.as_ref().unwrap().row_hash.clone();
    hedron
        .store
        .ack(&hash, "evan", "2026-09-26T18:30:00Z")
        .unwrap();
    let second = pass(
        &mut hedron,
        &lattice,
        manifest,
        &lanes,
        "2026-09-26T19:00:00Z",
        &QuarantineOnBadRow,
    );
    assert_ne!(second.lanes[0].status, "quarantined");
    assert_eq!(second.lanes[0].status, "gap");
    assert!(second.quarantine.is_empty());
    let stored = QuarantineStore::list(&hedron.store).unwrap();
    assert_eq!(stored[0].acked_by.as_deref(), Some("evan"));
}

#[test]
fn sniff_date_reads_a_later_top_level_key_not_a_nested_one() {
    let good = r#"
- lane: duha
  registered_by: "2026-09-25T14:00:00Z"
  date: 2026-09-25
  dev:
    source: Polyglot-Dev/Rust/2026-09-25-alpha/lesson.html
"#;
    let bundles = parse_manifest(good).unwrap();
    assert_eq!(bundles.len(), 1);
    assert_eq!(bundles[0].date, "2026-09-25");
    let lesson = "Archmagus-Stack/Polyglot-Dev/Rust/2026-09-25-alpha/lesson.md";
    let note = "agents/mail_room/Leo/2026-09-25-duha.md";
    let manifest = r#"
- lane: duha
  registered_by: "2026-09-25T14:00:00Z"
  date: 2026-09-25
  dev:
    source: Polyglot-Dev/Rust/2026-09-25-alpha/lesson.html
- lane: duha
  dev:
    date: 1999-01-01
    source: Polyglot-Dev/Rust/nested/lesson.html
  date: 2026-09-26
  dev:
    source: Polyglot-Dev/Rust/2026-09-26-b/lesson.html
"#;
    let paths = [lesson, note];
    let lattice = lattice_with(&paths, "2026-09-26T16:00:00Z", "2026-09-26T18:00:00Z");
    let mut hedron = StoreFixture::new();
    let lanes = [
        lane("2026-09-25", "duha", None, None),
        lane("2026-09-26", "duha", None, None),
    ];
    let batch = pass(
        &mut hedron,
        &lattice,
        manifest,
        &lanes,
        "2026-09-26T18:00:00Z",
        &QuarantineOnBadRow,
    );
    assert_eq!(report_on(&batch.lanes, "2026-09-25", "duha").status, "warm");
    let held = report_on(&batch.lanes, "2026-09-26", "duha");
    assert_eq!(held.status, "quarantined");
    assert!(
        held.quarantine.as_ref().unwrap().reason.contains("2026-09-26"),
        "{}",
        held.quarantine.as_ref().unwrap().reason
    );
    assert!(
        !held.quarantine.as_ref().unwrap().reason.contains("1999-01-01"),
        "nested date must not set the blast radius"
    );
    assert_eq!(batch.quarantine[0].date.as_deref(), Some("2026-09-26"));
}

#[test]
fn default_check_at_follows_new_york_across_the_fall_back() {
    let before = default_lane_check_at("2026-10-31", "maghrib").unwrap();
    let after = default_lane_check_at("2026-11-02", "maghrib").unwrap();
    assert_eq!(before, "2026-10-31T20:35:00-04:00");
    assert_eq!(after, "2026-11-02T20:35:00-05:00");
    let early = parse_timestamp(&before).unwrap();
    let late = parse_timestamp(&after).unwrap();
    assert_eq!(late - early, 49 * 3_600);
}
