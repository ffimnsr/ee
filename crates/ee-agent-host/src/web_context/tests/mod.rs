//! Web-context tests: shared transport fakes and harness.
use super::*;

#[derive(Debug)]
struct FakeTransport {
    addresses: Vec<IpAddr>,
    responses: std::sync::Mutex<Vec<WebTransportResponse>>,
    requests: std::sync::Mutex<Vec<WebTransportRequest>>,
}

impl FakeTransport {
    fn new(addresses: Vec<IpAddr>, responses: Vec<WebTransportResponse>) -> Self {
        Self {
            addresses,
            responses: std::sync::Mutex::new(responses),
            requests: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl WebTransport for FakeTransport {
    fn resolve<'a>(
        &'a self,
        _host: &'a str,
        _cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<IpAddr>, WebContextError>> {
        Box::pin(async move { Ok(self.addresses.clone()) })
    }

    fn request<'a>(
        &'a self,
        request: &'a WebTransportRequest,
        _cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<WebTransportResponse, WebContextError>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request.clone());
            Ok(self.responses.lock().unwrap().remove(0))
        })
    }
}

#[derive(Clone, Copy)]
enum CancellationPhase {
    Dns,
    Request,
    Body,
}

struct BlockingTransport {
    phase: CancellationPhase,
    started: std::sync::Arc<tokio::sync::Notify>,
}

impl BlockingTransport {
    fn new(phase: CancellationPhase) -> Self {
        Self { phase, started: std::sync::Arc::new(tokio::sync::Notify::new()) }
    }
}

impl WebTransport for BlockingTransport {
    fn resolve<'a>(
        &'a self,
        _host: &'a str,
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<IpAddr>, WebContextError>> {
        Box::pin(async move {
            if matches!(self.phase, CancellationPhase::Dns) {
                self.started.notify_one();
                cancellation.cancelled().await;
                return Err(cancellation_error());
            }
            Ok(vec![public_ip()])
        })
    }

    fn request<'a>(
        &'a self,
        _request: &'a WebTransportRequest,
        cancellation: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<WebTransportResponse, WebContextError>> {
        Box::pin(async move {
            if matches!(self.phase, CancellationPhase::Request | CancellationPhase::Body) {
                self.started.notify_one();
                cancellation.cancelled().await;
                return Err(cancellation_error());
            }
            unreachable!("DNS phase never calls get after cancellation")
        })
    }
}

fn public_ip() -> IpAddr {
    "8.8.8.8".parse().unwrap()
}

fn config() -> AgentWebContextConfig {
    AgentWebContextConfig {
        enabled: true,
        provider: WebSearchProvider::Searxng,
        provider_options: WebSearchProviderOptions::Searxng,
        search_endpoint: Some("https://search.example/search".to_owned()),
        preapproved_hosts: BTreeSet::from(["search.example".to_owned(), "docs.example".to_owned()]),
        limits: WebContextLimits::default(),
        provider_secret_reference: None,
        browser_run_account_id: None,
        browser_run_api_token_reference: None,
        browser_run_retry: BrowserRunRetryPolicy::default(),
        search_authorization: None,
        browser_run_api_token: None,
    }
}

fn response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> WebTransportResponse {
    WebTransportResponse {
        status,
        headers: headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
        body: body.to_vec(),
        body_truncated: false,
        connected_peer: public_ip(),
    }
}

async fn assert_cancellation_prevents_cache_insert(phase: CancellationPhase) {
    let transport = BlockingTransport::new(phase);
    let started = std::sync::Arc::clone(&transport.started);
    let service = std::sync::Arc::new(WebContextService::new(config(), transport).unwrap());
    let cancellation = CancellationToken::new();
    let task_service = std::sync::Arc::clone(&service);
    let task_cancellation = cancellation.clone();
    let task = tokio::spawn(async move {
        task_service
            .fetch_with_cancellation(
                WebFetchRequest { url: "https://docs.example/".to_owned() },
                &task_cancellation,
            )
            .await
    });

    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .expect("transport reaches cancellation phase");
    cancellation.cancel();
    assert_eq!(task.await.unwrap().unwrap_err().code, WebContextErrorCode::NetworkFailure);
    assert!(service.cache.lock().unwrap().entries.is_empty());
}

mod cache_tests;
mod transport_tests;
mod vendor_tests;
