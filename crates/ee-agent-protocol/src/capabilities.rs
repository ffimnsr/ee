//! Capability handling for ACP v1 and the v1→v2 agent capability mapping.
//!
//! The official SDK types ignore unknown capability fields on
//! deserialization (fail-open at the type level, as ACP requires for
//! forward compatibility).  These helpers capture the *unknown* entries from
//! the raw `clientCapabilities` / `agentCapabilities` objects so they can be
//! surfaced in diagnostics — while the rest of `ee` never enables behavior
//! for capabilities it does not implement.

use serde_json::{Map, Value};

use crate::{AgentCapabilities, v2};

/// Converts a v1 [`AgentCapabilities`] into the v2 capability shape for
/// `initialize` responses on v2 connections.
///
/// Providers keep returning v1 capabilities (their public contract is
/// unchanged); the framework translates them at the wire boundary.  The v1
/// surface has no stdio MCP marker (it was implied baseline) so the
/// translation advertises `session.mcp.stdio`, mirrors `http`/`acp`, drops the
/// removed SSE marker, and carries `delete` / `additionalDirectories` over
/// when the v1 agent advertised them.
#[must_use]
pub fn agent_capabilities_to_v2(capabilities: &AgentCapabilities) -> v2::AgentCapabilities {
    let mut session = v2::SessionCapabilities::new();

    let prompt = if capabilities.prompt_capabilities.image
        || capabilities.prompt_capabilities.audio
        || capabilities.prompt_capabilities.embedded_context
    {
        let image = if capabilities.prompt_capabilities.image {
            Some(v2::PromptImageCapabilities::new())
        } else {
            None
        };
        let audio = if capabilities.prompt_capabilities.audio {
            Some(v2::PromptAudioCapabilities::new())
        } else {
            None
        };
        let embedded_context = if capabilities.prompt_capabilities.embedded_context {
            Some(v2::PromptEmbeddedContextCapabilities::new())
        } else {
            None
        };
        Some(
            v2::PromptCapabilities::new()
                .image(image)
                .audio(audio)
                .embedded_context(embedded_context),
        )
    } else {
        None
    };
    if let Some(prompt) = prompt {
        session = session.prompt(prompt);
    }

    let mcp = v2::McpCapabilities::new()
        // v1 had no stdio marker; ee agents launch stdio servers, so the
        // baseline capability is advertised.
        .stdio(Some(v2::McpStdioCapabilities::new()))
        .http(capabilities.mcp_capabilities.http.then(v2::McpHttpCapabilities::new))
        // SSE transport is removed in v2; never carried over.
        .acp(capabilities.mcp_capabilities.acp.then(v2::McpAcpCapabilities::new));
    session = session.mcp(mcp);

    if capabilities.session_capabilities.delete.is_some() {
        session = session.delete(Some(v2::SessionDeleteCapabilities::new()));
    }
    if capabilities.session_capabilities.additional_directories.is_some() {
        session = session
            .additional_directories(Some(v2::SessionAdditionalDirectoriesCapabilities::new()));
    }

    v2::AgentCapabilities::new().session(session)
}

