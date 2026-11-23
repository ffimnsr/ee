//! Leave-one-out library evaluation (I46).
//!
//! Offline only: each fixture task is run twice, once with the full completed
//! library and once with the task's own experience entry (and all later
//! entries) excluded. The per-domain delta shows whether an entry transfers
//! rather than merely accumulating. This module is never called by an online
//! task path.

use std::path::Path;

use crate::channel::Channel;
use crate::driver::{HarnessConfig, HarnessRun, run_task};
use crate::error::{HarnessError, HarnessResult};
use crate::task::HarnessTask;

/// One leave-one-out fixture.
#[derive(Debug, Clone)]
pub struct LeaveOneOutFixture {
    /// Library domain.
    pub domain: String,
    /// Task to re-run.
    pub task: HarnessTask,
    /// Sequence of the entry produced by this task's own run.
    pub own_seq: u64,
}

/// Delivery summary for one side of the comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeSummary {
    /// Whether the run selected a final deliverable.
    pub delivered: bool,
    /// Selected internal label, when any.
    pub selected: Option<String>,
    /// Dead rollout count.
    pub dead_rollouts: u64,
}

/// One per-domain delta row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaRow {
    /// Library domain.
    pub domain: String,
    /// Task id.
    pub task_id: String,
    /// Excluded experience sequence.
    pub own_seq: u64,
    /// Run with the completed library.
    pub with_entry: OutcomeSummary,
    /// Run with this task's entry excluded.
    pub without_entry: OutcomeSummary,
    /// `without.delivered - with.delivered`.
    pub delivered_delta: i64,
    /// `without.dead_rollouts - with.dead_rollouts`.
    pub dead_rollout_delta: i64,
}

/// Leave-one-out report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaveOneOutReport {
    /// One row per fixture, in fixture order.
    pub rows: Vec<DeltaRow>,
}

impl LeaveOneOutReport {
    /// Total delivered delta across domains.
    #[must_use]
    pub fn total_delivered_delta(&self) -> i64 {
        self.rows.iter().map(|row| row.delivered_delta).sum()
    }
}

/// Runs the offline leave-one-out protocol.
pub async fn run_leave_one_out(
    channel: &dyn Channel,
    fixtures: &[LeaveOneOutFixture],
    config: &HarnessConfig,
    root: &Path,
) -> HarnessResult<LeaveOneOutReport> {
    let mut rows = Vec::new();
    for fixture in fixtures {
        if fixture.task.domain != fixture.domain {
            return Err(HarnessError::State(format!(
                "leave-one-out fixture domain {:?} does not match task domain {:?}",
                fixture.domain, fixture.task.domain
            )));
        }
        if fixture.own_seq == 0 {
            return Err(HarnessError::State(String::from(
                "leave-one-out own_seq must be a positive experience sequence",
            )));
        }
        let task_root = root.join(&fixture.domain).join(&fixture.task.id);
        let with_entry =
            run_task(channel, &fixture.task, config, &task_root.join("with-entry"), None).await?;
        let excluded = fixture.task.clone().with_library_before_seq(fixture.own_seq);
        let without_entry =
            run_task(channel, &excluded, config, &task_root.join("without-entry"), None).await?;
        rows.push(DeltaRow {
            domain: fixture.domain.clone(),
            task_id: fixture.task.id.clone(),
            own_seq: fixture.own_seq,
            with_entry: summary(&with_entry),
            without_entry: summary(&without_entry),
            delivered_delta: i64::from(u8::from(summary(&without_entry).delivered))
                - i64::from(u8::from(summary(&with_entry).delivered)),
            dead_rollout_delta: without_entry.ledger.dead_rollouts.len() as i64
                - with_entry.ledger.dead_rollouts.len() as i64,
        });
    }
    Ok(LeaveOneOutReport { rows })
}

