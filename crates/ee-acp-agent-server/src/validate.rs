//! Protocol-boundary validation for the agent server framework.
//!
//! All helpers fail closed with deterministic [`AcpServerError`] values; the
//! dispatch path converts them into shaped JSON-RPC errors (`-32602` invalid
//! params, `-32600` invalid request for oversized frames, `-32600` for
//! unsupported protocol versions).  Validation never touches the filesystem
//! and never panics on malformed provider output — providers returning empty
//! ids are rejected here before any session state is registered.

use std::path::{Path, PathBuf};

use ee_agent_protocol::{MessageId, ProtocolVersion, SessionId, ToolCallId};

use crate::error::AcpServerError;

/// Negotiates ACP v1 only; any other version fails closed with an
/// [`AcpServerError::UnsupportedVersion`].
///
/// # Errors
///
/// Returns [`AcpServerError::UnsupportedVersion`] when `version` is not ACP
/// v1.
pub fn validate_protocol_version_v1(version: ProtocolVersion) -> Result<(), AcpServerError> {
    if version == ProtocolVersion::V1 {
        Ok(())
    } else {
        Err(AcpServerError::UnsupportedVersion { version: version.as_u16().to_string() })
    }
}

/// Rejects relative paths: ACP requires absolute paths on every boundary.
///
/// # Errors
///
/// Returns [`AcpServerError::InvalidParams`] when `path` is relative.
pub fn validate_absolute_path(path: &Path) -> Result<(), AcpServerError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(AcpServerError::InvalidParams {
            reason: format!("path must be an absolute path: {}", path.display()),
        })
    }
}

/// Rejects relative `cwd` or additional directories before any provider call.
///
/// # Errors
///
/// Returns [`AcpServerError::InvalidParams`] naming the first offending
/// path.
pub fn validate_absolute_paths(
    cwd: &Path,
    additional_directories: &[PathBuf],
) -> Result<(), AcpServerError> {
    if !cwd.is_absolute() {
        return Err(AcpServerError::InvalidParams {
            reason: format!("cwd must be an absolute path: {}", cwd.display()),
        });
    }
    if let Some(relative) = additional_directories.iter().find(|path| !path.is_absolute()) {
        return Err(AcpServerError::InvalidParams {
            reason: format!(
                "additional directory must be an absolute path: {}",
                relative.display()
            ),
        });
    }
    Ok(())
}

/// Rejects empty session ids (provider-returned or client-supplied).
///
/// # Errors
///
/// Returns [`AcpServerError::InvalidParams`] when the id is empty.
pub fn validate_session_id(session_id: &SessionId) -> Result<(), AcpServerError> {
    if session_id.0.is_empty() {
        Err(AcpServerError::InvalidParams { reason: "session id must not be empty".to_string() })
    } else {
        Ok(())
    }
}

/// Rejects empty message ids (update emission identifiers).
///
/// # Errors
///
/// Returns [`AcpServerError::InvalidParams`] when the id is empty.
pub fn validate_message_id(message_id: &MessageId) -> Result<(), AcpServerError> {
    if message_id.0.is_empty() {
        Err(AcpServerError::InvalidParams { reason: "message id must not be empty".to_string() })
    } else {
        Ok(())
    }
}

/// Rejects empty tool-call ids (update emission identifiers).
///
/// # Errors
///
/// Returns [`AcpServerError::InvalidParams`] when the id is empty.
pub fn validate_tool_call_id(tool_call_id: &ToolCallId) -> Result<(), AcpServerError> {
    if tool_call_id.0.is_empty() {
        Err(AcpServerError::InvalidParams { reason: "tool call id must not be empty".to_string() })
    } else {
        Ok(())
    }
}

