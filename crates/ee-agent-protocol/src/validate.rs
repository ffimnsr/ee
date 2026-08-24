//! Protocol-boundary validation for ACP v1.
//!
//! ACP v1 invariants enforced here:
//!
//! - Paths are absolute on every protocol boundary (`fs/*`, terminal `cwd`,
//!   tool-call locations, diffs).  Relative paths fail closed with a
//!   JSON-RPC `invalid params` error before request dispatch.
//! - Line numbers are 1-based at the protocol boundary.  Conversion to
//!   editor-internal 0-based offsets happens only inside the editor.
//!
//! These helpers validate *wire* values; they never touch the filesystem.

use std::path::Path;

use agent_client_protocol::schema::v1::{
    CreateTerminalRequest, ReadTextFileRequest, WriteTextFileRequest,
};

use crate::Error;

fn invalid_params(message: impl Into<String>) -> Error {
    Error::invalid_params().data(message.into())
}

/// Returns `Ok(path)` when `path` is absolute, otherwise a JSON-RPC
/// `invalid params` error naming the offending path.
///
/// # Errors
///
/// Returns [`Error`] with code [`crate::ErrorCode::InvalidParams`] for
/// relative or empty paths.
pub fn require_absolute_path(path: &Path) -> std::result::Result<(), Error> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(invalid_params(format!(
            "path must be absolute on the ACP protocol boundary, got `{}`",
            path.display()
        )))
    }
}

/// Convenience wrapper over [`require_absolute_path`] for string paths.
///
/// # Errors
///
/// Returns [`Error`] with code [`crate::ErrorCode::InvalidParams`] for
/// relative or empty paths.
pub fn require_absolute_path_str(path: &str) -> std::result::Result<(), Error> {
    require_absolute_path(Path::new(path))
}

/// Returns `Ok(())` when `line` is 1-based (`>= 1`), otherwise a JSON-RPC
/// `invalid params` error.  ACP line numbers are 1-based at the protocol
/// boundary; 0-based conversion belongs to the editor boundary only.
///
/// # Errors
///
/// Returns [`Error`] with code [`crate::ErrorCode::InvalidParams`] when
/// `line == 0`.
pub fn require_one_based_line(line: u32) -> std::result::Result<(), Error> {
    if line >= 1 {
        Ok(())
    } else {
        Err(invalid_params("ACP line numbers are 1-based at the protocol boundary, got 0"))
    }
}

/// Validates `fs/read_text_file` params: absolute path and 1-based `line`.
///
/// # Errors
///
/// Returns [`Error`] with code [`crate::ErrorCode::InvalidParams`] on the
/// first violation.
pub fn validate_read_text_file(request: &ReadTextFileRequest) -> std::result::Result<(), Error> {
    require_absolute_path(&request.path)?;
    if let Some(line) = request.line {
        require_one_based_line(line)?;
    }
    Ok(())
}

/// Validates `fs/write_text_file` params: absolute path.
///
/// # Errors
///
/// Returns [`Error`] with code [`crate::ErrorCode::InvalidParams`] when the
/// path is relative.
pub fn validate_write_text_file(request: &WriteTextFileRequest) -> std::result::Result<(), Error> {
    require_absolute_path(&request.path)
}

/// Validates `terminal/create` params: absolute `cwd` when present.
///
/// # Errors
///
/// Returns [`Error`] with code [`crate::ErrorCode::InvalidParams`] when
/// `cwd` is relative.
pub fn validate_terminal_create(request: &CreateTerminalRequest) -> std::result::Result<(), Error> {
    if let Some(cwd) = &request.cwd {
        require_absolute_path(cwd)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCode;
    use std::path::PathBuf;

    use agent_client_protocol::schema::v1::{
        CreateTerminalRequest, ReadTextFileRequest, SessionId, WriteTextFileRequest,
    };

    #[test]
    fn absolute_paths_pass() {
        require_absolute_path(Path::new("/home/user/project/main.rs")).unwrap();
        require_absolute_path_str("/tmp/x").unwrap();
    }

    #[test]
    fn relative_paths_fail_closed() {
        for path in ["main.rs", "./main.rs", "../main.rs", "", "C:\\\\x"] {
            let err = require_absolute_path_str(path).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidParams, "path `{path}`");
            let data = err.data.as_ref().expect("error carries the offending path");
            assert!(data.as_str().unwrap().contains("absolute"), "path `{path}`: {data}");
        }
    }

    #[test]
    fn read_text_file_requires_absolute_path() {
        let request = ReadTextFileRequest::new(SessionId::new("s1"), PathBuf::from("main.rs"));
        let err = validate_read_text_file(&request).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);

        let request = ReadTextFileRequest::new(SessionId::new("s1"), PathBuf::from("/main.rs"));
        validate_read_text_file(&request).unwrap();
    }

    #[test]
    fn read_text_file_line_is_one_based() {
        let request =
            ReadTextFileRequest::new(SessionId::new("s1"), PathBuf::from("/main.rs")).line(0);
        let err = validate_read_text_file(&request).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);
        let data = err.data.as_ref().expect("error carries the reason");
        assert!(data.as_str().unwrap().contains("1-based"), "{data}");

        let request =
            ReadTextFileRequest::new(SessionId::new("s1"), PathBuf::from("/main.rs")).line(1);
        validate_read_text_file(&request).unwrap();
    }

    #[test]
    fn write_text_file_requires_absolute_path() {
        let request =
            WriteTextFileRequest::new(SessionId::new("s1"), PathBuf::from("config.json"), "{}");
        assert_eq!(validate_write_text_file(&request).unwrap_err().code, ErrorCode::InvalidParams);

        let request =
            WriteTextFileRequest::new(SessionId::new("s1"), PathBuf::from("/config.json"), "{}");
        validate_write_text_file(&request).unwrap();
    }

    #[test]
    fn terminal_create_cwd_requires_absolute_path() {
        let request = CreateTerminalRequest::new(SessionId::new("s1"), "npm").cwd("project");
        let err = validate_terminal_create(&request).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidParams);

        let request =
            CreateTerminalRequest::new(SessionId::new("s1"), "npm").cwd("/home/user/project");
        validate_terminal_create(&request).unwrap();
    }
}
