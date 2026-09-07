//! End-to-end tool routing tests over a live duplex MCP session.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_and_review_tools_return_structured_content() {
    let backend = Arc::new(ScriptedBackend::default());
    let (client, server) = connect(backend.clone()).await;

    let status = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client.call_tool(CallToolRequestParams::new("ee_git_status")),
    )
    .await
    .expect("status timed out")
    .expect("status call failed");
    assert_eq!(status.structured_content.expect("status content")["branch"], json!("main"));

    let staged_diff = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client.call_tool(CallToolRequestParams::new("ee_git_diff_staged")),
    )
    .await
    .expect("staged diff timed out")
    .expect("staged diff call failed");
    assert!(
        staged_diff.structured_content.expect("staged diff content")["diff"]
            .as_str()
            .expect("staged diff text")
            .contains("staged.rs")
    );

    let diff = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client.call_tool(
            CallToolRequestParams::new("ee_git_diff_file")
                .with_arguments(arguments(json!({ "path": "/abs/work/src/main.rs" }))),
        ),
    )
    .await
    .expect("diff timed out")
    .expect("diff call failed");
    assert!(
        diff.structured_content.expect("diff content")["diff"]
            .as_str()
            .expect("diff text")
            .contains("src/main.rs")
    );

    let changed = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client.call_tool(CallToolRequestParams::new("ee_changed_files")),
    )
    .await
    .expect("changed files timed out")
    .expect("changed files call failed");
    assert_eq!(
        changed.structured_content.expect("changed content")["files"].as_array().map(Vec::len),
        Some(1)
    );

    let review = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client.call_tool(CallToolRequestParams::new("ee_review_context")),
    )
    .await
    .expect("review timed out")
    .expect("review call failed");
    assert!(review.structured_content.expect("review content").get("testSuggestions").is_some());
    assert_eq!(
        backend.calls(),
        vec![
            "git_status",
            "git_diff_staged",
            "git_diff_file:/abs/work/src/main.rs",
            "changed_files",
            "review_context",
            "changed_files"
        ]
    );

    shutdown(&client, &server);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_roots_returns_structured_content() {
    let backend = Arc::new(ScriptedBackend::default());
    let (client, server) = connect(backend.clone()).await;

    let result = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client.call_tool(CallToolRequestParams::new("ee_workspace_roots")),
    )
    .await
    .expect("call timed out")
    .expect("call failed");
    assert_eq!(result.is_error, Some(false));
    let structured = result.structured_content.expect("structured content");
    assert_eq!(structured["roots"], json!(["/abs/work", "/abs/extra"]));
    assert_eq!(structured["activeRoot"], json!("/abs/work"));
    assert_eq!(backend.calls(), vec![String::from("workspace_roots")]);

    shutdown(&client, &server);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_directory_returns_structured_content() {
    let backend = Arc::new(ScriptedBackend::default());
    let (client, server) = connect(backend.clone()).await;

    let params = CallToolRequestParams::new("ee_list_directory")
        .with_arguments(arguments(json!({ "path": "/abs/work" })));
    let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
        .await
        .expect("call timed out")
        .expect("call failed");
    let structured = result.structured_content.expect("structured content");
    assert_eq!(structured["entries"][0]["path"], json!("/abs/work/src"));
    assert_eq!(structured["entries"][0]["kind"], json!("directory"));
    assert_eq!(backend.calls(), vec![String::from("list_directory:/abs/work")]);

    shutdown(&client, &server);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_tools_return_structured_content() {
    let backend = Arc::new(ScriptedBackend::default());
    let (client, server) = connect(backend.clone()).await;

    let files = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client.call_tool(
            CallToolRequestParams::new("ee_search_files")
                .with_arguments(arguments(json!({ "pattern": "src/*.rs" }))),
        ),
    )
    .await
    .expect("search_files timed out")
    .expect("search_files failed");
    assert_eq!(
        files.structured_content.expect("structured")["matches"],
        json!(["/abs/work/src/*.rs"])
    );

    let files_all = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client.call_tool(
            CallToolRequestParams::new("ee_search_files_all")
                .with_arguments(arguments(json!({ "pattern": ".git/*" }))),
        ),
    )
    .await
    .expect("search_files_all timed out")
    .expect("search_files_all failed");
    assert_eq!(
        files_all.structured_content.expect("structured")["matches"][0]["hidden"],
        json!(true)
    );

    let text = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client.call_tool(
            CallToolRequestParams::new("ee_search_text")
                .with_arguments(arguments(json!({ "query": "needle" }))),
        ),
    )
    .await
    .expect("search_text timed out")
    .expect("search_text failed");
    let structured = text.structured_content.expect("structured");
    assert_eq!(structured["matches"][0]["line"], json!(7));
    assert_eq!(structured["matches"][0]["context"], json!("found needle"));

    let regex = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client.call_tool(
            CallToolRequestParams::new("ee_search_text_regex")
                .with_arguments(arguments(json!({ "pattern": "main" }))),
        ),
    )
    .await
    .expect("search_text_regex timed out")
    .expect("search_text_regex failed");
    assert_eq!(regex.structured_content.expect("structured")["matches"][0]["line"], json!(9));

    let scoped =
        tokio::time::timeout(
            REQUEST_TIMEOUT,
            client.call_tool(CallToolRequestParams::new("ee_search_text_in_files").with_arguments(
                arguments(json!({ "query": "needle", "file_glob": "src/main.rs" })),
            )),
        )
        .await
        .expect("search_text_in_files timed out")
        .expect("search_text_in_files failed");
    assert_eq!(scoped.structured_content.expect("structured")["matches"][0]["line"], json!(11));

    assert_eq!(
        backend.calls(),
        vec![
            String::from("search_files:src/*.rs"),
            String::from("search_files_all:.git/*"),
            String::from("search_text:needle"),
            String::from("search_text_regex:main"),
            String::from("search_text_in_files:needle:src/main.rs"),
        ]
    );

    shutdown(&client, &server);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_context_tools_dispatch_flat_requests_and_return_provenance() {
    let backend = Arc::new(ScriptedBackend::default());
    let (client, server) = connect(backend.clone()).await;

    let search = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client.call_tool(
            CallToolRequestParams::new("ee_web_search")
                .with_arguments(arguments(json!({ "query": "rmcp docs" }))),
        ),
    )
    .await
    .expect("web search timed out")
    .expect("web search failed");
    let search = search.structured_content.expect("structured web search");
    assert_eq!(search["results"][0]["url"], json!("https://example.com/docs"));
    assert_eq!(search["provenance"], json!("configured_search_backend"));
    assert_eq!(search["cached"], json!(true));
    assert_eq!(search["truncated"], json!(false));

    let fetch = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client.call_tool(
            CallToolRequestParams::new("ee_fetch_url")
                .with_arguments(arguments(json!({ "url": "https://example.com/docs" }))),
        ),
    )
    .await
    .expect("URL fetch timed out")
    .expect("URL fetch failed");
    let fetch = fetch.structured_content.expect("structured URL fetch");
    assert_eq!(fetch["requestedUrl"], json!("https://example.com/docs"));
    assert_eq!(fetch["provenance"], json!("https://example.com/docs"));
    assert_eq!(fetch["cached"], json!(false));
    assert_eq!(fetch["truncated"], json!(true));
    assert_eq!(
        backend.calls(),
        vec![
            String::from("web_search:rmcp docs"),
            String::from("fetch_url:https://example.com/docs"),
        ]
    );

    shutdown(&client, &server);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_run_tools_dispatch_exact_flat_requests() {
    let backend = Arc::new(ScriptedBackend::default());
    let (client, server) = connect(backend.clone()).await;

    for (name, input, action) in [
        ("ee_browser_run_content", json!({ "url": "https://example.com/content" }), "content"),
        (
            "ee_browser_run_scrape",
            json!({ "url": "https://example.com/page", "selector": "main" }),
            "scrape",
        ),
        (
            "ee_browser_run_json",
            json!({ "url": "https://example.com/api", "prompt": "extract version" }),
            "json",
        ),
    ] {
        let result = tokio::time::timeout(
            REQUEST_TIMEOUT,
            client.call_tool(CallToolRequestParams::new(name).with_arguments(arguments(input))),
        )
        .await
        .expect("browser run timed out")
        .expect("browser run failed");
        let result = result.structured_content.expect("structured browser result");
        assert_eq!(result["action"], json!(action), "{name}");
        assert_eq!(result["result"]["kind"], json!(action), "{name}");
        assert_eq!(result["trust"], json!("untrusted_external_content"), "{name}");
    }
    assert_eq!(
        backend.calls(),
        vec![
            String::from("browser_run:content:https://example.com/content::"),
            String::from("browser_run:scrape:https://example.com/page:main:"),
            String::from("browser_run:json:https://example.com/api::extract version"),
        ]
    );

    shutdown(&client, &server);
}

