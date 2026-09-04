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

use super::discovery::{ConfigEnvironment, ConfigScope, config_path_for_scope_with_env};
use super::init::validate_config_contents;
use super::raw::parse_config_document;
use std::fs;
use std::path::PathBuf;

fn get_value_at_path<'a>(value: &'a toml::Value, key: &str) -> Option<&'a toml::Value> {
    if key.trim().is_empty() {
        return Some(value);
    }
    let mut current = value;
    for part in key.split('.') {
        let table = current.as_table()?;
        current = table.get(part)?;
    }
    Some(current)
}

pub(super) fn ensure_table(
    value: &mut toml::Value,
) -> Result<&mut toml::map::Map<String, toml::Value>, String> {
    match value {
        toml::Value::Table(table) => Ok(table),
        _ => Err(String::from("config root must be table")),
    }
}

fn set_value_at_path(root: &mut toml::Value, key: &str, value: toml::Value) -> Result<(), String> {
    let mut parts = key.split('.').peekable();
    if parts.peek().is_none() {
        return Err(String::from("config key must not be empty"));
    }
    let mut current = ensure_table(root)?;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            current.insert(part.to_string(), value);
            return Ok(());
        }
        let entry = current
            .entry(part.to_string())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        current = match entry {
            toml::Value::Table(table) => table,
            _ => return Err(format!("config key `{part}` already exists and is not table")),
        };
    }
    Ok(())
}

pub(super) fn get_config_value_with_env(
    scope: ConfigScope,
    key: &str,
    env: &ConfigEnvironment,
) -> Result<Option<String>, String> {
    let path = config_path_for_scope_with_env(scope, env)?;
    let document = parse_config_document(&path)?;
    get_value_at_path(&document, key)
        .map(ToString::to_string)
        .ok_or_else(|| format!("config key `{key}` not found in {}", path.display()))
        .map(Some)
}

pub(crate) fn get_config_value(scope: ConfigScope, key: &str) -> Result<Option<String>, String> {
    get_config_value_with_env(scope, key, &ConfigEnvironment::from_process())
}

fn parse_set_value(raw: &str) -> toml::Value {
    let wrapped = format!("value = {raw}");
    toml::from_str::<toml::Value>(&wrapped)
        .ok()
        .and_then(|value| value.get("value").cloned())
        .unwrap_or_else(|| toml::Value::String(raw.to_string()))
}

pub(super) fn set_config_value_with_env(
    scope: ConfigScope,
    key: &str,
    raw_value: &str,
    env: &ConfigEnvironment,
) -> Result<PathBuf, String> {
    let path = config_path_for_scope_with_env(scope, env)?;
    let mut document = parse_config_document(&path)?;
    set_value_at_path(&mut document, key, parse_set_value(raw_value))?;
    let text = toml::to_string_pretty(&document)
        .map_err(|err| format!("cannot serialize config {}: {err}", path.display()))?;
    validate_config_contents(&path, &text)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("Cannot create {}: {err}", parent.display()))?;
    }
    fs::write(&path, text).map_err(|err| format!("Cannot write {}: {err}", path.display()))?;
    Ok(path)
}

pub(crate) fn set_config_value(
    scope: ConfigScope,
    key: &str,
    raw_value: &str,
) -> Result<PathBuf, String> {
    set_config_value_with_env(scope, key, raw_value, &ConfigEnvironment::from_process())
}
