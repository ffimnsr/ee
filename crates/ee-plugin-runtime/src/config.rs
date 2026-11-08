//! Layered plugin configuration and safe interpolation.
//!
//! Layers merge in ascending precedence: host, workspace, manifest defaults,
//! runtime override. Interpolation only accepts the documented allowlist and
//! never resolves secret-looking environment variables.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use ee_plugin_api::error::PluginError;
use ee_plugin_api::manifest::ManifestV2;
use serde_json::Value;

/// Context used while interpolating configuration strings.
#[derive(Debug, Clone)]
pub struct InterpolationContext {
    pub env: BTreeMap<String, String>,
    pub env_allowlist: BTreeSet<String>,
    pub config_dir: PathBuf,
    pub workspace: PathBuf,
    pub plugin_dir: PathBuf,
}

impl InterpolationContext {
    pub fn new(config_dir: impl Into<PathBuf>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            env: BTreeMap::new(),
            env_allowlist: BTreeSet::new(),
            config_dir: config_dir.into(),
            workspace: workspace.into(),
            plugin_dir: PathBuf::new(),
        }
    }

    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }

    pub fn allow_env(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.env_allowlist.extend(names);
        self
    }

    pub fn plugin_dir(mut self, plugin_dir: impl Into<PathBuf>) -> Self {
        self.plugin_dir = plugin_dir.into();
        self
    }
}

/// Inputs for one plugin's configuration.
#[derive(Debug, Clone, Default)]
pub struct ConfigLayers {
    pub host: Option<Value>,
    pub workspace: Option<Value>,
    pub runtime: Option<Value>,
}

impl ConfigLayers {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn host(mut self, value: Option<&Value>) -> Self {
        self.host = value.cloned();
        self
    }

    pub fn workspace(mut self, value: Option<&Value>) -> Self {
        self.workspace = value.cloned();
        self
    }

    pub fn runtime(mut self, value: Option<&Value>) -> Self {
        self.runtime = value.cloned();
        self
    }
}

/// Resolve one plugin's effective configuration.
///
/// The manifest schema is validated after interpolation. A plugin without a
/// schema still gets the merged value; validation is skipped.
pub fn resolve_plugin_config(
    manifest: &ManifestV2,
    layers: &ConfigLayers,
    context: &InterpolationContext,
) -> Result<Value, PluginError> {
    let mut merged = Value::Object(serde_json::Map::new());
    for layer in [
        layers.host.as_ref(),
        layers.workspace.as_ref(),
        manifest.config.defaults.as_ref(),
        layers.runtime.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        merge_into(&mut merged, layer);
    }

    let interpolated = interpolate(&merged, context)?;
    validate_config(manifest.config.schema.as_ref(), &interpolated)?;
    Ok(interpolated)
}