#[test]
fn default_web_backend_failures_are_stable_structured_errors() {
    let search = DenyWriteBackend
        .web_search(WebSearchRequest { query: String::from("rust") })
        .expect_err("default web search must fail closed");
    let fetch = DenyWriteBackend
        .fetch_url(FetchUrlRequest { url: String::from("https://example.com") })
        .expect_err("default URL fetch must fail closed");
    let browser = DenyWriteBackend
        .browser_run(BrowserRunRequest {
            action: BrowserRunAction::Content,
            url: String::from("https://example.com"),
            selector: None,
            prompt: None,
        })
        .expect_err("default browser run must fail closed");
    assert_eq!(search.message, "web_search_unavailable: no configured web search backend");
    assert_eq!(fetch.message, "web_disabled: web fetching is unavailable in this proxy mode");
    assert_eq!(browser.message, "web_disabled: browser runs are unavailable in this proxy mode");
    assert_eq!(
        serde_json::to_value(WebToolError::new(
            WebToolErrorCode::NetworkApprovalRequired,
            "approval required",
        ))
        .expect("web error serializes"),
        json!({ "code": "network_approval_required", "message": "approval required" })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase2_tools_return_structured_content() {
    let backend = Arc::new(ScriptedBackend::default());
    let (client, server) = connect(backend.clone()).await;

    let replace = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client.call_tool(CallToolRequestParams::new("ee_replace_text").with_arguments(arguments(
            json!({
                "path": "/abs/work/src/main.rs",
                "old_text": "old",
                "new_text": "new"
            }),
        ))),
    )
    .await
    .expect("replace_text timed out")
    .expect("replace_text failed");
    assert_eq!(
        replace.structured_content.expect("structured")["newRevision"],
        json!("rev-replace")
    );

    let patch = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client.call_tool(CallToolRequestParams::new("ee_apply_patch").with_arguments(arguments(
            json!({
                "path": "/abs/work/src/main.rs",
                "edits": [{ "old_text": "a", "new_text": "b" }]
            }),
        ))),
    )
    .await
    .expect("apply_patch timed out")
    .expect("apply_patch failed");
    assert_eq!(patch.structured_content.expect("structured")["editCount"], json!(1));

    let open_buffers = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client.call_tool(CallToolRequestParams::new("ee_open_buffers")),
    )
    .await
    .expect("open_buffers timed out")
    .expect("open_buffers failed");
    assert_eq!(
        open_buffers.structured_content.expect("structured")["buffers"][0]["languageId"],
        json!("rust")
    );

    let _read_buffer = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client.call_tool(
            CallToolRequestParams::new("ee_read_buffer")
                .with_arguments(arguments(json!({ "path": "/abs/work/src/main.rs" }))),
        ),
    )
    .await
    .expect("read_buffer timed out")
    .expect("read_buffer failed");

    shutdown(&client, &server);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn phase2_argument_validation_rejects_invalid_patch_shapes() {
    let backend: Arc<dyn EeProxyBackend> = Arc::new(ScriptedBackend::default());
    let (client, server) = connect(backend).await;

    let error = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client.call_tool(CallToolRequestParams::new("ee_apply_patch").with_arguments(arguments(
            json!({
                "path": "/abs/work/src/main.rs",
                "edits": [{ "range": [1, 2], "new_text": "x" }]
            }),
        ))),
    )
    .await
    .expect("apply_patch invalid timed out")
    .expect_err("invalid patch shape must fail");
    assert!(format!("{error:?}").contains("old_text and new_text"));

    let error = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client.call_tool(CallToolRequestParams::new("ee_read_buffer_lines").with_arguments(
            arguments(json!({ "path": "/abs/work/src/main.rs", "line": 0, "limit": 1 })),
        )),
    )
    .await
    .expect("read_buffer_lines invalid timed out")
    .expect_err("line zero must fail");
    assert!(format!("{error:?}").contains("greater than zero"));

    shutdown(&client, &server);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_directory_all_returns_structured_content() {
    let backend = Arc::new(ScriptedBackend::default());
    let (client, server) = connect(backend.clone()).await;

    let params = CallToolRequestParams::new("ee_list_directory_all")
        .with_arguments(arguments(json!({ "path": "/abs/work" })));
    let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
        .await
        .expect("call timed out")
        .expect("call failed");
    let structured = result.structured_content.expect("structured content");
    assert_eq!(structured["entries"][0]["hidden"], json!(true));
    assert_eq!(structured["entries"][0]["ignored"], json!(true));
    assert_eq!(backend.calls(), vec![String::from("list_directory_all:/abs/work")]);

    shutdown(&client, &server);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_text_file_routes_to_backend() {
    let backend = Arc::new(ScriptedBackend::default());
    let (client, server) = connect(backend.clone()).await;

    let params = CallToolRequestParams::new("ee_read_text_file")
        .with_arguments(arguments(json!({ "path": "/abs/notes.txt", "line": 3, "limit": 10 })));
    let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
        .await
        .expect("call timed out")
        .expect("call failed");
    assert_eq!(result.is_error, Some(false));
    let text = result.content.first().and_then(ContentBlock::as_text).expect("text block");
    assert_eq!(text.text, "content of /abs/notes.txt");

    assert_eq!(backend.calls(), vec!["read:/abs/notes.txt:Some(3):Some(10)".to_owned()]);

    shutdown(&client, &server);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relative_path_is_rejected() {
    let backend = Arc::new(ScriptedBackend::default());
    let (client, server) = connect(backend.clone()).await;

    let params = CallToolRequestParams::new("ee_read_text_file")
        .with_arguments(arguments(json!({ "path": "relative.txt" })));
    let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
        .await
        .expect("call timed out");
    assert!(result.is_err(), "relative path must surface as a protocol error");

    assert!(backend.calls().is_empty(), "backend must not be invoked");

    shutdown(&client, &server);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_text_file_result() {
    let backend = Arc::new(ScriptedBackend::default());
    let (client, server) = connect(backend.clone()).await;

    let params = CallToolRequestParams::new("ee_write_text_file")
        .with_arguments(arguments(json!({ "path": "/abs/out.txt", "content": "hello" })));
    let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
        .await
        .expect("call timed out")
        .expect("call failed");
    assert_eq!(result.is_error, Some(false));
    let text = result.content.first().and_then(ContentBlock::as_text).expect("text block");
    assert!(text.text.contains("wrote 5 bytes to /abs/out.txt"));

    assert_eq!(backend.calls(), vec!["write:/abs/out.txt:hello".to_owned()]);

    shutdown(&client, &server);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permission_denied_surfaces_as_is_error_result() {
    let backend: Arc<dyn EeProxyBackend> = Arc::new(DenyWriteBackend);
    let (client, server) = connect(backend).await;

    let params = CallToolRequestParams::new("ee_write_text_file")
        .with_arguments(arguments(json!({ "path": "/abs/out.txt", "content": "hello" })));
    let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
        .await
        .expect("call timed out")
        .expect("denials are tool results, not protocol errors");
    assert_eq!(result.is_error, Some(true));
    let text = result.content.first().and_then(ContentBlock::as_text).expect("text block");
    assert!(text.text.contains("no write access"), "message must reach the caller");
    assert!(text.text.starts_with("denied:"), "denials are prefixed");

    shutdown(&client, &server);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_create_rejects_secret_env() {
    let backend = Arc::new(ScriptedBackend::default());
    let (client, server) = connect(backend.clone()).await;

    let params = CallToolRequestParams::new("ee_terminal_create")
        .with_arguments(arguments(json!({ "command": "cargo", "env": { "API_TOKEN": "sekrit" } })));
    let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
        .await
        .expect("call timed out");
    assert!(result.is_err(), "secret-like env keys must be rejected");

    assert!(backend.calls().is_empty(), "backend must not be invoked");

    shutdown(&client, &server);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_create_rejects_all_secret_like_environment_keys() {
    for key in ["TOKEN", "KEY", "SECRET", "PASSWORD", "AUTH", "CREDENTIAL", "api_token"] {
        let backend = Arc::new(ScriptedBackend::default());
        let (client, server) = connect(backend.clone()).await;
        let params = CallToolRequestParams::new("ee_terminal_create")
            .with_arguments(arguments(json!({ "command": "pwd", "env": { key: "redacted" } })));
        let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
            .await
            .expect("call timed out");
        assert!(result.is_err(), "{key} must be rejected");
        assert!(backend.calls().is_empty(), "{key} must not reach backend");
        shutdown(&client, &server);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_create_routes_to_backend() {
    let backend = Arc::new(ScriptedBackend::default());
    let (client, server) = connect(backend.clone()).await;

    let params =
        CallToolRequestParams::new("ee_terminal_create").with_arguments(arguments(json!({
            "command": "cargo",
            "args": ["check"],
            "cwd": "/abs/work",
            "env": { "RUST_BACKTRACE": "1" },
        })));
    let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
        .await
        .expect("call timed out")
        .expect("call failed");
    assert_eq!(result.is_error, Some(false));
    let text = result.content.first().and_then(ContentBlock::as_text).expect("text block");
    assert_eq!(text.text, "term-1");

    assert_eq!(
        backend.calls(),
        vec![
            "terminal:cargo:[\"check\"]:Some(\"/abs/work\"):[(\"RUST_BACKTRACE\", \"1\")]"
                .to_owned()
        ]
    );

    shutdown(&client, &server);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_lifecycle_routes_to_backend() {
    let backend = Arc::new(ScriptedBackend::default());
    let (client, server) = connect(backend.clone()).await;

    for tool in
        ["ee_terminal_output", "ee_terminal_wait", "ee_terminal_kill", "ee_terminal_release"]
    {
        let params = CallToolRequestParams::new(tool)
            .with_arguments(arguments(json!({ "terminal_id": "term-1" })));
        let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
            .await
            .expect("call timed out")
            .expect("call failed");
        assert_eq!(result.is_error, Some(false));
    }
    assert_eq!(
        backend.calls(),
        vec![
            String::from("terminal_output:term-1"),
            String::from("terminal_wait:term-1"),
            String::from("terminal_kill:term-1"),
            String::from("terminal_release:term-1"),
        ]
    );
    shutdown(&client, &server);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_tool_fails_closed() {
    let backend = Arc::new(ScriptedBackend::default());
    let (client, server) = connect(backend).await;

    let params = CallToolRequestParams::new("ee_nope");
    let result = tokio::time::timeout(REQUEST_TIMEOUT, client.call_tool(params))
        .await
        .expect("call timed out");
    assert!(result.is_err(), "unknown tools must be method-not-found errors");

    shutdown(&client, &server);
}
