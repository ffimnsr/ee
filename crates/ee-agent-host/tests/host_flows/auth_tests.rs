//! Host-flow tests: auth.
use super::*;

#[tokio::test]
async fn authenticate_and_logout_round_trip() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "auth": { "logout": {} }
            },
            "authMethods": [{ "id": "device", "name": "Device auth" }]
        }))
        .wait_for("authenticate")
        .respond(json!({}))
        .wait_for("logout")
        .respond(json!({}));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let auth_methods = connection.auth_methods();
    assert_eq!(auth_methods.len(), 1);
    assert_eq!(auth_methods[0].id().0.as_ref(), "device");
    assert!(connection.supports_logout());

    connection.authenticate(auth_methods[0].id().clone()).await.expect("authenticate");
    connection.logout().await.expect("logout");

    assert_eq!(fake.requests_by_method("authenticate").len(), 1);
    assert_eq!(fake.requests_by_method("logout").len(), 1);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}

#[tokio::test]
async fn logout_without_advertised_support_fails_locally() {
    let script = FakeAgentScript::new()
        .wait_for("initialize")
        .respond(json!({
            "protocolVersion": 1,
            "agentCapabilities": {},
            "authMethods": [{ "id": "device", "name": "Device auth" }]
        }))
        .wait_for("authenticate")
        .respond(json!({}));
    let (fake, host) = spawn_host(script, Arc::new(DenyAllHandler)).await;
    let connection = ready_connection(&fake, &host).await;

    let auth_methods = connection.auth_methods();
    assert_eq!(auth_methods.len(), 1);
    assert!(!connection.supports_logout());

    connection.authenticate(auth_methods[0].id().clone()).await.expect("authenticate");
    let error = connection.logout().await.unwrap_err();
    assert!(
        matches!(error, AgentError::CapabilityUnsupported { ref method } if method == "logout")
    );

    assert_eq!(fake.requests_by_method("authenticate").len(), 1);
    assert_eq!(fake.requests_by_method("logout").len(), 0);
    host.close().await;
    fake.join(TEST_TIMEOUT).await;
}
