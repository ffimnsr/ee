//! Stage implementations: rollout, propose, select, and reflect.
//!
//! The driver owns ordering and budget; this module owns the per-stage work.
//! Improver sessions are cached content-addressed, validated by I34, retried
//! once with the validator message, and finally replaced by the current base
//! harness with a recorded fallback.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ee_agent_orchestrator::harness_reflection::{
    self, AdoptionRecord, CandidateProbe, MAX_ROLLOUT_BYTES, ReflectionEvidence, ReflectionOutput,
    build_entry,
};
use ee_agent_orchestrator::harness_validation::{
    HarnessTree, ValidationError, ValidationRule, Verdict, check_script_syntax, materialize,
    validate,
};
use ee_agent_orchestrator::user_experience::{ExperienceConfidence, ExperienceRegistry};

use crate::channel::{
    Channel, ImproverReply, ImproverRequest, ImproverRole, RunRecord, RunRequest,
};
use crate::context::{self, SelectorCandidate, StageEvidence};
use crate::error::{HarnessError, HarnessResult};
use crate::ledger::{Ledger, LedgerKind};
use crate::runstate::{self, ImproverCache};
use crate::select::{Candidate, Selection, assign_ids, neutral_fallback, parse_choice};
use crate::task::HarnessTask;

use super::{HarnessConfig, MISSION_REFLECTOR, MISSION_SELECTOR, ReflectionWriter};

/// Maximum bytes written for one library cue catalog.
const MAX_LIBRARY_CUE_BYTES: usize = 4 * 1024;
/// Maximum bytes retained in one validator report.
const MAX_VALIDATOR_REPORT_BYTES: usize = 8 * 1024;

/// Writes a component tree into a workspace-shaped directory.
pub(crate) fn materialize_harness(tree: &HarnessTree, directory: &Path) -> HarnessResult<PathBuf> {
    let agents = directory.join(".agents");
    std::fs::create_dir_all(&agents)?;
    materialize(tree, &agents).map_err(HarnessError::Validation)?;
    Ok(directory.to_path_buf())
}

/// Bounded cue catalog for one task's domain.
pub(crate) fn library_cues(task: &HarnessTask) -> Vec<String> {
    let registry = ExperienceRegistry::discover(task.library_root(), &[], None);
    let mut cues = Vec::new();
    let mut used = 0usize;
    for entry in registry
        .filtered(Some(task.domain_of()), task.library_before_seq)
        .into_iter()
        .filter(|entry| !task.excludes_experience(entry.seq))
        .take(32)
    {
        let summary = entry.cue.clone().unwrap_or_else(|| {
            let components: Vec<&str> =
                entry.edits.iter().map(|edit| edit.component.as_str()).collect();
            format!("edits: {}", components.join(", "))
        });
        let line = format!("- [{} #{}] {}", entry.domain, entry.seq, truncate_chars(&summary, 200));
        if used + line.len() + 1 > MAX_LIBRARY_CUE_BYTES {
            break;
        }
        used += line.len() + 1;
        cues.push(line);
    }
    cues
}

/// One propose/compose request.
pub(crate) struct ImproveRequest {
    pub(crate) role: ImproverRole,
    pub(crate) stage: String,
    pub(crate) evidence: StageEvidence,
    pub(crate) base: HarnessTree,
    pub(crate) base_dir: PathBuf,
}

/// One selection request.
pub(crate) struct SelectionInputs<'a> {
    pub(crate) pool: Vec<Candidate>,
    pub(crate) finals: &'a [(Candidate, HarnessTree, RunRecord)],
    pub(crate) references: &'a [RunRecord],
    pub(crate) dropped: &'a [(String, String)],
    pub(crate) base_dir: &'a Path,
}

/// Selection output retained by the driver.
pub(crate) struct SelectionOutcome {
    pub(crate) selection: Selection,
    pub(crate) adoption: Option<AdoptionRecord>,
    pub(crate) raw_reply: Option<String>,
}

