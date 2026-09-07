//! Host-flow tests: snapshot.
use super::*;

#[tokio::test]
async fn initialize_request_includes_client_title() {
    let initialize = initialize_request_for_capabilities(HandlerCapabilities::none()).await;
    assert_eq!(initialize["params"]["clientInfo"]["name"], "ee");
    assert_eq!(initialize["params"]["clientInfo"]["title"], "ee");
}

#[tokio::test]
async fn initialize_snapshot_fs_read_only() {
    let initialize = initialize_request_for_capabilities(HandlerCapabilities {
        fs_read: true,
        ..HandlerCapabilities::none()
    })
    .await;
    assert_eq!(
        initialize["params"]["clientCapabilities"],
        json!({
            "fs": {
                "readTextFile": true,
                "writeTextFile": false,
            },
            "terminal": false,
        })
    );
}

#[tokio::test]
async fn initialize_snapshot_fs_write_only() {
    let initialize = initialize_request_for_capabilities(HandlerCapabilities {
        fs_write: true,
        ..HandlerCapabilities::none()
    })
    .await;
    assert_eq!(
        initialize["params"]["clientCapabilities"],
        json!({
            "fs": {
                "readTextFile": false,
                "writeTextFile": true,
            },
            "terminal": false,
        })
    );
}

#[tokio::test]
async fn initialize_snapshot_fs_read_and_write() {
    let initialize = initialize_request_for_capabilities(HandlerCapabilities {
        fs_read: true,
        fs_write: true,
        ..HandlerCapabilities::none()
    })
    .await;
    assert_eq!(
        initialize["params"]["clientCapabilities"],
        json!({
            "fs": {
                "readTextFile": true,
                "writeTextFile": true,
            },
            "terminal": false,
        })
    );
}

#[tokio::test]
async fn initialize_snapshot_terminal_support() {
    let initialize = initialize_request_for_capabilities(HandlerCapabilities {
        terminal: true,
        ..HandlerCapabilities::none()
    })
    .await;
    assert_eq!(
        initialize["params"]["clientCapabilities"],
        json!({
            "fs": {
                "readTextFile": false,
                "writeTextFile": false,
            },
            "terminal": true,
        })
    );
}

#[tokio::test]
async fn initialize_snapshot_boolean_session_config_support() {
    let initialize = initialize_request_for_capabilities(HandlerCapabilities {
        session_config_boolean: true,
        ..HandlerCapabilities::none()
    })
    .await;
    assert_eq!(
        initialize["params"]["clientCapabilities"],
        json!({
            "fs": {
                "readTextFile": false,
                "writeTextFile": false,
            },
            "terminal": false,
            "session": {
                "configOptions": {
                    "boolean": {},
                }
            }
        })
    );
}

#[tokio::test]
async fn initialize_snapshot_elicitation_form_support() {
    let initialize = initialize_request_for_capabilities(HandlerCapabilities {
        elicitation_form: true,
        ..HandlerCapabilities::none()
    })
    .await;
    assert_eq!(
        initialize["params"]["clientCapabilities"],
        json!({
            "fs": {
                "readTextFile": false,
                "writeTextFile": false,
            },
            "terminal": false,
            "elicitation": {
                "form": {},
            }
        })
    );
}

#[tokio::test]
async fn initialize_snapshot_elicitation_url_support() {
    let initialize = initialize_request_for_capabilities(HandlerCapabilities {
        elicitation_url: true,
        ..HandlerCapabilities::none()
    })
    .await;
    assert_eq!(
        initialize["params"]["clientCapabilities"],
        json!({
            "fs": {
                "readTextFile": false,
                "writeTextFile": false,
            },
            "terminal": false,
            "elicitation": {
                "url": {},
            }
        })
    );
}

#[tokio::test]
async fn initialize_snapshot_no_capabilities() {
    let initialize = initialize_request_for_capabilities(HandlerCapabilities::none()).await;
    assert_eq!(
        initialize["params"]["clientCapabilities"],
        json!({
            "fs": {
                "readTextFile": false,
                "writeTextFile": false,
            },
            "terminal": false,
        })
    );
}
