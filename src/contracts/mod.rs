//! Generated shape contracts (plain Rust, no Nickel VM).

pub mod docs_eod;

use serde_yaml::Value;

use crate::error::Result;
use crate::reconcile::{parse_curriculum_clock_spec, CURRICULUM_CLOCK_KIND, DOCS_EOD_KIND};

/// Reject specs that fail the kind shape contract before persisting.
pub fn validate_spec_shape(spec: &Value) -> Result<()> {
    match spec.get("kind").and_then(Value::as_str) {
        Some(DOCS_EOD_KIND) => {
            docs_eod::check_shape(spec)?;
        }
        Some(CURRICULUM_CLOCK_KIND) => {
            parse_curriculum_clock_spec(spec)?;
        }
        _ => {}
    }
    Ok(())
}
