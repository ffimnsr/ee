//! Credential redaction across frames, transcripts, and durable state.

use super::*;

/// Reads every durable state file under `directory` as text.
fn read_state_files(directory: &std::path::Path) -> String {
    let mut text = String::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for path in directory_entries(&directory) {
            if path.is_dir() {
                pending.push(path);
            } else if let Ok(contents) = fs::read_to_string(&path) {
                text.push_str(&contents);
            }
        }
    }
    text
}

#[tokio::test]
async fn a_live_turn_never_leaks_the_configured_key_into_frames_or_state() {
    let secret = "sk-opencode-release-gate-secret";

    for (surface, dialect) in DIALECT_MATRIX {
        let label = format!("{surface:?}/{dialect:?}");
        let dirs = TestDirs::new();
        let mut config = test_config(surface, route_for(surface, dialect).model_id);
        config.api_key = Some(secret.to_string());
        let (codec, harness, task) = spawn_agent_with_config(
            config,
            vec![
                ScriptedAnswer::reasoning("plan", "answer"),
                ScriptedAnswer::Failure(String::from("OpenCode request failed: HTTP 500")),
            ],
            &dirs,
        );
        initialize(&harness).await;
        let session_id = new_session(&harness, 2).await;

        harness.send(request(3, "session/prompt", prompt_params(&session_id, "hello")));
        drain_prompt_start(&harness).await;
        let (updates, result) = harness.collect_until_response(3).await;
        assert_eq!(result["stopReason"], "end_turn", "{label}");

        let frames = updates.iter().map(Value::to_string).collect::<String>();
        let requests = codec
            .requests()
            .iter()
            .map(ee_agent_opencode::adapter::test_support::RecordedRequest::json)
            .collect::<Vec<_>>();
        let transcripts = Value::Array(requests).to_string();
        let state = read_state_files(&dirs.state());

        assert!(!frames.contains(secret), "{label} update payloads must not carry the key");
        assert!(!result.to_string().contains(secret), "{label} prompt result must not carry it");
        assert!(!transcripts.contains(secret), "{label} transcripts must not carry it");
        assert!(!state.contains(secret), "{label} durable session state must not carry it");

        // The failing path is the one that most often leaks: a provider error
        // must name the failure, never the credential.
        harness.send(request(4, "session/prompt", prompt_params(&session_id, "again")));
        drain_prompt_start(&harness).await;
        let (updates, error) = harness.collect_until_error(4).await;

        assert!(error.message.contains("HTTP 500"), "{label}: {}", error.message);
        assert!(!error.message.contains(secret), "{label} error text must not carry the key");
        let frames = updates.iter().map(Value::to_string).collect::<String>();
        assert!(!frames.contains(secret), "{label} failure updates must not carry the key");
        assert!(!read_state_files(&dirs.state()).contains(secret), "{label} checkpoints too");

        harness.shutdown(task).await;
    }
}
