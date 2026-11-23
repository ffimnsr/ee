//! Recursive harness improvement loop (Harness2 adaptation).
//!
//! The crate drives Propose → Probe → Compose → Select and the post-run
//! reflector over an injected [`channel::Channel`]. The channel owns model and
//! host execution; this crate owns evidence curation, validation gates, budget
//! accounting, selection rules, run state, and the deterministic demo driver.
//!
//! Invocation is the only gate: nothing runs unless a caller invokes the
//! driver. Components mount from the workspace or a candidate directory through
//! [`ee_agent_orchestrator::harness_source::HarnessSource`].

pub mod channel;
pub mod compose;
pub mod context;
pub mod demo;
pub mod driver;
pub mod error;
pub mod leave_one_out;
pub mod ledger;
pub mod regression;
pub mod runstate;
pub mod select;
pub mod task;

pub use channel::{
    Channel, ChannelFuture, Deliverable, DeliverableAsset, ImproverModel, ImproverReply,
    ImproverRequest, ImproverRole, RunRecord, RunRequest,
};
pub use compose::{ComposeNote, parse_notes};
pub use driver::{
    DryRunPlan, HarnessConfig, HarnessMode, HarnessRun, ReflectionWriter, StreamTaskRun,
    run_stream, run_task, stream_order_path,
};
pub use ee_agent_orchestrator::harness_reflection::ReflectionToolRecord;
pub use ee_agent_orchestrator::harness_validation::{HarnessTree, materialize, validate};
pub use ee_agent_orchestrator::rollout_view::{
    RolloutDiffFile, RolloutToolRecord, RolloutTurn, RolloutView,
};
pub use ee_agent_orchestrator::user_experience::{ExperienceEntry, save_entry};
pub use error::{HarnessError, HarnessResult};
pub use ledger::{Ledger, LedgerReport};
pub use select::{AnonPool, Candidate, Selection};
pub use task::HarnessTask;

/// Current harness-loop report schema version.
pub const HARNESS_SCHEMA_VERSION: u32 = 1;
