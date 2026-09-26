//! In-memory parse of `_lesson-trio-manifest.yaml`. Nothing is stored.
//!
//! A bundle `source` is vault-relative under `Archmagus-Stack/` and ends in
//! `lesson.html`. The observable path is the sibling `lesson.md`. Trio rows
//! (no `lane`) are Maghrib seats `ops` / `dev` / `cert`. `lane: duha|asr` rows
//! carry one seat, usually `dev`.

use serde_yaml::Value;

use super::time::SCOPE_START;
use crate::error::{Error, Result};
use crate::quarantine::{sha256_hex, BadRow};

/// Manifest rows dated before the backfill cut are ignored.
pub const MANIFEST_FLOOR: &str = "2026-06-11";

const SEATS: [&str; 3] = ["ops", "dev", "cert"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntendedBundle {
    pub date: String,
    pub lane: String,
    pub seat: String,
    pub lesson_md_path: String,
    pub registered_by: Option<String>,
    /// The row was edited in place. `registered_by` is then not a ship hint.
    pub revised: bool,
}

pub fn parse_manifest(yaml: &str) -> Result<Vec<IntendedBundle>> {
    let part = partition_manifest(yaml)?;
    if let Some(bad) = part.bad.first() {
        return Err(Error::Invalid(bad.reason.clone()));
    }
    Ok(part.bundles)
}

/// One physical list item, good or bad.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RowStamp {
    pub span: [usize; 2],
    pub hash: String,
    pub good: bool,
}

/// Bundles from clean rows, plus the bad rows quarantine will hold.
#[derive(Debug, Default)]
pub(crate) struct ManifestPartition {
    pub bundles: Vec<IntendedBundle>,
    pub bad: Vec<BadRow>,
    pub legacy: Vec<BadRow>,
    pub rows: Vec<RowStamp>,
}

pub(crate) fn partition_manifest(yaml: &str) -> Result<ManifestPartition> {
    let trimmed = yaml.trim();
    if trimmed.is_empty() || trimmed == "[]" || trimmed == "null" || trimmed == "~" {
        return Ok(ManifestPartition::default());
    }
    if trimmed.starts_with('[') {
        return partition_flow(trimmed);
    }
    let mut part = ManifestPartition::default();
    for row in split_block_rows(yaml)? {
        classify_row(&mut part, row);
    }
    Ok(part)
}

fn partition_flow(yaml: &str) -> Result<ManifestPartition> {
    let value: Value = serde_yaml::from_str(yaml)?;
    let Some(rows) = value.as_sequence() else {
        return Err(Error::Invalid(
            "lesson manifest must be a YAML list of rows".into(),
        ));
    };
    let mut part = ManifestPartition::default();
    for row in rows {
        push_row(&mut part.bundles, row)?;
    }
    Ok(part)
}

struct RawRow {
    start_line: usize,
    end_line: usize,
    text: String,
}

/// Top-level `- ` items, keeping the raw bytes for the row hash.
fn split_block_rows(yaml: &str) -> Result<Vec<RawRow>> {
    let mut rows = Vec::new();
    let mut current: Option<RawRow> = None;
    for (idx, line) in yaml.split_inclusive('\n').enumerate() {
        let line_no = idx + 1;
        let body = line.trim_end_matches(['\n', '\r']);
        let item = body == "-" || body.starts_with("- ");
        if item {
            if let Some(prev) = current.replace(RawRow {
                start_line: line_no,
                end_line: line_no,
                text: line.to_string(),
            }) {
                rows.push(prev);
            }
            continue;
        }
        if let Some(buf) = current.as_mut() {
            buf.text.push_str(line);
            buf.end_line = line_no;
            continue;
        }
        let trimmed = body.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed == "---" || trimmed == "..." {
            continue;
        }
        return Err(Error::Invalid(
            "lesson manifest must be a YAML list of rows".into(),
        ));
    }
    if let Some(last) = current {
        rows.push(last);
    }
    if rows.is_empty() {
        return Err(Error::Invalid(
            "lesson manifest must be a YAML list of rows".into(),
        ));
    }
    Ok(rows)
}

