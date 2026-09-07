//! Vendor adapter and provider-parsing tests.
use super::cache::WebCacheKey;
use super::cache::provider_options_digest;
use super::*;

#[test]
fn vendor_provider_config_uses_fixed_origin_and_requires_matching_options_and_secret() {
    let mut exa = config();
    exa.provider = WebSearchProvider::Exa;
    exa.provider_options = WebSearchProviderOptions::Exa(ExaSearchOptions::default());
    exa.search_endpoint = None;
    let missing_secret =
        WebContextService::new(exa.clone(), FakeTransport::new(vec![public_ip()], Vec::new()));
    assert!(matches!(missing_secret, Err(WebContextConfigError::ProviderAuthorization)));

    let blank_secret = WebContextService::new(
        exa.clone().with_search_authorization(Zeroizing::new(String::from(" \t "))),
        FakeTransport::new(vec![public_ip()], Vec::new()),
    );
    assert!(matches!(blank_secret, Err(WebContextConfigError::ProviderAuthorization)));

    let service = WebContextService::new(
        exa.with_search_authorization(Zeroizing::new(String::from("provider-secret"))),
        FakeTransport::new(vec![public_ip()], Vec::new()),
    )
    .unwrap();
    assert_eq!(service.search_initial_host().unwrap(), "api.exa.ai");
    assert_eq!(service.search_endpoint_url().unwrap().as_str(), EXA_SEARCH_ENDPOINT);

    let mut endpoint_mismatch = config();
    endpoint_mismatch.provider = WebSearchProvider::Tavily;
    endpoint_mismatch.provider_options =
        WebSearchProviderOptions::Tavily(TavilySearchOptions::default());
    assert!(matches!(
        WebContextService::new(
            endpoint_mismatch,
            FakeTransport::new(vec![public_ip()], Vec::new())
        ),
        Err(WebContextConfigError::ProviderEndpoint)
    ));

    let mut options_mismatch = config();
    options_mismatch.provider = WebSearchProvider::Tavily;
    options_mismatch.search_endpoint = None;
    options_mismatch =
        options_mismatch.with_search_authorization(Zeroizing::new(String::from("provider-secret")));
    assert!(matches!(
        WebContextService::new(options_mismatch, FakeTransport::new(vec![public_ip()], Vec::new())),
        Err(WebContextConfigError::ProviderOptions)
    ));
}

#[tokio::test]
async fn disabled_vendor_service_needs_no_secret_and_never_dispatches() {
    let mut disabled = config();
    disabled.enabled = false;
    disabled.provider = WebSearchProvider::Exa;
    disabled.provider_options = WebSearchProviderOptions::Exa(ExaSearchOptions::default());
    disabled.search_endpoint = None;
    disabled.preapproved_hosts = BTreeSet::from([String::from("api.exa.ai")]);
    let service =
        WebContextService::new(disabled, FakeTransport::new(vec![public_ip()], Vec::new()))
            .unwrap();

    assert_eq!(
        service
            .search(WebSearchRequest { query: String::from("must stay offline") })
            .await
            .unwrap_err()
            .code,
        WebContextErrorCode::WebDisabled
    );
    assert!(service.transport.requests.lock().unwrap().is_empty());
}

fn vendor_config(
    provider: WebSearchProvider,
    provider_options: WebSearchProviderOptions,
    host: &str,
) -> AgentWebContextConfig {
    let mut config = config();
    config.provider = provider;
    config.provider_options = provider_options;
    config.search_endpoint = None;
    config.preapproved_hosts = BTreeSet::from([host.to_owned(), "docs.example".to_owned()]);
    config.with_search_authorization(Zeroizing::new(String::from("provider-secret")))
}

