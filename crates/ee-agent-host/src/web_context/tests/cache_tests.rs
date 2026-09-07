//! Cache, bounds, redirect, and browser-run tests.
use super::cache::CacheValidators;
use super::cache::WebCacheKey;
use super::cache::WebCacheValue;
use super::cache::WebContextCache;
use super::cache::WebRepresentation;
use super::*;

#[tokio::test]
async fn search_queries_are_bounded_before_transport() {
    let search =
        WebContextService::new(config(), FakeTransport::new(vec![public_ip()], Vec::new()))
            .unwrap();
    assert_eq!(
        search.search(WebSearchRequest { query: "  \t ".to_owned() }).await.unwrap_err().code,
        WebContextErrorCode::UrlRejected
    );
    assert_eq!(
        search
            .search(WebSearchRequest { query: "x".repeat(MAX_SEARCH_QUERY_BYTES + 1) })
            .await
            .unwrap_err()
            .code,
        WebContextErrorCode::UrlRejected
    );
    assert!(search.transport.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn search_cache_returns_normalized_query_hit() {
    let transport = FakeTransport::new(
        vec![public_ip()],
        vec![response(
            200,
            &[("content-type", "application/json")],
            br#"{"results":[{"title":"Docs","url":"https://docs.example/","content":"Reference"}]}"#,
        )],
    );
    let service = WebContextService::new(config(), transport).unwrap();

    let first =
        service.search(WebSearchRequest { query: "widget   api".to_owned() }).await.unwrap();
    let second = service.search(WebSearchRequest { query: "widget api".to_owned() }).await.unwrap();

    assert!(!first.cached);
    assert!(second.cached);
    assert_eq!(first.provenance.provider, WebSearchProvider::Searxng);
    assert_eq!(first.provenance.adapter, PROVIDER_ADAPTER_VERSION);
    assert_eq!(first.provenance, second.provenance);
    assert_eq!(service.transport.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn fetch_cache_preserves_original_retrieval_time() {
    let transport = FakeTransport::new(
        vec![public_ip()],
        vec![response(200, &[("content-type", "text/plain")], b"documentation")],
    );
    let service = WebContextService::new(config(), transport).unwrap();
    let request = WebFetchRequest { url: "https://docs.example/reference".to_owned() };

    let first = service.fetch(request.clone()).await.unwrap();
    let second = service.fetch(request).await.unwrap();

    assert!(!first.cached);
    assert!(second.cached);
    assert!(first.retrieved_at_unix_ms > 0);
    assert_eq!(first.retrieved_at_unix_ms, second.retrieved_at_unix_ms);
    assert_eq!(service.transport.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cache_marks_expired_entries_stale_and_evicts_least_recently_used_entries() {
    let mut cache =
        WebContextCache::new(1, 1024, 1024, Duration::from_secs(60), Duration::from_secs(60));
    let value = WebCacheValue::Search(WebSearchResponse {
        results: Vec::new(),
        provenance: WebSearchProvenance::for_provider(WebSearchProvider::Searxng),
        truncated: false,
        cached: false,
    });
    let first = WebCacheKey::Fetch {
        final_url: "https://docs.example/first".to_owned(),
        representation: WebRepresentation::TextV1,
    };
    let second = WebCacheKey::Fetch {
        final_url: "https://docs.example/second".to_owned(),
        representation: WebRepresentation::TextV1,
    };

    cache.insert(first.clone(), std::iter::empty(), value.clone(), CacheValidators::default());
    cache.entries.front_mut().unwrap().fresh_until =
        Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    assert!(cache.get(&first).is_some_and(|lookup| lookup.stale));

    cache.insert(second.clone(), std::iter::empty(), value, CacheValidators::default());
    assert!(cache.get(&first).is_none());
    assert!(cache.get(&second).is_some());
}

#[tokio::test]
async fn stale_fetch_revalidates_with_safe_validators_and_reuses_cached_text_on_not_modified() {
    let transport = FakeTransport::new(
        vec![public_ip()],
        vec![
            response(
                200,
                &[("content-type", "text/plain"), ("etag", "\"source-v1\"")],
                b"documentation",
            ),
            response(304, &[], b""),
        ],
    );
    let service = WebContextService::new(config(), transport).unwrap();
    let request = WebFetchRequest { url: "https://docs.example/reference".to_owned() };

    let first = service.fetch(request.clone()).await.unwrap();
    service.cache.lock().unwrap().entries.front_mut().unwrap().fresh_until =
        Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    let second = service.fetch(request).await.unwrap();

    assert!(!first.cached);
    assert!(second.cached);
    assert_eq!(second.text, "documentation");
    assert_eq!(first.retrieved_at_unix_ms, second.retrieved_at_unix_ms);
    let requests = service.transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].headers.get("if-none-match"), Some(&"\"source-v1\"".to_owned()));
    assert!(!requests[1].headers.contains_key("authorization"));
}

#[tokio::test]
async fn missing_search_backend_and_unapproved_hosts_fail_before_transport() {
    let mut missing_backend = config();
    missing_backend.search_endpoint = None;
    let search =
        WebContextService::new(missing_backend, FakeTransport::new(vec![public_ip()], Vec::new()))
            .unwrap();
    assert_eq!(
        search.search(WebSearchRequest { query: "rust docs".to_owned() }).await.unwrap_err().code,
        WebContextErrorCode::WebSearchUnavailable
    );
    assert!(search.transport.requests.lock().unwrap().is_empty());

    let fetch = WebContextService::new(config(), FakeTransport::new(vec![public_ip()], Vec::new()))
        .unwrap();
    assert_eq!(
        fetch
            .fetch(WebFetchRequest { url: "https://unapproved.example/".to_owned() })
            .await
            .unwrap_err()
            .code,
        WebContextErrorCode::NetworkApprovalRequired
    );
    assert!(fetch.transport.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn private_dns_and_truncated_decompressed_bodies_fail_closed() {
    let private_dns = WebContextService::new(
        config(),
        FakeTransport::new(vec!["127.0.0.1".parse().unwrap()], Vec::new()),
    )
    .unwrap();
    assert_eq!(
        private_dns
            .fetch(WebFetchRequest { url: "https://docs.example/".to_owned() })
            .await
            .unwrap_err()
            .code,
        WebContextErrorCode::DnsRejected
    );
    assert!(private_dns.transport.requests.lock().unwrap().is_empty());

    let mut capped = response(200, &[("content-type", "text/plain")], b"partial");
    capped.body_truncated = true;
    let oversized =
        WebContextService::new(config(), FakeTransport::new(vec![public_ip()], vec![capped]))
            .unwrap();
    assert_eq!(
        oversized
            .fetch(WebFetchRequest { url: "https://docs.example/".to_owned() })
            .await
            .unwrap_err()
            .code,
        WebContextErrorCode::ResponseTooLarge
    );
    assert!(oversized.cache.lock().unwrap().entries.is_empty());
}

#[tokio::test]
async fn malformed_search_json_and_binary_fetch_are_rejected_without_cache_entries() {
    let search = WebContextService::new(
        config(),
        FakeTransport::new(
            vec![public_ip()],
            vec![response(200, &[("content-type", "application/json")], b"{not-json")],
        ),
    )
    .unwrap();
    assert_eq!(
        search.search(WebSearchRequest { query: "rust docs".to_owned() }).await.unwrap_err().code,
        WebContextErrorCode::NetworkFailure
    );
    assert!(search.cache.lock().unwrap().entries.is_empty());

    let fetch = WebContextService::new(
        config(),
        FakeTransport::new(
            vec![public_ip()],
            vec![response(200, &[("content-type", "application/octet-stream")], b"binary")],
        ),
    )
    .unwrap();
    assert_eq!(
        fetch
            .fetch(WebFetchRequest { url: "https://docs.example/download".to_owned() })
            .await
            .unwrap_err()
            .code,
        WebContextErrorCode::UnsupportedContentType
    );
    assert!(fetch.cache.lock().unwrap().entries.is_empty());
}

#[tokio::test]
async fn redirects_stop_at_configured_cap_without_forwarding_search_credentials() {
    let mut limited = config()
        .with_search_authorization(zeroize::Zeroizing::new(String::from("provider-secret")));
    limited.limits.max_redirects = 1;
    let service = WebContextService::new(
        limited,
        FakeTransport::new(
            vec![public_ip()],
            vec![
                response(302, &[("location", "https://search.example/next")], b""),
                response(302, &[("location", "https://search.example/final")], b""),
            ],
        ),
    )
    .unwrap();

    assert_eq!(
        service.search(WebSearchRequest { query: "rust docs".to_owned() }).await.unwrap_err().code,
        WebContextErrorCode::RedirectRejected
    );
    let requests = service.transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].headers.contains_key("authorization"));
    assert!(!requests[1].headers.contains_key("authorization"));
    assert!(!requests.iter().any(|request| request.headers.contains_key("cookie")));
}

#[tokio::test]
async fn configured_search_endpoint_rejects_query_credentials_and_debug_redacts_them() {
    let mut invalid = config();
    invalid.search_endpoint =
        Some("https://search.example/search?api_key=provider-secret".to_owned());

    assert!(!format!("{invalid:?}").contains("provider-secret"));
    assert!(matches!(
        WebContextService::new(invalid, FakeTransport::new(vec![public_ip()], Vec::new())),
        Err(WebContextConfigError::SearchEndpoint)
    ));
}

#[tokio::test]
async fn query_bearing_urls_are_requested_but_never_returned_or_cached() {
    let secret = "signed-url-secret";
    let service = WebContextService::new(
        config(),
        FakeTransport::new(
            vec![public_ip()],
            vec![response(200, &[("content-type", "text/plain")], b"documentation")],
        ),
    )
    .unwrap();
    let fetched = service
        .fetch(WebFetchRequest { url: format!("https://docs.example/reference?token={secret}") })
        .await
        .unwrap();

    assert_eq!(fetched.requested_url, "https://docs.example/reference");
    assert_eq!(fetched.final_url, "https://docs.example/reference");
    assert!(!format!("{fetched:?}").contains(secret));
    assert!(!format!("{:?}", service.cache.lock().unwrap()).contains(secret));
    assert_eq!(
        service.transport.requests.lock().unwrap()[0].url.query(),
        Some("token=signed-url-secret")
    );
}

#[tokio::test]
async fn search_cache_hashes_query_and_redacts_result_url_queries() {
    let secret = "search-query-secret";
    let service = WebContextService::new(
        config(),
        FakeTransport::new(
            vec![public_ip()],
            vec![response(
                200,
                &[("content-type", "application/json")],
                br#"{"results":[{"title":"Docs","url":"https://docs.example/reference?token=result-url-secret","content":"Reference"}]}"#,
            )],
        ),
    )
    .unwrap();

    let first = service.search(WebSearchRequest { query: secret.to_owned() }).await.unwrap();
    let second = service.search(WebSearchRequest { query: secret.to_owned() }).await.unwrap();

    assert_eq!(first.results[0].url, "https://docs.example/reference");
    assert!(second.cached);
    let cache = format!("{:?}", service.cache.lock().unwrap());
    assert!(!cache.contains(secret));
    assert!(!cache.contains("result-url-secret"));
}

#[tokio::test]
async fn browser_run_uses_fixed_cloudflare_endpoint_and_redacts_target_query() {
    let mut browser_config = config();
    browser_config.browser_run_account_id = Some(String::from("0123456789abcdef0123456789abcdef"));
    browser_config.browser_run_api_token_reference = Some(String::from("secret://browser-run"));
    browser_config =
        browser_config.with_browser_run_api_token(Zeroizing::new(String::from("token-value")));
    let service = WebContextService::new(
        browser_config,
        FakeTransport::new(
            vec![public_ip()],
            vec![response(
                200,
                &[("content-type", "application/json")],
                br##"{"success":true,"result":"# Rendered"}"##,
            )],
        ),
    )
    .unwrap();
    let result = service
        .browser_run_with_approved_hosts_and_cancellation(
            ee_mcp::BrowserRunRequest {
                action: ee_mcp::BrowserRunAction::Markdown,
                url: String::from("https://docs.example/page?secret=query-value"),
                selector: None,
                prompt: None,
            },
            &BTreeSet::new(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(result.requested_url, "https://docs.example/page");
    assert_eq!(result.result, serde_json::Value::String(String::from("# Rendered")));
    let request = &service.transport.requests.lock().unwrap()[0];
    assert_eq!(request.url.host_str(), Some("api.cloudflare.com"));
    assert_eq!(
        request.url.path(),
        "/client/v4/accounts/0123456789abcdef0123456789abcdef/browser-rendering/markdown"
    );
    assert_eq!(request.headers["authorization"], "Bearer token-value");
    assert!(
        std::str::from_utf8(&request.body)
            .unwrap()
            .contains("docs.example/page?secret=query-value")
    );
    assert!(!format!("{:?}", service.config).contains("token-value"));
}

#[tokio::test]
async fn browser_run_retries_transient_cloudflare_rate_limit_with_capped_retry_after() {
    let mut browser_config = config();
    browser_config.browser_run_account_id = Some(String::from("0123456789abcdef0123456789abcdef"));
    browser_config.browser_run_api_token_reference = Some(String::from("secret://browser-run"));
    browser_config.browser_run_retry =
        BrowserRunRetryPolicy { max_attempts: 2, base_delay_ms: 1, max_delay_ms: 1 };
    browser_config =
        browser_config.with_browser_run_api_token(Zeroizing::new(String::from("token-value")));
    let service = WebContextService::new(
        browser_config,
        FakeTransport::new(
            vec![public_ip()],
            vec![
                response(429, &[("retry-after", "0")], br#"{"success":false}"#),
                response(
                    200,
                    &[("content-type", "application/json")],
                    br##"{"success":true,"result":"retried"}"##,
                ),
            ],
        ),
    )
    .unwrap();
    let result = service
        .browser_run_with_approved_hosts_and_cancellation(
            ee_mcp::BrowserRunRequest {
                action: ee_mcp::BrowserRunAction::Markdown,
                url: String::from("https://docs.example/page"),
                selector: None,
                prompt: None,
            },
            &BTreeSet::new(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(result.result, serde_json::Value::String(String::from("retried")));
    assert_eq!(service.transport.requests.lock().unwrap().len(), 2);
}
