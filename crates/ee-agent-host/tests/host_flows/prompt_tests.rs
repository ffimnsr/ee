//! Host-flow tests: prompt.
use super::*;

#[tokio::test]
async fn prompt_allows_text_and_resource_links_without_prompt_capabilities() {
    let script =
        base_script().wait_for("session/prompt").respond(json!({ "stopReason": "end_turn" }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let prompt = vec![
        ContentBlock::Text(TextContent::new("hi")),
        ContentBlock::ResourceLink(
            ResourceLink::new("readme", "file:///work/README.md")
                .title("README")
                .meta(Some(serde_json::from_value(json!({ "source": "local-test" })).unwrap())),
        ),
    ];
    thread.send_prompt(prompt).await.expect("text and resource link stay allowed");

    let request = &fake.requests_by_method("session/prompt")[0];
    assert_eq!(request["params"]["prompt"][0]["type"], "text");
    assert_eq!(request["params"]["prompt"][1]["type"], "resource_link");
    assert_eq!(request["params"]["prompt"][1]["_meta"]["source"], "local-test");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn prompt_rejects_unsupported_rich_content_locally_before_session_prompt() {
    let script = base_script();
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let image_error = thread
        .send_prompt(vec![ContentBlock::Image(ImageContent::new("ZmFrZQ==", "image/png"))])
        .await
        .unwrap_err();
    assert!(matches!(
        image_error,
        AgentError::InvalidParams(ref reason)
            if reason.contains("promptCapabilities.image")
    ));

    let audio_error = thread
        .send_prompt(vec![ContentBlock::Audio(AudioContent::new("ZmFrZQ==", "audio/wav"))])
        .await
        .unwrap_err();
    assert!(matches!(
        audio_error,
        AgentError::InvalidParams(ref reason)
            if reason.contains("promptCapabilities.audio")
    ));

    let resource_error = thread
        .send_prompt(vec![ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(
                "hello",
                "file:///work/readme.md",
            )),
        ))])
        .await
        .unwrap_err();
    assert!(matches!(
        resource_error,
        AgentError::InvalidParams(ref reason)
            if reason.contains("promptCapabilities.embeddedContext")
    ));
    assert_eq!(fake.requests_by_method("session/prompt").len(), 0);

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn prompt_allows_advertised_rich_content_and_preserves_meta() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "promptCapabilities": {
                    "image": true,
                    "audio": true,
                    "embeddedContext": true,
                    "_meta": { "diagnosticOnly": true }
                }
            }
        }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }))
        .wait_for("session/prompt")
        .respond(json!({ "stopReason": "end_turn" }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    let resource = ContentBlock::Resource(
        EmbeddedResource::new(EmbeddedResourceResource::TextResourceContents(
            TextResourceContents::new("hello", "file:///work/readme.md")
                .meta(Some(serde_json::from_value(json!({ "kind": "inline" })).unwrap())),
        ))
        .meta(Some(serde_json::from_value(json!({ "scope": "prompt" })).unwrap())),
    );
    let prompt = vec![
        ContentBlock::Image(
            ImageContent::new("ZmFrZQ==", "image/png")
                .meta(Some(serde_json::from_value(json!({ "slot": "preview" })).unwrap())),
        ),
        ContentBlock::Audio(
            AudioContent::new("ZmFrZQ==", "audio/wav")
                .meta(Some(serde_json::from_value(json!({ "slot": "clip" })).unwrap())),
        ),
        resource,
    ];
    thread.send_prompt(prompt).await.expect("advertised rich content accepted");

    let request = &fake.requests_by_method("session/prompt")[0];
    assert_eq!(request["params"]["prompt"][0]["_meta"]["slot"], "preview");
    assert_eq!(request["params"]["prompt"][1]["_meta"]["slot"], "clip");
    assert_eq!(request["params"]["prompt"][2]["_meta"]["scope"], "prompt");
    assert_eq!(request["params"]["prompt"][2]["resource"]["_meta"]["kind"], "inline");

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn prompt_capability_meta_stays_diagnostic_only() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "promptCapabilities": {
                    "_meta": {
                        "image": true,
                        "audio": true,
                        "embeddedContext": true
                    }
                }
            }
        }))
        .wait_for("session/new")
        .respond(json!({ "sessionId": "s1" }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;
    let thread =
        connection.new_session(vec![PathBuf::from("/work")], Vec::new(), None).await.unwrap();

    assert!(!connection.supports_prompt_images());
    assert!(!connection.supports_prompt_audio());
    assert!(!connection.supports_prompt_embedded_context());

    let error = thread
        .send_prompt(vec![ContentBlock::Image(ImageContent::new("ZmFrZQ==", "image/png"))])
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        AgentError::InvalidParams(ref reason)
            if reason.contains("promptCapabilities.image")
    ));
    assert_eq!(fake.requests_by_method("session/prompt").len(), 0);

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn unknown_capability_fields_stay_diagnostic_only() {
    let script = FakeAgentScript::new().wait_for("initialize").respond(json!({
        "protocolVersion": 1,
        "agentCapabilities": {
            "sessionCapabilities": { "mysteryLifecycle": {} },
            "promptCapabilities": { "mysteryPrompt": true }
        }
    }));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    assert!(!connection.supports_session_list());
    assert!(!connection.supports_session_delete());
    assert!(!connection.supports_session_resume());
    assert!(!connection.supports_session_close());
    assert!(!connection.supports_prompt_images());
    assert!(!connection.supports_prompt_audio());
    assert!(!connection.supports_prompt_embedded_context());

    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}