fn vendor_profiles() -> [(WebSearchProvider, WebSearchProviderOptions, &'static str); 3] {
    [
        (
            WebSearchProvider::Exa,
            WebSearchProviderOptions::Exa(ExaSearchOptions::default()),
            "api.exa.ai",
        ),
        (
            WebSearchProvider::Tavily,
            WebSearchProviderOptions::Tavily(TavilySearchOptions::default()),
            "api.tavily.com",
        ),
        (
            WebSearchProvider::BraveLlmContext,
            WebSearchProviderOptions::BraveLlmContext(BraveLlmContextOptions::default()),
            "api.search.brave.com",
        ),
    ]
}

fn malformed_vendor_body(provider: WebSearchProvider) -> &'static [u8] {
    match provider {
        WebSearchProvider::Exa => br#"{"results":[{"title":"missing fields"}]}"#,
        WebSearchProvider::Tavily => br#"{"results":[{"title":"missing fields"}]}"#,
        WebSearchProvider::BraveLlmContext => {
            br#"{"grounding":{"generic":[{"url":"https://docs.example/"}]}}"#
        }
        WebSearchProvider::Searxng => unreachable!("vendor fixture only"),
    }
}

fn invalid_url_vendor_body(provider: WebSearchProvider) -> &'static [u8] {
    match provider {
        WebSearchProvider::Exa => {
            br#"{"results":[{"title":"bad","url":"http://docs.example/","highlights":["snippet"]}]}"#
        }
        WebSearchProvider::Tavily => {
            br#"{"results":[{"title":"bad","url":"http://docs.example/","content":"snippet"}]}"#
        }
        WebSearchProvider::BraveLlmContext => {
            br#"{"grounding":{"generic":[{"url":"http://docs.example/","snippets":["snippet"]}]}}"#
        }
        WebSearchProvider::Searxng => unreachable!("vendor fixture only"),
    }
}

#[tokio::test]
async fn vendor_search_requests_are_posted_to_fixed_origins_and_parse_results() {
    let exa = WebContextService::new(
        vendor_config(
            WebSearchProvider::Exa,
            WebSearchProviderOptions::Exa(ExaSearchOptions::default()),
            "api.exa.ai",
        ),
        FakeTransport::new(
            vec![public_ip()],
            vec![response(
                200,
                &[("content-type", "application/json")],
                br#"{"results":[{"title":" Exa docs ","url":"https://docs.example/exa?token=result-secret","highlights":[" first highlight ","ignored"]},{"title":"duplicate","url":"https://docs.example/exa","highlights":["ignored"]}]}"#,
            )],
        ),
    )
    .unwrap();
    let result = exa.search(WebSearchRequest { query: "rust api".to_owned() }).await.unwrap();
    assert_eq!(result.results.len(), 1);
    assert_eq!(result.results[0].title, "Exa docs");
    assert_eq!(result.results[0].url, "https://docs.example/exa");
    assert_eq!(result.results[0].snippet, "first highlight");
    assert_eq!(result.provenance.provider, WebSearchProvider::Exa);
    assert_eq!(result.provenance.adapter, PROVIDER_ADAPTER_VERSION);
    assert!(result.provenance.retrieved_at_unix_ms > 0);
    let request = exa.transport.requests.lock().unwrap().remove(0);
    assert_eq!(request.method, WebTransportMethod::Post);
    assert_eq!(request.url.as_str(), EXA_SEARCH_ENDPOINT);
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer provider-secret")
    );
    assert_eq!(request.headers.get("content-type").map(String::as_str), Some("application/json"));
    assert_eq!(request.headers.len(), 4);
    assert!(!request.headers.contains_key("cookie"));
    assert!(!request.headers.contains_key("proxy-authorization"));
    assert!(!request.headers.contains_key("x-subscription-token"));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&request.body).unwrap(),
        serde_json::json!({"query":"rust api","type":"auto","numResults":10,"contents":{"highlights":true}})
    );
    assert!(!format!("{request:?}").contains("provider-secret"));
    assert!(!format!("{request:?}").contains("rust api"));
}

