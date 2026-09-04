//! Editor configuration loading for ee.
//!
//! Settings are resolved by merging layers in priority order (lowest first):
//!   1. built-in defaults
//!   2. `/etc/ee/config.toml`
//!   3. `$XDG_CONFIG_HOME/ee/config.toml` or `~/.config/ee/config.toml`
//!   4. fallback `~/.ee.toml` when XDG user config is missing
//!   5. every ancestor `.ee.toml` from outermost to innermost
//!   6. `.editorconfig` (walked up from the open file, per spec)
//!
//! Later layers override earlier ones for any key that is explicitly set.

use super::discovery::{ConfigEnvironment, ConfigLayerKind, load_config_with_env};
use super::editor_settings::EditorSettings;
#[cfg(test)]
#[cfg(test)]
use std::cell::Cell;
use std::path::Path;
#[cfg(test)]
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

/// Builds an isolated layered-config environment under `root`: workspace,
/// home, XDG, and system config paths never touch the developer machine.
#[cfg(test)]
pub(crate) fn test_config_environment(root: &Path) -> ConfigEnvironment {
    ConfigEnvironment {
        cwd: root.join("workspace"),
        home_dir: Some(root.join("home")),
        config_dir: Some(root.join("xdg")),
        system_config_path: root.join("etc").join("ee").join("config.toml"),
    }
}

/// Writes one config layer file inside a test environment.
#[cfg(test)]
pub(crate) fn write_config_layer(env: &ConfigEnvironment, kind: ConfigLayerKind, contents: &str) {
    let path = match kind {
        ConfigLayerKind::System => env.system_config_path.clone(),
        ConfigLayerKind::UserXdg => env.xdg_user_config_path().expect("xdg path in test env"),
        ConfigLayerKind::UserLegacy => {
            env.legacy_user_config_path().expect("legacy path in test env")
        }
        ConfigLayerKind::Ancestor => env.cwd.join(".ee.toml"),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, contents).unwrap();
}

/// Loads merged settings from an isolated test config environment.
#[cfg(test)]
pub(crate) fn load_config_for_test(env: &ConfigEnvironment) -> EditorSettings {
    load_config_with_env(None, env)
}

#[cfg(test)]
thread_local! {
    static TEST_CWD_LOCK_DEPTH: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
pub(crate) struct TestCwdLock {
    inner: Mutex<()>,
}

#[cfg(test)]
pub(crate) struct TestCwdGuard {
    _guard: Option<MutexGuard<'static, ()>>,
}

#[cfg(test)]
pub(super) struct TestEnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl TestCwdLock {
    pub(crate) fn lock(
        &'static self,
    ) -> Result<TestCwdGuard, PoisonError<MutexGuard<'static, ()>>> {
        if TEST_CWD_LOCK_DEPTH.with(|depth| {
            let current = depth.get();
            if current == 0 {
                return false;
            }
            depth.set(current + 1);
            true
        }) {
            return Ok(TestCwdGuard { _guard: None });
        }

        let guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        TEST_CWD_LOCK_DEPTH.with(|depth| depth.set(1));
        Ok(TestCwdGuard { _guard: Some(guard) })
    }
}

#[cfg(test)]
impl Drop for TestCwdGuard {
    fn drop(&mut self) {
        TEST_CWD_LOCK_DEPTH.with(|depth| {
            let current = depth.get();
            debug_assert!(current > 0, "cwd lock depth underflow");
            depth.set(current.saturating_sub(1));
        });
    }
}

#[cfg(test)]
impl TestEnvVarGuard {
    pub(crate) fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }
}

#[cfg(test)]
impl Drop for TestEnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => unsafe {
                std::env::set_var(self.key, value);
            },
            None => unsafe {
                std::env::remove_var(self.key);
            },
        }
    }
}

#[cfg(test)]
pub(crate) fn test_cwd_lock() -> &'static TestCwdLock {
    static LOCK: OnceLock<TestCwdLock> = OnceLock::new();
    LOCK.get_or_init(|| TestCwdLock { inner: Mutex::new(()) })
}
