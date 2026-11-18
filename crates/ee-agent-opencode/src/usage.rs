//! Read-only OpenCode usage and quota visibility.
//!
//! Reachable only through the explicit `--usage` action: no startup path calls
//! this module, so an ordinary session performs exactly the requests its turns
//! require. Two documented endpoints share the OpenCode origin:
//!
//! - `GET https://opencode.ai/console/api/policies/current-budgets` reports
//!   workspace policy budgets. Service-account keys get `403`, which means "no
//!   budgets", not an error. The console origin (`https://opencode.ai/console`)
//!   matches the reference `console-access.ts` `defaultServer`.
//! - `GET https://opencode.ai/zen/go/v1/usage` reports the Go meter (rolling,
//!   weekly, monthly). A workspace without a Go subscription gets `403`, which
//!   means "no allowance to show".
//!
//! The organization id comes from configuration (`OPENCODE_ORG_ID` or the agent
//! config file) and is never inferred from the API key. Results are never
//! cached beyond the invocation, and no credential is printed.

use std::sync::Arc;
use std::time::Duration;

use ee_chat_completions::{
    EndpointProfile, EndpointTransport, RetryPolicy, TokenSource, TrustedEndpoint,
};
use serde_json::{Value, json};

use crate::config::{API_KEY_ENV, MISSING_API_KEY};

/// Console server used by the documented budget endpoint.
pub const CONSOLE_URL: &str = "https://opencode.ai/console";
/// OpenCode origin shared by the budget and Go-meter endpoints.
pub const OPENCODE_ORIGIN: &str = "https://opencode.ai";
/// Workspace budget endpoint path.
pub const BUDGETS_PATH: &str = "/api/policies/current-budgets";
/// Go subscription meter endpoint path.
pub const GO_USAGE_PATH: &str = "/zen/go/v1/usage";
/// Whole-probe timeout; both requests share it.
pub const USAGE_TIMEOUT: Duration = Duration::from_secs(15);
/// Maximum budgets accepted from one response.
const MAX_BUDGETS: usize = 100;
/// Maximum entries accepted inside one budget.
const MAX_BUDGET_ENTRIES: usize = 100;
/// Maximum accepted model id length in bytes.
const MAX_MODEL_ID_BYTES: usize = 200;

/// Everything the usage probe needs. Deliberately independent of route
/// selection: asking for usage must not require choosing a model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageProbe {
    /// Static API key; never printed.
    pub api_key: Option<String>,
    /// Workspace organization id from configuration.
    pub org_id: Option<String>,
}

impl UsageProbe {
    /// Resolves the probe from the process environment and the agent config
    /// file, in that order.
    ///
    /// # Errors
    ///
    /// Returns a diagnostic for a malformed configuration file.
    pub fn resolve(lookup: &mut dyn FnMut(&str) -> Option<String>) -> Result<Self, String> {
        let file = crate::config_file::load(lookup)?;
        Ok(Self {
            api_key: lookup(API_KEY_ENV).filter(|value| !value.is_empty()),
            org_id: lookup("OPENCODE_ORG_ID").filter(|value| !value.is_empty()).or(file.org_id),
        })
    }

    /// Whether a usable key is configured.
    #[must_use]
    pub fn has_api_key(&self) -> bool {
        self.api_key.as_deref().is_some_and(|key| !key.is_empty())
    }

    /// Token source for the probe requests.
    fn token_source(&self) -> TokenSource {
        let api_key = self.api_key.clone();
        Arc::new(move || {
            api_key
                .clone()
                .filter(|key| !key.is_empty())
                .map(ee_chat_completions::BearerToken::new)
                .ok_or_else(|| String::from(MISSING_API_KEY))
        })
    }
}

/// One rate-limit-style meter.
#[derive(Debug, Clone, PartialEq)]
pub struct Meter {
    /// Reported percentage, clamped to `0..=100`.
    pub percent: f64,
    /// Whether the meter reports `rate-limited`.
    pub limited: bool,
    /// Provider reset timestamp, verbatim.
    pub resets_at: Option<String>,
}

/// Go subscription allowance.
#[derive(Debug, Clone, PartialEq)]
pub struct GoUsage {
    /// Rolling window meter.
    pub rolling: Meter,
    /// Weekly window meter.
    pub weekly: Meter,
    /// Monthly window meter.
    pub monthly: Meter,
}

/// One workspace policy budget.
#[derive(Debug, Clone, PartialEq)]
pub struct Budget {
    /// Policy rule id.
    pub id: String,
    /// Window start or scope reported by the provider.
    pub scope: Option<String>,
    /// Budget period (`day`, `week`, `month`, ...).
    pub per: Option<String>,
    /// Dollar limit.
    pub limit_usd: f64,
    /// Dollars spent.
    pub spent_usd: f64,
    /// Provider reset timestamp, verbatim.
    pub resets_at: Option<String>,
    /// Whether this budget is the picked one.
    pub picked: bool,
    /// Whether the budget is shared.
    pub shared: bool,
    /// Covered vendor names.
    pub vendors: Vec<String>,
    /// Covered models with their reported spend.
    pub models: Vec<BudgetModel>,
}

