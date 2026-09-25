//! Read-only Lapis lattice. `mode=ro` plus `PRAGMA query_only=ON`.
//! Never `immutable=1` (that skips the WAL and reads stale pages).

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, OpenFlags};

use crate::error::{Error, Result};
use crate::source::time::walk_watermark;

const DOCUMENT_COLUMNS: &[&str] = &[
    "path",
    "frontmatter_json",
    "mtime",
    "content_hash",
    "indexed_at",
];

const INDEX_STATE_COLUMNS: &[&str] = &[
    "last_reconcile_at",
    "last_indexer_at",
    "last_full_pass_at",
    "document_count",
];

const ROW_SQL: &str = "
SELECT d.path,
       d.indexed_at,
       d.mtime,
       d.content_hash,
       d.doc_type,
       json_extract(d.frontmatter_json, '$.status') AS fm_status,
       json_extract(d.frontmatter_json, '$.fired_at') AS fired_at,
       json_extract(d.frontmatter_json, '$.landed_at') AS landed_at,
       json_extract(d.frontmatter_json, '$.redo_landed_at') AS redo_landed_at,
       json_extract(d.frontmatter_json, '$.lesson_class') AS lesson_class
FROM documents d
";

const NOT_SUPERSEDED: &str = "d.path NOT LIKE '%/\\_superseded/%' ESCAPE '\\'";

pub struct SourceConfig {
    pub lattice_path: PathBuf,
    pub busy_timeout_ms: u64,
}

impl SourceConfig {
    pub fn new(lattice_path: impl Into<PathBuf>) -> Self {
        Self {
            lattice_path: lattice_path.into(),
            busy_timeout_ms: 2_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LatticeRow {
    pub path: String,
    pub indexed_at: String,
    pub mtime: Option<f64>,
    pub content_hash: Option<String>,
    pub doc_type: Option<String>,
    pub fm_status: Option<String>,
    pub fired_at: Option<String>,
    pub landed_at: Option<String>,
    pub redo_landed_at: Option<String>,
    pub lesson_class: Option<String>,
}

/// Walk stamps only. `last_indexer_at` is recorded and then ignored.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexFreshness {
    pub last_reconcile_at: Option<String>,
    pub last_full_pass_at: Option<String>,
    pub last_indexer_at: Option<String>,
    pub document_count: Option<i64>,
    pub live_count: i64,
    pub watermark_unix: Option<i64>,
}

pub struct LatticeSource {
    conn: Connection,
    path: PathBuf,
}

impl LatticeSource {
    pub fn open(config: &SourceConfig) -> Result<Self> {
        let path = config.lattice_path.clone();
        if !path.exists() {
            return Err(Error::SourceMissing(path.display().to_string()));
        }
        let uri = ro_uri(&path)?;
        let conn = Connection::open_with_flags(
            &uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(map_source_error)?;
        conn.execute_batch("PRAGMA query_only=ON;")
            .map_err(map_source_error)?;
        conn.busy_timeout(Duration::from_millis(config.busy_timeout_ms))
            .map_err(map_source_error)?;
        check_schema(&conn)?;
        Ok(Self { conn, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn freshness(&self) -> Result<IndexFreshness> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT last_reconcile_at, last_full_pass_at, last_indexer_at, document_count,
                        (SELECT count(*) FROM documents) AS live_count
                 FROM index_state WHERE id = 1",
            )
            .map_err(map_source_error)?;
        let mapped = stmt.query_row([], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, i64>(4)?,
            ))
        });
        let (last_reconcile_at, last_full_pass_at, last_indexer_at, document_count, live_count) =
            match mapped {
                Ok(row) => row,
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    let live_count: i64 = self
                        .conn
                        .query_row("SELECT count(*) FROM documents", [], |row| row.get(0))
                        .map_err(map_source_error)?;
                    (None, None, None, None, live_count)
                }
                Err(err) => return Err(map_source_error(err)),
            };
        let watermark_unix =
            walk_watermark(last_reconcile_at.as_deref(), last_full_pass_at.as_deref());
        Ok(IndexFreshness {
            last_reconcile_at,
            last_full_pass_at,
            last_indexer_at,
            document_count,
            live_count,
            watermark_unix,
        })
    }

    /// Point lookups for intended paths. Superseded ghosts are excluded.
    pub fn rows_for(&self, paths: &[String]) -> Result<Vec<LatticeRow>> {
        let payload = serde_json::to_string(paths)?;
        let sql = format!(
            "{ROW_SQL} WHERE d.path IN (SELECT value FROM json_each(?1)) AND {NOT_SUPERSEDED} ORDER BY d.path ASC"
        );
        self.query(&sql, params![payload])
    }

    /// Glob fallback for a missing manifest row. Superseded ghosts are excluded.
    pub fn glob_rows(&self, pattern: &str) -> Result<Vec<LatticeRow>> {
        let sql =
            format!("{ROW_SQL} WHERE d.path GLOB ?1 AND {NOT_SUPERSEDED} ORDER BY d.path ASC");
        self.query(&sql, params![pattern])
    }

