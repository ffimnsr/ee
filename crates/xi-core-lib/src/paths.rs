// Copyright 2026 The ee authors. All rights reserved.

//! Cross-platform user state-directory helpers shared by the editor CLI, the
//! core, and the LSP plugin binaries so user state (logs, sessions, trust
//! stores) never lands inside a project workspace.

use std::path::PathBuf;

/// Returns the platform "ee" state directory: `<state base>/ee`.
///
/// Resolution order:
/// 1. `XDG_STATE_HOME` when set and non-empty (matches the `ee` CLI override).
/// 2. [`dirs::state_dir`] — `~/.local/state` on Linux; `None` on macOS/Windows.
/// 3. [`dirs::data_local_dir`] — `~/Library/Application Support` on macOS.
/// 4. [`dirs::data_dir`] — last-resort platform data dir.
///
/// On Linux the result is unchanged from the XDG convention; the fallbacks
/// only matter on platforms without a state dir (macOS, Windows), where they
/// keep logs and other user state out of the project workspace.
pub fn user_state_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::state_dir)
        .or_else(dirs::data_local_dir)
        .or_else(dirs::data_dir);
    user_state_dir_from(base)
}

fn user_state_dir_from(state_base: Option<PathBuf>) -> Option<PathBuf> {
    state_base.map(|dir| dir.join("ee"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{user_state_dir, user_state_dir_from};

    /// Restores a single env var on drop so tests cannot leak process-global
    /// state while running in parallel. Only the XDG-override test below
    /// mutates the environment, so no other test in this binary can race it.
    struct EnvGuard {
        key: &'static str,
        old: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: Option<&std::path::Path>) -> Self {
            let old = std::env::var_os(key);
            match value {
                Some(path) => unsafe { std::env::set_var(key, path) },
                None => unsafe { std::env::remove_var(key) },
            }
            EnvGuard { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    #[test]
    fn user_state_dir_from_joins_ee_when_base_present() {
        let base = PathBuf::from("/tmp/state");
        assert_eq!(user_state_dir_from(Some(base.clone())), Some(base.join("ee")));
    }

    #[test]
    fn user_state_dir_from_returns_none_without_base() {
        assert_eq!(user_state_dir_from(None), None);
    }

    #[test]
    fn user_state_dir_honors_xdg_state_home_override() {
        let temp = tempfile::tempdir().unwrap();
        let state_home = temp.path().join("state-home");
        let _guard = EnvGuard::set("XDG_STATE_HOME", Some(&state_home));
        assert_eq!(user_state_dir(), Some(state_home.join("ee")));
    }
}
