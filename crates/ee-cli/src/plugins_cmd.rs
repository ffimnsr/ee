//! `ee do plugins` command: list, validate, and schema.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ee_plugin_api::effect::Effect;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::host::{HostAdapter, HostKind};
use ee_plugin_api::ids::PluginId;
use ee_plugin_api::manifest::{MANIFEST_VERSION, ManifestV2, manifest_schema};
use ee_plugin_runtime::discovery::{DiscoveryOptions, RootKind};
use ee_plugin_runtime::manager::{PluginManager, PluginManagerConfig};

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginListRow {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) runtime: String,
    pub(crate) activation: String,
    pub(crate) origin: Option<PathBuf>,
    pub(crate) health: &'static str,
}

/// Diagnostics-only host adapter. Listing and validation must never execute a
/// plugin, so effects are rejected instead of silently dropped.
struct DiagnosticsAdapter;

impl HostAdapter for DiagnosticsAdapter {
    fn host_kind(&self) -> HostKind {
        HostKind::Editor
    }

    fn submit_effects(
        &self,
        _plugin: Option<&PluginId>,
        _effects: Vec<Effect>,
    ) -> Result<(), PluginError> {
        Err(PluginError::Unsupported { feature: "plugin effects during diagnostics" })
    }
}

pub(crate) fn plugin_activation_label(manifest: &ManifestV2) -> String {
    let mut labels = Vec::new();
    if manifest.activation.on_startup {
        labels.push(String::from("startup"));
    }
    labels.extend(
        manifest.activation.on_language.iter().map(|language| format!("language:{language}")),
    );
    labels
        .extend(manifest.activation.on_command.iter().map(|command| format!("command:{command}")));
    labels.extend(manifest.activation.on_event.iter().map(|event| format!("event:{event}")));
    if labels.is_empty() {
        String::from("manual")
    } else {
        labels.sort();
        labels.dedup();
        labels.join(",")
    }
}

pub(crate) fn configured_plugin_paths() -> Vec<PathBuf> {
    let config_dir = config::xi_core_config_dir().map(|path| path.join("plugins"));
    [config::xi_core_client_extras_dir(), config_dir]
        .into_iter()
        .flatten()
        .filter(|path| path.exists())
        .collect()
}

pub(crate) fn configured_discovery() -> DiscoveryOptions {
    let mut discovery = DiscoveryOptions::new();
    for (index, path) in configured_plugin_paths().iter().enumerate() {
        let kind = if index == 0 { RootKind::ClientExtra } else { RootKind::User };
        discovery.push_root(kind, path);
    }
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    discovery.push_root(RootKind::Workspace, workspace.join(".ee").join("plugins"));
    discovery.trust_workspace(false)
}

fn diagnostics_manager(discovery: DiscoveryOptions) -> PluginManager {
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let config_dir = config::xi_core_config_dir().unwrap_or_default();
    let config =
        PluginManagerConfig::new(HostKind::Editor, config_dir, workspace).with_discovery(discovery);
    PluginManager::new(Arc::new(DiagnosticsAdapter), config)
}

pub(crate) fn plugin_list_rows(manager: &PluginManager) -> Vec<PluginListRow> {
    let statuses = manager.status().plugins.into_iter().map(|status| (status.name.clone(), status));
    let statuses = statuses.collect::<std::collections::BTreeMap<_, _>>();
    let mut rows = manager
        .manifests()
        .iter()
        .map(|(name, manifest)| {
            let status = statuses.get(name);
            PluginListRow {
                name: name.clone(),
                version: manifest.version.clone(),
                runtime: manifest.kind.as_str().to_owned(),
                activation: plugin_activation_label(manifest),
                origin: status.and_then(|status| status.origin.clone()),
                health: status.map_or("loaded", |status| status.health.state.as_str()),
            }
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| left.name.cmp(&right.name));
    rows
}

fn print_diagnostics(manager: &PluginManager) {
    let diagnostics = manager.diagnostics();
    if diagnostics.is_empty() {
        return;
    }
    println!();
    println!("diagnostics");
    for diagnostic in diagnostics {
        let plugin = diagnostic.plugin.as_deref().unwrap_or("-");
        let origin = diagnostic
            .origin
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| String::from("-"));
        println!(
            "  [{}/{}/{}] plugin={} origin={} {}",
            diagnostic.class.as_str(),
            diagnostic.code,
            diagnostic.phase.as_str(),
            plugin,
            origin,
            diagnostic.message
        );
    }
}

