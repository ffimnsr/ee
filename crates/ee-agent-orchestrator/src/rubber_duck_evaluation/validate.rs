//! Fixture, oracle, context, counter, and baseline validation.
use super::*;

pub(crate) fn validate_fixture(
    fixture: &RubberDuckEvaluationFixture,
    root_ids: &BTreeSet<&str>,
) -> Result<(), RubberDuckEvaluationError> {
    if fixture.schema_version != RUBBER_DUCK_EVALUATION_SCHEMA_VERSION {
        return Err(error(format!(
            "fixture {} has unsupported schema version {}",
            fixture.id, fixture.schema_version
        )));
    }
    validate_id("fixture id", &fixture.id)?;
    validate_id("root fixture id", &fixture.root_fixture_id)?;
    if !root_ids.contains(fixture.root_fixture_id.as_str()) {
        return Err(error(format!(
            "fixture {} references missing root fixture {}",
            fixture.id, fixture.root_fixture_id
        )));
    }
    validate_id("session id", &fixture.trigger.session_id)?;
    if !fixture.trigger.revision.is_empty() {
        validate_id("revision", &fixture.trigger.revision)?;
    }
    validate_id("material fingerprint", &fixture.trigger.material_fingerprint)?;
    validate_backend(&fixture.backend)?;
    if fixture.trigger.planned_file_count > MAX_PLANNED_OR_CHANGED_FILES
        || fixture.trigger.changed_file_count > MAX_PLANNED_OR_CHANGED_FILES
        || fixture.trigger.repeated_failure_count > MAX_REPEATED_FAILURES
    {
        return Err(error(format!("fixture {} trigger numeric bound exceeded", fixture.id)));
    }
    if fixture.observed_evidence.files.len() + fixture.observed_evidence.tools.len()
        > MAX_OBSERVED_EVIDENCE
    {
        return Err(error(format!("fixture {} has too much observed evidence", fixture.id)));
    }
    for path in &fixture.observed_evidence.files {
        validate_hermetic_path(path)?;
    }
    for tool in &fixture.observed_evidence.tools {
        validate_bounded_text("observed tool", tool, MAX_ID_CHARS)?;
    }
    validate_oracle(fixture)?;
    validate_repository_context(fixture)?;
    validate_counters(&fixture.critic_counters)?;
    match &fixture.critic_outcome {
        ScriptedCriticOutcome::Completed { report } => {
            if report.target != fixture.target {
                return Err(error(format!("fixture {} critique target mismatch", fixture.id)));
            }
        }
        ScriptedCriticOutcome::Unavailable { reason }
        | ScriptedCriticOutcome::Timeout { reason } => {
            validate_bounded_text("scripted terminal reason", reason, MAX_TEXT_CHARS)?;
        }
        ScriptedCriticOutcome::Malformed { raw } => {
            validate_bounded_text("malformed output", raw, MAX_TEXT_CHARS)?;
        }
    }
    validate_terminal_counters(fixture)
}

pub(crate) fn validate_oracle(
    fixture: &RubberDuckEvaluationFixture,
) -> Result<(), RubberDuckEvaluationError> {
    let oracle = &fixture.oracle;
    if oracle.expected_finding_keys.len() > MAX_FINDING_KEYS
        || oracle.root_known_finding_keys.len() > MAX_FINDING_KEYS
        || oracle.resolutions.len() > MAX_RESOLUTIONS
    {
        return Err(error(format!("fixture {} oracle exceeds bounds", fixture.id)));
    }
    if !oracle.root_known_finding_keys.is_subset(&oracle.expected_finding_keys) {
        return Err(error(format!(
            "fixture {} root-known keys must be oracle finding keys",
            fixture.id
        )));
    }
    for key in oracle.expected_finding_keys.iter().chain(&oracle.root_known_finding_keys) {
        validate_id("oracle finding key", key)?;
    }
    let mut resolution_keys = BTreeSet::new();
    for resolution in &oracle.resolutions {
        validate_id("resolution finding key", &resolution.finding_key)?;
        validate_resolution(&resolution.resolution)?;
        if !resolution_keys.insert(&resolution.finding_key) {
            return Err(error(format!(
                "fixture {} has duplicate resolution key {}",
                fixture.id, resolution.finding_key
            )));
        }
    }
    if oracle.trivial && oracle.complex {
        return Err(error(format!("fixture {} cannot be complex and trivial", fixture.id)));
    }
    Ok(())
}

