//! `editor.config` service.
//!
//! Host configuration is exposed read-only. Secret-looking keys are redacted
//! before the value crosses the plugin boundary.

use ee_plugin_api::error::PluginError;
use ee_plugin_api::service::Service;
use serde_json::{Map, Value};

/// Read-only configuration access.
#[derive(Clone)]
pub struct ConfigService {
    host: Value,
}

impl ConfigService {
    pub fn new(host: Value) -> Self {
        Self { host: redact(&host) }
    }

    pub fn host_config(&self) -> Value {
        self.host.clone()
    }

    pub fn section(&self, name: &str) -> Result<Value, PluginError> {
        let Some(table) = self.host.as_object() else {
            return Err(PluginError::Schema { message: "host config is not an object".to_owned() });
        };
        Ok(table.get(name).cloned().unwrap_or(Value::Null))
    }

    pub fn plugin_config(&self, plugin: &str) -> Value {
        self.host
            .get("plugins")
            .and_then(|plugins| plugins.get(plugin))
            .cloned()
            .unwrap_or(Value::Null)
    }
}

impl Service for ConfigService {
    const ID: &'static str = "editor.config@1";
}

/// Redact values whose key looks secret.
pub fn redact(value: &Value) -> Value {
    match value {
        Value::Object(table) => {
            let mut redacted = Map::new();
            for (key, value) in table {
                if looks_secret(key) {
                    redacted.insert(key.clone(), Value::String("[redacted]".to_owned()));
                } else {
                    redacted.insert(key.clone(), redact(value));
                }
            }
            Value::Object(redacted)
        }
        Value::Array(items) => Value::Array(items.iter().map(redact).collect()),
        other => other.clone(),
    }
}

fn looks_secret(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    ["SECRET", "TOKEN", "PASSWORD", "PASSWD", "CREDENTIAL", "PRIVATE_KEY", "API_KEY"]
        .iter()
        .any(|marker| upper.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sections_are_readable() {
        let service =
            ConfigService::new(json!({"theme": {"name": "dark"}, "plugins": {"demo": {"a": 1}}}));
        assert_eq!(service.section("theme").unwrap()["name"], "dark");
        assert_eq!(service.plugin_config("demo")["a"], 1);
        assert_eq!(service.section("missing").unwrap(), Value::Null);
    }

    #[test]
    fn secrets_are_redacted() {
        let service =
            ConfigService::new(json!({"api_token": "abc", "nested": {"password": "x", "ok": 1}}));
        let host = service.host_config();
        assert_eq!(host["api_token"], "[redacted]");
        assert_eq!(host["nested"]["password"], "[redacted]");
        assert_eq!(host["nested"]["ok"], 1);
    }
}