pub(crate) fn cmd_plugins_list() {
    let discovery = configured_discovery();
    let mut manager = diagnostics_manager(discovery);
    if let Err(error) = manager.load() {
        eprintln!("failed to discover plugins: {error}");
        std::process::exit(1);
    }

    println!("ee do plugins list");
    println!("──────────────────");

    let plugin_paths = configured_plugin_paths();
    if plugin_paths.is_empty() {
        println!("No plugin directories found.");
    } else {
        println!("plugin paths");
        for path in &plugin_paths {
            println!("  {}", path.display());
        }
    }

    let rows = plugin_list_rows(&manager);
    println!();
    if rows.is_empty() {
        println!("No plugins installed.");
    } else {
        println!("installed plugins");
        for row in &rows {
            println!(
                "  {}  [version={}] [runtime={}] [activation={}] [health={}]",
                row.name, row.version, row.runtime, row.activation, row.health
            );
            if let Some(origin) = &row.origin {
                println!("    manifest: {}", origin.display());
            }
        }
    }

    let diagnostics = manager.diagnostics().len();
    print_diagnostics(&manager);
    if diagnostics > 0 {
        std::process::exit(1);
    }
}

fn manifest_path(path: &Path) -> PathBuf {
    if path.is_dir() { path.join("manifest.toml") } else { path.to_path_buf() }
}

fn validate_manifest_file(path: &Path) -> Result<ManifestV2, PluginError> {
    let path = manifest_path(path);
    let raw = std::fs::read_to_string(&path)
        .map_err(|error| PluginError::Io { message: format!("{}: {error}", path.display()) })?;
    let value: toml::Value = toml::from_str(&raw)
        .map_err(|error| PluginError::Schema { message: format!("{}: {error}", path.display()) })?;
    let version = value.get("manifest_version").and_then(toml::Value::as_integer);
    if version != Some(i64::from(MANIFEST_VERSION)) {
        return Err(PluginError::Manifest(
            ee_plugin_api::manifest::ManifestError::UnsupportedVersion {
                found: version.and_then(|version| u32::try_from(version).ok()).unwrap_or(0),
                expected: MANIFEST_VERSION,
            },
        ));
    }
    let manifest: ManifestV2 = value
        .try_into()
        .map_err(|error| PluginError::Schema { message: format!("{}: {error}", path.display()) })?;
    manifest.validate()?;
    Ok(manifest)
}

pub(crate) fn cmd_plugins_validate(path: Option<&Path>) {
    let Some(path) = path else {
        let mut manager = diagnostics_manager(configured_discovery());
        if let Err(error) = manager.load() {
            eprintln!("failed to discover plugins: {error}");
            std::process::exit(1);
        }
        print_diagnostics(&manager);
        if manager.diagnostics().is_empty() {
            println!("All discovered plugin manifests are valid.");
        } else {
            std::process::exit(1);
        }
        return;
    };

    match validate_manifest_file(path) {
        Ok(manifest) => {
            println!("Plugin {} {} is valid.", manifest.name, manifest.version);
        }
        Err(error) => {
            eprintln!("invalid plugin manifest: {error}");
            std::process::exit(1);
        }
    }
}

