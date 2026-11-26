//! `/reflect` curates the last task into the workspace experience library.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ee_agent_acp_server::{ClientBridge, PromptResult, ProviderError, UpdateSink};
use ee_agent_protocol::{
    CreateElicitationRequest, ElicitationAction, ElicitationFormMode, ElicitationMode,
    ElicitationSchema, ElicitationScope, ElicitationSessionScope, SessionId, StopReason,
};
use tokio::sync::watch;

use super::{ProviderTelemetryTurn, finish_provider_telemetry};
use crate::harness_reflection::{self, ReflectionEvidence, ReflectionToolRecord};
use crate::harness_validation::HarnessTree;
use crate::model::{ModelContent, ModelMessage, ModelRole};
use crate::observability::{TelemetryRecorder, TelemetryTurnOutcome};
use crate::runtime::OrchestratorRuntime;
use crate::user_experience::{ExperienceConfidence, ExperienceRegistry, WORKSPACE_EXPERIENCE_DIR};

/// Inputs required to run one `/reflect` command.
pub(super) struct ReflectCommand<'a> {
    pub(super) runtime: &'a Arc<OrchestratorRuntime>,
    pub(super) sink: &'a UpdateSink,
    pub(super) client: ClientBridge,
    pub(super) cancel: watch::Receiver<bool>,
    pub(super) session_id: &'a SessionId,
    pub(super) instructions: Option<&'a str>,
    pub(super) workspace: &'a std::path::Path,
    pub(super) history: Vec<ModelMessage>,
    pub(super) telemetry: &'a Arc<Mutex<TelemetryRecorder>>,
    pub(super) telemetry_turn: &'a str,
    pub(super) telemetry_started_at: Instant,
    pub(super) telemetry_event_start: usize,
}

/// Parsed `/reflect` arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ReflectArgs {
    pub(super) domain: Option<String>,
    pub(super) library_root: Option<PathBuf>,
}

/// Executes `/reflect [--library-root DIR] [domain]` and returns the turn result.
pub(super) async fn handle_reflect_command(
    ctx: ReflectCommand<'_>,
) -> Result<PromptResult, ProviderError> {
    let session = ctx.session_id.to_string();
    let args = parse_reflect_args(ctx.instructions)?;
    let domain = resolve_domain(args.domain.as_deref(), ctx.workspace)?;
    let library_root = resolve_library_root(&args, ctx.workspace);
    let task = last_task(&ctx.history).ok_or_else(|| {
        ProviderError::InvalidRequest(String::from(
            "`/reflect` needs a completed task in this session; run the task first",
        ))
    })?;
    let chosen = HarnessTree::load_components(&ctx.workspace.join(".agents"))
        .map_err(ProviderError::BackendFailure)?;
    let tool_records = tool_records(&ctx.history);
    let workspace_diff = workspace_diff(ctx.runtime, &session, ctx.workspace);
    if tool_records.is_empty() && workspace_diff.is_none() {
        return Err(ProviderError::InvalidRequest(String::from(
            "`/reflect` has no tool records or workspace diff to contrast; nothing to reflect",
        )));
    }
    let evidence = ReflectionEvidence {
        task: task.clone(),
        domain: domain.clone(),
        chosen_harness: chosen.clone(),
        base_harness: None,
        candidates: Vec::new(),
        candidate_harnesses: Vec::new(),
        decisions: None,
        adoption: None,
        tool_records,
        workspace_diff,
        raw_selection_record: None,
    };
    let projection =
        harness_reflection::build_projection(&evidence).map_err(ProviderError::InvalidRequest)?;
    let role = ctx.runtime.command_role(Some("harness_reflector")).map_err(ProviderError::from)?;
    let result = ctx
        .runtime
        .run_user_command_subtask(
            role,
            projection,
            None,
            Vec::new(),
            ctx.client.clone(),
            ctx.cancel.clone(),
        )
        .await
        .map_err(ProviderError::from)?;
    let output = harness_reflection::parse_reflection(&result.handoff.summary)
        .map_err(ProviderError::InvalidRequest)?;
    if output.edits.is_empty() && output.experience.is_none() {
        let summary = serde_json::json!({
            "command": ee_agent_protocol::REFLECT_COMMAND_NAME,
            "session_id": session,
            "domain": domain,
            "seq": null,
            "library_root": library_root.display().to_string(),
            "edits": 0,
            "no_entry": true,
            "approved": false,
            "path": null,
        });
        let _ = ctx.sink.agent_message_chunk("reflect", summary.to_string());
        finish_provider_telemetry(
            ctx.telemetry,
            ProviderTelemetryTurn {
                turn_id: ctx.telemetry_turn,
                started_at: ctx.telemetry_started_at,
                events: &ctx.runtime.event_snapshot()[ctx.telemetry_event_start..],
                outcome: TelemetryTurnOutcome::Succeeded,
                terminal_state: None,
                evidence_ids: Vec::new(),
                usage: None,
            },
        );
        return Ok(PromptResult::new(StopReason::EndTurn));
    }
    let registry = ExperienceRegistry::discover_at(&library_root);
    let seq = registry.next_seq(&domain);
    let entry =
        harness_reflection::build_entry(&output, seq, &domain, &task, ExperienceConfidence::Medium)
            .map_err(ProviderError::InvalidRequest)?;

    let lesson = entry.lesson.as_deref().map_or("NO_ENTRY (edits only)", |lesson| lesson);
    let message = format!(
        "/reflect wants to write `{}` with {} edit row(s) and lesson: {lesson}. Allow?",
        library_root
            .join(&domain)
            .join("entries")
            .join(format!("{seq}-.../EXPERIENCE.md"))
            .display(),
        entry.edits.len()
    );
    let approved = confirm(&ctx.client, ctx.session_id, message).await?;
    let written = if approved {
        let dir = harness_reflection::write_reflection(&library_root, &entry, Some(&chosen), true)
            .map_err(ProviderError::BackendFailure)?;
        Some(dir.display().to_string())
    } else {
        None
    };
    let summary = serde_json::json!({
        "command": ee_agent_protocol::REFLECT_COMMAND_NAME,
        "session_id": session,
        "domain": domain,
        "seq": seq,
        "library_root": library_root.display().to_string(),
        "edits": entry.edits.len(),
        "no_entry": entry.is_no_entry(),
        "approved": approved,
        "path": written,
    });
    let _ = ctx.sink.agent_message_chunk("reflect", summary.to_string());
    finish_provider_telemetry(
        ctx.telemetry,
        ProviderTelemetryTurn {
            turn_id: ctx.telemetry_turn,
            started_at: ctx.telemetry_started_at,
            events: &ctx.runtime.event_snapshot()[ctx.telemetry_event_start..],
            outcome: TelemetryTurnOutcome::Succeeded,
            terminal_state: None,
            evidence_ids: Vec::new(),
            usage: None,
        },
    );
    Ok(PromptResult::new(StopReason::EndTurn))
}

