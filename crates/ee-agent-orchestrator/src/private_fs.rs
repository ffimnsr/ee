//! Owner-only durable filesystem helpers shared by the checkpoint, session
//! state, and session log stores.
//!
//! Each store previously carried byte-identical copies of these helpers. They
//! live here so restrictive permissions and durable writes stay consistent:
//! directories are `0700`, files are `0600`, and writes are synced before a
//! rename or append is considered durable.

use std::io::Write;
use std::path::Path;

use crate::error::OrchestratorError;

/// Lowercase hex encoding of arbitrary bytes.
#[must_use]
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Creates a directory with owner-only permissions where the platform exposes
/// POSIX modes. Existing directories are tightened too.
pub(crate) fn create_private_dir(path: &Path) -> Result<(), OrchestratorError> {
    std::fs::create_dir_all(path).map_err(|error| {
        OrchestratorError::Serialization(format!(
            "failed to create private directory {}: {error}",
            path.display()
        ))
    })?;
    set_private_directory_permissions(path)
}

/// Creates one owner-only file atomically (fails when it already exists) and
/// syncs its bytes before returning.
pub(crate) fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), OrchestratorError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| {
        OrchestratorError::Serialization(format!(
            "failed to create private file {}: {error}",
            path.display()
        ))
    })?;
    file.write_all(bytes).and_then(|()| file.sync_all()).map_err(|error| {
        OrchestratorError::Serialization(format!(
            "failed to durably write private file {}: {error}",
            path.display()
        ))
    })?;
    set_private_file_permissions(path)
}

/// Appends bytes to an owner-only file, syncing before returning.
pub(crate) fn append_private_file(path: &Path, bytes: &[u8]) -> Result<(), OrchestratorError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| {
        OrchestratorError::Serialization(format!(
            "failed to open private log {}: {error}",
            path.display()
        ))
    })?;
    file.write_all(bytes).and_then(|()| file.sync_all()).map_err(|error| {
        OrchestratorError::Serialization(format!(
            "failed to durably append to {}: {error}",
            path.display()
        ))
    })?;
    set_private_file_permissions(path)
}

/// Syncs a directory entry after a rename or append on Unix.
pub(crate) fn sync_directory(path: &Path) -> Result<(), OrchestratorError> {
    #[cfg(unix)]
    {
        std::fs::File::open(path).and_then(|directory| directory.sync_all()).map_err(|error| {
            OrchestratorError::Serialization(format!(
                "failed to sync directory {}: {error}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<(), OrchestratorError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|error| {
        OrchestratorError::Serialization(format!(
            "failed to restrict directory {}: {error}",
            path.display()
        ))
    })
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<(), OrchestratorError> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<(), OrchestratorError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|error| {
        OrchestratorError::Serialization(format!(
            "failed to restrict file {}: {error}",
            path.display()
        ))
    })
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<(), OrchestratorError> {
    Ok(())
}
