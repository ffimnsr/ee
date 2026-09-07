//! `ee do plugins list` command.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginListRow {
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) runtime: &'static str,
    pub(crate) activation: String,
    pub(crate) commands: usize,
    pub(crate) running: bool,
}

pub(crate) fn plugin_runtime_label(plugin: &plugin_manifest::PluginDescription) -> &'static str {
    match plugin.runtime {
        plugin_manifest::PluginRuntime::Native => "native",
        plugin_manifest::PluginRuntime::Wasm => "wasm",
    }
}

pub(crate) fn plugin_activation_label(plugin: &plugin_manifest::PluginDescription) -> String {
    if matches!(plugin.scope, plugin_manifest::PluginScope::SingleInvocation) {
        return String::from("single-invocation");
    }

    if plugin.activations.is_empty() {
        return String::from("startup");
    }

    let mut labels = Vec::new();
    for activation in &plugin.activations {
        match activation {
            plugin_manifest::PluginActivation::Autorun => labels.push(String::from("startup")),
            plugin_manifest::PluginActivation::OnCommand => {
                labels.push(String::from("command"));
            }
            plugin_manifest::PluginActivation::OnSyntax(language) => {
                labels.push(format!("syntax:{}", language.as_ref()));
            }
        }
    }
    labels.sort();
    labels.dedup();
    labels.join(",")
}

pub(crate) fn plugin_list_rows(catalog: &PluginCatalog) -> Vec<PluginListRow> {
    let mut rows = catalog
        .iter()
        .map(|plugin| PluginListRow {
            name: plugin.name.clone(),
            version: plugin.version.clone(),
            runtime: plugin_runtime_label(&plugin),
            activation: plugin_activation_label(&plugin),
            commands: plugin.commands.len(),
            running: false,
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| left.name.cmp(&right.name));
    rows
}

pub(crate) fn probe_running_plugins() -> io::Result<Vec<String>> {
    let (_config, general_config, initial_overrides) = config::xi_config_tables_for_file(None);
    let lsp_config = config::lsp_config_table_for_file(None);
    let mut backend = buffer::BufferManager::new_with_initial_config(
        None,
        general_config,
        initial_overrides,
        lsp_config,
    )?;
    backend.drain_events()?;

    let mut plugins = backend
        .available_plugins_for_current_view()
        .iter()
        .filter(|plugin| plugin.running)
        .map(|plugin| plugin.name.clone())
        .collect::<Vec<_>>();
    plugins.sort();
    plugins.dedup();
    Ok(plugins)
}

pub(crate) fn configured_plugin_paths() -> Vec<PathBuf> {
    let config_dir = config::xi_core_config_dir().map(|path| path.join("plugins"));
    [config::xi_core_client_extras_dir(), config_dir]
        .into_iter()
        .flatten()
        .filter(|path| path.exists())
        .collect()
}

pub(crate) fn cmd_plugins_list() {
    let plugin_paths = configured_plugin_paths();
    let mut catalog = PluginCatalog::default();
    let errors = catalog.reload_from_paths(&plugin_paths);
    let mut rows = plugin_list_rows(&catalog);
    let running_plugins = probe_running_plugins().unwrap_or_else(|error| {
        eprintln!("warning: failed probing running plugins: {error}");
        Vec::new()
    });
    let running_names = running_plugins.iter().cloned().collect::<std::collections::HashSet<_>>();
    rows.iter_mut().for_each(|row| {
        row.running = running_names.contains(&row.name);
    });

    println!("ee do plugins list");
    println!("──────────────────");

    if plugin_paths.is_empty() {
        println!("No plugin directories found.");
    } else {
        println!("plugin paths");
        for path in &plugin_paths {
            println!("  {}", path.display());
        }
    }

    println!();
    if rows.is_empty() {
        println!("No plugins installed.");
    } else {
        println!("installed plugins");
        for row in rows {
            println!(
                "  {}  [version={}] [runtime={}] [activation={}] [commands={}] [running={}]",
                row.name,
                row.version,
                row.runtime,
                row.activation,
                row.commands,
                if row.running { "yes" } else { "no" }
            );
        }
    }

    println!();
    println!("running plugins");
    if running_plugins.is_empty() {
        println!("  none");
    } else {
        for plugin in running_plugins {
            println!("  {plugin}");
        }
    }

    if !errors.is_empty() {
        println!();
        println!("load errors");
        for error in errors {
            println!("  {error}");
        }
        std::process::exit(1);
    }
}