pub(crate) fn validate_repository_context(
    fixture: &RubberDuckEvaluationFixture,
) -> Result<(), RubberDuckEvaluationError> {
    if fixture.repository_context.len() > MAX_REPOSITORY_CONTEXT_ITEMS {
        return Err(error(format!("fixture {} repository context exceeds bounds", fixture.id)));
    }
    for item in &fixture.repository_context {
        validate_bounded_text("repository context", item, MAX_TEXT_CHARS)?;
    }
    if fixture.scenario == RubberDuckScenario::RepositoryPromptInjection {
        if fixture.repository_context.is_empty() {
            return Err(error("prompt-injection fixture requires repository context"));
        }
    } else if !fixture.repository_context.is_empty() {
        return Err(error(format!(
            "fixture {} carries repository context outside prompt-injection scenario",
            fixture.id
        )));
    }
    Ok(())
}

pub(crate) fn validate_counters(
    counters: &ScriptedCriticCounters,
) -> Result<(), RubberDuckEvaluationError> {
    for (name, actual, maximum) in [
        ("calls", counters.calls, MAX_CRITIC_CALLS),
        ("latency_ms", counters.latency_ms, MAX_CRITIC_LATENCY_MS),
        ("input_tokens", counters.input_tokens, MAX_CRITIC_TOKENS),
        ("output_tokens", counters.output_tokens, MAX_CRITIC_TOKENS),
        ("estimated_cost_microusd", counters.estimated_cost_microusd, MAX_CRITIC_COST_MICROUSD),
        ("mutation_calls", counters.mutation_calls, MAX_CRITIC_COUNTER),
        ("execute_calls", counters.execute_calls, MAX_CRITIC_COUNTER),
        ("delegate_calls", counters.delegate_calls, MAX_CRITIC_COUNTER),
        ("approval_prompts", counters.approval_prompts, MAX_CRITIC_COUNTER),
        ("policy_violations", counters.policy_violations, MAX_CRITIC_COUNTER),
    ] {
        if actual > maximum {
            return Err(error(format!("critic counter {name} exceeds {maximum}")));
        }
    }
    Ok(())
}

