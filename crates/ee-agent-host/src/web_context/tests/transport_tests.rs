//! Fetch/search transport and approval tests.
use super::html::extract_html_text;
use super::html::is_html_mime;
use super::html::is_text_mime;
use super::*;

#[tokio::test]
async fn cancellation_during_dns_prevents_cache_insert() {
    assert_cancellation_prevents_cache_insert(CancellationPhase::Dns).await;
}

#[tokio::test]
async fn cancellation_during_request_prevents_cache_insert() {
    assert_cancellation_prevents_cache_insert(CancellationPhase::Request).await;
}

#[tokio::test]
async fn cancellation_during_body_prevents_cache_insert() {
    assert_cancellation_prevents_cache_insert(CancellationPhase::Body).await;
}

#[tokio::test]
async fn default_config_enables_fetch_without_search_provider() {
    let config = AgentWebContextConfig::default();
    assert!(config.enabled);

    let service =
        WebContextService::new(config, FakeTransport::new(vec![public_ip()], Vec::new())).unwrap();
    assert_eq!(
        service.search_initial_host().unwrap_err().code,
        WebContextErrorCode::WebSearchUnavailable
    );
    assert_eq!(
        service
            .fetch(WebFetchRequest { url: "https://docs.example/".to_owned() })
            .await
            .unwrap_err()
            .code,
        WebContextErrorCode::NetworkApprovalRequired
    );
}

#[tokio::test]
async fn preapproved_host_lookup_canonicalizes_and_rejects_invalid_hosts() {
    let service =
        WebContextService::new(config(), FakeTransport::new(Vec::new(), Vec::new())).unwrap();

    assert!(service.is_preapproved_host("DOCS.Example"));
    for invalid_host in ["", "https://docs.example/", "docs.example/path", "user@docs.example"] {
        assert!(!service.is_preapproved_host(invalid_host), "{invalid_host}");
    }
}

#[tokio::test]
async fn error_codes_are_stable() {
    assert_eq!(WebContextErrorCode::WebDisabled.as_str(), "web_disabled");
    assert_eq!(
        serde_json::to_string(&WebContextErrorCode::NetworkApprovalRequired).unwrap(),
        "\"network_approval_required\""
    );
}

#[tokio::test]
async fn strict_url_validation_rejects_unsafe_targets() {
    for url in [
        "http://example.com/",
        "file:///tmp/secret",
        "data:text/plain,hello",
        "javascript:alert(1)",
        "https://user@example.com/",
        "https://example.com/#fragment",
        "https://example.com:8443/",
        "https://",
    ] {
        assert_eq!(
            validate_https_url(url).unwrap_err().code,
            WebContextErrorCode::UrlRejected,
            "{url}"
        );
    }
    assert_eq!(validate_https_url("https://example.com:443/path").unwrap().scheme(), "https");
}

#[tokio::test]
async fn public_ip_validation_fails_closed() {
    for address in [
        "127.0.0.1",
        "10.0.0.1",
        "100.64.0.1",
        "169.254.1.1",
        "192.168.1.1",
        "198.51.100.1",
        "::1",
        "fc00::1",
        "fe80::1",
        "ff00::1",
        "2001:db8::1",
    ] {
        assert!(!is_public_ip(address.parse().unwrap()), "{address}");
    }
    assert!(is_public_ip("8.8.8.8".parse().unwrap()));
    assert!(is_public_ip("2606:4700:4700::1111".parse().unwrap()));
}

#[tokio::test]
async fn text_mime_validation_is_explicit() {
    assert!(is_text_mime(Some("application/problem+json; charset=utf-8")));
    assert!(is_text_mime(Some("text/markdown")));
    assert!(!is_text_mime(Some("application/octet-stream")));
    assert!(!is_text_mime(None));
}

#[tokio::test]
async fn searxng_results_are_bounded_normalized_and_deduplicated() {
    let body = br#"{
        "results": [
            {"title":" First\n result ","url":"https://docs.example/a","content":" one\t two "},
            {"title":"duplicate","url":"https://docs.example/a","content":"ignored"},
            {"title":"unsafe","url":"http://127.0.0.1/","content":"ignored"},
            {"title":"second","url":"https://docs.example/b","content":"kept"}
        ]
    }"#;
    let results = parse_searxng_json(body, 4096, 2).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].title, "First result");
    assert_eq!(results[0].snippet, "one two");
    assert_eq!(results[0].rank, 1);
    assert_eq!(results[1].url, "https://docs.example/b");
    assert_eq!(results[1].rank, 2);
}