#[tokio::test]
async fn tavily_uses_bearer_post_and_ignores_answer() {
    let tavily = WebContextService::new(
        vendor_config(
            WebSearchProvider::Tavily,
            WebSearchProviderOptions::Tavily(TavilySearchOptions::default()),
            "api.tavily.com",
        ),
        FakeTransport::new(
            vec![public_ip()],
            vec![response(
                200,
                &[("content-type", "application/json")],
                br#"{"answer":"do not use","results":[{"title":"Tavily docs","url":"https://docs.example/tavily","content":"source content","raw_content":"raw content"}]}"#,
            )],
        ),
    )
    .unwrap();
    let result = tavily.search(WebSearchRequest { query: "tavily api".to_owned() }).await.unwrap();
    assert_eq!(result.results[0].snippet, "source content");
    let request = tavily.transport.requests.lock().unwrap().remove(0);
    assert_eq!(request.method, WebTransportMethod::Post);
    assert_eq!(request.url.as_str(), TAVILY_SEARCH_ENDPOINT);
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer provider-secret")
    );
    assert_eq!(request.headers.get("content-type").map(String::as_str), Some("application/json"));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&request.body).unwrap(),
        serde_json::json!({"query":"tavily api","search_depth":"advanced","chunks_per_source":3,"max_results":10})
    );
    assert_eq!(request.headers.len(), 4);
    assert!(!request.headers.contains_key("cookie"));
    assert!(!request.headers.contains_key("proxy-authorization"));
    assert!(!request.headers.contains_key("x-subscription-token"));
}

#[tokio::test]
async fn brave_uses_subscription_token_and_empty_grounding_succeeds() {
    let brave = WebContextService::new(
        vendor_config(
            WebSearchProvider::BraveLlmContext,
            WebSearchProviderOptions::BraveLlmContext(BraveLlmContextOptions::default()),
            "api.search.brave.com",
        ),
        FakeTransport::new(
            vec![public_ip()],
            vec![response(
                200,
                &[("content-type", "application/json")],
                br#"{"grounding":{"generic":[{"url":"https://docs.example/brave","snippets":["first","second"]},{"url":"https://docs.example/brave","snippets":["duplicate"]}]},"sources":[{"url":"https://docs.example/brave","title":"Brave docs"}]}"#,
            )],
        ),
    )
    .unwrap();
    let result =
        brave.search(WebSearchRequest { query: "brave context".to_owned() }).await.unwrap();
    assert_eq!(result.results.len(), 1);
    assert_eq!(result.results[0].title, "Brave docs");
    assert_eq!(result.results[0].snippet, "first second");
    let request = brave.transport.requests.lock().unwrap().remove(0);
    assert_eq!(request.method, WebTransportMethod::Post);
    assert_eq!(request.url.as_str(), BRAVE_LLM_CONTEXT_ENDPOINT);
    assert_eq!(
        request.headers.get("x-subscription-token").map(String::as_str),
        Some("provider-secret")
    );
    assert!(!request.headers.contains_key("authorization"));
    assert_eq!(request.headers.get("content-type").map(String::as_str), Some("application/json"));
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["q"], "brave context");
    assert_eq!(body["enable_local"], false);
    assert_eq!(body["relevance_threshold"], 0.5);
    assert_eq!(body["freshness"], "all");
    assert_eq!(body["safesearch"], "moderate");
    assert_eq!(body["maximum_number_of_tokens_per_url"], 400);
    assert_eq!(body["maximum_number_of_snippets_per_url"], 1);
    assert_eq!(
        body.as_object().unwrap().keys().map(String::as_str).collect::<BTreeSet<_>>(),
        [
            "count",
            "enable_local",
            "freshness",
            "maximum_number_of_snippets",
            "maximum_number_of_snippets_per_url",
            "maximum_number_of_tokens",
            "maximum_number_of_tokens_per_url",
            "maximum_number_of_urls",
            "q",
            "relevance_threshold",
            "safesearch",
        ]
        .into_iter()
        .collect()
    );
    assert_eq!(request.headers.len(), 4);
    assert!(!request.headers.contains_key("cookie"));
    assert!(!request.headers.contains_key("proxy-authorization"));
    assert!(!request.headers.contains_key("authorization"));

    let empty = WebContextService::new(
        vendor_config(
            WebSearchProvider::BraveLlmContext,
            WebSearchProviderOptions::BraveLlmContext(BraveLlmContextOptions::default()),
            "api.search.brave.com",
        ),
        FakeTransport::new(
            vec![public_ip()],
            vec![response(200, &[("content-type", "application/json")], br#"{"grounding":{}}"#)],
        ),
    )
    .unwrap();
    assert!(
        empty
            .search(WebSearchRequest { query: "empty".to_owned() })
            .await
            .unwrap()
            .results
            .is_empty()
    );
}

#[tokio::test]
async fn vendor_redirect_is_rejected_without_second_credentialed_request() {
    let exa = WebContextService::new(
        vendor_config(
            WebSearchProvider::Exa,
            WebSearchProviderOptions::Exa(ExaSearchOptions::default()),
            "api.exa.ai",
        ),
        FakeTransport::new(
            vec![public_ip()],
            vec![
                response(302, &[("location", "https://docs.example/redirect")], b""),
                response(200, &[("content-type", "application/json")], br#"{"results":[]}"#),
            ],
        ),
    )
    .unwrap();
    assert_eq!(
        exa.search(WebSearchRequest { query: "no redirect".to_owned() }).await.unwrap_err().code,
        WebContextErrorCode::RedirectRejected
    );
    let requests = exa.transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].headers.get("authorization").map(String::as_str),
        Some("Bearer provider-secret")
    );
}