pub(crate) fn validate_terminal_counters(
    fixture: &RubberDuckEvaluationFixture,
) -> Result<(), RubberDuckEvaluationError> {
    let counters = &fixture.critic_counters;
    if matches!(fixture.expected_trigger, ReplayTriggerExpectation::Skip { .. }) {
        if !counters.all_zero() || !fixture.oracle.resolutions.is_empty() {
            return Err(error(format!(
                "skipped fixture {} must have zero critic counters and no resolutions",
                fixture.id
            )));
        }
        return Ok(());
    }
    match &fixture.critic_outcome {
        ScriptedCriticOutcome::Completed { .. } | ScriptedCriticOutcome::Malformed { .. } => {
            if counters.calls != 1
                || counters.latency_ms == 0
                || counters.input_tokens == 0
                || counters.output_tokens == 0
            {
                return Err(error(format!(
                    "completed/malformed fixture {} requires one call and nonzero usage",
                    fixture.id
                )));
            }
        }
        ScriptedCriticOutcome::Unavailable { .. } => {
            if !counters.usage_zero() || !fixture.oracle.resolutions.is_empty() {
                return Err(error(format!(
                    "unavailable fixture {} must have zero usage and no resolutions",
                    fixture.id
                )));
            }
        }
        ScriptedCriticOutcome::Timeout { .. } => {
            if counters.calls != 1
                || counters.latency_ms == 0
                || counters.input_tokens == 0
                || counters.output_tokens != 0
                || !fixture.oracle.resolutions.is_empty()
            {
                return Err(error(format!(
                    "timeout fixture {} has invalid call, usage, or resolution counters",
                    fixture.id
                )));
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_required_coverage(
    fixtures: &[RubberDuckEvaluationFixture],
) -> Result<(), RubberDuckEvaluationError> {
    let mut counts = BTreeMap::new();
    for fixture in fixtures {
        *counts.entry(fixture.scenario).or_insert(0_usize) += 1;
    }
    for scenario in [
        RubberDuckScenario::FlawedPlan,
        RubberDuckScenario::MissingAuthorization,
        RubberDuckScenario::StaleRevision,
        RubberDuckScenario::UnsafeMigration,
        RubberDuckScenario::MissingRegressionTests,
        RubberDuckScenario::CleanMechanicalEdit,
        RubberDuckScenario::FalsePositiveCritique,
        RubberDuckScenario::UnavailableCritic,
        RubberDuckScenario::Timeout,
        RubberDuckScenario::MalformedReport,
        RubberDuckScenario::RepositoryPromptInjection,
    ] {
        if counts.get(&scenario) != Some(&1) {
            return Err(error(format!("required scenario must occur exactly once: {scenario:?}")));
        }
    }
    for scenario in [
        RubberDuckScenario::CrossAgentSameImplementation,
        RubberDuckScenario::CrossAgentDifferentImplementation,
    ] {
        if counts.get(&scenario) != Some(&2) {
            return Err(error(format!(
                "cross-agent scenario requires exactly two fixtures: {scenario:?}"
            )));
        }
    }
    let internal_count =
        fixtures.iter().filter(|fixture| fixture.backend.is_internal()).count() as u64;
    let external_count = fixtures.len() as u64 - internal_count;
    if internal_count != REQUIRED_INTERNAL_FIXTURE_COUNT
        || external_count != REQUIRED_EXTERNAL_FIXTURE_COUNT
    {
        return Err(error("required internal/external fixture counts changed"));
    }
    validate_cross_agent_attribution(fixtures)
}

pub(crate) fn validate_cross_agent_attribution(
    fixtures: &[RubberDuckEvaluationFixture],
) -> Result<(), RubberDuckEvaluationError> {
    let cross_agent_ids = fixtures
        .iter()
        .filter(|fixture| {
            matches!(
                fixture.scenario,
                RubberDuckScenario::CrossAgentSameImplementation
                    | RubberDuckScenario::CrossAgentDifferentImplementation
            )
        })
        .filter_map(|fixture| external_parts(fixture).map(|(agent_id, _, _)| agent_id))
        .collect::<BTreeSet<_>>();
    if cross_agent_ids.len() != REQUIRED_EXTERNAL_FIXTURE_COUNT as usize {
        return Err(error("cross-agent fixtures require globally distinct agent ids"));
    }
    for (scenario, same_implementation) in [
        (RubberDuckScenario::CrossAgentSameImplementation, true),
        (RubberDuckScenario::CrossAgentDifferentImplementation, false),
    ] {
        let pair =
            fixtures.iter().filter(|fixture| fixture.scenario == scenario).collect::<Vec<_>>();
        let Some((agent_a, implementation_a, model_a)) = external_parts(pair[0]) else {
            return Err(error("cross-agent fixture must use external backend"));
        };
        let Some((agent_b, implementation_b, model_b)) = external_parts(pair[1]) else {
            return Err(error("cross-agent fixture must use external backend"));
        };
        if agent_a == agent_b {
            return Err(error("cross-agent fixture pair requires distinct agent ids"));
        }
        if same_implementation {
            if implementation_a != implementation_b || model_a == model_b {
                return Err(error(
                    "same-implementation fixtures require equal implementation and distinct startup models",
                ));
            }
        } else if implementation_a == implementation_b {
            return Err(error("different ACP fixtures require distinct implementation identities"));
        }
    }
    Ok(())
}

pub(crate) fn external_parts(fixture: &RubberDuckEvaluationFixture) -> Option<(&str, &str, &str)> {
    match &fixture.backend {
        ReplayCriticBackend::External { agent_id, implementation, startup_model_id } => {
            Some((agent_id, implementation, startup_model_id))
        }
        ReplayCriticBackend::Internal { .. } => None,
    }
}

pub(crate) fn evaluate_trigger(facts: &ReplayTriggerFacts) -> ReplayTriggerExpectation {
    let borrowed = RubberDuckTriggerFacts {
        session_id: &facts.session_id,
        revision: &facts.revision,
        material_fingerprint: &facts.material_fingerprint,
        strategy: facts.strategy,
        strategy_reason: facts.strategy_reason,
        impacts: &facts.impacts,
        planned_file_count: facts.planned_file_count,
        changed_file_count: facts.changed_file_count,
        diagnostics_present: facts.diagnostics_present,
        validation_passed: facts.validation_passed,
        validation_partial_or_skipped: facts.validation_partial_or_skipped,
        recovery_occurred: facts.recovery_occurred,
        repeated_failure_count: facts.repeated_failure_count,
        selected_adjacent_tests: facts.selected_adjacent_tests,
        cancelled: facts.cancelled,
    };
    match RubberDuckTriggerPolicy::new(RubberDuckTriggerConfig {
        mode: RubberDuckTriggerMode::Automatic,
    })
    .evaluate(facts.trigger, &borrowed)
    {
        RubberDuckTriggerDecision::Run { reason, .. } => ReplayTriggerExpectation::Run { reason },
        RubberDuckTriggerDecision::Skip(reason) => ReplayTriggerExpectation::Skip { reason },
    }
}

pub(crate) fn verify_scripted_outcome(
    fixture: &RubberDuckEvaluationFixture,
) -> Result<ReplayCriticTerminal, RubberDuckEvaluationError> {
    match &fixture.critic_outcome {
        ScriptedCriticOutcome::Completed { report } => {
            let raw = serde_json::to_string(report).map_err(|source| {
                error(format!("fixture {} report encode failed: {source}", fixture.id))
            })?;
            let observed = fixture.observed_evidence.production_evidence();
            let verified = CritiqueReportVerifier
                .parse_and_accept_for_target(&raw, &fixture.target, &observed)
                .map_err(|source| {
                    error(format!("fixture {} report rejected: {source}", fixture.id))
                })?;
            Ok(ReplayCriticTerminal::Completed {
                finding_keys: verified
                    .report()
                    .findings
                    .iter()
                    .map(|finding| finding.key.clone())
                    .collect(),
            })
        }
        ScriptedCriticOutcome::Unavailable { .. } => Ok(ReplayCriticTerminal::Unavailable),
        ScriptedCriticOutcome::Timeout { .. } => Ok(ReplayCriticTerminal::Timeout),
        ScriptedCriticOutcome::Malformed { raw } => {
            if CritiqueReportVerifier.parse(raw).is_ok() {
                return Err(error(format!("fixture {} malformed report parsed", fixture.id)));
            }
            Ok(ReplayCriticTerminal::Quarantined)
        }
    }
}

pub(crate) fn bind_resolutions(
    fixture: &RubberDuckEvaluationFixture,
    verified_keys: &BTreeSet<String>,
) -> Result<BTreeMap<String, FindingResolution>, RubberDuckEvaluationError> {
    let mut resolutions = BTreeMap::new();
    for entry in &fixture.oracle.resolutions {
        if !verified_keys.contains(&entry.finding_key) {
            return Err(error(format!(
                "fixture {} resolution references unknown finding key {}",
                fixture.id, entry.finding_key
            )));
        }
        if resolutions.insert(entry.finding_key.clone(), entry.resolution.clone()).is_some() {
            return Err(error(format!(
                "fixture {} has duplicate resolution key {}",
                fixture.id, entry.finding_key
            )));
        }
    }
    let resolution_keys = resolutions.keys().cloned().collect::<BTreeSet<_>>();
    if &resolution_keys != verified_keys {
        let missing = verified_keys.difference(&resolution_keys).cloned().collect::<Vec<_>>();
        return Err(error(format!(
            "fixture {} missing resolutions for verified finding keys {missing:?}",
            fixture.id
        )));
    }
    Ok(resolutions)
}

#[derive(Clone, Copy)]
pub(crate) enum ResolutionKind {
    Accepted,
    Rejected,
    Deferred,
}

pub(crate) fn resolution_keys(
    resolutions: &BTreeMap<String, FindingResolution>,
    kind: ResolutionKind,
) -> BTreeSet<String> {
    resolutions
        .iter()
        .filter(|(_, resolution)| {
            matches!(
                (kind, resolution),
                (ResolutionKind::Accepted, FindingResolution::Accepted { .. })
                    | (ResolutionKind::Rejected, FindingResolution::Rejected { .. })
                    | (ResolutionKind::Deferred, FindingResolution::Deferred { .. })
            )
        })
        .map(|(key, _)| key.clone())
        .collect()
}

pub(crate) fn quality_score(
    expected: &BTreeSet<String>,
    addressed: &BTreeSet<String>,
    false_positives: u64,
) -> u8 {
    let recall = if expected.is_empty() {
        100
    } else {
        addressed.intersection(expected).count() as u64 * 100 / expected.len() as u64
    };
    recall.saturating_sub(false_positives.saturating_mul(FALSE_POSITIVE_PENALTY)) as u8
}

pub(crate) fn derive_root_completion(root: &FixtureRun) -> CompletionState {
    if !root.score.task_completed || root.score.policy_violations > 0 {
        CompletionState::Blocked
    } else if root.score.validation_success {
        CompletionState::Verified
    } else {
        CompletionState::PartiallyVerified
    }
}

pub(crate) fn verify_guarded_repository_context(
    fixture: &RubberDuckEvaluationFixture,
) -> Result<(), RubberDuckEvaluationError> {
    let context = ReviewContext {
        diagnostic_summaries: fixture.repository_context.clone(),
        revision: Some(fixture.trigger.revision.clone()),
        ..ReviewContext::default()
    };
    let messages = build_critique_messages(&fixture.target, &context);
    let repository_text = fixture.repository_context.join("\n");
    let guarded =
        messages.iter().find(|message| message.role == ModelRole::User).ok_or_else(|| {
            error(format!("fixture {} guarded critique omitted repository evidence", fixture.id))
        })?;
    if guarded.trust != TrustLevel::ToolOutputUntrusted
        || !guarded.text_content().contains("[tool_output]")
        || !guarded.text_content().contains(&repository_text)
    {
        return Err(error(format!(
            "fixture {} repository evidence was not preserved as guarded untrusted data",
            fixture.id
        )));
    }
    if !messages.iter().any(|message| {
        message.role == ModelRole::System && message.text_content().contains(POLICY_REMINDER)
    }) {
        return Err(error(format!(
            "fixture {} critique omitted injection policy reminder",
            fixture.id
        )));
    }
    if messages
        .iter()
        .filter(|message| message.role == ModelRole::System)
        .any(|message| message.text_content().contains(&repository_text))
    {
        return Err(error(format!(
            "fixture {} repository instruction crossed trusted message boundary",
            fixture.id
        )));
    }
    Ok(())
}

pub(crate) fn validate_baseline_aggregate(
    aggregate: &RubberDuckAggregate,
) -> Result<(), RubberDuckEvaluationError> {
    if aggregate.fixture_count != REQUIRED_INTERNAL_FIXTURE_COUNT
        || aggregate.external_fixture_count != REQUIRED_EXTERNAL_FIXTURE_COUNT
    {
        return Err(error("baseline internal/external fixture count mismatch"));
    }
    if aggregate.model_agent_calls != aggregate.internal_model_agent_calls {
        return Err(error("baseline gate calls must equal internal calls"));
    }
    if aggregate.complex_fixture_count > aggregate.fixture_count
        || aggregate.trivial_fixture_count > aggregate.fixture_count
        || aggregate.trivial_skip_count > aggregate.trivial_fixture_count
        || aggregate.final_quality_score_sum > aggregate.fixture_count.saturating_mul(100)
        || aggregate.root_quality_score_sum > aggregate.fixture_count.saturating_mul(100)
        || aggregate.final_validation_score_sum > aggregate.fixture_count.saturating_mul(100)
        || aggregate
            .accepted_findings
            .saturating_add(aggregate.rejected_findings)
            .saturating_add(aggregate.deferred_findings)
            != aggregate.useful_findings.saturating_add(aggregate.false_positives)
    {
        return Err(error("baseline aggregate invariant failed"));
    }
    Ok(())
}

pub(crate) fn validate_resolution(
    resolution: &FindingResolution,
) -> Result<(), RubberDuckEvaluationError> {
    match resolution {
        FindingResolution::Accepted { .. } => Ok(()),
        FindingResolution::Rejected { reason, evidence } => {
            validate_bounded_text("rejection reason", reason, MAX_TEXT_CHARS)?;
            if evidence.is_empty() || evidence.len() > MAX_OBSERVED_EVIDENCE {
                return Err(error("rejected resolution evidence count is out of bounds"));
            }
            for item in evidence {
                match item {
                    crate::delegation_quality::FindingEvidence::File(path) => {
                        validate_hermetic_path(path)?;
                    }
                    crate::delegation_quality::FindingEvidence::Tool(tool) => {
                        validate_bounded_text("resolution tool", tool, MAX_ID_CHARS)?;
                    }
                }
            }
            Ok(())
        }
        FindingResolution::Deferred { reason } => {
            validate_bounded_text("deferral reason", reason, MAX_TEXT_CHARS)
        }
    }
}

pub(crate) fn validate_backend(
    backend: &ReplayCriticBackend,
) -> Result<(), RubberDuckEvaluationError> {
    match backend {
        ReplayCriticBackend::Internal { provider_version, model_id, startup_model_id } => {
            validate_bounded_text("provider version", provider_version, MAX_ID_CHARS)?;
            validate_id("model id", model_id)?;
            validate_id("startup model id", startup_model_id)
        }
        ReplayCriticBackend::External { agent_id, implementation, startup_model_id } => {
            validate_id("agent id", agent_id)?;
            validate_bounded_text("ACP implementation", implementation, MAX_ID_CHARS)?;
            validate_id("startup model id", startup_model_id)
        }
    }
}

pub(crate) fn validate_id(field: &str, value: &str) -> Result<(), RubberDuckEvaluationError> {
    if value.is_empty()
        || value.chars().count() > MAX_ID_CHARS
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/' | b':')
        })
    {
        return Err(error(format!("{field} is empty, oversized, or malformed")));
    }
    Ok(())
}

pub(crate) fn validate_hermetic_path(path: &str) -> Result<(), RubberDuckEvaluationError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.starts_with('~')
        || path.contains('\\')
        || path.split('/').any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(error(format!("non-hermetic observed path: {path:?}")));
    }
    validate_bounded_text("observed path", path, MAX_ID_CHARS)
}