#[tokio::test]
async fn disabled_service_does_not_make_network_request() {
    let transport = FakeTransport::new(vec![public_ip()], Vec::new());
    let config = AgentWebContextConfig { enabled: false, ..Default::default() };
    let service = WebContextService::new(config, transport).unwrap();
    assert_eq!(
        service
            .fetch(WebFetchRequest { url: "https://docs.example/".to_owned() })
            .await
            .unwrap_err()
            .code,
        WebContextErrorCode::WebDisabled
    );
}

#[tokio::test]
async fn fetch_revalidates_connected_peer_against_dns() {
    let mut network_response = response(200, &[("content-type", "text/plain")], b"ok");
    network_response.connected_peer = "1.1.1.1".parse().unwrap();
    let transport = FakeTransport::new(vec![public_ip()], vec![network_response]);
    let service = WebContextService::new(config(), transport).unwrap();
    assert_eq!(
        service
            .fetch(WebFetchRequest { url: "https://docs.example/".to_owned() })
            .await
            .unwrap_err()
            .code,
        WebContextErrorCode::DnsRejected
    );
}

#[tokio::test]
async fn redirect_to_unapproved_host_requires_new_approval() {
    let transport = FakeTransport::new(
        vec![public_ip()],
        vec![response(302, &[("location", "https://unapproved.example/next")], b"")],
    );
    let service = WebContextService::new(config(), transport).unwrap();
    assert_eq!(
        service
            .fetch(WebFetchRequest { url: "https://docs.example/".to_owned() })
            .await
            .unwrap_err()
            .code,
        WebContextErrorCode::NetworkApprovalRequired
    );
}

