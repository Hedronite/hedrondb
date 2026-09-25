//! Timestamp parse for lattice text. Accepts `Z`, `±HH:MM`, and `T` or space.
//! Naive values (no zone) are UTC. `last_indexer_at` is not a watermark input.

/// First lesson-ship date this slice evaluates. Earlier dates are ignored.
pub const SCOPE_START: &str = "2026-09-25";

/// Unix seconds. `None` when `raw` is not a timestamp we accept.
pub fn parse_timestamp(raw: &str) -> Option<i64> {
    let s = raw.trim();
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let year = parse_digits(&b[0..4])?;
    if b[4] != b'-' || b[7] != b'-' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let month = parse_digits(&b[5..7])?;
    let day = parse_digits(&b[8..10])?;
    if b[10] != b'T' && b[10] != b't' && b[10] != b' ' {
        return None;
    }
    let hour = parse_digits(&b[11..13])?;
    let minute = parse_digits(&b[14..16])?;
    let second = parse_digits(&b[17..19])?;
    if !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let month_u = month as u32;
    let day_u = day as u32;
    if !valid_ymd(year, month_u, day_u) {
        return None;
    }
    let mut rest = &s[19..];
    if rest.starts_with('.') {
        let digits = rest[1..].chars().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        rest = &rest[1 + digits..];
    }
    let offset = if rest.is_empty() || rest == "Z" || rest == "z" {
        0
    } else {
        parse_offset(rest)?
    };
    let local = days_from_civil(year, month_u, day_u) * 86_400
        + i64::from(hour) * 3_600
        + i64::from(minute) * 60
        + i64::from(second);
    Some(local - offset)
}

/// `max(last_reconcile_at, last_full_pass_at)`. Does not read `last_indexer_at`.
pub fn walk_watermark(
    last_reconcile_at: Option<&str>,
    last_full_pass_at: Option<&str>,
) -> Option<i64> {
    match (
        last_reconcile_at.and_then(parse_timestamp),
        last_full_pass_at.and_then(parse_timestamp),
    ) {
        (Some(reconcile), Some(full_pass)) => Some(reconcile.max(full_pass)),
        (Some(reconcile), None) => Some(reconcile),
        (None, Some(full_pass)) => Some(full_pass),
        (None, None) => None,
    }
}

pub fn format_unix_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400) as u32;
    let hour = rem / 3_600;
    let minute = (rem % 3_600) / 60;
    let second = rem % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn parse_offset(rest: &str) -> Option<i64> {
    let b = rest.as_bytes();
    if b.len() != 6 || (b[0] != b'+' && b[0] != b'-') || b[3] != b':' {
        return None;
    }
    let hours = parse_digits(&b[1..3])?;
    let mins = parse_digits(&b[4..6])?;
    if hours > 23 || mins > 59 {
        return None;
    }
    let magnitude = i64::from(hours) * 3_600 + i64::from(mins) * 60;
    Some(if b[0] == b'-' { -magnitude } else { magnitude })
}

fn parse_digits(bytes: &[u8]) -> Option<i32> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    text.parse().ok()
}

fn valid_ymd(year: i32, month: u32, day: u32) -> bool {
    if day == 0 {
        return false;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let last = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    day <= last
}

/// Days since 1970-01-01 (Howard Hinnant).
fn days_from_civil(mut year: i32, month: u32, day: u32) -> i64 {
    year -= i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = (year - era * 400) as u64;
    let month_adj = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * month_adj + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + u64::from(doy);
    era as i64 * 146_097 + doe as i64 - 719_468
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    (year, month as u32, day as u32)
}