#[tokio::test]
async fn vendor_failure_is_explicit_and_never_falls_back() {
    let exa = WebContextService::new(
        vendor_config(
            WebSearchProvider::Exa,
            WebSearchProviderOptions::Exa(ExaSearchOptions::default()),
            "api.exa.ai",
        ),
        FakeTransport::new(
            vec![public_ip()],
            vec![response(429, &[("content-type", "application/json")], br#"{}"#)],
        ),
    )
    .unwrap();

    assert_eq!(
        exa.search(WebSearchRequest { query: "rate limited".to_owned() }).await.unwrap_err().code,
        WebContextErrorCode::NetworkFailure
    );
    assert_eq!(exa.transport.requests.lock().unwrap().len(), 1);
    assert!(exa.cache.lock().unwrap().entries.is_empty());
}

#[tokio::test]
async fn every_vendor_fails_closed_without_cache_or_fallback() {
    for (provider, options, host) in vendor_profiles() {
        let mut peer_mismatch = response(200, &[("content-type", "application/json")], b"{}");
        peer_mismatch.connected_peer = "1.1.1.1".parse().unwrap();
        let mut oversized = response(200, &[("content-type", "application/json")], b"{}");
        oversized.body_truncated = true;
        let scenarios = vec![
            (
                "unauthorized",
                response(401, &[("content-type", "application/json")], b"{}"),
                WebContextErrorCode::NetworkFailure,
            ),
            (
                "rate limited",
                response(429, &[("content-type", "application/json")], b"{}"),
                WebContextErrorCode::NetworkFailure,
            ),
            (
                "server failure",
                response(500, &[("content-type", "application/json")], b"{}"),
                WebContextErrorCode::NetworkFailure,
            ),
            (
                "redirect",
                response(302, &[("location", "https://docs.example/redirect")], b""),
                WebContextErrorCode::RedirectRejected,
            ),
            (
                "wrong MIME",
                response(200, &[("content-type", "text/plain")], b"{}"),
                WebContextErrorCode::UnsupportedContentType,
            ),
            ("oversized body", oversized, WebContextErrorCode::ResponseTooLarge),
            (
                "malformed JSON",
                response(200, &[("content-type", "application/json")], b"{"),
                WebContextErrorCode::NetworkFailure,
            ),
            (
                "malformed result",
                response(
                    200,
                    &[("content-type", "application/json")],
                    malformed_vendor_body(provider),
                ),
                WebContextErrorCode::NetworkFailure,
            ),
            (
                "cross-scheme result",
                response(
                    200,
                    &[("content-type", "application/json")],
                    invalid_url_vendor_body(provider),
                ),
                WebContextErrorCode::NetworkFailure,
            ),
            ("connected peer mismatch", peer_mismatch, WebContextErrorCode::DnsRejected),
        ];

        for (name, transport_response, expected) in scenarios {
            let service = WebContextService::new(
                vendor_config(provider, options.clone(), host),
                FakeTransport::new(vec![public_ip()], vec![transport_response]),
            )
            .unwrap();

            let error = service
                .search(WebSearchRequest { query: format!("{name} query") })
                .await
                .unwrap_err();
            assert_eq!(error.code, expected, "{}: {name}", provider.id());
            assert_eq!(
                service.transport.requests.lock().unwrap().len(),
                1,
                "{}: {name}",
                provider.id()
            );
            assert!(service.cache.lock().unwrap().entries.is_empty(), "{}: {name}", provider.id());
        }

        let private_dns = WebContextService::new(
            vendor_config(provider, options, host),
            FakeTransport::new(vec!["127.0.0.1".parse().unwrap()], Vec::new()),
        )
        .unwrap();
        assert_eq!(
            private_dns
                .search(WebSearchRequest { query: "private DNS".to_owned() })
                .await
                .unwrap_err()
                .code,
            WebContextErrorCode::DnsRejected,
            "{} private DNS",
            provider.id()
        );
        assert!(private_dns.transport.requests.lock().unwrap().is_empty());
        assert!(private_dns.cache.lock().unwrap().entries.is_empty());
    }
}

#[tokio::test]
async fn provider_decoders_reject_results_or_grounding_beyond_configured_bounds() {
    let exa = WebContextService::new(
        vendor_config(
            WebSearchProvider::Exa,
            WebSearchProviderOptions::Exa(ExaSearchOptions {
                max_results: 1,
                ..ExaSearchOptions::default()
            }),
            "api.exa.ai",
        ),
        FakeTransport::new(
            vec![public_ip()],
            vec![response(
                200,
                &[("content-type", "application/json")],
                br#"{"results":[{"title":"one","url":"https://docs.example/one","highlights":["one"]},{"title":"two","url":"https://docs.example/two","highlights":["two"]}]}"#,
            )],
        ),
    )
    .unwrap();
    assert_eq!(
        exa.search(WebSearchRequest { query: String::from("bounded results") })
            .await
            .unwrap_err()
            .code,
        WebContextErrorCode::NetworkFailure
    );
    assert!(exa.cache.lock().unwrap().entries.is_empty());

    let brave = WebContextService::new(
        vendor_config(
            WebSearchProvider::BraveLlmContext,
            WebSearchProviderOptions::BraveLlmContext(BraveLlmContextOptions {
                max_snippets: 1,
                ..BraveLlmContextOptions::default()
            }),
            "api.search.brave.com",
        ),
        FakeTransport::new(
            vec![public_ip()],
            vec![response(
                200,
                &[("content-type", "application/json")],
                br#"{"grounding":{"generic":[{"url":"https://docs.example/brave","snippets":["one","two"]}]}}"#,
            )],
        ),
    )
    .unwrap();
    assert_eq!(
        brave
            .search(WebSearchRequest { query: String::from("bounded grounding") })
            .await
            .unwrap_err()
            .code,
        WebContextErrorCode::NetworkFailure
    );
    assert!(brave.cache.lock().unwrap().entries.is_empty());
}

#[tokio::test]
async fn vendor_search_aggregate_text_budget_truncates_untrusted_snippets() {
    let mut limited = vendor_config(
        WebSearchProvider::Exa,
        WebSearchProviderOptions::Exa(ExaSearchOptions::default()),
        "api.exa.ai",
    );
    limited.limits.max_text_bytes = 100;
    let upstream_id = "provider-request-id";
    let service = WebContextService::new(
        limited,
        FakeTransport::new(
            vec![public_ip()],
            vec![response(
                200,
                &[("content-type", "application/json"), ("x-request-id", upstream_id)],
                br#"{"results":[{"title":"First source","url":"https://docs.example/one","highlights":["abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz"]},{"title":"Second source","url":"https://docs.example/two","highlights":["ignored after aggregate cap"]}],"output":"untrusted-provider-output"}"#,
            )],
        ),
    )
    .unwrap();

    let response = service
        .search(WebSearchRequest { query: "aggregate truncation".to_owned() })
        .await
        .unwrap();
    let output_bytes = response
        .results
        .iter()
        .map(|result| {
            result.title.len() + result.url.len() + result.host.len() + result.snippet.len()
        })
        .sum::<usize>();
    assert!(response.truncated);
    assert!(output_bytes <= 100);
    assert_eq!(response.results.len(), 1);
    assert!(response.results[0].snippet.len() < MAX_SNIPPET_BYTES);
    assert!(!format!("{response:?}").contains(upstream_id));
    assert!(!format!("{:?}", service.cache.lock().unwrap()).contains(upstream_id));
    assert!(!format!("{:?}", service.cache.lock().unwrap()).contains("untrusted-provider-output"));
}

#[tokio::test]
async fn vendor_malformed_results_fail_without_cache_and_cache_keys_are_separate() {
    let exa = WebContextService::new(
        vendor_config(
            WebSearchProvider::Exa,
            WebSearchProviderOptions::Exa(ExaSearchOptions::default()),
            "api.exa.ai",
        ),
        FakeTransport::new(
            vec![public_ip()],
            vec![response(
                200,
                &[("content-type", "application/json")],
                br#"{"results":[{"url":"https://docs.example/"}]}"#,
            )],
        ),
    )
    .unwrap();
    assert_eq!(
        exa.search(WebSearchRequest { query: "same query".to_owned() }).await.unwrap_err().code,
        WebContextErrorCode::NetworkFailure
    );
    assert!(exa.cache.lock().unwrap().entries.is_empty());
    let query_digest = sha256_hex(b"same query");
    assert_ne!(
        WebCacheKey::Search {
            provider_identity: "searxng:https://search.example/search:v1".to_owned(),
            options_digest: sha256_hex(b"Searxng"),
            query_digest: query_digest.clone()
        },
        WebCacheKey::Search {
            provider_identity: "exa:https://api.exa.ai/search:v1".to_owned(),
            options_digest: provider_options_digest(&WebSearchProviderOptions::Exa(
                ExaSearchOptions::default()
            )),
            query_digest
        },
    );

    let provider_keys = [
        WebCacheKey::Search {
            provider_identity: "searxng:https://search.example/search:v1".to_owned(),
            options_digest: provider_options_digest(&WebSearchProviderOptions::Searxng),
            query_digest: sha256_hex(b"same query"),
        },
        WebCacheKey::Search {
            provider_identity: "exa:https://api.exa.ai/search:v1".to_owned(),
            options_digest: provider_options_digest(&WebSearchProviderOptions::Exa(
                ExaSearchOptions::default(),
            )),
            query_digest: sha256_hex(b"same query"),
        },
        WebCacheKey::Search {
            provider_identity: "tavily:https://api.tavily.com/search:v1".to_owned(),
            options_digest: provider_options_digest(&WebSearchProviderOptions::Tavily(
                TavilySearchOptions::default(),
            )),
            query_digest: sha256_hex(b"same query"),
        },
        WebCacheKey::Search {
            provider_identity:
                "brave_llm_context:https://api.search.brave.com/res/v1/llm/context:v1".to_owned(),
            options_digest: provider_options_digest(&WebSearchProviderOptions::BraveLlmContext(
                BraveLlmContextOptions::default(),
            )),
            query_digest: sha256_hex(b"same query"),
        },
    ];
    assert_eq!(provider_keys.into_iter().collect::<BTreeSet<_>>().len(), 4);

    let exa_auto =
        provider_options_digest(&WebSearchProviderOptions::Exa(ExaSearchOptions::default()));
    let exa_neural = provider_options_digest(&WebSearchProviderOptions::Exa(ExaSearchOptions {
        search_mode: ExaSearchMode::Neural,
        ..ExaSearchOptions::default()
    }));
    assert_ne!(exa_auto, exa_neural, "semantic provider options must partition cache keys");
}
