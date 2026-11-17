//! Bounded agent configuration-file loading and `${VAR}` interpolation.
//!
//! Both provider agents read one JSON config file from the ee config home. The
//! loader here owns the mechanics so the two agents cannot drift:
//!
//! - the file is size-bounded before it is parsed;
//! - only a JSON object is accepted;
//! - `${VAR}` references resolve only against an explicit allowlist, and an
//!   unresolved reference fails closed with the variable *name* (never a value);
//! - resolved values are never echoed by this module.

use std::path::Path;

use serde_json::Value;

/// Largest accepted configuration file, in bytes.
pub const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Reads and parses one bounded JSON object configuration file.
///
/// Returns `Ok(None)` when the file does not exist; a missing file is a normal
/// state, not an error.
///
/// # Errors
///
/// Returns a diagnostic naming the path and the failure, never file contents.
pub fn read_document(path: &Path) -> Result<Option<Value>, String> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "configuration file {} could not be inspected: {error}",
                path.display()
            ));
        }
    };
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(format!(
            "configuration file {} is {} bytes; the limit is {MAX_CONFIG_BYTES}",
            path.display(),
            metadata.len()
        ));
    }
    let text = std::fs::read_to_string(path).map_err(|error| {
        format!("configuration file {} could not be read: {error}", path.display())
    })?;
    let value: Value = serde_json::from_str(&text).map_err(|error| {
        format!("configuration file {} is not valid JSON: {error}", path.display())
    })?;
    if !value.is_object() {
        return Err(format!(
            "configuration file {} must contain a JSON object at the top level",
            path.display()
        ));
    }
    Ok(Some(value))
}

/// Resolves every `${VAR}` reference in `value` against `allowed`.
///
/// `lookup` is called only for allowlisted names. A reference to a name that is
/// not allowlisted, or to an allowlisted name with no value, fails closed.
///
/// # Errors
///
/// Returns a diagnostic naming the unresolved variable.
pub fn interpolate(
    value: &mut Value,
    allowed: &[&str],
    lookup: &mut dyn FnMut(&str) -> Option<String>,
) -> Result<(), String> {
    match value {
        Value::String(text) => {
            *text = resolve_text(text, allowed, lookup)?;
            Ok(())
        }
        Value::Array(items) => {
            for item in items {
                interpolate(item, allowed, lookup)?;
            }
            Ok(())
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                interpolate(item, allowed, lookup)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Resolves `${VAR}` references inside one string.
fn resolve_text(
    text: &str,
    allowed: &[&str],
    lookup: &mut dyn FnMut(&str) -> Option<String>,
) -> Result<String, String> {
    if !text.contains("${") {
        return Ok(text.to_string());
    }
    let mut resolved = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        resolved.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            // An unterminated reference is a configuration error, not literal
            // text: silently keeping it would send `${...}` to a provider.
            return Err(String::from("unterminated ${VAR} reference"));
        };
        let name = &after[..end];
        if name.is_empty() || !is_identifier(name) {
            return Err(format!("invalid configuration variable reference {name:?}"));
        }
        if !allowed.contains(&name) {
            return Err(format!(
                "configuration variable {name} is not allowed here: expected one of {}",
                allowed.join(", ")
            ));
        }
        let value = lookup(name)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("configuration variable {name} is not set"))?;
        resolved.push_str(&value);
        rest = &after[end + 1..];
    }
    resolved.push_str(rest);
    Ok(resolved)
}

/// Whether `name` is a legal environment-variable identifier.
fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lookup(name: &str) -> Option<String> {
        match name {
            "OPEN_TOKEN" => Some(String::from("secret-value")),
            _ => None,
        }
    }

    #[test]
    fn interpolation_resolves_only_allowlisted_names() {
        let mut value = json!({ "key": "${OPEN_TOKEN}", "nested": ["x-${OPEN_TOKEN}"] });
        interpolate(&mut value, &["OPEN_TOKEN"], &mut lookup).expect("resolves");
        assert_eq!(value["key"], "secret-value");
        assert_eq!(value["nested"][0], "x-secret-value");
    }

    #[test]
    fn missing_and_unlisted_variables_fail_with_the_name_only() {
        let mut missing = json!({ "key": "${OPEN_TOKEN}" });
        let error =
            interpolate(&mut missing, &["OPEN_TOKEN"], &mut |_| None).expect_err("missing fails");
        assert!(error.contains("OPEN_TOKEN"), "{error}");

        let mut unlisted = json!({ "key": "${OTHER_TOKEN}" });
        let error =
            interpolate(&mut unlisted, &["OPEN_TOKEN"], &mut lookup).expect_err("unlisted fails");
        assert!(error.contains("OTHER_TOKEN"), "{error}");
        assert!(!error.contains("secret-value"), "{error}");
    }

    #[test]
    fn unterminated_reference_is_an_error() {
        let mut value = json!({ "key": "${OPEN_TOKEN" });
        assert!(interpolate(&mut value, &["OPEN_TOKEN"], &mut lookup).is_err());
    }

    #[test]
    fn plain_strings_pass_through_unchanged() {
        let mut value = json!({ "key": "plain", "n": 3, "b": true });
        interpolate(&mut value, &[], &mut |_| None).expect("no references");
        assert_eq!(value["key"], "plain");
    }
}