/// One reflection request.
pub(crate) struct ReflectInputs<'a> {
    pub(crate) base: &'a HarnessTree,
    pub(crate) chosen: &'a HarnessTree,
    pub(crate) finals: &'a [(Candidate, HarnessTree, RunRecord)],
    pub(crate) references: &'a [RunRecord],
    pub(crate) probes: &'a [(String, HarnessTree, RunRecord)],
    pub(crate) adoption: Option<AdoptionRecord>,
    pub(crate) decisions: &'a [(String, String)],
    pub(crate) selection: &'a Selection,
    pub(crate) raw_selection_record: Option<String>,
    pub(crate) base_dir: &'a Path,
    pub(crate) writer: Option<&'a dyn ReflectionWriter>,
}

/// Shared per-task stage driver.
pub(crate) struct StageRuntime<'a> {
    pub(crate) channel: &'a dyn Channel,
    pub(crate) task: &'a HarnessTask,
    pub(crate) config: &'a HarnessConfig,
    pub(crate) run_root: &'a Path,
    pub(crate) cache: &'a ImproverCache,
}

struct Invocation<'a> {
    role: ImproverRole,
    label: String,
    kind: LedgerKind,
    evidence: StageEvidence,
    base_dir: &'a Path,
    candidate_dir: PathBuf,
    seed: Option<HarnessTree>,
}