/// One model entry inside a budget.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetModel {
    /// Provider id.
    pub provider_id: String,
    /// Model id.
    pub model_id: String,
    /// Display name.
    pub name: String,
    /// Dollars spent on this model, when reported.
    pub spent_usd: Option<f64>,
}

/// Result of one usage probe.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageReport {
    /// Workspace organization id, when configured.
    pub workspace_id: Option<String>,
    /// Workspace budgets; empty when the provider reports none.
    pub budgets: Vec<Budget>,
    /// Go allowance; `None` when the workspace is not subscribed.
    pub go: Option<GoUsage>,
    /// Bounded operator notices (for example "no Go allowance").
    pub notices: Vec<String>,
}

impl UsageReport {
    /// Renders the report as display-only JSON. Never carries a credential.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "workspace_id": self.workspace_id,
            "budgets": self.budgets.iter().map(budget_json).collect::<Vec<_>>(),
            "go": self.go.as_ref().map(go_json),
            "notices": self.notices,
        })
    }
}

fn budget_json(budget: &Budget) -> Value {
    json!({
        "id": budget.id,
        "scope": budget.scope,
        "per": budget.per,
        "limit_usd": budget.limit_usd,
        "spent_usd": budget.spent_usd,
        "resets_at": budget.resets_at,
        "picked": budget.picked,
        "shared": budget.shared,
        "vendors": budget.vendors,
        "models": budget
            .models
            .iter()
            .map(|model| json!({
                "provider_id": model.provider_id,
                "model_id": model.model_id,
                "name": model.name,
                "spent_usd": model.spent_usd,
            }))
            .collect::<Vec<_>>(),
    })
}

fn go_json(go: &GoUsage) -> Value {
    json!({
        "rolling": meter_json(&go.rolling),
        "weekly": meter_json(&go.weekly),
        "monthly": meter_json(&go.monthly),
    })
}

fn meter_json(meter: &Meter) -> Value {
    json!({
        "percent": meter.percent,
        "limited": meter.limited,
        "resets_at": meter.resets_at,
    })
}

/// Budget endpoint, including the Console path segment.
#[must_use]
pub fn budgets_endpoint() -> String {
    format!("{CONSOLE_URL}{BUDGETS_PATH}")
}

/// Go meter endpoint on the OpenCode origin.
#[must_use]
pub fn go_usage_endpoint() -> String {
    crate::routes::OpenCodeSurface::Go.quota_url().unwrap_or(GO_USAGE_PATH).to_string()
}

/// Fetches the usage report. One explicit invocation, no caching.
///
/// # Errors
///
/// Returns a bounded diagnostic when an endpoint is unusable or a non-`403`
/// request fails. `403` is a normal "not available" result.
pub async fn fetch_usage(probe: &UsageProbe) -> Result<UsageReport, String> {
    if !probe.has_api_key() {
        return Err(String::from(MISSING_API_KEY));
    }
    let workspace_id = probe.org_id.clone();
    let org = workspace_id.as_deref();
    let probe_future = async {
        let (budgets, go) = tokio::join!(fetch_budgets(probe, org), fetch_go(probe, org));
        (budgets, go)
    };
    let (budgets, go) = tokio::time::timeout(USAGE_TIMEOUT, probe_future)
        .await
        .map_err(|_| String::from("OpenCode usage limits timed out"))?;
    let go = go?;
    let mut notices = Vec::new();
    if go.is_none() {
        notices.push(String::from(
            "OpenCode Go usage is not available: the workspace is not subscribed to Go",
        ));
    }
    Ok(UsageReport { workspace_id, budgets: budgets?, go, notices })
}

/// Reads workspace budgets. `403` is "no budgets", not an error.
async fn fetch_budgets(probe: &UsageProbe, org: Option<&str>) -> Result<Vec<Budget>, String> {
    let Some(value) = get_json(probe, &budgets_endpoint(), org, "workspace budgets").await? else {
        return Ok(Vec::new());
    };
    parse_budgets(&value)
}

/// Reads the Go meter. `403` is "not subscribed", not an error.
async fn fetch_go(probe: &UsageProbe, org: Option<&str>) -> Result<Option<GoUsage>, String> {
    let Some(value) = get_json(probe, &go_usage_endpoint(), org, "Go usage").await? else {
        return Ok(None);
    };
    parse_go(&value).map(Some)
}

