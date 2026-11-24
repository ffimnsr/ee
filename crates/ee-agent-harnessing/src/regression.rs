//! Harness-edit regression suite (I44).
//!
//! Per-domain fixtures run the real driver against a deterministic replay
//! channel, then check deterministic deliverable assertions. No LLM judge is
//! involved: a fixture passes only when the final deliverable is non-empty,
//! carries the domain marker, and exposes the expected asset. A deliberately
//! bad edit (a skill missing its trigger line) must be caught by the I34
//! validator before it can enter the library.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use ee_agent_orchestrator::harness_validation::{HarnessTree, materialize, validate};

use crate::channel::{
    Channel, ChannelFuture, Deliverable, DeliverableAsset, ImproverReply, ImproverRequest,
    ImproverRole, RunRecord, RunRequest,
};
use crate::driver::{HarnessConfig, run_task};
use crate::error::{HarnessError, HarnessResult};
use crate::task::HarnessTask;

/// One domain fixture.
#[derive(Debug, Clone)]
pub struct DomainFixture {
    /// Library domain.
    pub domain: String,
    /// Task prompt.
    pub prompt: String,
    /// Base harness that must pass.
    pub base: HarnessTree,
    /// An accepted edit that keeps the trigger line.
    pub accepted_edit: HarnessTree,
    /// A rejected edit that drops the skill trigger line.
    pub rejected_edit: HarnessTree,
    /// Files the deliverable depends on.
    pub required_files: Vec<String>,
    /// Marker the deterministic deliverable must carry.
    pub marker: String,
}

/// Result of one fixture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixtureOutcome {
    /// Domain.
    pub domain: String,
    /// The base harness passed validation and delivery.
    pub base_ok: bool,
    /// The accepted edit passed validation.
    pub accepted_ok: bool,
    /// The bad edit was caught by a repair or an error.
    pub rejected_caught: bool,
    /// The deterministic deliverable checks passed.
    pub deliverable_ok: bool,
}

/// Built-in per-domain fixtures.
#[must_use]
pub fn required_fixtures() -> Vec<DomainFixture> {
    ["rust", "docs", "sql"]
        .into_iter()
        .map(|domain| {
            let skill = format!("{domain}-fixture");
            let base = tree(&[
                ("guardrails.md", &format!("Use the {skill} skill for {domain} work.\n")),
                (
                    &format!("skills/{skill}/SKILL.md"),
                    &format!(
                        "---\nname: {skill}\ndescription: {domain} fixture guidance\n---\n\
                         Follow the domain contract.\n"
                    ),
                ),
            ]);
            let mut accepted = base.clone();
            accepted
                .files
                .insert(String::from("scripts/check.sh"), String::from("#!/bin/sh\ntrue\n"));
            let mut rejected = base.clone();
            rejected
                .files
                .insert(String::from("guardrails.md"), format!("Use skills for {domain} work.\n"));
            DomainFixture {
                domain: domain.to_string(),
                prompt: format!("Produce the {domain} fixture deliverable."),
                base,
                accepted_edit: accepted,
                rejected_edit: rejected,
                required_files: vec![
                    String::from("guardrails.md"),
                    format!("skills/{skill}/SKILL.md"),
                ],
                marker: format!("{domain} deliverable ok"),
            }
        })
        .collect()
}

