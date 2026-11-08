//! Native plugin entry points.

use ee_plugin_api::context::{Plugin, PluginContext};
use ee_plugin_api::error::PluginError;

use crate::config::PluginConfig;

/// Apply a plugin using configuration from the process environment.
///
/// Phase 0 runs a headless apply. The runtime transport loop that dispatches
/// events, commands, and contribution requests is owned by `ee-plugin-runtime`.
pub fn run<P: Plugin>(plugin: P) -> Result<(), PluginError> {
    let config = PluginConfig::from_env()?;
    run_with_config(plugin, config)
}

/// Apply a plugin with explicit configuration.
pub fn run_with_config<P: Plugin>(mut plugin: P, config: PluginConfig) -> Result<(), PluginError> {
    let context =
        PluginContext::new(config.plugin_id.as_str()).configured_with(config.config.clone());
    plugin.apply(&context)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use ee_plugin_api::host::HostKind;
    use ee_plugin_api::manifest::Limits;

    use super::*;

    struct AppliedPlugin {
        applied: Arc<AtomicBool>,
    }

    impl Plugin for AppliedPlugin {
        fn name(&self) -> &str {
            "applied"
        }

        fn apply(&mut self, _ctx: &PluginContext) -> Result<(), PluginError> {
            self.applied.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    fn config() -> PluginConfig {
        PluginConfig {
            plugin_id: ee_plugin_api::PluginId::new("applied").unwrap(),
            host: HostKind::Editor,
            api: ee_plugin_api::API_VERSION.to_owned(),
            config: serde_json::json!({"enabled": true}),
            limits: Limits::default(),
            services: Vec::new(),
        }
    }

    #[test]
    fn run_with_config_applies_plugin() {
        let applied = Arc::new(AtomicBool::new(false));
        let plugin = AppliedPlugin { applied: Arc::clone(&applied) };
        run_with_config(plugin, config()).unwrap();
        assert!(applied.load(Ordering::SeqCst));
    }
}
