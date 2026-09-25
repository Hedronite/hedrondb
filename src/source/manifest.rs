//! In-memory parse of `_lesson-trio-manifest.yaml`. Nothing is stored.
//!
//! A bundle `source` is vault-relative under `Archmagus-Stack/` and ends in
//! `lesson.html`. The observable path is the sibling `lesson.md`. Trio rows
//! (no `lane`) are Maghrib seats `ops` / `dev` / `cert`. `lane: duha|asr` rows
//! carry one seat, usually `dev`.

use serde_yaml::Value;

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
}

pub fn parse_manifest(yaml: &str) -> Result<Vec<IntendedBundle>> {
    let value: Value = serde_yaml::from_str(yaml)?;
    let rows = manifest_rows(&value)?;
    let mut bundles = Vec::new();
    for row in rows {
        push_row(&mut bundles, row)?;
    }
    Ok(bundles)
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

fn manifest_rows(value: &Value) -> Result<&serde_yaml::Sequence> {
    if let Some(rows) = value.as_sequence() {
        return Ok(rows);
    }
    if let Some(map) = value.as_mapping() {
        for key in ["rows", "entries", "bundles", "lessons"] {
            if let Some(rows) = map
                .get(Value::String(key.to_string()))
                .and_then(Value::as_sequence)
            {
                return Ok(rows);
            }
        }
    }
    Err(Error::Invalid(
        "lesson manifest must be a YAML list of rows".into(),
    ))
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
        push_bundle(out, date, &lane, seat, &source, registered_by.clone());
    }
    if !found_seat {
        if let Some(source) = field_str(map, "source") {
            let lane = explicit_lane.unwrap_or_else(|| "maghrib".to_string());
            let seat = field_str(map, "seat").unwrap_or("dev");
            push_bundle(out, date, &lane, seat, source, registered_by);
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
