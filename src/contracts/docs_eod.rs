//! Generated from HedronDB `DocsEodSpec` (hedrondb @ c681943).
//! Adapted from hedron-ncl `src/gen/docs_eod.rs` — shape contract for `kind: docs_eod`.
//! No nickel-lang-core in this module.

use serde_yaml::Value;

use crate::error::{Error, Result};
use crate::reconcile::DOCS_EOD_KIND;
use crate::types::DocsEodSpec;

/// Validate a `kind: docs_eod` spec body (`date`, `required_briefs`).
pub fn check_shape(spec: &Value) -> Result<DocsEodSpec> {
    serde_yaml::from_value(spec.clone()).map_err(|err| {
        Error::Invalid(format!("{DOCS_EOD_KIND} spec: {err}"))
    })
}