async fn confirm(
    client: &ClientBridge,
    session_id: &SessionId,
    message: String,
) -> Result<bool, ProviderError> {
    let scope = ElicitationScope::Session(ElicitationSessionScope::new(session_id.clone()));
    let mode = ElicitationMode::Form(ElicitationFormMode::new(scope, ElicitationSchema::new()));
    let response = client.create_elicitation(CreateElicitationRequest::new(mode, message)).await?;
    Ok(matches!(response.action, ElicitationAction::Accept(_)))
}

/// Parses `/reflect [--library-root DIR] [domain]`.
pub(super) fn parse_reflect_args(instructions: Option<&str>) -> Result<ReflectArgs, ProviderError> {
    let mut args = ReflectArgs::default();
    let mut tokens = instructions.unwrap_or_default().split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "--library-root" {
            let value = tokens.next().ok_or_else(|| {
                ProviderError::InvalidRequest(String::from(
                    "usage: /reflect [--library-root DIR] [domain]; --library-root needs a directory",
                ))
            })?;
            set_library_root(&mut args, value)?;
        } else if let Some(value) = token.strip_prefix("--library-root=") {
            set_library_root(&mut args, value)?;
        } else if token.starts_with('-') {
            return Err(ProviderError::InvalidRequest(format!(
                "unknown `/reflect` argument {token:?}; usage: /reflect [--library-root DIR] [domain]"
            )));
        } else if args.domain.is_some() {
            return Err(ProviderError::InvalidRequest(String::from(
                "usage: /reflect [--library-root DIR] [domain]; only one domain is accepted",
            )));
        } else {
            args.domain = Some(token.to_string());
        }
    }
    Ok(args)
}

fn set_library_root(args: &mut ReflectArgs, value: &str) -> Result<(), ProviderError> {
    if value.trim().is_empty() {
        return Err(ProviderError::InvalidRequest(String::from(
            "--library-root must not be empty",
        )));
    }
    args.library_root = Some(PathBuf::from(value));
    Ok(())
}

fn resolve_library_root(args: &ReflectArgs, workspace: &std::path::Path) -> PathBuf {
    args.library_root.clone().unwrap_or_else(|| workspace.join(WORKSPACE_EXPERIENCE_DIR))
}