#[tokio::test]
async fn search_accepts_ephemeral_initial_host_without_mutating_config() {
    let mut config = config();
    config.preapproved_hosts.remove("search.example");
    let transport = FakeTransport::new(
        vec![public_ip()],
        vec![response(200, &[("content-type", "application/json")], br#"{"results":[]}"#)],
    );
    let service = WebContextService::new(config, transport).unwrap();
    let approved_hosts = BTreeSet::from(["SEARCH.EXAMPLE".to_owned()]);

    assert_eq!(service.search_initial_host().unwrap(), "search.example");
    service
        .search_with_approved_hosts(
            WebSearchRequest { query: "widget api".to_owned() },
            &approved_hosts,
        )
        .await
        .unwrap();

    assert!(!service.config.preapproved_hosts.contains("search.example"));
    assert_eq!(service.transport.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn ephemeral_redirect_requires_separate_host_approval() {
    let mut config = config();
    config.preapproved_hosts.remove("docs.example");
    let transport = FakeTransport::new(
        vec![public_ip()],
        vec![response(
            302,
            &[("location", "https://Unapproved.Example/next?token=provider-secret")],
            b"",
        )],
    );
    let service = WebContextService::new(config, transport).unwrap();
    let approved_hosts = BTreeSet::from(["docs.example".to_owned()]);
    let request = WebFetchRequest { url: "https://docs.example/".to_owned() };

    assert_eq!(service.fetch_initial_host(&request).unwrap(), "docs.example");
    let error = service.fetch_with_approved_hosts(request, &approved_hosts).await.unwrap_err();

    assert_eq!(error.code, WebContextErrorCode::NetworkApprovalRequired);
    assert_eq!(error.host.as_deref(), Some("unapproved.example"));
    assert!(!serde_json::to_string(&error).unwrap().contains("next?token=provider-secret"));
    assert_eq!(service.transport.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn html_fetch_strips_active_and_hidden_content() {
    let transport = FakeTransport::new(
        vec![public_ip()],
        vec![response(
            200,
            &[("content-type", "text/html; charset=utf-8")],
            br#"<html><head><title> Docs title </title><style>secret-css</style></head><body><script>ignore()</script><form>secret form</form><p>Useful docs</p><div hidden>private</div><div aria-hidden='true'>also private</div></body></html>"#,
        )],
    );
    let service = WebContextService::new(config(), transport).unwrap();
    let fetched =
        service.fetch(WebFetchRequest { url: "https://docs.example/".to_owned() }).await.unwrap();
    assert_eq!(fetched.title.as_deref(), Some("Docs title"));
    assert!(fetched.text.contains("Useful docs"));
    for omitted in ["ignore", "secret-css", "secret form", "private", "also private"] {
        assert!(!fetched.text.contains(omitted), "{omitted}");
    }
}

#[tokio::test]
async fn html_tokenizer_drops_interactive_css_hidden_and_xhtml_content() {
    let (title, text) = extract_html_text(
        "<html><head><title>Visible title</title></head><body>visible <div style='display: none'>css hidden</div><textarea>typed secret</textarea><select><option>choice secret</option></select><button>button secret</button><details>details secret</details><div inert>inert secret</div></body></html>",
    );
    assert_eq!(title.as_deref(), Some("Visible title"));
    assert!(text.contains("visible"));
    for omitted in [
        "css hidden",
        "typed secret",
        "choice secret",
        "button secret",
        "details secret",
        "inert secret",
    ] {
        assert!(!text.contains(omitted), "{omitted}");
    }
    assert!(is_html_mime("application/xhtml+xml; charset=utf-8"));
}

#[tokio::test]
async fn fetch_caps_title_and_body_within_configured_text_budget() {
    let mut limited = config();
    limited.limits.max_text_bytes = 12;
    let transport = FakeTransport::new(
        vec![public_ip()],
        vec![response(
            200,
            &[("content-type", "text/html")],
            b"<title>1234567890</title><p>abcdefghij</p>",
        )],
    );
    let service = WebContextService::new(limited, transport).unwrap();
    let fetched =
        service.fetch(WebFetchRequest { url: "https://docs.example/".to_owned() }).await.unwrap();
    assert!(fetched.title.as_ref().map_or(0, String::len) + fetched.text.len() <= 12);
    assert!(fetched.truncated);
}

#[tokio::test]
async fn search_uses_fixed_headers_and_parses_json() {
    let transport = FakeTransport::new(
        vec![public_ip()],
        vec![response(
            200,
            &[("content-type", "application/json")],
            br#"{"results":[{"title":"Docs","url":"https://docs.example/","content":"Reference"}]}"#,
        )],
    );
    let service = WebContextService::new(config(), transport).unwrap();
    let results =
        service.search(WebSearchRequest { query: "widget api".to_owned() }).await.unwrap();
    assert!(!results.cached);
    assert_eq!(results.results[0].host, "docs.example");
    let request = service.transport.requests.lock().unwrap().remove(0);
    assert_eq!(request.headers.get("accept").unwrap(), "text/plain, text/html, application/json");
    assert!(request.url.query().unwrap().contains("format=json"));
    assert!(request.url.query().unwrap().contains("q=widget+api"));
}

#[tokio::test]
async fn search_authorization_is_redacted_and_only_sent_to_initial_search_request() {
    let config = config()
        .with_search_authorization(zeroize::Zeroizing::new(String::from("provider-secret")));
    assert!(!format!("{config:?}").contains("provider-secret"));
    let transport = FakeTransport::new(
        vec![public_ip()],
        vec![
            response(302, &[("location", "https://docs.example/results")], b""),
            response(200, &[("content-type", "application/json")], br#"{"results":[]}"#),
        ],
    );
    let service = WebContextService::new(config, transport).unwrap();

    service.search(WebSearchRequest { query: "widget api".to_owned() }).await.unwrap();

    let requests = service.transport.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].headers.get("authorization").map(String::as_str),
        Some("Bearer provider-secret")
    );
    assert!(!requests[1].headers.contains_key("authorization"));
    assert!(!format!("{requests:?}").contains("provider-secret"));
    assert!(!format!("{:?}", service.cache.lock().unwrap()).contains("provider-secret"));
}

#[tokio::test]
async fn search_authorization_is_never_sent_to_fetch() {
    let transport = FakeTransport::new(
        vec![public_ip()],
        vec![response(200, &[("content-type", "text/plain")], b"ok")],
    );
    let service = WebContextService::new(
        config()
            .with_search_authorization(zeroize::Zeroizing::new(String::from("provider-secret"))),
        transport,
    )
    .unwrap();

    service.fetch(WebFetchRequest { url: "https://docs.example/".to_owned() }).await.unwrap();

    let request = service.transport.requests.lock().unwrap().remove(0);
    assert!(!request.headers.contains_key("authorization"));
    assert!(!format!("{request:?}").contains("provider-secret"));
}
