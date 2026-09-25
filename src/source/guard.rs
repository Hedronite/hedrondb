//! Freshness guard. An absent path is a trusted miss only when the walk
//! watermark is at or after the ship hint. Otherwise the lane-day is stale.
//! `last_indexer_at` is not an input.

/// One intended path after the lattice read.
pub struct AbsenceCheck {
    pub path: String,
    pub present: bool,
    /// Earliest evidence the file should already be visible, unix seconds.
    pub landed_hint: Option<i64>,
    /// Used only when `landed_hint` is absent.
    pub check_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardVerdict {
    Trusted,
    Stale { untrusted: Vec<String> },
}

/// `watermark` is `max(last_reconcile_at, last_full_pass_at)` in unix seconds.
pub fn judge_freshness(paths: &[AbsenceCheck], watermark: Option<i64>) -> GuardVerdict {
    let mut untrusted = Vec::new();
    for path in paths {
        if path.present || absence_trusted(path, watermark) {
            continue;
        }
        untrusted.push(path.path.clone());
    }
    if untrusted.is_empty() {
        GuardVerdict::Trusted
    } else {
        GuardVerdict::Stale { untrusted }
    }
}

fn absence_trusted(path: &AbsenceCheck, watermark: Option<i64>) -> bool {
    let Some(mark) = watermark else {
        return false;
    };
    if let Some(hint) = path.landed_hint {
        return mark >= hint;
    }
    if let Some(check_at) = path.check_at {
        return mark >= check_at;
    }
    false
}