/// Checks every fixture and returns its outcome.
pub async fn run_regression(
    config: &HarnessConfig,
    root: &Path,
) -> HarnessResult<Vec<FixtureOutcome>> {
    let mut outcomes = Vec::new();
    for fixture in required_fixtures() {
        let outcome = run_fixture(&fixture, config, root).await?;
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

async fn run_fixture(
    fixture: &DomainFixture,
    config: &HarnessConfig,
    root: &Path,
) -> HarnessResult<FixtureOutcome> {
    let base_verdict = validate(&fixture.base, None);
    let accepted = validate(&fixture.accepted_edit, Some(&fixture.base));
    let rejected = validate(&fixture.rejected_edit, Some(&fixture.base));
    let rejected_caught = !rejected.ok || !rejected.repairs_applied.is_empty();

    let workspace = root.join(&fixture.domain).join("workspace");
    std::fs::create_dir_all(workspace.join(".agents"))?;
    materialize(&fixture.base, &workspace.join(".agents")).map_err(HarnessError::Validation)?;
    std::fs::write(workspace.join("README.md"), "fixture workspace\n")?;
    let task = HarnessTask::new(
        format!("fixture-{}", fixture.domain),
        &fixture.prompt,
        &fixture.domain,
        &workspace,
    );
    let channel = FixtureChannel::new(fixture.clone());
    let run_root = root.join(&fixture.domain).join("run");
    let run = run_task(&channel, &task, config, &run_root, None).await?;
    let deliverable = channel.last_deliverable();
    let deliverable_ok =
        deliverable.as_ref().is_some_and(|deliverable| check_deliverable(fixture, deliverable));
    Ok(FixtureOutcome {
        domain: fixture.domain.clone(),
        base_ok: base_verdict.ok && run.selection.chosen.is_some(),
        accepted_ok: accepted.ok,
        rejected_caught,
        deliverable_ok,
    })
}

/// Deterministic deliverable assertion, mirroring an evaluation fixture.
#[must_use]
pub fn check_deliverable(fixture: &DomainFixture, deliverable: &Deliverable) -> bool {
    !deliverable.text.trim().is_empty()
        && deliverable.text.contains(&fixture.marker)
        && deliverable.assets.iter().any(|asset| asset.name == format!("{}.md", fixture.domain))
}

fn tree(files: &[(&str, &str)]) -> HarnessTree {
    HarnessTree::new(
        files
            .iter()
            .map(|(path, content)| ((*path).to_string(), (*content).to_string()))
            .collect::<BTreeMap<_, _>>(),
    )
}

/// Deterministic replay channel: the same harness always produces the same
/// rollout and deliverable.
struct FixtureChannel {
    fixture: DomainFixture,
    delivered: Arc<Mutex<Vec<Deliverable>>>,
}

impl FixtureChannel {
    fn new(fixture: DomainFixture) -> Self {
        Self { fixture, delivered: Arc::new(Mutex::new(Vec::new())) }
    }

    fn last_deliverable(&self) -> Option<Deliverable> {
        self.delivered.lock().expect("delivered").last().cloned()
    }
}

impl Channel for FixtureChannel {
    fn run_task(&self, request: RunRequest) -> ChannelFuture<RunRecord> {
        let fixture = self.fixture.clone();
        let delivered = self.delivered.clone();
        Box::pin(async move {
            let tree = HarnessTree::load_components(&request.harness_root.join(".agents"))
                .map_err(HarnessError::Validation)?;
            let has_required =
                fixture.required_files.iter().all(|path| tree.files.contains_key(path));
            let text = if has_required {
                format!("{}; files={}", fixture.marker, fixture.required_files.len())
            } else {
                format!("{} deliverable missing required files", fixture.domain)
            };
            let deliverable = Deliverable {
                text: text.clone(),
                assets: vec![DeliverableAsset {
                    name: format!("{}.md", fixture.domain),
                    bytes: text.len(),
                    content: Some(text.clone()),
                }],
            };
            delivered.lock().expect("delivered").push(deliverable.clone());
            Ok(RunRecord {
                label: request.label,
                delivered: Some(deliverable),
                rollout: ee_agent_orchestrator::rollout_view::RolloutView::default(),
                tool_records: Vec::new(),
                workspace_diff: Some(format!("- added {}.md", fixture.domain)),
                sessions: 1,
                rollouts: 1,
                usage_tokens: None,
            })
        })
    }

    fn improve(&self, request: ImproverRequest) -> ChannelFuture<ImproverReply> {
        let fixture = self.fixture.clone();
        Box::pin(async move {
            let tree = match request.role {
                ImproverRole::Inv | ImproverRole::Dom | ImproverRole::Composer => {
                    fixture.base.clone()
                }
                ImproverRole::Selector | ImproverRole::Reflector => HarnessTree::default(),
            };
            Ok(ImproverReply {
                tree,
                notes: String::from("kept -> base harness -> fixture replay delivered"),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::HarnessMode;

    #[tokio::test]
    async fn regression_suite_is_green_on_the_base_and_catches_a_bad_edit() {
        let root = tempfile::tempdir().expect("root");
        let config = HarnessConfig {
            k: 1,
            mode: HarnessMode::Parallel,
            dry_run: false,
            reflect: false,
            reflection_source: crate::driver::ReflectionSource::None,
            improver: crate::channel::ImproverModel {
                model_id: None,
                effort: None,
                max_steps: 4,
                timeout_secs: 30,
            },
            max_workers: 1,
            resume: false,
        };
        let outcomes = run_regression(&config, root.path()).await.expect("suite");
        assert_eq!(outcomes.len(), 3);
        for outcome in outcomes {
            assert!(outcome.base_ok, "{} base", outcome.domain);
            assert!(outcome.accepted_ok, "{} accepted edit", outcome.domain);
            assert!(outcome.rejected_caught, "{} bad edit", outcome.domain);
            assert!(outcome.deliverable_ok, "{} deliverable", outcome.domain);
        }
    }

    #[test]
    fn a_missing_trigger_line_is_repaired_and_recorded() {
        for fixture in required_fixtures() {
            let verdict = validate(&fixture.rejected_edit, Some(&fixture.base));
            assert!(
                !verdict.repairs_applied.is_empty(),
                "{} bad edit was not caught",
                fixture.domain
            );
            assert!(
                verdict
                    .tree
                    .files
                    .get("guardrails.md")
                    .is_some_and(|text| text.contains(&format!("{}-fixture", fixture.domain))),
                "{} trigger line was not repaired",
                fixture.domain
            );
        }
    }
}
