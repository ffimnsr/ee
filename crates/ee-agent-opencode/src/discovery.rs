//! Bounded, display-only `/models` metadata discovery.
//!
//! Static routing in [`crate::routes`] is the only thing that can select a
//! dialect or an endpoint. This module exists purely so setup can show what the
//! surface currently advertises next to what EE actually routes, and it is
//! reachable only through the explicit `--discover-models` action:
//!
//! - The request goes to `<surface root>/models`, built from the same trusted
//!   constants the catalog uses, so no environment value can redirect the
//!   credential to another origin.
//! - The response is read under the shared bound
//!   ([`ee_chat_completions::MAX_JSON_BODY_BYTES`]), parsed into a bounded,
//!   deduplicated id list, and stripped of every other provider field before it
//!   is printed.
//! - `routed` mirrors [`routes::resolve_route`] exactly, so live metadata can
//!   never mark an unrouted id as routable or the reverse.

use std::future::Future;
use std::pin::Pin;

use ee_chat_completions::{EndpointProfile, EndpointTransport, RetryPolicy, TrustedEndpoint};
use serde_json::{Value, json};

use crate::config::{API_KEY_ENV, Config};
use crate::routes::{self, OpenCodeSurface};

/// Path appended to a surface API root for its model metadata list.
pub const MODELS_PATH: &str = "/models";
/// Maximum metadata entries accepted from one response.
pub const MAX_METADATA_ENTRIES: usize = 500;
/// Maximum accepted model id length in bytes.
pub const MAX_MODEL_ID_BYTES: usize = 200;
/// Routing statement carried by every report, so a consumer cannot mistake live
/// metadata for a routing input.
pub const ROUTING_NOTE: &str = "static-catalog-only";

/// Future returned by a metadata fetcher.
pub type FetchFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>>;

/// Source of one surface's model metadata.
///
/// Implementations must return raw JSON or a bounded, credential-free error.
pub trait MetadataFetcher: Send + Sync {
    /// Fetches the raw metadata document for `surface`.
    fn fetch<'a>(&'a self, surface: OpenCodeSurface) -> FetchFuture<'a>;
}

/// One advertised model id, annotated with static routing knowledge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveModel {
    /// Model id exactly as the surface advertises it.
    pub id: String,
    /// Whether [`routes::resolve_route`] accepts this id on this surface.
    pub routed: bool,
}

/// Display-only discovery result.
///
/// The report carries no endpoint, dialect, or credential field, and its JSON
/// shape is exactly `{ surface, routing, models: [{ id, routed }] }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryReport {
    /// Surface the metadata was fetched from.
    pub surface: OpenCodeSurface,
    /// Advertised ids with their static routing status, in arrival order.
    pub models: Vec<LiveModel>,
}

impl DiscoveryReport {
    /// Renders the report as the bounded display-only JSON printed by
    /// `--discover-models`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "surface": self.surface.as_str(),
            "routing": ROUTING_NOTE,
            "models": self
                .models
                .iter()
                .map(|model| json!({ "id": model.id, "routed": model.routed }))
                .collect::<Vec<_>>(),
        })
    }
}

/// Builds the trusted metadata endpoint for one surface.
///
/// # Errors
///
/// Returns a diagnostic when the catalog root cannot form a credential-safe
/// HTTPS endpoint, which is a catalog defect rather than a user error.
pub fn metadata_endpoint(surface: OpenCodeSurface) -> Result<TrustedEndpoint, String> {
    let endpoint = format!("{}{MODELS_PATH}", surface.api_root());
    TrustedEndpoint::parse(&endpoint).map_err(|error| {
        format!("OpenCode {} model metadata endpoint is not usable: {error}", surface.as_str())
    })
}

/// Fetches model metadata from the resolved surface root over HTTP.
pub struct HttpMetadataFetcher {
    transport: EndpointTransport,
    surface: OpenCodeSurface,
    endpoint: TrustedEndpoint,
}

