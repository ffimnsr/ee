//! Execution and improver channel contract.
//!
//! The channel is the only place models and hosts run. The driver consumes
//! [`Channel`] so hermetic tests can supply a deterministic fake while
//! production wiring runs reference/probe rollouts and improver sessions.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use ee_agent_orchestrator::harness_reflection::ReflectionToolRecord;
use ee_agent_orchestrator::harness_validation::HarnessTree;
use ee_agent_orchestrator::rollout_view::RolloutView;

use crate::task::HarnessTask;

/// Boxed channel future.
pub type ChannelFuture<T> =
    Pin<Box<dyn Future<Output = Result<T, crate::error::HarnessError>> + Send>>;

/// One bounded deliverable produced by a run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Deliverable {
    /// Final answer or deliverable body.
    pub text: String,
    /// Named assets produced by the run.
    pub assets: Vec<DeliverableAsset>,
}

/// One named deliverable asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliverableAsset {
    /// Exact asset name (never a whole run directory).
    pub name: String,
    /// Asset size in bytes.
    pub bytes: usize,
    /// Bounded content, when the adapter chose to include it.
    pub content: Option<String>,
}

/// One reference or probe execution request.
#[derive(Debug, Clone)]
pub struct RunRequest {
    /// Stable label (`ref-0`, `c-inv-1`, `composed-0`, ...).
    pub label: String,
    /// Task under test.
    pub task: HarnessTask,
    /// Workspace-shaped harness root mounted for this run.
    pub harness_root: PathBuf,
    /// Per-run scratch root. The channel owns everything inside it.
    pub scratch_root: PathBuf,
}

/// One completed run.
#[derive(Debug, Clone)]
pub struct RunRecord {
    /// Stable label echoed from the request.
    pub label: String,
    /// Deliverable, when the run delivered one.
    pub delivered: Option<Deliverable>,
    /// Bounded rollout projection.
    pub rollout: RolloutView,
    /// Tool records used by the reflector.
    pub tool_records: Vec<ReflectionToolRecord>,
    /// Bounded workspace diff summary.
    pub workspace_diff: Option<String>,
    /// Model sessions consumed by the run itself.
    pub sessions: u64,
    /// Rollouts consumed by the run itself.
    pub rollouts: u64,
    /// Total tokens reported, when known.
    pub usage_tokens: Option<u64>,
}

/// One improver role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ImproverRole {
    /// Process/execution evidence editor.
    Inv,
    /// Domain/artifact-contract editor.
    Dom,
    /// Evidence-tied composer.
    Composer,
    /// Anonymized deliverable selector.
    Selector,
    /// Post-run reflector.
    Reflector,
}

impl ImproverRole {
    /// Stable label used in ledger events and cache keys.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inv => "inv",
            Self::Dom => "dom",
            Self::Composer => "composer",
            Self::Selector => "selector",
            Self::Reflector => "reflector",
        }
    }

    /// Builtin role name used by the runtime subagent manager.
    #[must_use]
    pub fn builtin_role_name(self) -> &'static str {
        match self {
            Self::Inv => "harness_editor_inv",
            Self::Dom => "harness_editor_dom",
            Self::Composer => "harness_composer",
            Self::Selector => "harness_selector",
            Self::Reflector => "harness_reflector",
        }
    }
}

/// Improver execution policy, configurable independently of the executor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImproverModel {
    /// Explicit model id; `None` uses the host default.
    pub model_id: Option<String>,
    /// Optional reasoning-effort label.
    pub effort: Option<String>,
    /// Maximum model steps for one improver session.
    pub max_steps: usize,
    /// Wall-clock timeout in seconds.
    pub timeout_secs: u64,
}

impl ImproverModel {
    /// Validates the policy bounds.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_steps == 0 || self.max_steps > 64 {
            return Err("improver max_steps must be 1-64".into());
        }
        if self.timeout_secs == 0 || self.timeout_secs > 3_600 {
            return Err("improver timeout_secs must be 1-3600".into());
        }
        Ok(())
    }
}

/// One improver session request.
#[derive(Debug, Clone)]
pub struct ImproverRequest {
    /// Stable label (`inv-0`, `retry-inv-0`, ...).
    pub label: String,
    /// Role under test.
    pub role: ImproverRole,
    /// Stage mission text (contract + current stage goal).
    pub mission: String,
    /// Curated evidence directory built by the driver.
    pub context_dir: PathBuf,
    /// Read-only base harness (workspace-shaped).
    pub base_harness: PathBuf,
    /// Output directory the improver writes its candidate harness into.
    pub candidate_dir: PathBuf,
    /// Improver execution policy.
    pub model: ImproverModel,
}

/// One improver session reply.
#[derive(Debug, Clone)]
pub struct ImproverReply {
    /// Harvested candidate tree. Empty replies are rejected by the driver.
    pub tree: HarnessTree,
    /// Free-form notes (composition notes for the composer).
    pub notes: String,
}

/// Execution and improver channel.
pub trait Channel: Send + Sync {
    /// Runs one reference or probe.
    fn run_task(&self, request: RunRequest) -> ChannelFuture<RunRecord>;

    /// Runs one improver session and harvests its candidate tree.
    fn improve(&self, request: ImproverRequest) -> ChannelFuture<ImproverReply>;
}