pub(super) fn resolve_domain(
    domain: Option<&str>,
    workspace: &std::path::Path,
) -> Result<String, ProviderError> {
    if let Some(domain) = domain.map(str::trim).filter(|domain| !domain.is_empty()) {
        if valid_domain(domain) {
            return Ok(domain.to_string());
        }
        return Err(ProviderError::InvalidRequest(format!(
            "usage: /reflect [--library-root DIR] [domain]; domain {domain:?} must be lowercase letters, digits, '_', or '-'"
        )));
    }
    let derived = workspace
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            name.chars()
                .map(|character| {
                    if character.is_ascii_alphanumeric() {
                        character.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
                .trim_matches('-')
                .to_string()
        })
        .filter(|name| valid_domain(name))
        .unwrap_or_else(|| String::from("workspace"));
    Ok(derived)
}

fn valid_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain.chars().count() <= 64
        && domain
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
        && domain.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '_' | '-')
        })
}

pub(super) fn last_task(history: &[ModelMessage]) -> Option<String> {
    history
        .iter()
        .rev()
        .filter(|message| message.role == ModelRole::User)
        .map(ModelMessage::text_content)
        .map(|text| text.trim().to_string())
        .find(|text| {
            !text.is_empty()
                && !text.starts_with('/')
                && !text.starts_with("Current runtime context")
        })
}

fn tool_records(history: &[ModelMessage]) -> Vec<ReflectionToolRecord> {
    let mut names = BTreeMap::new();
    for message in history {
        for content in &message.content {
            if let ModelContent::ToolCall { tool_call_id, name, .. } = content {
                names.insert(tool_call_id.clone(), name.clone());
            }
        }
    }
    let mut records = Vec::new();
    let mut turn = 0u64;
    let mut step = 0u64;
    for message in history {
        match message.role {
            ModelRole::User => {
                turn += 1;
                step = 0;
            }
            ModelRole::Assistant => step += 1,
            _ => {}
        }
        for content in &message.content {
            let ModelContent::ToolResult { tool_call_id, result } = content else { continue };
            records.push(ReflectionToolRecord {
                turn,
                iteration: step,
                tool: names.get(tool_call_id).cloned().unwrap_or_else(|| "unknown".to_string()),
                success: result.success,
                summary: result.summary_text(),
            });
        }
    }
    records.truncate(harness_reflection::MAX_TOOL_RECORDS);
    records
}

fn workspace_diff(
    runtime: &OrchestratorRuntime,
    session_id: &str,
    workspace: &std::path::Path,
) -> Option<String> {
    let snapshots = runtime.workspace_snapshots();
    let turn = snapshots.latest_turn(session_id).ok().flatten()?;
    let diff = snapshots.turn_diff(session_id, turn, workspace).ok().flatten()?;
    let mut text = format!("turn {}:\n", diff.turn);
    for file in diff.files {
        text.push_str(&format!("- {} {}\n", diff_label(file.change), file.path));
    }
    Some(text)
}

fn diff_label(change: crate::workspace_snapshot::DiffChange) -> &'static str {
    use crate::workspace_snapshot::DiffChange;
    match change {
        DiffChange::Added => "added",
        DiffChange::Modified => "modified",
        DiffChange::Deleted => "deleted",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reflect_arguments_parse_domain_and_library_root() {
        assert_eq!(parse_reflect_args(None).expect("defaults"), ReflectArgs::default());
        assert_eq!(
            parse_reflect_args(Some("rust")).expect("domain"),
            ReflectArgs { domain: Some(String::from("rust")), library_root: None }
        );
        assert_eq!(
            parse_reflect_args(Some("--library-root /tmp/lib rust")).expect("flag first"),
            ReflectArgs {
                domain: Some(String::from("rust")),
                library_root: Some(PathBuf::from("/tmp/lib")),
            }
        );
        assert_eq!(
            parse_reflect_args(Some("rust --library-root=/tmp/lib")).expect("equals form"),
            ReflectArgs {
                domain: Some(String::from("rust")),
                library_root: Some(PathBuf::from("/tmp/lib")),
            }
        );
    }

    #[test]
    fn reflect_arguments_reject_bad_flags() {
        assert!(parse_reflect_args(Some("--library-root")).is_err());
        assert!(parse_reflect_args(Some("--library-root=")).is_err());
        assert!(parse_reflect_args(Some("--force")).is_err());
        assert!(parse_reflect_args(Some("rust docs")).is_err());
    }

    #[test]
    fn default_library_root_is_the_workspace_experience_directory() {
        let workspace = std::path::Path::new("/work");
        assert_eq!(
            resolve_library_root(&ReflectArgs::default(), workspace),
            std::path::PathBuf::from("/work/.agents/experience")
        );
        assert_eq!(
            resolve_library_root(
                &ReflectArgs { domain: None, library_root: Some(PathBuf::from("/scratch")) },
                workspace,
            ),
            std::path::PathBuf::from("/scratch")
        );
    }
}