fn classify_row(part: &mut ManifestPartition, row: RawRow) {
    let hash = sha256_hex(row.text.as_bytes());
    let span = [row.start_line, row.end_line];
    match serde_yaml::from_str::<Value>(&row.text) {
        Ok(value) => match value.as_sequence().and_then(|seq| seq.first()) {
            Some(mapping) if mapping.is_mapping() => {
                let mut bundles = Vec::new();
                if push_row(&mut bundles, mapping).is_err() {
                    record_malformed(part, &row, hash, span, "manifest row must be a mapping");
                    return;
                }
                part.bundles.append(&mut bundles);
                part.rows.push(RowStamp {
                    span,
                    hash,
                    good: true,
                });
            }
            _ => record_malformed(part, &row, hash, span, "manifest row must be a mapping"),
        },
        Err(err) => record_malformed(part, &row, hash, span, &err.to_string()),
    }
}

fn record_malformed(
    part: &mut ManifestPartition,
    row: &RawRow,
    hash: String,
    span: [usize; 2],
    err: &str,
) {
    let pairs = top_level_pairs(&row.text);
    let date = pairs.iter().find_map(|(key, value)| {
        if key == "date" {
            normalize_date(value)
        } else {
            None
        }
    });
    if let Some(date) = date.as_deref() {
        if date < SCOPE_START {
            part.legacy.push(BadRow {
                row_span: span,
                row_hash: hash.clone(),
                reason: "legacy".into(),
                date: Some(date.to_string()),
                lanes: sniffed_lanes(&pairs),
            });
            part.rows.push(RowStamp {
                span,
                hash,
                good: false,
            });
            return;
        }
    }
    if let Some(date) = date.clone() {
        if let Some(sections) = recover_lane_sections(&row.text) {
            let mut failed = Vec::new();
            let mut section_err = String::new();
            for (lane, parsed) in sections {
                match parsed {
                    Ok(mut bundles) => part.bundles.append(&mut bundles),
                    Err(message) => {
                        section_err = message;
                        failed.push(lane);
                    }
                }
            }
            if failed.is_empty() {
                part.rows.push(RowStamp {
                    span,
                    hash,
                    good: true,
                });
                return;
            }
            let detail = if section_err.is_empty() {
                err.to_string()
            } else {
                section_err
            };
            part.bad.push(BadRow {
                row_span: span,
                row_hash: hash.clone(),
                reason: format!("malformed manifest row dated {date}: {detail}"),
                date: Some(date),
                lanes: failed,
            });
            part.rows.push(RowStamp {
                span,
                hash,
                good: false,
            });
            return;
        }
    }
    let when = date.as_deref().unwrap_or("undated");
    part.bad.push(BadRow {
        row_span: span,
        row_hash: hash.clone(),
        reason: format!("malformed manifest row dated {when}: {err}"),
        date,
        lanes: sniffed_lanes(&pairs),
    });
    part.rows.push(RowStamp {
        span,
        hash,
        good: false,
    });
}

