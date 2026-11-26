//! `/usage` ACP command flow: advertisement and local execution.

use super::*;

#[tokio::test]
async fn usage_command_is_advertised_and_answered_without_a_model_call() {
    let dirs = TestDirs::new();
    let (surface, dialect) = DIALECT_MATRIX[0];
    let (codec, harness, task) =
        spawn_agent(surface, dialect, vec![ScriptedAnswer::text("must not run")], &dirs);
    initialize(&harness).await;

    harness.send(request(
        2,
        "session/new",
        json!({
            "cwd": harness.workspace,
            "additionalDirectories": [],
            "mcpServers": [],
        }),
    ));
    let session_id = result_of(harness.next_frame().await)["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    let update = notification_params(harness.next_frame().await);
    assert_eq!(update["update"]["sessionUpdate"], "available_commands_update");
    let commands = update["update"]["availableCommands"]
        .as_array()
        .unwrap_or_else(|| panic!("commands advertised: {update}"));
    assert!(
        commands.iter().any(|command| command["name"] == "usage"),
        "usage command advertised: {commands:?}"
    );

    harness.send(request(3, "session/prompt", prompt_params(&session_id, "/usage")));
    let (updates, result) = harness.collect_until_response(3).await;
    assert_eq!(result["stopReason"], "end_turn");
    let text = update_text(&updates, "agent_message_chunk");
    assert!(
        text.contains("usage probing is disabled in this test fixture"),
        "usage command answers locally: {text}"
    );
    assert!(codec.requests().is_empty(), "usage must not call the model");

    harness.shutdown(task).await;
}
