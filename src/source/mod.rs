//! Read-only lesson-ship observe: lattice rows in, desired-state status out.
//!
//! The Lapis lattice file is never written and its rows are never copied into
//! HedronDB. `Observation.missing` still comes from `adapt_lapis_observe`.

mod guard;
mod lattice;
mod manifest;
mod ship;
mod time;

pub use guard::{judge_freshness, AbsenceCheck, GuardVerdict};
pub use lattice::{IndexFreshness, LatticeRow, LatticeSource, RejectedWrite, SourceConfig};
pub use manifest::{
    close_note_path, lesson_md_path, parse_manifest, IntendedBundle, MANIFEST_FLOOR,
};
pub use ship::{
    cannot_tell_batch, evaluate_requirements, observe_lesson_ships, reconcile_lesson_ships,
    LaneDue, LaneReport, ObserveBatch, PathReason, ShipRequirement,
};
pub use time::{format_unix_utc, parse_timestamp, walk_watermark, SCOPE_START};