/// Split a failed row on top-level `lane:` keys. `None` when there is only
/// one lane (nothing to salvage beside the whole row).
fn recover_lane_sections(
    chunk: &str,
) -> Option<Vec<(String, std::result::Result<Vec<IntendedBundle>, String>)>> {
    let lines: Vec<&str> = chunk.lines().collect();
    let key_col = top_level_column(&lines)?;
    let mut lane_at = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if let Some((key, value)) = line_key_at(line, key_col) {
            if key == "lane" {
                let name = clean_scalar(&value);
                if !name.is_empty() {
                    lane_at.push((idx, name));
                }
            }
        }
    }
    if lane_at.len() < 2 {
        return None;
    }
    let prelude_end = lane_at[0].0;
    let mut out = Vec::new();
    for (n, (idx, name)) in lane_at.iter().enumerate() {
        let end = lane_at.get(n + 1).map(|(next, _)| *next).unwrap_or(lines.len());
        let mut owned = Vec::new();
        if *idx == 0 {
            for line in &lines[..end] {
                owned.push((*line).to_string());
            }
        } else {
            for line in &lines[..prelude_end] {
                owned.push((*line).to_string());
            }
            for line in &lines[*idx..end] {
                owned.push((*line).to_string());
            }
        }
        let synthetic = as_list_item(&owned);
        let parsed = match serde_yaml::from_str::<Value>(&synthetic) {
            Ok(value) => match value.as_sequence().and_then(|seq| seq.first()) {
                Some(mapping) if mapping.is_mapping() => {
                    let mut bundles = Vec::new();
                    match push_row(&mut bundles, mapping) {
                        Ok(()) => Ok(bundles),
                        Err(err) => Err(err.to_string()),
                    }
                }
                _ => Err("manifest row must be a mapping".into()),
            },
            Err(err) => Err(err.to_string()),
        };
        out.push((name.clone(), parsed));
    }
    Some(out)
}

fn as_list_item(lines: &[String]) -> String {
    let mut out = String::new();
    let mut started = false;
    for line in lines {
        let trimmed = line.trim();
        if !started && (trimmed.is_empty() || trimmed.starts_with('#')) {
            continue;
        }
        if !started {
            started = true;
            if trimmed == "-" || trimmed.starts_with("- ") {
                out.push_str(line.trim_end());
            } else {
                out.push_str("- ");
                out.push_str(trimmed);
            }
            out.push('\n');
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Top-level `date:` anywhere in the row. Nested blocks are ignored.
fn top_level_pairs(chunk: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = chunk.lines().collect();
    let Some(col) = top_level_column(&lines) else {
        return Vec::new();
    };
    let mut pairs = Vec::new();
    for line in &lines {
        if let Some(pair) = line_key_at(line, col) {
            pairs.push(pair);
        }
    }
    pairs
}

fn top_level_column(lines: &[&str]) -> Option<usize> {
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("- ") {
            let rest = rest.trim_start();
            if rest.is_empty() || rest.starts_with('#') {
                continue;
            }
            return line.find(rest);
        }
        if trimmed == "-" {
            continue;
        }
        if trimmed.contains(':') {
            return Some(line.len() - line.trim_start().len());
        }
    }
    None
}

fn line_key_at(line: &str, key_col: usize) -> Option<(String, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let (col, body) = if let Some(rest) = trimmed.strip_prefix("- ") {
        let rest = rest.trim_start();
        if rest.is_empty() || rest.starts_with('#') {
            return None;
        }
        (line.find(rest)?, rest)
    } else if trimmed == "-" {
        return None;
    } else {
        (line.len() - line.trim_start().len(), trimmed)
    };
    if col != key_col {
        return None;
    }
    split_yaml_key(body)
}

fn split_yaml_key(body: &str) -> Option<(String, String)> {
    let (key, value) = body.split_once(':')?;
    if key.is_empty() || !key.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return None;
    }
    Some((key.to_string(), value.trim().to_string()))
}

fn sniffed_lanes(pairs: &[(String, String)]) -> Vec<String> {
    let mut lanes = Vec::new();
    for (key, value) in pairs {
        if key == "lane" {
            let name = clean_scalar(value);
            if !name.is_empty() && !lanes.contains(&name) {
                lanes.push(name);
            }
        }
    }
    if lanes.is_empty() && pairs.iter().any(|(key, _)| matches!(key.as_str(), "ops" | "dev" | "cert"))
    {
        lanes.push("maghrib".to_string());
    }
    lanes
}

fn clean_scalar(raw: &str) -> String {
    let token = raw.split_whitespace().next().unwrap_or("");
    token
        .trim_matches(|ch| ch == '"' || ch == '\'')
        .to_string()
}

fn normalize_date(raw: &str) -> Option<String> {
    let token = clean_scalar(raw);
    if token.len() >= 10 && token.as_bytes()[4] == b'-' && token.as_bytes()[7] == b'-' {
        let date = &token[..10];
        if date.chars().enumerate().all(|(idx, ch)| {
            idx == 4 || idx == 7 || ch.is_ascii_digit()
        }) {
            return Some(date.to_string());
        }
    }
    None
}

/// `Archmagus-Stack/` + directory of `source` + `/lesson.md`.
pub fn lesson_md_path(source: &str) -> String {
    let source = source.trim().trim_start_matches('/');
    if source.is_empty() {
        return String::new();
    }
    if source.starts_with("Archmagus-Stack/") && source.ends_with("/lesson.md") {
        return source.to_string();
    }
    let dir = source.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
    let relative = if dir.is_empty() {
        "lesson.md".to_string()
    } else {
        format!("{dir}/lesson.md")
    };
    if relative.starts_with("Archmagus-Stack/") {
        relative
    } else {
        format!("Archmagus-Stack/{relative}")
    }
}

pub fn close_note_path(date: &str, lane: &str) -> String {
    format!("agents/mail_room/Leo/{date}-{lane}.md")
}

pub fn lab_ref_path(lesson_md: &str) -> String {
    match lesson_md.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/lab-ref.md"),
        None => "lab-ref.md".to_string(),
    }
}