/// One authorized JSON GET. Returns `None` for `403`.
async fn get_json(
    probe: &UsageProbe,
    endpoint: &str,
    org: Option<&str>,
    label: &str,
) -> Result<Option<Value>, String> {
    let endpoint = TrustedEndpoint::parse(endpoint)
        .map_err(|error| format!("OpenCode {label} endpoint is not usable: {error}"))?;
    let profile =
        EndpointProfile::new(format!("OpenCode {label}"), API_KEY_ENV, endpoint, String::new())
            .with_timeouts(ee_chat_completions::Timeouts::new().with_total(Some(USAGE_TIMEOUT)))
            // One explicit invocation is one attempt: no retries, no backoff.
            .with_retry(RetryPolicy::none());
    let transport = EndpointTransport::new(profile, probe.token_source())?;
    let headers = transport.headers().map_err(|error| error.to_string())?;
    let mut request =
        transport.http_client().get(transport.profile().endpoint.as_str()).headers(headers);
    if let Some(org) = org {
        request = request.header("x-opencode-org-id", org).header("x-org-id", org);
    }
    let mut response = request
        .send()
        .await
        .map_err(|error| format!("OpenCode {label} request failed: {error}"))?;
    if response.status() == reqwest::StatusCode::FORBIDDEN {
        return Ok(None);
    }
    let status = response.status();
    let value = transport
        .read_json(&mut response, "usage response")
        .await
        .map_err(|error| error.to_string())?;
    if !status.is_success() {
        return Err(transport.status_error(status, response.headers(), &value).to_string());
    }
    Ok(Some(value))
}

/// Parses the documented budget array.
///
/// # Errors
///
/// Returns a diagnostic when the payload is not an array or an entry is
/// structurally invalid.
pub fn parse_budgets(value: &Value) -> Result<Vec<Budget>, String> {
    let entries = value
        .as_array()
        .ok_or_else(|| String::from("OpenCode workloads budgets were not an array"))?;
    let mut budgets = Vec::new();
    for entry in entries.iter().take(MAX_BUDGETS) {
        let Some(object) = entry.as_object() else {
            continue;
        };
        let Some(id) = object.get("ruleID").and_then(Value::as_str) else {
            continue;
        };
        let limit = object.get("limitDollars").and_then(Value::as_f64).unwrap_or(0.0);
        let spent = object.get("spentDollars").and_then(Value::as_f64).unwrap_or(0.0);
        budgets.push(Budget {
            id: id.to_string(),
            scope: string_of(object, "scope"),
            per: string_of(object, "per"),
            limit_usd: non_negative(limit),
            spent_usd: non_negative(spent),
            resets_at: string_of(object, "resetsAt"),
            picked: object.get("picked").and_then(Value::as_bool).unwrap_or(false),
            shared: object.get("shared").and_then(Value::as_bool).unwrap_or(false),
            vendors: object
                .get("vendors")
                .and_then(Value::as_array)
                .map(|vendors| {
                    vendors
                        .iter()
                        .take(MAX_BUDGET_ENTRIES)
                        .filter_map(|vendor| {
                            vendor.get("name").and_then(Value::as_str).map(str::to_string)
                        })
                        .collect()
                })
                .unwrap_or_default(),
            models: object
                .get("models")
                .and_then(Value::as_array)
                .map(|models| {
                    models
                        .iter()
                        .take(MAX_BUDGET_ENTRIES)
                        .filter_map(|model| {
                            let provider_id = model.get("providerID").and_then(Value::as_str)?;
                            let model_id = model.get("modelID").and_then(Value::as_str)?;
                            if provider_id.len() > MAX_MODEL_ID_BYTES
                                || model_id.len() > MAX_MODEL_ID_BYTES
                            {
                                return None;
                            }
                            Some(BudgetModel {
                                provider_id: provider_id.to_string(),
                                model_id: model_id.to_string(),
                                name: model
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or(model_id)
                                    .to_string(),
                                spent_usd: model
                                    .get("spentDollars")
                                    .and_then(Value::as_f64)
                                    .map(non_negative),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
        });
    }
    Ok(budgets)
}

/// Parses the documented Go meter payload.
///
/// # Errors
///
/// Returns a diagnostic when `usage` or a meter is missing or carries an
/// unrecognized status.
pub fn parse_go(value: &Value) -> Result<GoUsage, String> {
    let usage = value
        .get("usage")
        .ok_or_else(|| String::from("OpenCode Go usage payload carried no usage object"))?;
    Ok(GoUsage {
        rolling: parse_meter(usage.get("rolling"), "rolling")?,
        weekly: parse_meter(usage.get("weekly"), "weekly")?,
        monthly: parse_meter(usage.get("monthly"), "monthly")?,
    })
}

fn parse_meter(value: Option<&Value>, name: &str) -> Result<Meter, String> {
    let object = value
        .and_then(Value::as_object)
        .ok_or_else(|| format!("OpenCode Go {name} meter is missing"))?;
    let status = object.get("status").and_then(Value::as_str).unwrap_or("");
    let limited = match status {
        "ok" => false,
        "rate-limited" => true,
        other => {
            return Err(format!("OpenCode Go {name} meter has unknown status {other:?}"));
        }
    };
    let percent = object.get("percent").and_then(Value::as_f64).unwrap_or(0.0);
    Ok(Meter {
        percent: percent.clamp(0.0, 100.0),
        limited,
        resets_at: string_of(object, "resetsAt"),
    })
}

fn string_of(object: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_string)
}

fn non_negative(value: f64) -> f64 {
    if value.is_finite() && value >= 0.0 { value } else { 0.0 }
}

#[cfg(test)]
mod tests;
