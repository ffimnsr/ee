//! `impl App`: web context service, dispatch, and value conversion.

use std::collections::BTreeSet;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::Ordering;

use ee_agent_host::{AgentError, ClientRequestResponse, ClientRequestResult};
use tokio::runtime::Builder as TokioBuilder;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::super::*;

use crate::app::agents_mcp::ProxyRoute;

#[cfg(test)]
use super::WEB_DISPATCH_TEST_COUNT;

use super::approval::{ApprovalPolicy, WebApprovalCall};
use super::prompt::ApprovalPrompt;
use super::write::ActionLogEntry;

pub(super) static NEXT_WEB_LIFECYCLE_ID: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

pub(super) fn web_context_agent_error(error: ee_agent_host::WebContextError) -> AgentError {
    AgentError::HandlerError(format!("{}: {}", error.code.as_str(), error.message))
}

fn web_context_config_agent_error(error: ee_agent_host::WebContextConfigError) -> AgentError {
    AgentError::HandlerError(format!("web_search_invalid_configuration: {error}"))
}

pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;

    let digest = sha2::Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

impl App {
    pub(super) fn web_context_service(
        &mut self,
    ) -> Result<Arc<ee_agent_host::WebContextService<ee_agent_host::ReqwestWebTransport>>, AgentError>
    {
        // Config is frontend-resolved and secret-redacted in `Debug`. A changed
        // semantic configuration must not reuse prior remote cache entries or
        // session network approvals.
        let fingerprint = self.config.agents.web_context.semantic_fingerprint();
        if self.agents.web_context_service.is_some()
            && self.agents.web_context_config_fingerprint.as_deref() != Some(fingerprint.as_str())
        {
            self.agents.web_context_service = None;
            self.agents.web_context_config_fingerprint = None;
            self.agents.approval_policy = ApprovalPolicy::default();
        }
        if self.agents.web_context_service.is_none() {
            let mut config = self.config.agents.web_context.clone();
            if config.enabled
                && let Some(reference) = config.provider_secret_reference.take()
            {
                let reference =
                    crate::secrets::SecretReference::parse(&reference).map_err(|_| {
                        AgentError::HandlerError(String::from(
                            "invalid agents.web_context.provider_secret_reference",
                        ))
                    })?;
                let store = self.build_agents_secret_store().ok_or_else(|| {
                    AgentError::HandlerError(String::from(
                        "web search authorization unavailable: secrets store unavailable",
                    ))
                })?;
                let secret = store.get(reference.name()).map_err(|_| {
                    AgentError::HandlerError(String::from(
                        "web search authorization unavailable: provider secret could not be resolved",
                    ))
                })?;
                self.agents.resolved_secret_values.push(secret.to_string());
                config = config.with_search_authorization(secret);
            }
            if config.enabled
                && let Some(reference) = config.browser_run_api_token_reference.take()
            {
                let reference =
                    crate::secrets::SecretReference::parse(&reference).map_err(|_| {
                        AgentError::HandlerError(String::from(
                            "invalid agents.web_context.browser_run_api_token_reference",
                        ))
                    })?;
                let store = self.build_agents_secret_store().ok_or_else(|| {
                    AgentError::HandlerError(String::from(
                        "Browser Run authorization unavailable: secrets store unavailable",
                    ))
                })?;
                let secret = store.get(reference.name()).map_err(|_| {
                    AgentError::HandlerError(String::from(
                        "Browser Run authorization unavailable: API token could not be resolved",
                    ))
                })?;
                self.agents.resolved_secret_values.push(secret.to_string());
                config = config.with_browser_run_api_token(secret);
            }
            let limits = config.limits.clone();
            let transport = ee_agent_host::ReqwestWebTransport::new(&limits)
                .map_err(web_context_agent_error)?;
            let service = ee_agent_host::WebContextService::new(config, transport)
                .map_err(web_context_config_agent_error)?;
            self.agents.web_context_service = Some(Arc::new(service));
            self.agents.web_context_config_fingerprint = Some(fingerprint);
        }
        self.agents.web_context_service.clone().ok_or_else(|| {
            AgentError::HandlerError(String::from("web context service unavailable"))
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn dispatch_web_call(
        &mut self,
        route: ProxyRoute,
        network_session_id: String,
        requested_host: String,
        call: WebApprovalCall,
        approved_hosts: BTreeSet<String>,
        cancellation: CancellationToken,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        #[cfg(test)]
        WEB_DISPATCH_TEST_COUNT.fetch_add(1, Ordering::SeqCst);

        match call {
            WebApprovalCall::Search { query } => {
                let service = match self.web_context_service() {
                    Ok(service) => service,
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        return;
                    }
                };
                let provider_label = service.search_provider_approval_label();
                let response = TokioBuilder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("web context runtime")
                    .block_on(service.search_with_approved_hosts_and_cancellation(
                        ee_agent_host::WebSearchRequest { query: query.clone() },
                        &approved_hosts,
                        &cancellation,
                    ));
                if cancellation.is_cancelled() || reply.is_closed() {
                    return;
                }
                match response {
                    Ok(response) => {
                        let source_url = service
                            .search_initial_host()
                            .map(|host| format!("https://{host}/"))
                            .unwrap_or_else(|_| String::from("https://search.invalid/"));
                        let retrieved_at = i64::try_from(response.provenance.retrieved_at_unix_ms)
                            .ok()
                            .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
                            .map(|time| time.to_rfc3339())
                            .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
                        let provenance = response.provenance.identity();
                        self.record_web_source(
                            &network_session_id,
                            "search",
                            source_url,
                            retrieved_at,
                            None,
                            0,
                            response.results.len(),
                            response.cached,
                            response.truncated,
                            provenance,
                        );
                        let _ = reply.send(
                            Self::web_search_value(query, response)
                                .map(ClientRequestResponse::ProxyValue),
                        );
                    }
                    Err(error)
                        if error.code
                            == ee_agent_host::WebContextErrorCode::NetworkApprovalRequired =>
                    {
                        let Some(host) = error.host else {
                            let _ = reply.send(Err(web_context_agent_error(error)));
                            return;
                        };
                        self.request_web_approval(ApprovalPrompt::web(
                            route,
                            network_session_id,
                            requested_host,
                            host,
                            Some(provider_label),
                            WebApprovalCall::Search { query },
                            approved_hosts,
                            cancellation,
                            reply,
                        ));
                    }
                    Err(error) => {
                        let host = error.host.as_deref().unwrap_or("configured search");
                        self.record_web_failure("search", host, error.code.as_str());
                        let _ = reply.send(Err(web_context_agent_error(error)));
                    }
                }
            }
            WebApprovalCall::Fetch { url } => {
                let response = match self.web_context_service() {
                    Ok(service) => TokioBuilder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("web context runtime")
                        .block_on(service.fetch_with_approved_hosts_and_cancellation(
                            ee_agent_host::WebFetchRequest { url: url.clone() },
                            &approved_hosts,
                            &cancellation,
                        )),
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        return;
                    }
                };
                if cancellation.is_cancelled() || reply.is_closed() {
                    return;
                }
                match response {
                    Ok(response) => {
                        let sha256 = sha256_hex(response.text.as_bytes());
                        let retrieved_at = i64::try_from(response.retrieved_at_unix_ms)
                            .ok()
                            .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
                            .map(|time| time.to_rfc3339())
                            .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
                        self.record_web_source(
                            &network_session_id,
                            "fetch",
                            response.final_url.clone(),
                            retrieved_at.clone(),
                            Some(sha256.clone()),
                            response.text.len(),
                            1,
                            response.cached,
                            response.truncated,
                            response.final_url.clone(),
                        );
                        let _ = reply.send(
                            Self::web_fetch_value(response, sha256, retrieved_at)
                                .map(ClientRequestResponse::ProxyValue),
                        );
                    }
                    Err(error)
                        if error.code
                            == ee_agent_host::WebContextErrorCode::NetworkApprovalRequired =>
                    {
                        let Some(host) = error.host else {
                            let _ = reply.send(Err(web_context_agent_error(error)));
                            return;
                        };
                        self.request_web_approval(ApprovalPrompt::web(
                            route,
                            network_session_id,
                            requested_host,
                            host,
                            None,
                            WebApprovalCall::Fetch { url },
                            approved_hosts,
                            cancellation,
                            reply,
                        ));
                    }
                    Err(error) => {
                        let host = error.host.as_deref().unwrap_or("requested host");
                        self.record_web_failure("fetch", host, error.code.as_str());
                        let _ = reply.send(Err(web_context_agent_error(error)));
                    }
                }
            }
            WebApprovalCall::BrowserRun { request } => {
                let action = request.action.as_str();
                let response = match self.web_context_service() {
                    Ok(service) => TokioBuilder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("web context runtime")
                        .block_on(service.browser_run_with_approved_hosts_and_cancellation(
                            request.clone(),
                            &approved_hosts,
                            &cancellation,
                        )),
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        return;
                    }
                };
                if cancellation.is_cancelled() || reply.is_closed() {
                    return;
                }
                match response {
                    Ok(response) => {
                        let byte_count =
                            serde_json::to_vec(&response.result).map_or(0, |result| result.len());
                        let retrieved_at = chrono::Utc::now().to_rfc3339();
                        self.record_web_source(
                            &network_session_id,
                            action,
                            response.requested_url.clone(),
                            retrieved_at,
                            None,
                            byte_count,
                            1,
                            false,
                            response.truncated,
                            String::from("cloudflare_browser_run"),
                        );
                        let _ = reply.send(
                            serde_json::to_value(response)
                                .map(ClientRequestResponse::ProxyValue)
                                .map_err(|error| AgentError::HandlerError(error.to_string())),
                        );
                    }
                    Err(error)
                        if error.code
                            == ee_agent_host::WebContextErrorCode::NetworkApprovalRequired =>
                    {
                        let Some(host) = error.host else {
                            let _ = reply.send(Err(web_context_agent_error(error)));
                            return;
                        };
                        self.request_web_approval(ApprovalPrompt::web(
                            route,
                            network_session_id,
                            requested_host,
                            host,
                            Some("Cloudflare Browser Run"),
                            WebApprovalCall::BrowserRun { request },
                            approved_hosts,
                            cancellation,
                            reply,
                        ));
                    }
                    Err(error) => {
                        let host = error.host.as_deref().unwrap_or("requested host");
                        self.record_web_failure(action, host, error.code.as_str());
                        let _ = reply.send(Err(web_context_agent_error(error)));
                    }
                }
            }
        }
    }