/// Deep merge `source` into `target`. Objects merge recursively; every other
/// value replaces the target.
pub fn merge_into(target: &mut Value, source: &Value) {
    match (target, source) {
        (Value::Object(target), Value::Object(source)) => {
            for (key, value) in source {
                match target.get_mut(key) {
                    Some(existing) => merge_into(existing, value),
                    None => {
                        target.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (target, source) => *target = source.clone(),
    }
}

/// Interpolate every string in `value`.
pub fn interpolate(value: &Value, context: &InterpolationContext) -> Result<Value, PluginError> {
    match value {
        Value::String(text) => Ok(Value::String(interpolate_string(text, context)?)),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(interpolate(item, context)?);
            }
            Ok(Value::Array(out))
        }
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, item) in map {
                out.insert(key.clone(), interpolate(item, context)?);
            }
            Ok(Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

/// Interpolate a single string.
pub fn interpolate_string(
    text: &str,
    context: &InterpolationContext,
) -> Result<String, PluginError> {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err(config_error(format!("unterminated interpolation in `{text}`")));
        };
        let expression = &after[..end];
        if expression.contains("${") {
            return Err(config_error(format!(
                "nested interpolation is not allowed: `${{{expression}}}`"
            )));
        }
        output.push_str(&resolve_expression(expression, context)?);
        rest = &after[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn resolve_expression(
    expression: &str,
    context: &InterpolationContext,
) -> Result<String, PluginError> {
    let Some((kind, name)) = expression.split_once(':') else {
        return Err(config_error(format!("unknown interpolation `${{{expression}}}`")));
    };
    if name.is_empty() {
        return Err(config_error(format!("empty interpolation variable in `${{{expression}}}`")));
    }

    match kind {
        "env" => resolve_env(name, context),
        "host" => match name {
            "config_dir" => Ok(context.config_dir.display().to_string()),
            "workspace" => Ok(context.workspace.display().to_string()),
            _ => Err(config_error(format!("unknown host interpolation `${{{expression}}}`"))),
        },
        "plugin" => match name {
            "dir" => Ok(context.plugin_dir.display().to_string()),
            _ => Err(config_error(format!("unknown plugin interpolation `${{{expression}}}`"))),
        },
        _ => Err(config_error(format!("unknown interpolation `${{{expression}}}`"))),
    }
}

fn resolve_env(name: &str, context: &InterpolationContext) -> Result<String, PluginError> {
    if !valid_env_name(name) {
        return Err(config_error(format!("invalid environment variable name `{name}`")));
    }
    if looks_secret(name) {
        return Err(config_error(format!(
            "secret environment variable `{name}` may not be interpolated"
        )));
    }
    if !context.env_allowlist.contains(name) {
        return Err(config_error(format!("environment variable `{name}` is not allowlisted")));
    }
    context
        .env
        .get(name)
        .cloned()
        .ok_or_else(|| config_error(format!("unresolved interpolation `${{env:{name}}}`")))
}

fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn looks_secret(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    const MARKERS: [&str; 7] =
        ["SECRET", "TOKEN", "PASSWORD", "PASSWD", "CREDENTIAL", "PRIVATE_KEY", "API_KEY"];
    MARKERS.iter().any(|marker| upper.contains(marker))
}

/// Validate `config` against an optional JSON schema.
pub fn validate_config(schema: Option<&Value>, config: &Value) -> Result<(), PluginError> {
    let Some(schema) = schema else {
        return Ok(());
    };
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| PluginError::Schema { message: format!("invalid schema: {error}") })?;
    if let Some(error) = validator.iter_errors(config).next() {
        return Err(PluginError::Schema {
            message: format!("config at `{}`: {error}", error.instance_path),
        });
    }
    Ok(())
}

fn config_error(message: String) -> PluginError {
    PluginError::Config { message }
}

/// Extract the `[plugins.<name>.config]` table from a host or workspace value.
pub fn plugin_config_table<'a>(root: &'a Value, name: &str) -> Option<&'a Value> {
    root.get("plugins")?.get(name)?.get("config")
}

/// Extract `[plugins.<name>].enabled`, defaulting to `true`.
pub fn plugin_enabled(root: &Value, name: &str) -> Option<bool> {
    root.get("plugins")?.get(name)?.get("enabled")?.as_bool()
}

/// Resolve `${plugin:dir}` against a manifest directory.
pub fn context_for_plugin(
    mut context: InterpolationContext,
    plugin_dir: &Path,
) -> InterpolationContext {
    context.plugin_dir = plugin_dir.to_path_buf();
    context
}

#[cfg(test)]
mod tests {
    use ee_plugin_api::manifest::{ManifestV2, PluginKind};
    use serde_json::json;

    use super::*;

    fn context() -> InterpolationContext {
        InterpolationContext::new("/home/user/.config/ee", "/work/space")
            .plugin_dir("/plugins/demo")
            .with_env(BTreeMap::from([
                ("HOME".to_owned(), "/home/user".to_owned()),
                ("MODE".to_owned(), "debug".to_owned()),
                ("API_TOKEN".to_owned(), "top-secret".to_owned()),
            ]))
            .allow_env(vec!["HOME".to_owned(), "MODE".to_owned()])
    }

    fn manifest(defaults: Option<Value>, schema: Option<Value>) -> ManifestV2 {
        ManifestV2 {
            manifest_version: 2,
            name: "demo".to_owned(),
            version: "1.0.0".to_owned(),
            api: "ee-plugin/2".to_owned(),
            targets: vec![ee_plugin_api::manifest::HostTarget::Editor],
            kind: PluginKind::Builtin,
            entry: "builtin:demo".to_owned(),
            launch: Default::default(),
            activation: Default::default(),
            requires: Default::default(),
            provides: Default::default(),
            permissions: Default::default(),
            limits: Default::default(),
            contributes: Default::default(),
            config: ee_plugin_api::manifest::ConfigSchema { schema, defaults },
        }
    }

    #[test]
    fn layers_merge_in_precedence_order() {
        let manifest = manifest(Some(json!({"b": {"default": true}, "c": 3})), None);
        let layers = ConfigLayers::new()
            .host(Some(&json!({"a": 1, "b": {"x": 1}})))
            .workspace(Some(&json!({"b": {"y": 2}})))
            .runtime(Some(&json!({"a": 9})));
        let resolved = resolve_plugin_config(&manifest, &layers, &context()).unwrap();
        assert_eq!(resolved, json!({"a": 9, "b": {"x": 1, "y": 2, "default": true}, "c": 3}));
    }

    #[test]
    fn interpolation_supports_every_allowed_form() {
        let value = json!({
            "home": "${env:HOME}",
            "mode": "prefix-${env:MODE}-suffix",
            "config": "${host:config_dir}/plugins",
            "workspace": "${host:workspace}",
            "dir": "${plugin:dir}",
        });
        let resolved = interpolate(&value, &context()).unwrap();
        assert_eq!(resolved["home"], "/home/user");
        assert_eq!(resolved["mode"], "prefix-debug-suffix");
        assert_eq!(resolved["config"], "/home/user/.config/ee/plugins");
        assert_eq!(resolved["dir"], "/plugins/demo");
    }

    #[test]
    fn unknown_expression_is_rejected() {
        let error = interpolate(&json!("${shell:rm -rf /}"), &context()).unwrap_err();
        assert!(matches!(error, PluginError::Config { .. }));
    }

    #[test]
    fn unresolved_and_non_allowlisted_env_is_rejected() {
        let error = interpolate(&json!("${env:MISSING}"), &context()).unwrap_err();
        assert!(error.to_string().contains("not allowlisted"));

        let mut context = context();
        context.env_allowlist.insert("MISSING".to_owned());
        let error = interpolate(&json!("${env:MISSING}"), &context).unwrap_err();
        assert!(error.to_string().contains("unresolved"));
    }

    #[test]
    fn secret_env_names_are_never_interpolated() {
        let mut context = context();
        context.env_allowlist.insert("API_TOKEN".to_owned());
        let error = interpolate(&json!("${env:API_TOKEN}"), &context).unwrap_err();
        assert!(error.to_string().contains("secret"));
    }

    #[test]
    fn schema_failure_is_reported() {
        let manifest = manifest(
            None,
            Some(
                json!({"type": "object", "required": ["enabled"], "properties": {"enabled": {"type": "boolean"}}}),
            ),
        );
        let layers = ConfigLayers::new().runtime(Some(&json!({"enabled": "yes"})));
        let error = resolve_plugin_config(&manifest, &layers, &context()).unwrap_err();
        assert_eq!(error.class(), ee_plugin_api::error::ErrorClass::Schema);
    }

    #[test]
    fn config_reload_reflects_new_runtime_layer() {
        let manifest = manifest(None, None);
        let first = resolve_plugin_config(
            &manifest,
            &ConfigLayers::new().runtime(Some(&json!({"value": 1}))),
            &context(),
        )
        .unwrap();
        let second = resolve_plugin_config(
            &manifest,
            &ConfigLayers::new().runtime(Some(&json!({"value": 2}))),
            &context(),
        )
        .unwrap();
        assert_eq!(first["value"], 1);
        assert_eq!(second["value"], 2);
    }

    #[test]
    fn enabled_and_config_tables_are_extracted() {
        let root = json!({"plugins": {"demo": {"enabled": false, "config": {"x": 1}}}});
        assert_eq!(plugin_enabled(&root, "demo"), Some(false));
        assert_eq!(plugin_config_table(&root, "demo"), Some(&json!({"x": 1})));
        assert_eq!(plugin_enabled(&json!({}), "demo"), None);
    }
}
