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

use super::constants::SYSTEM_CONFIG_PATH;
use super::editor_settings::EditorSettings;
use super::editorconfig::apply_editorconfig;
use super::lsp::LspSettingsBuilder;
use super::raw::{EeToml, parse_ee_toml};
#[cfg(any(feature = "agents", test))]
use super::web_context_merge::resolve_agent_web_context_with_env;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::{env, fs};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigLayerKind {
    System,
    UserXdg,
    UserLegacy,
    Ancestor,
}

impl ConfigLayerKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::UserXdg => "user xdg",
            Self::UserLegacy => "user legacy fallback",
            Self::Ancestor => "ancestor",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigScope {
    Global,
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigLayer {
    pub kind: ConfigLayerKind,
    pub path: PathBuf,
    pub root: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigLayerReport {
    pub kind: ConfigLayerKind,
    pub path: PathBuf,
    pub exists: bool,
    pub loaded: bool,
    pub root: Option<bool>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigSearchReport {
    pub anchor: PathBuf,
    pub layers: Vec<ConfigLayerReport>,
    pub editorconfig_applies: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ConfigEnvironment {
    pub(super) cwd: PathBuf,
    pub(super) home_dir: Option<PathBuf>,
    pub(super) config_dir: Option<PathBuf>,
    pub(super) system_config_path: PathBuf,
}

impl ConfigEnvironment {
    pub(super) fn from_process() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_default(),
            home_dir: dirs::home_dir(),
            config_dir: process_config_dir(),
            system_config_path: PathBuf::from(SYSTEM_CONFIG_PATH),
        }
    }

    fn anchor_dir(&self, file_path: Option<&Path>) -> PathBuf {
        let Some(path) = file_path else {
            return self.cwd.clone();
        };
        let path = if path.is_absolute() { path.to_path_buf() } else { self.cwd.join(path) };
        if path.is_dir() {
            path
        } else {
            path.parent().map(Path::to_path_buf).unwrap_or_else(|| self.cwd.clone())
        }
    }

    pub(crate) fn xdg_user_config_path(&self) -> Option<PathBuf> {
        self.config_dir.as_ref().map(|dir| dir.join("ee").join("config.toml"))
    }

    pub(crate) fn legacy_user_config_path(&self) -> Option<PathBuf> {
        self.home_dir.as_ref().map(|home| home.join(".ee.toml"))
    }

    fn workspace_candidate_paths(&self, file_path: Option<&Path>) -> Vec<PathBuf> {
        let legacy_user_path = self.legacy_user_config_path();
        let mut candidates = Vec::new();
        let mut dir = self.anchor_dir(file_path);
        loop {
            let candidate = dir.join(".ee.toml");
            if legacy_user_path.as_ref() != Some(&candidate) {
                candidates.push(candidate);
            }
            if !dir.pop() {
                break;
            }
        }
        candidates.reverse();
        candidates
    }
}

pub(crate) fn xi_core_config_dir() -> Option<PathBuf> {
    process_config_dir().map(|dir| dir.join("ee"))
}

pub(crate) fn xi_core_client_extras_dir() -> Option<PathBuf> {
    let bundled_plugins_dir = bundled_runtime_root().join("plugins");
    fs::metadata(&bundled_plugins_dir).ok().filter(|meta| meta.is_dir())?;
    Some(bundled_plugins_dir)
}

fn process_config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
}

fn bundled_runtime_root() -> PathBuf {
    let env_override = env::var_os("EE_RUNTIME_DIR").map(PathBuf::from);
    let exe_path = env::current_exe().ok();
    let fallback_dir = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    resolve_bundled_runtime_root(env_override.as_deref(), exe_path.as_deref(), &fallback_dir)
}

pub(super) fn resolve_bundled_runtime_root(
    env_override: Option<&Path>,
    exe_path: Option<&Path>,
    fallback_dir: &Path,
) -> PathBuf {
    if let Some(path) = env_override.filter(|path| !path.as_os_str().is_empty()) {
        return path.to_path_buf();
    }
    if let Some(exe_path) = exe_path {
        if cfg!(windows) {
            if let Some(parent) = exe_path.parent() {
                return parent.join("runtime");
            }
        } else if let Some(bin_dir) = exe_path.parent()
            && bin_dir.file_name().is_some_and(|name| name == "bin")
            && let Some(prefix_dir) = bin_dir.parent()
        {
            return prefix_dir.join("share").join("ee");
        }
    }
    fallback_dir.join("runtime")
}

#[derive(Debug, Clone)]
pub(super) struct ConfigProbe {
    pub(super) exists: bool,
    pub(super) root: Option<bool>,
}

#[derive(Debug, Clone)]
pub(super) struct ConfigDiscovery {
    pub(super) layers: Vec<ConfigLayer>,
    pub(super) root_stop_path: Option<PathBuf>,
}

pub(super) fn probe_config_file(path: &Path) -> ConfigProbe {
    match std::fs::read_to_string(path) {
        Ok(text) => match toml::from_str::<EeToml>(&text) {
            Ok(config) => ConfigProbe { exists: true, root: config.root },
            Err(_) => ConfigProbe { exists: true, root: None },
        },
        Err(err) if err.kind() == ErrorKind::NotFound => ConfigProbe { exists: false, root: None },
        Err(_) => ConfigProbe { exists: true, root: None },
    }
}

pub(super) fn discover_config_layers_with_env(
    env: &ConfigEnvironment,
    file_path: Option<&Path>,
) -> ConfigDiscovery {
    let workspace_candidates = env.workspace_candidate_paths(file_path);
    let mut high_to_low_layers = Vec::new();
    let mut root_stop_path = None;

    for path in workspace_candidates.iter().rev() {
        let probe = probe_config_file(path);
        if !probe.exists {
            continue;
        }
        high_to_low_layers.push(ConfigLayer {
            kind: ConfigLayerKind::Ancestor,
            path: path.clone(),
            root: probe.root,
        });
        if probe.root == Some(true) {
            root_stop_path = Some(path.clone());
            break;
        }
    }

    if root_stop_path.is_none() {
        let xdg_path = env.xdg_user_config_path();
        let xdg_exists = xdg_path.as_ref().is_some_and(|path| probe_config_file(path).exists);

        if let Some(path) = xdg_path
            && xdg_exists
        {
            let probe = probe_config_file(&path);
            high_to_low_layers.push(ConfigLayer {
                kind: ConfigLayerKind::UserXdg,
                path: path.clone(),
                root: probe.root,
            });
            if probe.root == Some(true) {
                root_stop_path = Some(path);
            }
        } else if let Some(legacy_path) = env.legacy_user_config_path() {
            let legacy_probe = probe_config_file(&legacy_path);
            if legacy_probe.exists {
                high_to_low_layers.push(ConfigLayer {
                    kind: ConfigLayerKind::UserLegacy,
                    path: legacy_path.clone(),
                    root: legacy_probe.root,
                });
                if legacy_probe.root == Some(true) {
                    root_stop_path = Some(legacy_path);
                }
            }
        }
    }

    if root_stop_path.is_none() && probe_config_file(&env.system_config_path).exists {
        high_to_low_layers.push(ConfigLayer {
            kind: ConfigLayerKind::System,
            path: env.system_config_path.clone(),
            root: Some(true),
        });
    }

    high_to_low_layers.reverse();
    ConfigDiscovery { layers: high_to_low_layers, root_stop_path }
}

pub(super) fn load_config_with_env(
    file_path: Option<&Path>,
    env: &ConfigEnvironment,
) -> EditorSettings {
    let mut settings = EditorSettings::default();
    let mut lsp = LspSettingsBuilder::default();

    for layer in discover_config_layers_with_env(env, file_path).layers {
        if let Some(patch) = parse_ee_toml(&layer.path) {
            settings.merge_toml(&patch, layer.kind);
            if let Some(lsp_patch) = &patch.lsp {
                lsp.merge_toml(lsp_patch);
            }
            for (language_id, language_patch) in &patch.languages {
                lsp.merge_language_toml(language_id, language_patch);
            }
        }
    }

    if let Some(file_path) = file_path {
        apply_editorconfig(&mut settings, file_path);
    }

    settings.lsp = lsp.finalize();
    #[cfg(any(feature = "agents", test))]
    {
        settings.agents.web_context = resolve_agent_web_context_with_env(file_path, env);
    }
    settings.finalize_agents();

    settings
}

pub(super) fn config_path_for_scope_with_env(
    scope: ConfigScope,
    env: &ConfigEnvironment,
) -> Result<PathBuf, String> {
    match scope {
        ConfigScope::Global => env
            .xdg_user_config_path()
            .ok_or_else(|| String::from("cannot resolve global config path")),
        ConfigScope::Local => Ok(env.cwd.join(".ee.toml")),
    }
}

pub(crate) fn default_config_layers(file_path: Option<&Path>) -> Vec<ConfigLayer> {
    discover_config_layers_with_env(&ConfigEnvironment::from_process(), file_path).layers
}

pub(crate) fn config_search_report(file_path: Option<&Path>) -> ConfigSearchReport {
    config_search_report_with_env(&ConfigEnvironment::from_process(), file_path)
}

pub(super) fn config_search_report_with_env(
    env: &ConfigEnvironment,
    file_path: Option<&Path>,
) -> ConfigSearchReport {
    let discovery = discover_config_layers_with_env(env, file_path);
    let workspace_candidates = env.workspace_candidate_paths(file_path);
    let xdg_path = env.xdg_user_config_path();
    let legacy_path = env.legacy_user_config_path();
    let xdg_exists = xdg_path.as_ref().is_some_and(|path| probe_config_file(path).exists);

    let mut layers = Vec::new();

    let system_probe = probe_config_file(&env.system_config_path);
    layers.push(ConfigLayerReport {
        kind: ConfigLayerKind::System,
        path: env.system_config_path.clone(),
        exists: system_probe.exists,
        loaded: discovery.layers.iter().any(|layer| layer.path == env.system_config_path),
        root: Some(true),
        note: if !system_probe.exists {
            Some(String::from("not found"))
        } else if discovery.layers.iter().any(|layer| layer.path == env.system_config_path) {
            Some(String::from("terminal fallback"))
        } else {
            discovery
                .root_stop_path
                .as_ref()
                .map(|path| format!("skipped: root=true at {}", path.display()))
        },
    });

    if let Some(path) = xdg_path {
        let probe = probe_config_file(&path);
        let loaded = discovery.layers.iter().any(|layer| layer.path == path);
        layers.push(ConfigLayerReport {
            kind: ConfigLayerKind::UserXdg,
            path,
            exists: probe.exists,
            loaded,
            root: probe.root,
            note: if !probe.exists {
                Some(String::from("not found"))
            } else if loaded {
                None
            } else {
                discovery
                    .root_stop_path
                    .as_ref()
                    .map(|stop| format!("skipped: root=true at {}", stop.display()))
            },
        });
    }

    if let Some(path) = legacy_path {
        let probe = probe_config_file(&path);
        let loaded = discovery.layers.iter().any(|layer| layer.path == path);
        layers.push(ConfigLayerReport {
            kind: ConfigLayerKind::UserLegacy,
            path,
            exists: probe.exists,
            loaded,
            root: probe.root,
            note: if xdg_exists {
                Some(String::from("skipped: XDG user config takes precedence"))
            } else if !probe.exists {
                Some(String::from("not found"))
            } else if loaded {
                Some(String::from("loaded because XDG user config is missing"))
            } else {
                discovery
                    .root_stop_path
                    .as_ref()
                    .map(|stop| format!("skipped: root=true at {}", stop.display()))
            },
        });
    }

    for path in workspace_candidates {
        let probe = probe_config_file(&path);
        let loaded = discovery.layers.iter().any(|layer| layer.path == path);
        layers.push(ConfigLayerReport {
            kind: ConfigLayerKind::Ancestor,
            path,
            exists: probe.exists,
            loaded,
            root: probe.root,
            note: if !probe.exists {
                Some(String::from("not found"))
            } else if loaded {
                None
            } else {
                discovery
                    .root_stop_path
                    .as_ref()
                    .map(|stop| format!("skipped: root=true at {}", stop.display()))
            },
        });
    }

    ConfigSearchReport {
        anchor: env.anchor_dir(file_path),
        layers,
        editorconfig_applies: file_path.is_some(),
    }
}
