//! Per-dialect behavior matrix: one route of every dialect.

use super::*;

// ── Per-dialect behavior matrix (Phase 6) ────────────────────────────────
//
// The single-dialect tests above assert detailed frame content. These tests run
// the same production path for one route of every dialect, so cancellation,
// stream ordering, policy admission, durable recovery, and session close are
// proven per codec rather than once.

#[tokio::test]
async fn every_dialect_streams_thought_then_answer_and_ends_the_turn() {
    for (surface, dialect) in DIALECT_MATRIX {
        let dirs = TestDirs::new();
        let (codec, harness, task) = spawn_agent(
            surface,
            dialect,
            vec![ScriptedAnswer::reasoning("thinking", "answer")],
            &dirs,
        );
        initialize(&harness).await;
        let session_id = new_session(&harness, 2).await;

        harness.send(request(3, "session/prompt", prompt_params(&session_id, "hello")));
        drain_prompt_start(&harness).await;
        let (updates, result) = harness.collect_until_response(3).await;

        let label = format!("{surface:?}/{dialect:?}");
        assert_eq!(result["stopReason"], "end_turn", "{label}");
        assert!(has_text_chunk(&updates, "agent_message_chunk", "answer"), "{label}");
        let kinds = update_kinds(&updates);
        let thought = kinds.iter().position(|kind| kind == "agent_thought_chunk");
        let answer = kinds.iter().position(|kind| kind == "agent_message_chunk");
        assert!(
            matches!((thought, answer), (Some(thought), Some(answer)) if thought < answer),
            "reasoning must stream before the answer for {label}: {kinds:?}"
        );
        assert_eq!(codec.requests().len(), 1, "one model round for one answer on {label}");

        harness.shutdown(task).await;
    }
}

#[tokio::test]
async fn every_dialect_cancels_a_pending_model_call_without_retrying() {
    for (surface, dialect) in DIALECT_MATRIX {
        let dirs = TestDirs::new();
        let (codec, harness, task) =
            spawn_agent(surface, dialect, vec![ScriptedAnswer::Pending], &dirs);
        initialize(&harness).await;
        let session_id = new_session(&harness, 2).await;

        harness.send(request(3, "session/prompt", prompt_params(&session_id, "wait")));
        drain_prompt_start(&harness).await;
        wait_for_requests(&codec, 1).await;
        harness.send(notification("session/cancel", json!({ "sessionId": session_id })));
        let (updates, result) = harness.collect_until_response(3).await;

        let label = format!("{surface:?}/{dialect:?}");
        assert_eq!(result["stopReason"], "cancelled", "{label}");
        assert_eq!(codec.requests().len(), 1, "a cancelled call is never retried on {label}");
        assert!(
            !update_kinds(&updates).contains(&String::from("agent_message_chunk")),
            "a cancelled turn emits no answer on {label}"
        );

        harness.shutdown(task).await;
    }
}

#[tokio::test]
async fn every_dialect_denies_a_policy_blocked_tool_before_any_host_request() {
    for (surface, dialect) in DIALECT_MATRIX {
        let dirs = TestDirs::new();
        // `kill_terminal` is registered but carries the destructive
        // `TerminalKill` subclass, so the shared ee agent policy denies it and
        // the host is never asked to kill anything.
        let (codec, harness, task) = spawn_agent(
            surface,
            dialect,
            vec![
                ScriptedAnswer::tool_call(
                    "call_1",
                    "kill_terminal",
                    json!({ "terminal_id": "t-1" }),
                ),
                ScriptedAnswer::text("stopped"),
            ],
            &dirs,
        );
        initialize(&harness).await;
        let session_id = new_session(&harness, 2).await;

        harness.send(request(3, "session/prompt", prompt_params(&session_id, "kill it")));
        drain_prompt_start(&harness).await;
        // `collect_until_response` panics on server-initiated requests, so a
        // clean result proves no host mutation request was made.
        let (updates, result) = harness.collect_until_response(3).await;

        let label = format!("{surface:?}/{dialect:?}");
        assert_eq!(result["stopReason"], "end_turn", "{label}");
        let failed = updates.iter().any(|params| {
            params["update"]["sessionUpdate"] == "tool_call_update"
                && params["update"]["status"] == "failed"
        });
        assert!(failed, "the denied tool reports a failed call on {label}: {updates:?}");
        assert!(has_text_chunk(&updates, "agent_message_chunk", "stopped"), "{label}");
        assert_eq!(codec.requests().len(), 2, "the turn continued after the denial on {label}");

        harness.shutdown(task).await;
    }
}