impl HttpMetadataFetcher {
    /// Builds the fetcher for the configured surface.
    ///
    /// The credential is never read here; it is resolved per request, and only
    /// after the endpoint has been validated against the catalog root.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic when the catalog endpoint is unusable or the HTTP
    /// client cannot be built.
    pub fn new(config: &Config) -> Result<Self, String> {
        let surface = config.route.surface;
        let endpoint = metadata_endpoint(surface)?;
        let profile = EndpointProfile::new(
            format!("OpenCode {} model metadata", surface.as_str()),
            API_KEY_ENV,
            endpoint.clone(),
            // Metadata requests carry no model; the profile keeps the surface in
            // its label so failures name the routed surface.
            String::new(),
        )
        .with_timeout(config.timeout)
        // One explicit local action is one attempt: no retries, no backoff.
        .with_retry(RetryPolicy::none());
        let transport = EndpointTransport::new(profile, config.token_source())?;
        Ok(Self { transport, surface, endpoint })
    }

    /// Returns the exact endpoint this fetcher contacts.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        self.endpoint.as_str()
    }
}

impl MetadataFetcher for HttpMetadataFetcher {
    fn fetch<'a>(&'a self, surface: OpenCodeSurface) -> FetchFuture<'a> {
        Box::pin(async move {
            if surface != self.surface {
                return Err(format!(
                    "OpenCode {} model metadata cannot be fetched from the configured {} surface",
                    surface.as_str(),
                    self.surface.as_str()
                ));
            }
            // Resolving the credential first keeps a missing key from producing
            // any request at all.
            let headers = self.transport.headers().map_err(|error| error.to_string())?;
            let label = self.transport.profile().label.clone();
            let mut response = self
                .transport
                .http_client()
                .get(self.endpoint.as_str())
                .headers(headers)
                .send()
                .await
                .map_err(|error| format!("{label} request failed: {error}"))?;
            let status = response.status();
            let value = self
                .transport
                .read_json(&mut response, "model metadata response")
                .await
                .map_err(|error| error.to_string())?;
            if !status.is_success() {
                return Err(self
                    .transport
                    .status_error(status, response.headers(), &value)
                    .to_string());
            }
            Ok(value)
        })
    }
}

/// Fetches and annotates one surface's metadata.
///
/// Fetched ids are display-only: they preserve arrival order, are capped by
/// [`MAX_METADATA_ENTRIES`], deduplicated, and annotated from the static
/// catalog. Nothing in the returned report can select an endpoint or dialect.
///
/// # Errors
///
/// Returns the fetcher's bounded error, or a diagnostic when the document is
/// not a model list.
pub async fn discover_models(
    fetcher: &dyn MetadataFetcher,
    surface: OpenCodeSurface,
) -> Result<DiscoveryReport, String> {
    let value = fetcher.fetch(surface).await?;
    Ok(DiscoveryReport { surface, models: annotate(surface, &value)? })
}

/// Parses a metadata document into routed-annotated ids.
///
/// Only `data[].id` is read; every other provider field, and any credential,
/// is dropped before the value leaves this function.
///
/// # Errors
///
/// Returns a diagnostic when the document carries no `data` array, which means
/// the surface answered with something other than a model list.
pub fn annotate(surface: OpenCodeSurface, value: &Value) -> Result<Vec<LiveModel>, String> {
    let entries = value.get("data").and_then(Value::as_array).ok_or_else(|| {
        format!(
            "OpenCode {} model metadata did not carry a `data` array; no models were shown",
            surface.as_str()
        )
    })?;
    let mut models: Vec<LiveModel> = Vec::new();
    for entry in entries.iter().take(MAX_METADATA_ENTRIES) {
        let Some(id) = entry.get("id").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if id.is_empty() || id.len() > MAX_MODEL_ID_BYTES {
            continue;
        }
        if models.iter().any(|model| model.id == id) {
            continue;
        }
        models.push(LiveModel {
            id: id.to_string(),
            routed: routes::resolve_route(surface, id).is_ok(),
        });
    }
    Ok(models)
}

#[cfg(test)]
mod tests;
