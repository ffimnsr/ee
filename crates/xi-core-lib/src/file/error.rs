// Copyright 2016 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! File error type and its wire mapping.

use std::fmt;
use std::io;
use std::path::PathBuf;

use xi_rpc::RemoteErrorDetails;

use crate::text_store::DocumentMode;

#[derive(Debug)]
pub enum FileError {
    Io(io::Error, PathBuf),
    UnknownEncoding(PathBuf),
    HasChanged(PathBuf),
    /// The path contains non-UTF-8 bytes and cannot be used as an RPC string.
    NonUtf8Path(PathBuf),
    /// File size could not be determined; refusing to load to avoid memory exhaustion.
    MetadataUntrusted(PathBuf),
    /// File exceeds the full-memory confirmation threshold for its location.
    ///
    /// The caller must surface `reason` to the user.  If the user accepts, retry
    /// with [`FileManager::open_with_override`] passing the appropriate
    /// [`ModeOverride`].
    ConfirmationRequired {
        path: PathBuf,
        reason: &'static str,
        /// The mode that would be used after confirmation.
        mode: DocumentMode,
    },
}

impl RemoteErrorDetails for FileError {
    fn remote_error_code(&self) -> i64 {
        match self {
            FileError::Io(_, _) => 5,
            FileError::UnknownEncoding(_) => 6,
            FileError::HasChanged(_) => 7,
            FileError::NonUtf8Path(_) => 8,
            FileError::MetadataUntrusted(_) => 9,
            FileError::ConfirmationRequired { .. } => 10,
        }
    }
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            FileError::Io(e, p) => write!(f, "{}. File path: {}", e, p.display()),
            FileError::UnknownEncoding(p) => {
                write!(f, "Error decoding UTF-8 file contents: {}", p.display())
            }
            FileError::HasChanged(p) => write!(
                f,
                "File has changed on disk. \
                 Please save elsewhere and reload the file. File path: {}",
                p.display()
            ),
            FileError::NonUtf8Path(p) => {
                write!(f, "File path contains non-UTF-8 bytes and cannot be used: {}", p.display())
            }
            FileError::MetadataUntrusted(p) => write!(
                f,
                "File size could not be determined safely; refusing to load: {}",
                p.display()
            ),
            FileError::ConfirmationRequired { path, reason, mode } => {
                write!(f, "{}; selected mode: {:?}. File path: {}", reason, mode, path.display())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use xi_rpc::RemoteError;

    #[test]
    fn file_error_converts_into_remote_error() {
        let err: RemoteError = FileError::UnknownEncoding(PathBuf::from("/tmp/demo.txt")).into();

        assert_eq!(
            err,
            RemoteError::custom(
                6,
                "Error decoding UTF-8 file contents: /tmp/demo.txt",
                None::<serde_json::Value>,
            )
        );
    }

    #[test]
    fn metadata_untrusted_has_correct_code() {
        let err = FileError::MetadataUntrusted(PathBuf::from("/tmp/big.bin"));
        use xi_rpc::RemoteErrorDetails;
        assert_eq!(err.remote_error_code(), 9);
        assert!(err.to_string().contains("refusing to load"));
    }

    #[test]
    fn confirmation_required_has_correct_code() {
        use xi_rpc::RemoteErrorDetails;
        let err = FileError::ConfirmationRequired {
            path: PathBuf::from("/tmp/huge.bin"),
            reason: "file is too large for a full-memory open; use VLF mode or confirm normal open",
            mode: DocumentMode::Normal,
        };
        assert_eq!(err.remote_error_code(), 10);
        assert!(err.to_string().contains("full-memory open"));
        assert!(err.to_string().contains("selected mode: Normal"));
    }
}