#[tokio::test]
async fn every_dialect_recovers_a_durable_session_after_a_restart() {
    for (surface, dialect) in DIALECT_MATRIX {
        let label = format!("{surface:?}/{dialect:?}");
        let dirs = TestDirs::new();
        let (first_codec, harness, task) =
            spawn_agent(surface, dialect, vec![ScriptedAnswer::text("first answer")], &dirs);
        initialize(&harness).await;
        let session_id = new_session(&harness, 2).await;
        harness.send(request(3, "session/prompt", prompt_params(&session_id, "hello")));
        drain_prompt_start(&harness).await;
        let (updates, result) = harness.collect_until_response(3).await;
        assert_eq!(result["stopReason"], "end_turn", "{label}");
        assert!(has_text_chunk(&updates, "agent_message_chunk", "first answer"), "{label}");
        assert_eq!(first_codec.requests().len(), 1, "{label} model round before the restart");
        harness.shutdown(task).await;

        // Durability is only real if the state survives the process: a fresh
        // provider over the same state directory must reload the session and
        // keep taking prompts on it.
        assert!(
            !directory_entries(&dirs.state()).is_empty(),
            "durable session state is written on {label}"
        );
        let (codec, harness, task) =
            spawn_agent(surface, dialect, vec![ScriptedAnswer::text("resumed answer")], &dirs);
        initialize(&harness).await;
        harness.send(request(
            2,
            "session/load",
            json!({
                "sessionId": session_id,
                "cwd": dirs.workspace(),
                "mcpServers": [],
            }),
        ));
        let (updates, loaded) = harness.collect_until_response(2).await;
        assert!(loaded.is_object(), "{label} load returns a result object: {loaded}");
        let commands = updates
            .iter()
            .find(|params| params["update"]["sessionUpdate"] == "available_commands_update")
            .unwrap_or_else(|| panic!("{label} advertises commands after load: {updates:?}"));
        assert_eq!(
            commands["sessionId"],
            json!(session_id),
            "{label} reloads the same durable session id"
        );

        harness.send(request(3, "session/prompt", prompt_params(&session_id, "continue")));
        drain_prompt_start(&harness).await;
        let (updates, result) = harness.collect_until_response(3).await;
        assert_eq!(result["stopReason"], "end_turn", "{label} after reload");
        assert!(
            has_text_chunk(&updates, "agent_message_chunk", "resumed answer"),
            "{label} keeps prompting after a restart"
        );
        assert_eq!(codec.requests().len(), 1, "{label} model round after reload");

        harness.shutdown(task).await;
    }
}

#[tokio::test]
async fn every_dialect_keeps_durable_recovery_state_wired_to_the_checkpoint_directory() {
    for (surface, dialect) in DIALECT_MATRIX {
        let dirs = TestDirs::new();
        let config = test_config(surface, route_for(surface, dialect).model_id);
        let mut durable = config;
        durable.checkpoint_dir = Some(dirs.checkpoints());

        let provider_config =
            ee_agent_opencode::adapter::opencode_orchestrator_config(&durable, dirs.state());

        assert!(
            provider_config.orchestrator.recovery.is_durable(),
            "{surface:?}/{dialect:?} keeps the configured durable recovery directory"
        );
        assert_eq!(
            provider_config.orchestrator.recovery.checkpoint_dir.as_deref(),
            Some(dirs.checkpoints().as_path())
        );
    }
}

#[tokio::test]
async fn every_dialect_closes_the_session_and_exits_on_eof() {
    for (surface, dialect) in DIALECT_MATRIX {
        let dirs = TestDirs::new();
        let (_codec, harness, task) =
            spawn_agent(surface, dialect, vec![ScriptedAnswer::text("unused")], &dirs);
        initialize(&harness).await;
        let session_id = new_session(&harness, 2).await;

        harness.send(request(3, "session/close", json!({ "sessionId": session_id })));
        assert_eq!(
            result_of(harness.next_frame().await),
            json!({}),
            "{surface:?}/{dialect:?} closes cleanly"
        );

        harness.shutdown(task).await;
    }
}