pub fn is_superseded_path(path: &str) -> bool {
    path.split('/').any(|segment| segment == "_superseded")
}

fn push_row(out: &mut Vec<IntendedBundle>, row: &Value) -> Result<()> {
    let Some(map) = row.as_mapping() else {
        return Err(Error::Invalid("manifest row must be a mapping".into()));
    };
    let Some(date) = field_str(map, "date") else {
        return Ok(());
    };
    if date.len() != 10 || date < MANIFEST_FLOOR {
        return Ok(());
    }
    if field_str(map, "era") == Some("pre-manifest-backfill") {
        return Ok(());
    }
    let registered_by = field_str(map, "registered_by").map(str::to_string);
    let revised = map.contains_key(Value::String("revised".to_string()));
    let explicit_lane = field_str(map, "lane").map(str::to_string);
    let mut found_seat = false;
    for seat in SEATS {
        let Some(source) = seat_source(map, seat) else {
            continue;
        };
        found_seat = true;
        let lane = explicit_lane
            .clone()
            .unwrap_or_else(|| "maghrib".to_string());
        push_bundle(
            out,
            date,
            &lane,
            seat,
            &source,
            registered_by.clone(),
            revised,
        );
    }
    if !found_seat {
        if let Some(source) = field_str(map, "source") {
            let lane = explicit_lane.unwrap_or_else(|| "maghrib".to_string());
            let seat = field_str(map, "seat").unwrap_or("dev");
            push_bundle(out, date, &lane, seat, source, registered_by, revised);
        }
    }
    Ok(())
}

fn push_bundle(
    out: &mut Vec<IntendedBundle>,
    date: &str,
    lane: &str,
    seat: &str,
    source: &str,
    registered_by: Option<String>,
    revised: bool,
) {
    let lesson_md_path = lesson_md_path(source);
    if lesson_md_path.is_empty() || is_superseded_path(&lesson_md_path) {
        return;
    }
    out.push(IntendedBundle {
        date: date.to_string(),
        lane: lane.to_string(),
        seat: seat.to_string(),
        lesson_md_path,
        registered_by,
        revised,
    });
}

fn seat_source(map: &serde_yaml::Mapping, seat: &str) -> Option<String> {
    let value = map.get(Value::String(seat.to_string()))?;
    if let Some(source) = value.as_str() {
        return Some(source.to_string());
    }
    field_str(value.as_mapping()?, "source").map(str::to_string)
}

fn field_str<'a>(map: &'a serde_yaml::Mapping, key: &str) -> Option<&'a str> {
    map.get(Value::String(key.to_string()))
        .and_then(Value::as_str)
}
