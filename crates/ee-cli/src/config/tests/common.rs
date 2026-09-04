use super::super::*;
use std::path::PathBuf;

pub(super) const AGENT_REF_TOML: &str = r#"
[agents]
enabled = true

[agents.servers.gh]
command = "agent-bin"
env = { OPENROUTER_API_KEY = "secret://openrouter-api-key", LANG = "en_US.UTF-8" }
"#;
pub(super) fn load_for(env: &ConfigEnvironment) -> EditorSettings {
    load_config_with_env(None, env)
}

pub(super) fn layer_paths(layers: &[ConfigLayer]) -> Vec<PathBuf> {
    layers.iter().map(|layer| layer.path.clone()).collect()
}