pub(crate) fn validate_bounded_text(
    field: &str,
    value: &str,
    max: usize,
) -> Result<(), RubberDuckEvaluationError> {
    if value.trim().is_empty() || value.chars().count() > max {
        return Err(error(format!("{field} is empty or exceeds {max} characters")));
    }
    if value.contains("/home/") || value.contains("~/.") {
        return Err(error(format!("{field} contains non-hermetic user-home reference")));
    }
    Ok(())
}

pub(crate) fn checked_add(
    left: u64,
    right: u64,
    field: &str,
) -> Result<u64, RubberDuckEvaluationError> {
    left.checked_add(right).ok_or_else(|| error(format!("{field} overflow")))
}

pub(crate) fn add(
    target: &mut u64,
    value: u64,
    field: &str,
) -> Result<(), RubberDuckEvaluationError> {
    *target = checked_add(*target, value, field)?;
    Ok(())
}

pub(crate) const fn completion_rank(state: CompletionState) -> u8 {
    match state {
        CompletionState::Blocked => 0,
        CompletionState::Unverified => 1,
        CompletionState::PartiallyVerified => 2,
        CompletionState::Verified => 3,
    }
}

pub(crate) fn error(message: impl Into<String>) -> RubberDuckEvaluationError {
    RubberDuckEvaluationError(message.into())
}