impl StageRuntime<'_> {
    /// Runs one reference or probe rollout.
    pub(crate) async fn run_rollout(
        &self,
        label: &str,
        harness_dir: &Path,
        ledger: &mut Ledger,
    ) -> HarnessResult<RunRecord> {
        let scratch = self.run_root.join("runs").join(safe_name(label));
        std::fs::create_dir_all(&scratch)?;
        let request = RunRequest {
            label: label.to_string(),
            task: self.task.clone(),
            harness_root: harness_dir.to_path_buf(),
            scratch_root: scratch,
        };
        match self.channel.run_task(request).await {
            Ok(record) => {
                if self.task.delivered(&record) {
                    ledger.record(LedgerKind::Rollout, label, "delivered")?;
                } else {
                    ledger.dead_rollout(label, "run returned no deliverable")?;
                }
                Ok(record)
            }
            Err(error) => {
                let detail = truncate_chars(&error.to_string(), 240);
                ledger.dead_rollout(label, format!("channel error: {detail}"))?;
                Ok(empty_record(label))
            }
        }
    }

    /// Runs a propose or compose stage with validation, one retry, then inherit.
    pub(crate) async fn improve(
        &self,
        request: ImproveRequest,
        ledger: &mut Ledger,
    ) -> HarnessResult<ImproverReply> {
        let base = request.base.clone();
        let stage = request.stage.clone();
        let evidence = request.evidence;
        let candidate_dir = self.run_root.join("candidates").join(&stage);
        let first = match self
            .invoke(
                Invocation {
                    role: request.role,
                    label: stage.clone(),
                    kind: LedgerKind::ImprovementSession,
                    evidence: evidence.clone(),
                    base_dir: &request.base_dir,
                    candidate_dir: candidate_dir.clone(),
                    seed: None,
                },
                ledger,
            )
            .await
        {
            Ok(reply) => reply,
            Err(HarnessError::Channel(error)) => {
                let detail = truncate_chars(&error, 240);
                tracing::warn!(
                    stage = %stage,
                    %detail,
                    "improver session failed; inheriting the current harness"
                );
                ledger.record(LedgerKind::ImprovementSession, stage.clone(), "failed")?;
                return Ok(ImproverReply {
                    tree: request.base,
                    notes: format!(
                        "inherited the current harness after a channel failure: {detail}"
                    ),
                });
            }
            Err(error) => return Err(error),
        };
        let verdict = self.validate_candidate(&first.tree, &base, &candidate_dir)?;
        if verdict.ok {
            record_verdict(&stage, &verdict);
            return Ok(ImproverReply { tree: verdict.tree, notes: first.notes });
        }
        tracing::warn!(
            stage = %stage,
            errors = verdict.errors.len(),
            "candidate failed validation; retrying once with the validator message"
        );
        let mut retry_evidence = evidence;
        retry_evidence.stage = format!("{stage}-retry");
        retry_evidence.harnesses.push((String::from("attempt"), verdict.tree.clone()));
        retry_evidence.observations = Some(validator_report(&verdict));
        let retry_dir = self.run_root.join("candidates").join(format!("{stage}-retry"));
        let retry = match self
            .invoke(
                Invocation {
                    role: request.role,
                    label: format!("retry-{stage}"),
                    kind: LedgerKind::ImprovementRetry,
                    evidence: retry_evidence,
                    base_dir: &request.base_dir,
                    candidate_dir: retry_dir.clone(),
                    seed: Some(verdict.tree.clone()),
                },
                ledger,
            )
            .await
        {
            Ok(reply) => reply,
            Err(HarnessError::Channel(error)) => {
                let detail = truncate_chars(&error, 240);
                tracing::warn!(
                    stage = %stage,
                    %detail,
                    "improver retry failed; inheriting the current harness"
                );
                ledger.record(LedgerKind::ImprovementRetry, format!("retry-{stage}"), "failed")?;
                return Ok(ImproverReply {
                    tree: request.base,
                    notes: format!(
                        "inherited the current harness after a retry channel failure: {detail}"
                    ),
                });
            }
            Err(error) => return Err(error),
        };
        let retry_verdict = self.validate_candidate(&retry.tree, &base, &retry_dir)?;
        if retry_verdict.ok {
            record_verdict(&stage, &retry_verdict);
            return Ok(ImproverReply { tree: retry_verdict.tree, notes: retry.notes });
        }
        let reason = validator_summary(&retry_verdict);
        tracing::warn!(
            stage = %stage,
            reason = %reason,
            "candidate failed validation twice; inheriting the current harness"
        );
        Ok(ImproverReply {
            tree: request.base,
            notes: format!("inherited the current harness after two validation failures: {reason}"),
        })
    }

    /// Runs the anonymized selector unless the pool is empty.
    pub(crate) async fn select(
        &self,
        inputs: SelectionInputs<'_>,
        ledger: &mut Ledger,
    ) -> HarnessResult<SelectionOutcome> {
        if inputs.pool.is_empty() {
            ledger.record(LedgerKind::SelectionSession, "select", "empty pool")?;
            return Ok(SelectionOutcome {
                selection: neutral_fallback(&[]),
                adoption: None,
                raw_reply: None,
            });
        }
        let pool =
            assign_ids(&self.task.id, self.config.k, self.config.mode.as_str(), &inputs.pool);
        let records: BTreeMap<&str, &RunRecord> = inputs
            .references
            .iter()
            .chain(inputs.finals.iter().map(|(_, _, record)| record))
            .map(|record| (record.label.as_str(), record))
            .collect();
        let mut selector = Vec::new();
        for anon in &pool.order {
            let Some(label) = pool.label(anon) else { continue };
            let Some(candidate) = inputs.pool.iter().find(|candidate| candidate.label == label)
            else {
                continue;
            };
            let Some(deliverable) = candidate.deliverable.clone() else { continue };
            let trajectory = records
                .get(label)
                .filter(|record| record.rollout.has_trajectory())
                .map(|record| record.rollout.render(context::MAX_ROLLOUT_BYTES));
            selector.push(SelectorCandidate { anon: anon.clone(), deliverable, trajectory });
        }
        let observations = if inputs.dropped.is_empty() {
            None
        } else {
            Some(format!(
                "# Dropped candidates\n\n{}",
                inputs
                    .dropped
                    .iter()
                    .map(|(label, cause)| format!("- {label}: {cause}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ))
        };
        let evidence = StageEvidence {
            stage: String::from("select"),
            mission: MISSION_SELECTOR.to_string(),
            task: self.task.clone(),
            harnesses: Vec::new(),
            rollouts: Vec::new(),
            deliverables: Vec::new(),
            library_cues: Vec::new(),
            decisions: Vec::new(),
            selector,
            observations,
        };
        let reply = match self
            .invoke(
                Invocation {
                    role: ImproverRole::Selector,
                    label: String::from("select"),
                    kind: LedgerKind::SelectionSession,
                    evidence,
                    base_dir: inputs.base_dir,
                    candidate_dir: self.run_root.join("candidates").join("select"),
                    seed: None,
                },
                ledger,
            )
            .await
        {
            Ok(reply) => reply,
            Err(error) => {
                let detail = truncate_chars(&error.to_string(), 240);
                tracing::warn!(%detail, "selector session failed; applying the neutral fallback");
                if let Err(record_error) =
                    ledger.record(LedgerKind::SelectionSession, "select", "failed")
                {
                    tracing::warn!(%record_error, "selector failure could not be recorded");
                }
                let selection = neutral_fallback(&inputs.pool);
                let adoption = AdoptionRecord {
                    anonymous_ids: pool.ids.clone(),
                    selection_why: selection.why.join("\n"),
                };
                return Ok(SelectionOutcome {
                    selection,
                    adoption: Some(adoption),
                    raw_reply: None,
                });
            }
        };
        let raw_reply = Some(reply.notes.clone());
        let (selection, adoption) = match parse_choice(&reply.notes, &pool) {
            Ok(selection) => {
                let adoption = AdoptionRecord {
                    anonymous_ids: pool.ids.clone(),
                    selection_why: selection.why.join("\n"),
                };
                (selection, Some(adoption))
            }
            Err(error) => {
                tracing::warn!(%error, "selector reply unusable; applying the neutral fallback");
                let selection = neutral_fallback(&inputs.pool);
                let adoption = AdoptionRecord {
                    anonymous_ids: pool.ids.clone(),
                    selection_why: selection.why.join("\n"),
                };
                (selection, Some(adoption))
            }
        };
        Ok(SelectionOutcome { selection, adoption, raw_reply })
    }

    /// Runs the reflector over the curated improvement record.
    pub(crate) async fn reflect(
        &self,
        inputs: ReflectInputs<'_>,
        ledger: &mut Ledger,
    ) -> HarnessResult<(Option<ReflectionOutput>, Option<PathBuf>)> {
        let chosen_record = inputs
            .references
            .iter()
            .chain(inputs.finals.iter().map(|(_, _, record)| record))
            .find(|record| Some(record.label.as_str()) == inputs.selection.chosen.as_deref());
        let decisions = if inputs.decisions.is_empty() {
            None
        } else {
            Some(
                inputs
                    .decisions
                    .iter()
                    .map(|(label, notes)| format!("## {label}\n\n{}", notes.trim()))
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            )
        };
        let evidence = ReflectionEvidence {
            task: self.task.prompt.clone(),
            domain: self.task.domain_of().to_string(),
            chosen_harness: inputs.chosen.clone(),
            base_harness: Some(inputs.base.clone()),
            candidates: inputs
                .finals
                .iter()
                .map(|(candidate, _, record)| CandidateProbe {
                    label: candidate.label.clone(),
                    rollout: record.rollout.render(MAX_ROLLOUT_BYTES),
                })
                .chain(inputs.probes.iter().map(|(label, _, record)| CandidateProbe {
                    label: label.clone(),
                    rollout: record.rollout.render(MAX_ROLLOUT_BYTES),
                }))
                .collect(),
            candidate_harnesses: inputs
                .finals
                .iter()
                .map(|(candidate, tree, _)| (candidate.label.clone(), tree.clone()))
                .chain(inputs.probes.iter().map(|(label, tree, _)| (label.clone(), tree.clone())))
                .collect(),
            decisions,
            adoption: inputs.adoption.clone(),
            tool_records: chosen_record
                .map(|record| record.tool_records.clone())
                .unwrap_or_default(),
            workspace_diff: chosen_record.and_then(|record| record.workspace_diff.clone()),
            raw_selection_record: inputs.raw_selection_record,
        };
        let projection =
            harness_reflection::build_projection(&evidence).map_err(HarnessError::Parse)?;
        let stage_evidence = StageEvidence {
            stage: String::from("reflect"),
            mission: MISSION_REFLECTOR.to_string(),
            task: self.task.clone(),
            harnesses: vec![
                (String::from("base"), inputs.base.clone()),
                (String::from("chosen"), inputs.chosen.clone()),
            ],
            rollouts: Vec::new(),
            deliverables: Vec::new(),
            library_cues: library_cues(self.task),
            decisions: inputs.decisions.to_vec(),
            selector: Vec::new(),
            observations: Some(projection),
        };
        let reply = match self
            .invoke(
                Invocation {
                    role: ImproverRole::Reflector,
                    label: String::from("reflect"),
                    kind: LedgerKind::ReflectionSession,
                    evidence: stage_evidence,
                    base_dir: inputs.base_dir,
                    candidate_dir: self.run_root.join("candidates").join("reflect"),
                    seed: None,
                },
                ledger,
            )
            .await
        {
            Ok(reply) => reply,
            Err(error) => {
                let detail = truncate_chars(&error.to_string(), 240);
                tracing::warn!(%detail, "reflector session failed; no lesson written");
                if let Err(record_error) =
                    ledger.record(LedgerKind::ReflectionSession, "reflect", "failed")
                {
                    tracing::warn!(%record_error, "reflector failure could not be recorded");
                }
                return Ok((None, None));
            }
        };
        let output = match harness_reflection::parse_reflection(&reply.notes) {
            Ok(output) => output,
            Err(error) => {
                tracing::warn!(%error, "reflector reply unusable; no lesson written");
                return Ok((None, None));
            }
        };
        let registry = ExperienceRegistry::discover(self.task.library_root(), &[], None);
        let seq = registry.next_seq(self.task.domain_of());
        let entry = match build_entry(
            &output,
            seq,
            self.task.domain_of(),
            &task_summary(&self.task.prompt),
            ExperienceConfidence::Medium,
        ) {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(%error, "reflection entry could not be built; no lesson written");
                return Ok((Some(output), None));
            }
        };
        let path = inputs.writer.and_then(|writer| match writer.write(&entry, inputs.chosen) {
            Ok(path) => Some(path),
            Err(error) => {
                tracing::warn!(%error, "reflection write declined or failed; no lesson written");
                None
            }
        });
        Ok((Some(output), path))
    }

    async fn invoke(
        &self,
        request: Invocation<'_>,
        ledger: &mut Ledger,
    ) -> HarnessResult<ImproverReply> {
        let context_dir = context::build(self.run_root, &request.evidence)?;
        let digest = runstate::context_digest(&context_dir);
        let key = ImproverCache::key(
            &request.label,
            &request.evidence.mission,
            request.role.builtin_role_name(),
            &digest,
        );
        let expects_tree =
            matches!(request.role, ImproverRole::Inv | ImproverRole::Dom | ImproverRole::Composer);
        let cached = self.cache.get(&key, |tree| !expects_tree || !tree.is_empty());
        if let Some(reply) = cached {
            ledger.record(request.kind, request.label, "cached")?;
            return Ok(reply);
        }
        prepare_dir(&request.candidate_dir)?;
        if let Some(seed) = &request.seed {
            materialize(seed, &request.candidate_dir.join(".agents"))
                .map_err(HarnessError::Validation)?;
        }
        let reply = self
            .channel
            .improve(ImproverRequest {
                label: request.label.clone(),
                role: request.role,
                mission: request.evidence.mission.clone(),
                context_dir,
                base_harness: request.base_dir.to_path_buf(),
                candidate_dir: request.candidate_dir,
                model: self.config.improver.clone(),
            })
            .await?;
        ledger.record(request.kind, request.label.clone(), "session")?;
        if !reply.tree.is_empty() || !reply.notes.trim().is_empty() {
            self.cache.store(&key, &reply)?;
        }
        Ok(reply)
    }

    fn validate_candidate(
        &self,
        tree: &HarnessTree,
        base: &HarnessTree,
        candidate_dir: &Path,
    ) -> HarnessResult<Verdict> {
        let mut verdict = validate(tree, Some(base));
        if !verdict.ok {
            return Ok(verdict);
        }
        let scripts: Vec<(String, String)> = verdict
            .tree
            .files
            .keys()
            .filter_map(|path| {
                if !path.starts_with("scripts/") {
                    return None;
                }
                let extension = Path::new(path).extension().and_then(|value| value.to_str())?;
                matches!(extension, "sh" | "py").then(|| (path.clone(), extension.to_string()))
            })
            .collect();
        if scripts.is_empty() {
            return Ok(verdict);
        }
        let check_dir = candidate_dir.join("syntax");
        prepare_dir(&check_dir)?;
        materialize(&verdict.tree, &check_dir).map_err(HarnessError::Validation)?;
        for (path, extension) in scripts {
            if let Err(message) = check_script_syntax(&check_dir.join(&path), &extension) {
                verdict.errors.push(ValidationError {
                    file: path,
                    rule: ValidationRule::ScriptSyntax,
                    message,
                });
                verdict.ok = false;
            }
        }
        Ok(verdict)
    }
}

fn empty_record(label: &str) -> RunRecord {
    RunRecord {
        label: label.to_string(),
        delivered: None,
        rollout: ee_agent_orchestrator::rollout_view::RolloutView::default(),
        tool_records: Vec::new(),
        workspace_diff: None,
        sessions: 0,
        rollouts: 1,
        usage_tokens: None,
    }
}

fn prepare_dir(directory: &Path) -> HarnessResult<()> {
    if directory.exists() {
        std::fs::remove_dir_all(directory)?;
    }
    std::fs::create_dir_all(directory)?;
    Ok(())
}

fn validator_report(verdict: &Verdict) -> String {
    let mut text = String::from("# Validator rejected the candidate\n\n");
    for error in &verdict.errors {
        text.push_str(&format!("- {} [{:?}] {}\n", error.file, error.rule, error.message));
    }
    for repair in &verdict.repairs_applied {
        text.push_str(&format!("- repair {} [{:?}] {}\n", repair.file, repair.rule, repair.detail));
    }
    for warning in &verdict.warnings {
        text.push_str(&format!("- warning {warning}\n"));
    }
    text.push_str(&format!(
        "\nalways-on characters: {} (delta {:+})\ncensus: files={} skills={} scripts={} subagents={}\n",
        verdict.always_on_chars,
        verdict.always_on_char_delta,
        verdict.census.files,
        verdict.census.skills,
        verdict.census.scripts,
        verdict.census.subagents,
    ));
    truncate_chars(&text, MAX_VALIDATOR_REPORT_BYTES)
}

/// Records a validated candidate's census, cost delta, and repairs.
///
/// Autotrigger repairs are never silent: a repaired trigger line changes the
/// adopted tree, so it must appear in the operator log even when validation
/// accepts the candidate.
fn record_verdict(stage: &str, verdict: &Verdict) {
    tracing::info!(
        stage = %stage,
        files = verdict.census.files,
        skills = verdict.census.skills,
        scripts = verdict.census.scripts,
        subagents = verdict.census.subagents,
        always_on_chars = verdict.always_on_chars,
        always_on_char_delta = verdict.always_on_char_delta,
        repairs = verdict.repairs_applied.len(),
        "harness candidate validated"
    );
    for repair in &verdict.repairs_applied {
        tracing::info!(
            stage = %stage,
            file = %repair.file,
            rule = ?repair.rule,
            detail = %repair.detail,
            "harness candidate repair applied"
        );
    }
}

fn validator_summary(verdict: &Verdict) -> String {
    verdict
        .errors
        .first()
        .map(|error| format!("{}: {}", error.file, error.message))
        .unwrap_or_else(|| String::from("validator reported errors"))
}

fn task_summary(prompt: &str) -> String {
    let mut text = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > 120 {
        text = text.chars().take(120).collect::<String>();
        text.push('…');
    }
    text
}

fn truncate_chars(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut cut = max_bytes.min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… [truncated]", &text[..cut])
}

fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.trim_matches('_').is_empty() { String::from("item") } else { cleaned }
}
