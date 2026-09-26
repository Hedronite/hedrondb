//! Read-only lesson-ship observe: lattice rows in, desired-state status out.
//!
//! The Lapis lattice file is never written and its rows are never copied into
//! HedronDB. `Observation.missing` still comes from `adapt_lapis_observe`.

mod guard;
mod lattice;
mod manifest;
mod register_log;
mod ship;
mod time;

pub use guard::{judge_freshness, AbsenceCheck, GuardVerdict};
pub use lattice::{IndexFreshness, LatticeRow, LatticeSource, SourceConfig};
pub use manifest::{
    close_note_path, lesson_md_path, parse_manifest, IntendedBundle, MANIFEST_FLOOR,
};
pub use ship::{
    cannot_tell_batch, evaluate_requirements, observe_lesson_ships, observe_lesson_ships_with,
    reconcile_lesson_ships, reconcile_lesson_ships_with, LaneDue, LaneReport, ObserveBatch,
    PathReason, RowQuarantine, ShipRequirement,
};
pub use time::{
    default_lane_check_at, format_unix_utc, new_york_wall_time, parse_timestamp, walk_watermark,
    SCOPE_START,
};
