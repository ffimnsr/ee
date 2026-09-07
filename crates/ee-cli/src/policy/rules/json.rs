//! Strict arguments_json parsing and canonicalization.
use super::*;

// ── Canonical arguments_json ─────────────────────────────────────────────────

/// Strict JSON value: duplicate object keys anywhere in the payload are
/// rejected, unlike `serde_json::Value` which silently keeps the last one.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StrictJson {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<StrictJson>),
    Object(std::collections::BTreeMap<String, StrictJson>),
}

impl<'de> Deserialize<'de> for StrictJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(StrictJsonVisitor)
    }
}

pub(crate) struct StrictJsonVisitor;

impl<'de> Visitor<'de> for StrictJsonVisitor {
    type Value = StrictJson;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_unit<E>(self) -> Result<StrictJson, E> {
        Ok(StrictJson::Null)
    }

    fn visit_none<E>(self) -> Result<StrictJson, E> {
        Ok(StrictJson::Null)
    }

    fn visit_bool<E>(self, value: bool) -> Result<StrictJson, E> {
        Ok(StrictJson::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<StrictJson, E> {
        Ok(StrictJson::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<StrictJson, E> {
        Ok(StrictJson::Number(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<StrictJson, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(StrictJson::Number)
            .ok_or_else(|| E::custom("invalid JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<StrictJson, E> {
        Ok(StrictJson::String(value.to_string()))
    }

    fn visit_string<E>(self, value: String) -> Result<StrictJson, E> {
        Ok(StrictJson::String(value))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<StrictJson, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        let mut items = Vec::new();
        while let Some(item) = sequence.next_element::<StrictJson>()? {
            items.push(item);
        }
        Ok(StrictJson::Array(items))
    }

    fn visit_map<A>(self, mut map: A) -> Result<StrictJson, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut entries = std::collections::BTreeMap::new();
        while let Some(key) = map.next_key::<String>()? {
            if entries.contains_key(&key) {
                return Err(A::Error::custom(format!("duplicate object key: {key}")));
            }
            let value = map.next_value::<StrictJson>()?;
            entries.insert(key, value);
        }
        Ok(StrictJson::Object(entries))
    }
}

pub(crate) fn is_sensitive_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    SENSITIVE_KEY_MARKERS.iter().any(|marker| upper.contains(marker))
}

pub(crate) fn reject_sensitive(value: &StrictJson) -> Result<(), String> {
    match value {
        StrictJson::Object(entries) => {
            for (key, value) in entries {
                if key.chars().any(|c| c.is_control() || c == '\u{0}') {
                    return Err("argument key contains control characters".to_string());
                }
                if is_sensitive_key(key) {
                    return Err(format!("sensitive argument key: {key}"));
                }
                reject_sensitive(value)?;
            }
            Ok(())
        }
        StrictJson::Array(items) => {
            for item in items {
                reject_sensitive(item)?;
            }
            Ok(())
        }
        StrictJson::String(text) => {
            if text.chars().any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r')) {
                return Err("string value contains binary or control characters".to_string());
            }
            Ok(())
        }
        StrictJson::Null | StrictJson::Bool(_) | StrictJson::Number(_) => Ok(()),
    }
}

pub(crate) fn strict_json_to_value(value: StrictJson) -> serde_json::Value {
    match value {
        StrictJson::Null => serde_json::Value::Null,
        StrictJson::Bool(value) => serde_json::Value::Bool(value),
        StrictJson::Number(value) => serde_json::Value::Number(value),
        StrictJson::String(value) => serde_json::Value::String(value),
        StrictJson::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(strict_json_to_value).collect())
        }
        StrictJson::Object(entries) => serde_json::Value::Object(
            entries.into_iter().map(|(key, value)| (key, strict_json_to_value(value))).collect(),
        ),
    }
}
/// Parses, validates, and canonicalizes `arguments_json`: it must be a JSON
/// object, free of duplicate keys, sensitive keys, binary/control content,
/// and oversized payloads.  The canonical form sorts object keys and uses
/// compact whitespace.
pub(crate) fn canonicalize_arguments_json(raw: &str) -> Result<String, String> {
    if raw.len() > MAX_ARGUMENTS_JSON_BYTES {
        return Err(format!("arguments_json exceeds the {MAX_ARGUMENTS_JSON_BYTES} byte cap"));
    }
    let parsed: StrictJson = serde_json::from_str(raw)
        .map_err(|error| format!("arguments_json is not valid JSON: {error}"))?;
    if !matches!(parsed, StrictJson::Object(_)) {
        return Err("arguments_json must be a JSON object".to_string());
    }
    reject_sensitive(&parsed)?;
    Ok(strict_json_to_value(parsed).to_string())
}