    /// Retains one compact source record and lifecycle row without copying
    /// untrusted remote bytes or agent-supplied query text into local state.
    #[allow(clippy::too_many_arguments)]
    fn record_web_source(
        &mut self,
        network_session_id: &str,
        action: &str,
        url: String,
        retrieved_at: String,
        sha256: Option<String>,
        byte_count: usize,
        result_count: usize,
        cached: bool,
        truncated: bool,
        provenance: String,
    ) {
        let host = ee_agent_host::web_context::validate_https_url(&url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_else(|| String::from("unknown"));
        let session_id = network_session_id.to_owned();
        self.agents.action_log.push(ActionLogEntry::ExternalSource {
            action: action.to_owned(),
            host: host.clone(),
            url: url.clone(),
            retrieved_at: retrieved_at.clone(),
            sha256: sha256.clone(),
            byte_count,
            result_count,
            cached,
            truncated,
            provenance: provenance.clone(),
            session_id,
        });
        let lifecycle_id = NEXT_WEB_LIFECYCLE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let cache_state = if cached { "cached" } else { "fresh" };
        let detail = match action {
            "search" => format!(
                "kind: search · host: {host} · results: {result_count} · cache: {cache_state} · provenance: {provenance} · trust: untrusted external content"
            ),
            _ => format!(
                "kind: fetch · host: {host} · url: {url} · bytes: {byte_count} · cache: {cache_state} · SHA-256: {} · retrieved: {retrieved_at} · truncated: {truncated} · trust: untrusted external content",
                sha256.as_deref().unwrap_or("none")
            ),
        };
        self.record_web_lifecycle(
            &format!("web-{lifecycle_id}"),
            &format!("web/{action}"),
            "completed",
            &detail,
        );
    }

    fn web_search_value(
        query: String,
        response: ee_agent_host::WebSearchResponse,
    ) -> Result<serde_json::Value, AgentError> {
        let result = ee_mcp::WebSearchResult {
            query,
            results: response
                .results
                .into_iter()
                .map(|entry| ee_mcp::WebSearchEntry {
                    title: entry.title,
                    url: entry.url,
                    host: entry.host,
                    snippet: entry.snippet,
                    rank: entry.rank as u32,
                })
                .collect(),
            provenance: response.provenance.identity(),
            trust: String::from("untrusted_external_content"),
            cached: response.cached,
            truncated: response.truncated,
        };
        serde_json::to_value(result).map_err(|error| AgentError::HandlerError(error.to_string()))
    }

    fn web_fetch_value(
        response: ee_agent_host::WebFetchResponse,
        sha256: String,
        retrieved_at: String,
    ) -> Result<serde_json::Value, AgentError> {
        let result = ee_mcp::FetchUrlResult {
            requested_url: response.requested_url,
            url: response.final_url.clone(),
            title: response.title,
            content_type: response.content_type,
            sha256,
            text: response.text,
            retrieved_at,
            links: Vec::new(),
            provenance: response.final_url,
            trust: String::from("untrusted_external_content"),
            cached: response.cached,
            truncated: response.truncated,
        };
        serde_json::to_value(result).map_err(|error| AgentError::HandlerError(error.to_string()))
    }
}

#[cfg(test)]
#[cfg(test)]
#[cfg(test)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_value_conversions_preserve_provenance_and_untrusted_markers() {
        let search = App::web_search_value(
            String::from("Rust MCP"),
            ee_agent_host::WebSearchResponse {
                results: vec![ee_agent_host::WebSearchResult {
                    title: String::from("Docs"),
                    url: String::from("https://docs.example/search"),
                    host: String::from("docs.example"),
                    snippet: String::from("MCP reference"),
                    rank: 1,
                }],
                provenance: ee_agent_host::WebSearchProvenance {
                    provider: ee_agent_host::web_context::WebSearchProvider::Exa,
                    adapter: String::from("v1"),
                    retrieved_at_unix_ms: 1,
                },
                truncated: false,
                cached: true,
            },
        )
        .expect("search response converts");
        assert_eq!(search["provenance"], "exa:v1");
        assert_eq!(search["trust"], "untrusted_external_content");
        assert_eq!(search["results"][0]["rank"], 1);
        assert_eq!(search["cached"], true);

        let fetch = App::web_fetch_value(
            ee_agent_host::WebFetchResponse {
                requested_url: String::from("https://docs.example/start"),
                final_url: String::from("https://docs.example/final"),
                title: Some(String::from("Docs")),
                content_type: String::from("text/html"),
                text: String::from("untrusted response"),
                retrieved_at_unix_ms: 1,
                truncated: true,
                redirects: 1,
                cached: false,
            },
            String::from("sha256"),
            String::from("2026-08-25T00:00:00Z"),
        )
        .expect("fetch response converts");
        assert_eq!(fetch["requestedUrl"], "https://docs.example/start");
        assert_eq!(fetch["url"], "https://docs.example/final");
        assert_eq!(fetch["provenance"], "https://docs.example/final");
        assert_eq!(fetch["trust"], "untrusted_external_content");
        assert_eq!(fetch["truncated"], true);
    }
}
