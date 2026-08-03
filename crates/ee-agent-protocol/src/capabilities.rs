//! Unknown-capability handling for ACP v1.
//!
//! The official SDK types ignore unknown capability fields on
//! deserialization (fail-open at the type level, as ACP requires for
//! forward compatibility).  These helpers capture the *unknown* entries from
//! the raw `clientCapabilities` / `agentCapabilities` objects so they can be
//! surfaced in diagnostics — while the rest of `ee` never enables behavior
//! for capabilities it does not implement.

use serde_json::{Map, Value};

/// Capability names defined by ACP v1 on the client side (plus `_meta`).
///
/// `plan`, `auth`, `nes`, and `positionEncodings` are spec-defined
/// (currently unstable) names; they count as known even though `ee` does not
/// implement them, so they are not reported as unknown.
pub const KNOWN_CLIENT_CAPABILITY_NAMES: &[&str] =
    &["fs", "terminal", "session", "elicitation", "plan", "auth", "nes", "positionEncodings"];

/// Capability names defined by ACP v1 on the agent side (plus `_meta`).
pub const KNOWN_AGENT_CAPABILITY_NAMES: &[&str] = &[
    "loadSession",
    "promptCapabilities",
    "mcpCapabilities",
    "sessionCapabilities",
    "auth",
    "providers",
    "nes",
    "positionEncoding",
];

/// Returns `(name, value)` pairs for entries in `map` whose name is not in
/// `known`.  Purely diagnostic: callers must not act on the values.
#[must_use]
pub fn unknown_entries(map: &Map<String, Value>, known: &[&str]) -> Vec<(String, Value)> {
    map.iter()
        .filter(|(name, _)| name.as_str() != "_meta" && !known.contains(&name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// Extracts unknown entries from a raw `clientCapabilities` object.
///
/// Pass the raw JSON of the whole `initialize` request or response; the
/// `clientCapabilities` member is located automatically when present.
#[must_use]
pub fn unknown_client_capabilities(raw: &Value) -> Vec<(String, Value)> {
    raw.get("clientCapabilities")
        .and_then(Value::as_object)
        .map(|map| unknown_entries(map, KNOWN_CLIENT_CAPABILITY_NAMES))
        .unwrap_or_default()
}

/// Extracts unknown entries from a raw `agentCapabilities` object.
///
/// Pass the raw JSON of the whole `initialize` request or response; the
/// `agentCapabilities` member is located automatically when present.
#[must_use]
pub fn unknown_agent_capabilities(raw: &Value) -> Vec<(String, Value)> {
    raw.get("agentCapabilities")
        .and_then(Value::as_object)
        .map(|map| unknown_entries(map, KNOWN_AGENT_CAPABILITY_NAMES))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_entries_skip_known_names_and_meta() {
        let map = serde_json::from_value::<Map<String, Value>>(json!({
            "fs": {"readTextFile": true},
            "terminal": true,
            "futureCapability": {"x": 1},
            "_meta": {"anything": true},
        }))
        .unwrap();
        let unknown = unknown_entries(&map, KNOWN_CLIENT_CAPABILITY_NAMES);
        assert_eq!(unknown.len(), 1);
        assert_eq!(unknown[0].0, "futureCapability");
    }

    #[test]
    fn extracts_unknown_client_capabilities_from_raw_initialize() {
        let raw = json!({
            "protocolVersion": 1,
            "clientCapabilities": {
                "fs": {"readTextFile": true},
                "terminal": true,
                "cryptoSign": {"algorithm": "ed25519"},
            },
        });
        let unknown = unknown_client_capabilities(&raw);
        assert_eq!(unknown.len(), 1);
        assert_eq!(unknown[0].0, "cryptoSign");
        // Known spec names are never surfaced.
        assert!(unknown.iter().all(|(name, _)| name != "fs" && name != "terminal"));
    }

    #[test]
    fn extracts_unknown_agent_capabilities_from_raw_initialize_response() {
        let raw = json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "loadSession": true,
                "promptCapabilities": {"image": true},
                "quantumPlan": {},
            },
        });
        let unknown = unknown_agent_capabilities(&raw);
        assert_eq!(unknown.len(), 1);
        assert_eq!(unknown[0].0, "quantumPlan");
    }

    #[test]
    fn missing_capabilities_object_yields_no_unknowns() {
        assert!(unknown_client_capabilities(&json!({"protocolVersion": 1})).is_empty());
        assert!(unknown_agent_capabilities(&json!({"protocolVersion": 1})).is_empty());
    }
}
