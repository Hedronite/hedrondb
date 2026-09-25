//! HedronDB Phase 0 core: Desired State, causal Event Log, vault isolation.
//!
//! Query current state and causal history through separate APIs. The product
//! CLI is `hedron` (`import`, `hql`). This crate is a sync rusqlite library —
//! not a server, not a general-purpose database.

mod contracts;
mod error;
pub mod hql;
pub mod import;
pub mod jev;
pub mod mcp;
pub mod reconcile;
pub mod source;
mod store;
mod types;

pub use error::{Error, Result};
pub use jev::{ask, Choice, Decision, Transport};
pub use reconcile::{
    adapt_lapis_observe, CurriculumClock, DocsEod, Observation, Reconciler, CURRICULUM_CLOCK_KIND,
    DOCS_EOD_KIND,
};
pub use source::{
    cannot_tell_batch, close_note_path, evaluate_requirements, format_unix_utc, judge_freshness,
    lesson_md_path, observe_lesson_ships, parse_manifest, parse_timestamp, reconcile_lesson_ships,
    walk_watermark, AbsenceCheck, GuardVerdict, IndexFreshness, IntendedBundle, LaneDue,
    LaneReport, LatticeSource, ObserveBatch, PathReason, ShipRequirement, SourceConfig,
    SCOPE_START,
};
pub use store::{Store, SCHEMA_SQL};
pub use types::{
    Bootstrap, Condition, ConditionKind, CurriculumClockSpec, CurriculumExpected, CurriculumGlobs,
    CurriculumPath, DesiredState, DocsEodSpec, Edge, Event, LessonClockSpec, Node, NodeType,
    Status, Tier, CAUSAL_CAUSED_BY, CAUSAL_RECONCILES, CAUSAL_SUPERSEDES, EDGE_GRANT,
};