    fn query(&self, sql: &str, params: impl rusqlite::Params) -> Result<Vec<LatticeRow>> {
        let mut stmt = self.conn.prepare(sql).map_err(map_source_error)?;
        let rows = stmt.query_map(params, row_from).map_err(map_source_error)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(map_source_error)?);
        }
        Ok(out)
    }
}

fn check_schema(conn: &Connection) -> Result<()> {
    require_columns(conn, "documents", DOCUMENT_COLUMNS)?;
    require_columns(conn, "index_state", INDEX_STATE_COLUMNS)?;
    Ok(())
}

fn require_columns(conn: &Connection, table: &str, needed: &[&str]) -> Result<()> {
    let sql = match table {
        "documents" => "PRAGMA table_info(documents)",
        "index_state" => "PRAGMA table_info(index_state)",
        _ => {
            return Err(Error::SourceSchemaDrift(format!(
                "unknown lattice table {table}"
            )))
        }
    };
    let mut stmt = conn.prepare(sql).map_err(map_source_error)?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(map_source_error)?;
    let mut have = Vec::new();
    for name in rows {
        have.push(name.map_err(map_source_error)?);
    }
    if have.is_empty() {
        return Err(Error::SourceSchemaDrift(format!("missing table {table}")));
    }
    let missing: Vec<&str> = needed
        .iter()
        .copied()
        .filter(|column| !have.iter().any(|got| got == column))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(Error::SourceSchemaDrift(format!(
            "{table} missing columns {}",
            missing.join(", ")
        )))
    }
}

fn row_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<LatticeRow> {
    Ok(LatticeRow {
        path: row.get(0)?,
        indexed_at: row.get(1)?,
        mtime: row.get(2)?,
        content_hash: row.get(3)?,
        doc_type: row.get(4)?,
        fm_status: row.get(5)?,
        fired_at: row.get(6)?,
        landed_at: row.get(7)?,
        redo_landed_at: row.get(8)?,
        lesson_class: row.get(9)?,
    })
}

fn ro_uri(path: &Path) -> Result<String> {
    let absolute = path
        .canonicalize()
        .map_err(|err| Error::SourceUnreadable(format!("{}: {err}", path.display())))?;
    let mut encoded = String::from("file:");
    for byte in os_bytes(&absolute) {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'_' | b'-' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded.push_str("?mode=ro");
    Ok(encoded)
}

fn os_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().into_owned().into_bytes()
    }
}

fn map_source_error(err: rusqlite::Error) -> Error {
    match &err {
        rusqlite::Error::SqliteFailure(code, _) => match code.code {
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                Error::SourceLocked(err.to_string())
            }
            rusqlite::ErrorCode::CannotOpen
            | rusqlite::ErrorCode::SystemIoFailure
            | rusqlite::ErrorCode::PermissionDenied
            | rusqlite::ErrorCode::NotADatabase => Error::SourceUnreadable(err.to_string()),
            _ => Error::Sqlite(err),
        },
        rusqlite::Error::InvalidPath(_) => Error::SourceUnreadable(err.to_string()),
        _ => Error::Sqlite(err),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::SystemTime;

    use rusqlite::{params, Connection};

    use super::{LatticeSource, SourceConfig};

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

    #[test]
    fn g7_insert_is_sqlite_readonly() {
        let dir = std::env::temp_dir().join(format!(
            "hedron-lattice-ro-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lattice.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(DDL).unwrap();
        conn.execute(
            "INSERT INTO index_state (id, last_reconcile_at, last_indexer_at, last_full_pass_at, document_count)
             VALUES (1, '2026-09-25T15:00:00Z', '2026-09-25T15:00:00Z', '2026-06-07T13:55:36Z', 0)",
            [],
        )
        .unwrap();
        drop(conn);
        let before = fingerprint(&path);
        let source = LatticeSource::open(&SourceConfig::new(&path)).unwrap();
        let err = source
            .conn
            .execute(
                "INSERT INTO documents (path, indexed_at) VALUES (?1, ?2)",
                params!["__hedron_ro_probe__", "1970-01-01T00:00:00Z"],
            )
            .expect_err("read-only lattice must reject INSERT");
        let code = match &err {
            rusqlite::Error::SqliteFailure(code, _) => code.extended_code,
            _ => panic!("expected sqlite failure, got {err}"),
        };
        assert_eq!(code & 0xFF, rusqlite::ffi::SQLITE_READONLY, "{err}");
        assert_eq!(fingerprint(&path), before);
        let probes: i64 = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT count(*) FROM documents WHERE path = '__hedron_ro_probe__'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(probes, 0);
        drop(source);
        let _ = fs::remove_dir_all(&dir);
    }

    fn fingerprint(path: &std::path::Path) -> (blake3::Hash, SystemTime) {
        let bytes = fs::read(path).unwrap();
        let modified = fs::metadata(path).unwrap().modified().unwrap();
        (blake3::hash(&bytes), modified)
    }
}
