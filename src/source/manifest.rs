//! In-memory parse of `_lesson-trio-manifest.yaml`. Nothing is stored.
//!
//! A bundle `source` is vault-relative under `Archmagus-Stack/` and ends in
//! `lesson.html`. The observable path is the sibling `lesson.md`. Trio rows
//! (no `lane`) are Maghrib seats `ops` / `dev` / `cert`. `lane: duha|asr` rows
//! carry one seat, usually `dev`.

use serde_yaml::Value;

use super::time::SCOPE_START;
use crate::error::{Error, Result};

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
    let trimmed = yaml.trim();
    if trimmed.is_empty() || trimmed == "[]" || trimmed == "null" || trimmed == "~" {
        return Ok(Vec::new());
    }
    if trimmed.starts_with('[') {
        let value: Value = serde_yaml::from_str(trimmed)?;
        let Some(rows) = value.as_sequence() else {
            return Err(Error::Invalid(
                "lesson manifest must be a YAML list of rows".into(),
            ));
        };
        let mut bundles = Vec::new();
        for row in rows {
            push_row(&mut bundles, row)?;
        }
        return Ok(bundles);
    }
    let mut bundles = Vec::new();
    for chunk in split_block_rows(yaml)? {
        match serde_yaml::from_str::<Value>(&chunk) {
            Ok(value) => {
                let Some(row) = value.as_sequence().and_then(|seq| seq.first()) else {
                    return Err(Error::Invalid("manifest row must be a mapping".into()));
                };
                push_row(&mut bundles, row)?;
            }
            Err(err) => {
                let date = sniff_date(&chunk);
                if let Some(date) = date.as_deref() {
                    if date < SCOPE_START {
                        continue;
                    }
                }
                let when = date.as_deref().unwrap_or("undated");
                return Err(Error::Invalid(format!(
                    "malformed manifest row dated {when}: {err}"
                )));
            }
        }
    }
    Ok(bundles)
}

/// Top-level `- ` items. One bad legacy row must not fail the whole document.
fn split_block_rows(yaml: &str) -> Result<Vec<String>> {
    let mut rows = Vec::new();
    let mut current: Option<String> = None;
    for line in yaml.lines() {
        let item = line == "-" || line.starts_with("- ");
        if item {
            if let Some(prev) = current.replace(format!("{line}\n")) {
                rows.push(prev);
            }
            continue;
        }
        if let Some(buf) = current.as_mut() {
            buf.push_str(line);
            buf.push('\n');
            continue;
        }
        let trimmed = line.trim();
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

fn sniff_date(chunk: &str) -> Option<String> {
    for line in chunk.lines() {
        let trimmed = line.trim().trim_start_matches('-').trim();
        let rest = trimmed.strip_prefix("date:")?.trim();
        let rest = rest.trim_matches(|ch| ch == '"' || ch == '\'');
        if rest.len() >= 10 && rest.as_bytes()[4] == b'-' && rest.as_bytes()[7] == b'-' {
            return Some(rest[..10].to_string());
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
