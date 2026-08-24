//! Strict ACP protocol-version negotiation.
//!
//! ACP v1 is the only supported protocol version.  Any other version fails
//! closed with a JSON-RPC `invalid params` error so peers never silently
//! fall back to a different wire format.

use agent_client_protocol::schema::ProtocolVersion;

use crate::Error;

/// The protocol version implemented by this crate: ACP v1.
pub const ACP_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V1;

/// Returns `true` when `version` is exactly the supported ACP v1.
#[must_use]
pub fn protocol_version_supported(version: ProtocolVersion) -> bool {
    version == ACP_PROTOCOL_VERSION
}

/// Negotiates a protocol version for the `initialize` handshake.
///
/// Accepts only ACP v1.  Any other version (including draft v2 and legacy
/// v0) fails closed with an ACP-compatible JSON-RPC error carrying the
/// supported version in `data` for diagnostics.
///
/// # Errors
///
/// Returns [`Error`] with code [`crate::ErrorCode::InvalidParams] when `requested`
/// is not ACP v1.
pub fn negotiate_protocol_version(
    requested: ProtocolVersion,
) -> std::result::Result<ProtocolVersion, Error> {
    if protocol_version_supported(requested) {
        return Ok(ACP_PROTOCOL_VERSION);
    }
    Err(Error::invalid_params().data(serde_json::json!({
        "requestedProtocolVersion": requested.as_u16(),
        "supportedProtocolVersion": ACP_PROTOCOL_VERSION.as_u16(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCode;
    use serde_json::json;

    #[test]
    fn accepts_only_v1() {
        assert_eq!(negotiate_protocol_version(ProtocolVersion::V1).unwrap(), ACP_PROTOCOL_VERSION);
        assert!(protocol_version_supported(ProtocolVersion::V1));
        assert!(!protocol_version_supported(ProtocolVersion::V0));
    }

    #[test]
    fn other_versions_fail_closed_with_invalid_params() {
        let v2: ProtocolVersion = serde_json::from_value(json!(2)).unwrap();
        let max: ProtocolVersion = serde_json::from_value(json!(65_535)).unwrap();
        for version in [ProtocolVersion::V0, v2, max] {
            let err = negotiate_protocol_version(version).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidParams);
            let data = err.data.as_ref().expect("error carries data");
            assert_eq!(data["requestedProtocolVersion"], json!(version.as_u16()));
            assert_eq!(data["supportedProtocolVersion"], json!(1));
        }
    }

    #[test]
    fn v1_serializes_as_wire_number() {
        assert_eq!(serde_json::to_value(ACP_PROTOCOL_VERSION).unwrap(), json!(1));
    }
}