fn summary(run: &HarnessRun) -> OutcomeSummary {
    OutcomeSummary {
        delivered: run.selection.chosen.is_some(),
        selected: run.selection.chosen.clone(),
        dead_rollouts: run.ledger.dead_rollouts.len() as u64,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use ee_agent_orchestrator::harness_validation::HarnessTree;
    use ee_agent_orchestrator::user_experience::{
        EditRow, EditVerdict, ExperienceConfidence, ExperienceEntry, ExperienceScope, save_entry,
    };

    use super::*;
    use crate::channel::{
        ChannelFuture, Deliverable, ImproverReply, ImproverRequest, ImproverRole, RunRecord,
        RunRequest,
    };

    struct CueChannel {
        seen: Arc<Mutex<BTreeSet<String>>>,
    }

    impl CueChannel {
        fn new() -> Self {
            Self { seen: Arc::new(Mutex::new(BTreeSet::new())) }
        }
    }

    impl Channel for CueChannel {
        fn run_task(&self, request: RunRequest) -> ChannelFuture<RunRecord> {
            Box::pin(async move {
                Ok(RunRecord {
                    label: request.label,
                    delivered: Some(Deliverable {
                        text: String::from("delivered"),
                        assets: Vec::new(),
                    }),
                    rollout: ee_agent_orchestrator::rollout_view::RolloutView::default(),
                    tool_records: Vec::new(),
                    workspace_diff: None,
                    sessions: 1,
                    rollouts: 1,
                    usage_tokens: None,
                })
            })
        }

        fn improve(&self, request: ImproverRequest) -> ChannelFuture<ImproverReply> {
            let seen = self.seen.clone();
            Box::pin(async move {
                if request.role == ImproverRole::Inv {
                    let cues = std::fs::read_to_string(request.context_dir.join("library/cues.md"))
                        .unwrap_or_default();
                    seen.lock().expect("seen").insert(cues);
                }
                let tree = HarnessTree::load_components(&request.base_harness.join(".agents"))
                    .unwrap_or_default();
                Ok(ImproverReply {
                    tree,
                    notes: String::from("kept -> base harness -> probe delivered"),
                })
            })
        }
    }

    fn entry(seq: u64, cue: &str) -> ExperienceEntry {
        ExperienceEntry {
            seq,
            domain: String::from("rust"),
            task: String::from("prior task"),
            confidence: ExperienceConfidence::High,
            edits: vec![EditRow {
                component: String::from("guardrails.md"),
                candidates: vec![String::from("probe-0-inv")],
                change: String::from("added the prior rule"),
                observed: String::from("probe delivered"),
                verdict: EditVerdict::Helped,
            }],
            cue: Some(cue.to_string()),
            lesson: Some(String::from("keep the prior rule")),
            evidence: Some(String::from("contrast between ref-0 and probe-0-inv")),
            content: String::new(),
            scope: ExperienceScope::Workspace,
            path: PathBuf::new(),
            display_path: String::new(),
        }
    }

    #[tokio::test]
    async fn exclusion_removes_the_tasks_own_entry_from_the_library_cues() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(workspace.join(".agents")).expect("agents");
        std::fs::write(workspace.join(".agents/guardrails.md"), "base rule\n").expect("base");
        save_entry(&workspace, &entry(1, "prior-entry cue"), None).expect("entry");
        let channel = CueChannel::new();
        let task = HarnessTask::new("t1", "do the task", "rust", &workspace);
        let fixtures = vec![LeaveOneOutFixture { domain: String::from("rust"), task, own_seq: 1 }];
        let config =
            HarnessConfig { k: 1, reflect: false, resume: false, ..HarnessConfig::default() };
        let report = run_leave_one_out(&channel, &fixtures, &config, root.path())
            .await
            .expect("leave-one-out");
        assert_eq!(report.rows.len(), 1);
        assert_eq!(report.rows[0].delivered_delta, 0);
        let seen = channel.seen.lock().expect("seen").clone();
        assert!(
            seen.iter().any(|cues| cues.contains("prior-entry cue")),
            "the with-entry run must see the entry"
        );
        assert!(
            seen.iter().any(|cues| !cues.contains("prior-entry cue")),
            "the without-entry run must exclude the entry"
        );
    }

    #[test]
    fn report_sums_delivery_deltas() {
        let report = LeaveOneOutReport {
            rows: vec![
                DeltaRow {
                    domain: String::from("a"),
                    task_id: String::from("t"),
                    own_seq: 1,
                    with_entry: OutcomeSummary {
                        delivered: true,
                        selected: Some(String::from("ref-0")),
                        dead_rollouts: 0,
                    },
                    without_entry: OutcomeSummary {
                        delivered: false,
                        selected: None,
                        dead_rollouts: 1,
                    },
                    delivered_delta: -1,
                    dead_rollout_delta: 1,
                },
                DeltaRow {
                    domain: String::from("b"),
                    task_id: String::from("u"),
                    own_seq: 2,
                    with_entry: OutcomeSummary {
                        delivered: false,
                        selected: None,
                        dead_rollouts: 2,
                    },
                    without_entry: OutcomeSummary {
                        delivered: true,
                        selected: Some(String::from("ref-0")),
                        dead_rollouts: 0,
                    },
                    delivered_delta: 1,
                    dead_rollout_delta: -2,
                },
            ],
        };
        assert_eq!(report.total_delivered_delta(), 0);
    }

    #[test]
    fn fixture_validation_rejects_domain_mismatch() {
        let root = tempfile::tempdir().expect("root");
        std::fs::create_dir_all(root.path().join("workspace")).expect("workspace");
        let task = HarnessTask::new("t", "p", "docs", root.path());
        let fixture = LeaveOneOutFixture { domain: String::from("rust"), task, own_seq: 1 };
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let error = runtime
            .block_on(run_leave_one_out(
                &CueChannel::new(),
                &[fixture],
                &HarnessConfig { reflect: false, ..HarnessConfig::default() },
                root.path(),
            ))
            .expect_err("domain mismatch");
        assert!(error.to_string().contains("does not match"), "{error}");
    }
}