/// Enforces the transport frame-size cap before any parsing happens.
///
/// # Errors
///
/// Returns [`AcpServerError::Protocol`] when `len` exceeds `max_frame_bytes`.
pub fn validate_frame_len(len: usize, max_frame_bytes: usize) -> Result<(), AcpServerError> {
    if len <= max_frame_bytes {
        Ok(())
    } else {
        Err(AcpServerError::Protocol(format!(
            "frame of {len} bytes exceeds the {max_frame_bytes} byte cap"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn protocol_version_v1_is_accepted() {
        validate_protocol_version_v1(ProtocolVersion::V1).expect("v1 negotiates");
    }

    #[test]
    fn other_protocol_versions_fail_closed() {
        let v2: ProtocolVersion = serde_json::from_value(serde_json::json!(2)).unwrap();
        for version in [ProtocolVersion::V0, v2] {
            match validate_protocol_version_v1(version) {
                Err(AcpServerError::UnsupportedVersion { version: wire }) => {
                    assert_eq!(wire, version.as_u16().to_string());
                }
                other => panic!("expected UnsupportedVersion, got {other:?}"),
            }
        }
    }

    #[test]
    fn absolute_paths_pass() {
        validate_absolute_path(Path::new("/work")).expect("absolute cwd passes");
        validate_absolute_path(Path::new("/home/user/project")).expect("absolute dir passes");
    }

    #[test]
    fn relative_paths_fail_closed() {
        for path in ["relative/dir", ".", "..", "main.rs", ""] {
            match validate_absolute_path(Path::new(path)) {
                Err(AcpServerError::InvalidParams { reason }) => {
                    assert!(reason.contains("absolute"), "{path}: {reason}");
                }
                other => panic!("expected InvalidParams for {path:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn absolute_paths_helper_accepts_absolute_cwd_and_dirs() {
        validate_absolute_paths(
            Path::new("/work"),
            &[PathBuf::from("/extra"), PathBuf::from("/other")],
        )
        .expect("all absolute passes");
        validate_absolute_paths(Path::new("/work"), &[]).expect("no additional dirs passes");
    }

    #[test]
    fn absolute_paths_helper_rejects_relative_cwd() {
        match validate_absolute_paths(Path::new("relative"), &[]) {
            Err(AcpServerError::InvalidParams { reason }) => {
                assert!(reason.contains("cwd must be an absolute path"), "{reason}");
            }
            other => panic!("expected InvalidParams, got {other:?}"),
        }
    }

    #[test]
    fn absolute_paths_helper_rejects_relative_additional_directory() {
        match validate_absolute_paths(Path::new("/work"), &[PathBuf::from("relative/extra")]) {
            Err(AcpServerError::InvalidParams { reason }) => {
                assert!(
                    reason.contains("additional directory must be an absolute path"),
                    "{reason}"
                );
            }
            other => panic!("expected InvalidParams, got {other:?}"),
        }
    }

    #[test]
    fn session_ids_must_not_be_empty() {
        validate_session_id(&SessionId::new("session-1")).expect("non-empty id passes");
        match validate_session_id(&SessionId::new("")) {
            Err(AcpServerError::InvalidParams { reason }) => {
                assert!(reason.contains("session id must not be empty"), "{reason}");
            }
            other => panic!("expected InvalidParams, got {other:?}"),
        }
    }

    #[test]
    fn message_ids_must_not_be_empty() {
        validate_message_id(&MessageId::new("m-1")).expect("non-empty id passes");
        match validate_message_id(&MessageId::new("")) {
            Err(AcpServerError::InvalidParams { reason }) => {
                assert!(reason.contains("message id must not be empty"), "{reason}");
            }
            other => panic!("expected InvalidParams, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_ids_must_not_be_empty() {
        validate_tool_call_id(&ToolCallId::new("tc-1")).expect("non-empty id passes");
        match validate_tool_call_id(&ToolCallId::new("")) {
            Err(AcpServerError::InvalidParams { reason }) => {
                assert!(reason.contains("tool call id must not be empty"), "{reason}");
            }
            other => panic!("expected InvalidParams, got {other:?}"),
        }
    }

    #[test]
    fn frame_len_within_cap_passes() {
        validate_frame_len(0, 1024).expect("empty frame passes");
        validate_frame_len(1024, 1024).expect("frame at the cap passes");
    }

    #[test]
    fn oversized_frame_len_fails_closed() {
        match validate_frame_len(1025, 1024) {
            Err(AcpServerError::Protocol(message)) => {
                assert!(message.contains("1024 byte cap"), "{message}");
                assert!(message.contains("1025"), "{message}");
            }
            other => panic!("expected Protocol, got {other:?}"),
        }
    }
}
