//! In-memory read of `Archmagus-Stack/_tools/register.log`.
//!
//! Lines are UTC. A row with `revised:` takes the latest matching line and
//! does not trust `registered_by`. Matching requires `date=` (or the stamp's
//! UTC date) plus `lanes=` containing the lane, or `{lane}=true`. `{lane}=false`
//! does not match.
//!
//! `trio` is maghrib's dependency chain (ops / dev / cert), not its own lane.
//! `trio=true`, a bare `trio` token, or `lanes=trio` match maghrib. An explicit
//! `maghrib=false` still wins.

use super::time::{format_unix_utc, parse_timestamp};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegisterLog {
    lines: Vec<LogHit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogHit {
    ts: i64,
    date: String,
    lanes: Vec<String>,
    flags: Vec<(String, bool)>,
    /// Bare `trio` token, or `kind=trio`. `trio=true/false` lives in `flags`.
    trio_word: bool,
}

/// Maghrib is the trio chain. Register lines name that chain `trio`.
const TRIO_LANE: &str = "maghrib";

impl RegisterLog {
    pub(crate) fn parse(text: &str) -> Self {
        let lines = text.lines().filter_map(parse_line).collect();
        Self { lines }
    }

    /// Latest unix stamp for `date` + `lane`. `None` when nothing matches.
    pub(crate) fn latest_for(&self, date: &str, lane: &str) -> Option<i64> {
        self.lines
            .iter()
            .filter(|hit| hit.matches(date, lane))
            .map(|hit| hit.ts)
            .max()
    }
}

impl LogHit {
    fn matches(&self, date: &str, lane: &str) -> bool {
        if self.date != date {
            return false;
        }
        if let Some((_, on)) = self.flags.iter().find(|(name, _)| name == lane) {
            return *on;
        }
        if self.lanes.iter().any(|name| name == lane) {
            return true;
        }
        if lane != TRIO_LANE {
            return false;
        }
        if let Some((_, on)) = self.flags.iter().find(|(name, _)| name == "trio") {
            return *on;
        }
        self.trio_word || self.lanes.iter().any(|name| name == "trio")
    }
}

fn parse_line(line: &str) -> Option<LogHit> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let ts = parse_timestamp(line)?;
    let parts: Vec<&str> = line.split_whitespace().collect();
    let mut index = 0;
    if parts
        .first()
        .and_then(|token| parse_timestamp(token))
        .is_some()
    {
        index = 1;
    }
    let mut date = None;
    let mut lanes = Vec::new();
    let mut flags = Vec::new();
    let mut trio_word = false;
    while index < parts.len() {
        if parts[index].eq_ignore_ascii_case("trio") {
            trio_word = true;
            index += 1;
            continue;
        }
        let Some((key, value)) = parts[index].split_once('=') else {
            index += 1;
            continue;
        };
        match key {
            "date" => date = Some(value.to_string()),
            "lanes" => {
                push_tokens(&mut lanes, value);
                index += 1;
                while index < parts.len() && !parts[index].contains('=') {
                    if parts[index].eq_ignore_ascii_case("trio") {
                        trio_word = true;
                    }
                    push_tokens(&mut lanes, parts[index]);
                    index += 1;
                }
                continue;
            }
            "trio" => {
                if let Some(flag) = parse_bool(value) {
                    flags.push(("trio".to_string(), flag));
                } else if !value.is_empty() {
                    trio_word = true;
                }
            }
            "kind" | "registration" if value.eq_ignore_ascii_case("trio") => {
                trio_word = true;
            }
            _ => {
                if let Some(flag) = parse_bool(value) {
                    flags.push((key.to_string(), flag));
                }
            }
        }
        index += 1;
    }
    let date = match date {
        Some(date) => date,
        None => {
            let formatted = format_unix_utc(ts);
            formatted[..10].to_string()
        }
    };
    Some(LogHit {
        ts,
        date,
        lanes,
        flags,
        trio_word,
    })
}

fn push_tokens(out: &mut Vec<String>, raw: &str) {
    for token in raw.split(|ch: char| ch == ',' || ch.is_ascii_whitespace()) {
        if !token.is_empty() {
            out.push(token.to_string());
        }
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    if value.eq_ignore_ascii_case("true") {
        Some(true)
    } else if value.eq_ignore_ascii_case("false") {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::RegisterLog;

    #[test]
    fn trio_reregistration_resolves_to_the_latest_line() {
        let log = RegisterLog::parse(
            "_tools/register.py 2026-09-25T19:54Z date=2026-09-25 trio=True ok\n\
             _tools/register.py 2026-09-25T19:55Z date=2026-09-25 trio=True ok\n\
             _tools/register.py 2026-09-25T23:01Z date=2026-09-25 trio=True ok\n\
             _tools/register.py 2026-09-25T23:02Z date=2026-09-25 trio=True ok\n",
        );
        assert_eq!(
            log.latest_for("2026-09-25", "maghrib"),
            crate::source::time::parse_timestamp("2026-09-25T23:02:00Z")
        );
    }

    #[test]
    fn explicit_maghrib_false_beats_trio() {
        let log = RegisterLog::parse(
            "2026-09-25T19:55Z date=2026-09-25 asr=True maghrib=False trio=True\n",
        );
        assert_eq!(log.latest_for("2026-09-25", "maghrib"), None);
        assert_eq!(
            log.latest_for("2026-09-25", "asr"),
            crate::source::time::parse_timestamp("2026-09-25T19:55:00Z")
        );
    }
}
