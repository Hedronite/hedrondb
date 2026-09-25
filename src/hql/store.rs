//! Read-only HedronDB store. rusqlite OPEN_READ_ONLY; never writes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use rusqlite::{Connection, OpenFlags};

use crate::error::{Error, Result};
use crate::hql::row::{parse_extra, DesiredStateView, EdgeView, HistoryEventView, NodeView};
use crate::store::{Store, SCHEMA_SQL};
use crate::types::Event;
use uuid::Uuid;

/// Expected tables and `(column, declared type)` pairs, derived from the
/// crate `schema.sql` embedded at compile time. Not a second handwritten list.
fn expected_schema() -> &'static [(String, Vec<(String, String)>)] {
    static PARSED: OnceLock<Vec<(String, Vec<(String, String)>)>> = OnceLock::new();
    PARSED.get_or_init(|| parse_schema_sql(SCHEMA_SQL))
}

/// Minimal `CREATE TABLE` reader for `schema.sql`: table name plus columns in
/// declaration order. Table constraints (`UNIQUE`, `PRIMARY KEY`, ...) are not
/// columns and are skipped. `--` comments are stripped first.
pub(crate) fn parse_schema_sql(sql: &str) -> Vec<(String, Vec<(String, String)>)> {
    const CONSTRAINTS: [&str; 5] = ["UNIQUE", "PRIMARY", "FOREIGN", "CHECK", "CONSTRAINT"];
    let stripped: String = sql
        .lines()
        .map(|line| line.split("--").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    let mut tables = Vec::new();
    let mut rest = stripped.as_str();
    while let Some(start) = rest.find("CREATE TABLE") {
        let after = &rest[start + "CREATE TABLE".len()..];
        let (Some(open), Some(close)) = (after.find('('), after.find(");")) else {
            break;
        };
        let name = after[..open].split_whitespace().last().unwrap_or("").to_string();
        let mut cols = Vec::new();
        for piece in split_top_level(&after[open + 1..close]) {
            let mut words = piece.split_whitespace();
            let Some(col) = words.next() else { continue };
            if CONSTRAINTS.contains(&col.to_ascii_uppercase().as_str()) {
                continue;
            }
            let ty = words.next().unwrap_or("").to_string();
            cols.push((col.to_string(), ty));
        }
        tables.push((name, cols));
        rest = &after[close + 2..];
    }
    tables
}

/// Split a column body on commas outside parentheses, so
/// `UNIQUE (vault_id, name)` stays one piece.
fn split_top_level(body: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, c) in body.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                pieces.push(&body[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    pieces.push(&body[start..]);
    pieces
}

/// Read-only sqlite handle. Never migrate; report schema drift only.
pub struct RoStore {
    conn: Connection,
    path: PathBuf,
}

impl RoStore {
    pub fn open(db: impl AsRef<Path>) -> Result<Self> {
        let path = db.as_ref().to_path_buf();
        if !path.exists() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("store not found: {}", path.display()),
            )));
        }
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(Self { conn, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn schema_mismatches(&self) -> Result<Vec<String>> {
        let existing = self.table_columns()?;
        let mut mismatches = Vec::new();
        let expected = expected_schema();
        let expected_names: Vec<&str> = expected.iter().map(|(n, _)| n.as_str()).collect();
        for (table, cols) in expected {
            let Some(got) = existing.get(table.as_str()) else {
                mismatches.push(format!("missing table {table}"));
                continue;
            };
            let want_names: Vec<&str> = cols.iter().map(|(n, _)| n.as_str()).collect();
            let got_names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
            if got_names != want_names {
                mismatches.push(format!(
                    "table {table} columns {got_names:?} != {want_names:?}"
                ));
            }
            for ((want_name, want_type), (got_name, got_type)) in cols.iter().zip(got.iter()) {
                if want_name != got_name {
                    continue;
                }
                if !want_type.eq_ignore_ascii_case(got_type) {
                    mismatches.push(format!(
                        "table {table} column {want_name} type {got_type:?} != {want_type:?}"
                    ));
                }
            }
        }
        for table in existing.keys() {
            if table.starts_with("sqlite_") {
                continue;
            }
            if !expected_names.contains(&table.as_str()) {
                mismatches.push(format!("unexpected table {table}"));
            }
        }
        Ok(mismatches)
    }

    pub fn nodes(&self) -> Result<Vec<NodeView>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, vault_id, type, path, extra FROM nodes")?;
        let rows = stmt.query_map([], |row| {
            let extra: Option<String> = row.get(4)?;
            let extra = extra.unwrap_or_default();
            Ok(NodeView {
                id: row.get(0)?,
                vault_id: row.get(1)?,
                node_type: row.get(2)?,
                path: row.get(3)?,
                extra_map: parse_extra(&extra),
                extra,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Error::from)
    }

    pub fn edges(&self) -> Result<Vec<EdgeView>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, vault_id, from_id, to_id, to_raw, type, properties FROM edges")?;
        let rows = stmt.query_map([], |row| {
            let properties: Option<String> = row.get(6)?;
            Ok(EdgeView {
                id: row.get(0)?,
                from_id: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                to_id: row.get(3)?,
                to_raw: row.get(4)?,
                edge_type: row.get(5)?,
                properties: properties.unwrap_or_default(),
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Error::from)
    }

    /// Warm path: every named desired state, ordered by vault then name.
    /// Never reads events.
    pub fn desired_states(&self) -> Result<Vec<DesiredStateView>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, vault_id, name, state_version, reconciled_by, importance, spec, status \
             FROM desired_states ORDER BY vault_id ASC, name ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let spec: Option<String> = row.get(6)?;
            let status: Option<String> = row.get(7)?;
            Ok(DesiredStateView {
                id: row.get(0)?,
                vault_id: row.get(1)?,
                name: row.get(2)?,
                state_version: row.get(3)?,
                reconciled_by: row.get(4)?,
                importance: row.get(5)?,
                spec: spec.unwrap_or_default(),
                status: status.unwrap_or_default(),
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Error::from)
    }

    /// Cool path: events with `reconciles = desired_state_id`, `ts ASC, id ASC`.
    /// Drops `data`; never returns spec vs status.
    pub fn causal_chain(&self, desired_state_id: &str) -> Result<Vec<HistoryEventView>> {
        let id =
            Uuid::parse_str(desired_state_id).map_err(|err| Error::Invalid(err.to_string()))?;
        let events = Store::read_causal_chain(&self.conn, id)?;
        Ok(events.iter().map(history_view).collect())
    }

    pub fn vault_ids_named(&self, name: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, path, extra FROM nodes WHERE type = 'Vault'")?;
        let rows = stmt.query_map([], |row| {
            let extra: Option<String> = row.get(2)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                extra.unwrap_or_default(),
            ))
        })?;
        let mut ids = Vec::new();
        for row in rows {
            let (id, path, extra) = row?;
            let extra_map = parse_extra(&extra);
            let extra_name = extra_map.get("name").and_then(|v| v.as_deref());
            if path.as_deref() == Some(name) || extra_name == Some(name) {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    fn table_columns(&self) -> Result<HashMap<String, Vec<(String, String)>>> {
        let mut tables = Vec::new();
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?;
        let names = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for name in names {
            tables.push(name?);
        }
        let mut out = HashMap::new();
        for table in tables {
            if !table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(Error::Invalid(format!("unexpected table name {table}")));
            }
            let mut cols = Vec::new();
            let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
            let rows = stmt.query_map([], |row| {
                let name: String = row.get(1)?;
                let ty: String = row.get::<_, Option<String>>(2)?.unwrap_or_default();
                Ok((name, ty))
            })?;
            for row in rows {
                cols.push(row?);
            }
            out.insert(table, cols);
        }
        Ok(out)
    }
}

fn history_view(event: &Event) -> HistoryEventView {
    let caused_by = if event.caused_by.is_empty() {
        None
    } else {
        Some(
            event
                .caused_by
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(","),
        )
    };
    HistoryEventView {
        id: event.id.to_string(),
        vault_id: event.vault_id.to_string(),
        ts: event.ts,
        actor: event.actor.to_string(),
        event_type: event.event_type.clone(),
        caused_by,
        reconciles: event.reconciles.map(|id| id.to_string()),
        supersedes: event.supersedes.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parsed expectation must equal what sqlite reports after executing
    /// the same `schema.sql`; otherwise the drift check is checking a fiction.
    #[test]
    fn parsed_schema_matches_executed_schema() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        let parsed = parse_schema_sql(SCHEMA_SQL);
        assert!(!parsed.is_empty(), "schema.sql parsed to zero tables");
        for (table, cols) in &parsed {
            let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})")).unwrap();
            let live: Vec<(String, String)> = stmt
                .query_map([], |row| Ok((row.get(1)?, row.get(2)?)))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            assert_eq!(&live, cols, "table {table}: PRAGMA vs parsed schema.sql");
        }
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY rowid")
            .unwrap();
        let live_tables: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        let parsed_tables: Vec<String> = parsed.iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(live_tables, parsed_tables);
    }
}