pub(crate) fn cmd_plugins_schema(output: &Path) {
    let schema = serde_json::to_string_pretty(&manifest_schema()).expect("manifest schema");
    if let Some(parent) = output.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        eprintln!("cannot create {}: {error}", parent.display());
        std::process::exit(1);
    }
    if let Err(error) = std::fs::write(output, format!("{schema}\n")) {
        eprintln!("cannot write {}: {error}", output.display());
        std::process::exit(1);
    }
    println!("Generated plugin manifest schema: {}", output.display());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::helpers::EnvVarGuard;
    use ee_plugin_runtime::HealthState;
    use tempfile::TempDir;

    fn write_manifest(dir: &Path, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("manifest.toml"), body).unwrap();
    }

    fn v2_manifest(name: &str) -> String {
        format!(
            "manifest_version = 2\nname = \"{name}\"\nversion = \"1.0.0\"\napi = \"ee-plugin/2\"\ntargets = [\"editor\"]\nkind = \"native\"\nentry = \"bin/{name}\"\n"
        )
    }

    #[test]
    fn list_rows_include_discovered_manifest() {
        let root = TempDir::new().unwrap();
        write_manifest(&root.path().join("demo"), &v2_manifest("demo"));
        write_manifest(&root.path().join("wasm"), &v2_manifest("wasm").replace("native", "wasm"));

        let mut manager =
            diagnostics_manager(DiscoveryOptions::new().with_root(RootKind::User, root.path()));
        manager.load().unwrap();
        let rows = plugin_list_rows(&manager);
        assert_eq!(rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(), ["demo", "wasm"]);
        assert_eq!(rows[0].runtime, "native");
        assert_eq!(rows[1].runtime, "wasm");
        assert!(manager.diagnostics().is_empty());
    }

    #[test]
    fn validate_accepts_v2_manifest_and_rejects_v1() {
        let root = TempDir::new().unwrap();
        write_manifest(&root.path().join("valid"), &v2_manifest("valid"));
        write_manifest(
            &root.path().join("v1"),
            &format!(
                "manifest_version = {}\nname = \"old\"\nversion = \"0.1.0\"\nexec_path = \"bin/old\"\n",
                1
            ),
        );

        assert!(validate_manifest_file(&root.path().join("valid")).is_ok());
        let error = validate_manifest_file(&root.path().join("v1")).unwrap_err();
        assert!(matches!(
            error,
            PluginError::Manifest(ee_plugin_api::manifest::ManifestError::UnsupportedVersion {
                found: 1,
                ..
            })
        ));
    }

    #[test]
    fn blocked_workspace_plugin_surfaces_permission_diagnostic() {
        let workspace = TempDir::new().unwrap();
        let plugin_root = workspace.path().join(".ee/plugins/workspace-demo");
        write_manifest(&plugin_root, &v2_manifest("workspace-demo"));

        let discovery = DiscoveryOptions::new()
            .with_root(RootKind::Workspace, workspace.path().join(".ee/plugins"));
        let mut manager = diagnostics_manager(discovery);
        manager.load().unwrap();

        assert!(manager.diagnostics().iter().any(|diagnostic| {
            diagnostic.class == ee_plugin_api::error::ErrorClass::Permission
                && diagnostic.plugin.as_deref() == Some("workspace-demo")
        }));
        let rows = plugin_list_rows(&manager);
        assert_eq!(rows[0].health, HealthState::Blocked.as_str());
    }

    #[test]
    fn schema_output_is_current_manifest_schema() {
        let dir = TempDir::new().unwrap();
        let output = dir.path().join("plugin-manifest-v2.json");
        cmd_plugins_schema(&output);

        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&output).unwrap()).unwrap();
        assert_eq!(written, manifest_schema());
    }

    #[test]
    fn configured_paths_ignore_missing_directories() {
        let home = TempDir::new().unwrap();
        let _guard = EnvVarGuard::set("XDG_CONFIG_HOME", home.path());
        assert!(configured_plugin_paths().iter().all(|path| path.exists()));
    }
}