/// Converts v2 agent capabilities (the session-nested shape) back into the
/// v1 [`AgentCapabilities`] the host stores and surfaces.
///
/// The reverse of [`agent_capabilities_to_v2`].  v2 has no stdio MCP marker in
/// `session.mcp` direction that v1 could carry, and baseline v2 session
/// methods (`list`/`resume`/`close`) carry no markers at all — hosts using
/// this conversion must treat those as supported when `session` is present.
#[must_use]
pub fn agent_capabilities_from_v2(capabilities: &v2::AgentCapabilities) -> AgentCapabilities {
    let mut converted = AgentCapabilities::default();
    let Some(session) = &capabilities.session else {
        return converted;
    };
    if let Some(prompt) = &session.prompt {
        converted.prompt_capabilities.image = prompt.image.is_some();
        converted.prompt_capabilities.audio = prompt.audio.is_some();
        converted.prompt_capabilities.embedded_context = prompt.embedded_context.is_some();
    }
    if let Some(mcp) = &session.mcp {
        converted.mcp_capabilities.http = mcp.http.is_some();
        converted.mcp_capabilities.acp = mcp.acp.is_some();
        // v1 has no stdio marker; stdio servers stay usable either way.
    }
    if session.delete.is_some() {
        converted.session_capabilities.delete = Some(crate::SessionDeleteCapabilities::new());
    }
    if session.additional_directories.is_some() {
        converted.session_capabilities.additional_directories =
            Some(crate::SessionAdditionalDirectoriesCapabilities::new());
    }
    converted
}

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

    #[test]
    fn agent_capabilities_map_to_v2_session_capabilities() {
        use crate::{
            AgentCapabilities, McpCapabilities as V1Mcp, PromptCapabilities as V1Prompt,
            SessionCapabilities as V1Session,
        };
        let caps = AgentCapabilities::default()
            .prompt_capabilities(V1Prompt::new().image(true).audio(true).embedded_context(true))
            .mcp_capabilities(V1Mcp::new().http(true).sse(false))
            .session_capabilities(
                V1Session::new()
                    .delete(crate::SessionDeleteCapabilities::new())
                    .additional_directories(crate::SessionAdditionalDirectoriesCapabilities::new()),
            );

        let v2_caps = agent_capabilities_to_v2(&caps);
        let session = v2_caps.session.expect("session capability advertised");
        let prompt = session.prompt.expect("prompt capabilities advertised");
        assert!(prompt.image.is_some(), "image capability carried over");
        assert!(prompt.audio.is_some(), "audio capability carried over");
        assert!(prompt.embedded_context.is_some(), "embedded context carried over");
        let mcp = session.mcp.expect("mcp capabilities advertised");
        assert!(mcp.stdio.is_some(), "stdio baseline advertised");
        assert!(mcp.http.is_some(), "http capability carried over");
        assert!(session.delete.is_some(), "delete capability carried over");
        assert!(
            session.additional_directories.is_some(),
            "additional directories capability carried over"
        );
    }

    #[test]
    fn empty_agent_capabilities_map_to_baseline_session() {
        let v2_caps = agent_capabilities_to_v2(&crate::AgentCapabilities::default());
        let session = v2_caps.session.expect("baseline session advertised");
        assert!(session.prompt.is_none());
        assert!(session.mcp.is_some(), "stdio baseline always advertised");
        assert!(session.delete.is_none());
        assert!(session.additional_directories.is_none());
    }

    #[test]
    fn v2_capabilities_round_trip_through_v1_shape() {
        let mut v2_caps = v2::AgentCapabilities::new();
        let mut session = v2::SessionCapabilities::new();
        session = session.prompt(
            v2::PromptCapabilities::new()
                .image(v2::PromptImageCapabilities::new())
                .embedded_context(v2::PromptEmbeddedContextCapabilities::new()),
        );
        session = session.mcp(v2::McpCapabilities::new().http(v2::McpHttpCapabilities::new()));
        session = session.delete(v2::SessionDeleteCapabilities::new());
        v2_caps = v2_caps.session(session);

        let v1_caps = agent_capabilities_from_v2(&v2_caps);
        assert!(v1_caps.prompt_capabilities.image, "image carried over");
        assert!(!v1_caps.prompt_capabilities.audio, "audio stays off");
        assert!(v1_caps.prompt_capabilities.embedded_context, "embedded context carried over");
        assert!(v1_caps.mcp_capabilities.http, "http carried over");
        assert!(!v1_caps.mcp_capabilities.sse, "sse never set");
        assert!(v1_caps.session_capabilities.delete.is_some(), "delete marker carried over");
        assert!(
            v1_caps.session_capabilities.additional_directories.is_none(),
            "additional directories not advertised"
        );
    }

    #[test]
    fn empty_v2_capabilities_yield_default_v1_capabilities() {
        let v1_caps = agent_capabilities_from_v2(&v2::AgentCapabilities::new());
        assert!(!v1_caps.load_session);
        assert!(v1_caps.session_capabilities.delete.is_none());
    }
}
