//! Strict ACP protocol-version negotiation.
//!
//! ACP v1 and the draft v2 are the supported protocol versions.  Any other
//! version fails closed with a JSON-RPC `invalid params` error so peers never
//! silently fall back to a different wire format.  One connection speaks
//! exactly one negotiated version after `initialize`; this module only answers
//! which versions are supported, the per-connection surface selection happens
//! in the server framework (`ee-acp-agent-server`) and the host.

use agent_client_protocol::schema::ProtocolVersion;

use crate::Error;

/// The legacy protocol version this crate speaks on the client side
/// (`ee-agent-host` still negotiates v1; v2 inbound support lives in the
/// agent-side framework).
pub const ACP_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V1;

/// The newest protocol version implemented by this crate: the value a
/// v2-capable client sends in `initialize` and the version an agent answers
/// when a peer requests it.
pub const LATEST_SUPPORTED_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V2;

/// Every protocol version this crate implements, oldest first.
pub const SUPPORTED_ACP_VERSIONS: [ProtocolVersion; 2] = [ProtocolVersion::V1, ProtocolVersion::V2];

/// Returns `true` when `version` is exactly one of the supported versions
/// (ACP v1 or draft v2).
#[must_use]
pub fn protocol_version_supported(version: ProtocolVersion) -> bool {
    matches!(version, ProtocolVersion::V1 | ProtocolVersion::V2)
}

/// The protocol version a client should send in `initialize`: the newest
/// supported version.
#[must_use]
pub const fn client_protocol_version() -> ProtocolVersion {
    LATEST_SUPPORTED_PROTOCOL_VERSION
}

/// Negotiates a protocol version for the `initialize` handshake (agent side).
///
/// Accepts ACP v1 and draft v2 and answers with the *requested* version (the
/// spec's "same version if supported" arm; per-connection surfaces must match
/// the request, not the answerer's latest).  Any other version (legacy v0 and
/// unknown future versions) fails closed with an ACP-compatible JSON-RPC error
/// carrying the supported versions in `data` for diagnostics.
///
/// # Errors
///
/// Returns [`Error`] with code [`crate::ErrorCode::InvalidParams] when `requested`
/// is not ACP v1 or v2.
pub fn negotiate_protocol_version(
    requested: ProtocolVersion,
) -> std::result::Result<ProtocolVersion, Error> {
    if protocol_version_supported(requested) {
        return Ok(requested);
    }
    Err(Error::invalid_params().data(serde_json::json!({
        "requestedProtocolVersion": requested.as_u16(),
        "supportedProtocolVersions": SUPPORTED_ACP_VERSIONS
            .map(|version| version.as_u16()),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCode;
    use serde_json::json;

    #[test]
    fn accepts_v1_and_v2() {
        assert_eq!(negotiate_protocol_version(ProtocolVersion::V1).unwrap(), ProtocolVersion::V1);
        assert_eq!(negotiate_protocol_version(ProtocolVersion::V2).unwrap(), ProtocolVersion::V2);
        assert!(protocol_version_supported(ProtocolVersion::V1));
        assert!(protocol_version_supported(ProtocolVersion::V2));
        assert!(!protocol_version_supported(ProtocolVersion::V0));
    }

    #[test]
    fn client_sends_the_latest_supported_version() {
        assert_eq!(client_protocol_version(), ProtocolVersion::V2);
    }

    #[test]
    fn other_versions_fail_closed_with_invalid_params() {
        let max: ProtocolVersion = serde_json::from_value(json!(65_535)).unwrap();
        for version in [ProtocolVersion::V0, max] {
            let err = negotiate_protocol_version(version).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidParams);
            let data = err.data.as_ref().expect("error carries data");
            assert_eq!(data["requestedProtocolVersion"], json!(version.as_u16()));
            assert_eq!(data["supportedProtocolVersions"], json!([1, 2]));
        }
    }

    #[test]
    fn supported_versions_serialize_as_wire_numbers() {
        assert_eq!(serde_json::to_value(ACP_PROTOCOL_VERSION).unwrap(), json!(1));
        assert_eq!(serde_json::to_value(LATEST_SUPPORTED_PROTOCOL_VERSION).unwrap(), json!(2));
    }
}
