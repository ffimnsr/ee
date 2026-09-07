//! Raw-field validation and expiry/uses parsing.
use super::*;

// ── Raw → domain validation ──────────────────────────────────────────────────

pub(crate) fn require_non_empty(field: &str, value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    Ok(value.to_string())
}

/// Audit-safe stable id: 1-80 ASCII letters, digits, `_`, or `-`.
pub(crate) fn validate_rule_id(value: &str) -> Result<String, String> {
    if value.is_empty() || value.len() > 80 {
        return Err("id must contain 1 to 80 ASCII characters".to_string());
    }
    if !value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')) {
        return Err("id must contain only ASCII alphanumeric, _, or - characters".to_string());
    }
    Ok(value.to_string())
}

pub(crate) fn optional_non_empty(
    field: &str,
    value: Option<String>,
) -> Result<Option<String>, String> {
    value.map(|value| require_non_empty(field, &value)).transpose()
}

pub(crate) fn validate_no_control(field: &str, value: &str) -> Result<(), String> {
    if value.chars().any(|c| c.is_control() || c == '\u{0}') {
        return Err(format!("{field} contains control characters"));
    }
    Ok(())
}

pub(crate) fn parse_required_expiry(raw: Option<String>) -> Result<SystemTime, String> {
    raw.ok_or_else(|| "expires_at is required".to_string()).and_then(|text| parse_expiry(&text))
}

pub(crate) fn parse_optional_expiry(raw: Option<String>) -> Result<Option<SystemTime>, String> {
    raw.as_deref().map(parse_expiry).transpose()
}

pub(crate) fn parse_required_uses(raw: Option<u64>) -> Result<u64, String> {
    let uses = raw.ok_or_else(|| "max_uses is required".to_string())?;
    if uses == 0 {
        return Err("max_uses must be at least 1".to_string());
    }
    Ok(uses)
}

pub(crate) fn parse_optional_uses(raw: Option<u64>) -> Result<Option<u64>, String> {
    match raw {
        None => Ok(None),
        Some(0) => Err("max_uses must be at least 1".to_string()),
        Some(uses) => Ok(Some(uses)),
    }
}

pub(crate) fn parse_effect_scope(
    effect: TrustEffect,
    expires_at: Option<String>,
    max_uses: Option<u64>,
    bounded_allow: bool,
) -> Result<(Option<SystemTime>, Option<u64>), String> {
    match effect {
        TrustEffect::Allow if bounded_allow => {
            Ok((Some(parse_required_expiry(expires_at)?), Some(parse_required_uses(max_uses)?)))
        }
        TrustEffect::Allow => {
            Ok((parse_optional_expiry(expires_at)?, parse_optional_uses(max_uses)?))
        }
        TrustEffect::Deny | TrustEffect::Confirm => {
            if max_uses.is_some() {
                return Err("max_uses is valid only for allow rules".to_string());
            }
            Ok((parse_optional_expiry(expires_at)?, None))
        }
    }
}

pub(crate) fn parse_positive(field: &str, value: u64) -> Result<u64, String> {
    if value == 0 {
        return Err(format!("{field} must be at least 1"));
    }
    Ok(value)
}

pub(crate) fn is_zero(value: &u64) -> bool {
    *value == 0
}

pub(crate) fn parse_allow_ceiling(
    effect: TrustEffect,
    field: &str,
    value: u64,
) -> Result<u64, String> {
    if effect != TrustEffect::Allow {
        return Ok(0);
    }
    parse_positive(field, value)
}

pub(crate) fn parse_path_prefix(raw: String) -> Result<PathPrefix, String> {
    PathPrefix::parse(&raw)
}

pub(crate) fn validate_category(
    category: Option<TrustCategory>,
) -> Result<Option<TrustCategory>, String> {
    if category == Some(TrustCategory::Unknown) {
        return Err("unknown category cannot scope a rule".to_string());
    }
    Ok(category)
}

pub(crate) fn parse_identity_fields(
    server: &str,
    transport_identity: &str,
    tool: &str,
    tool_schema_version: u64,
) -> Result<(), String> {
    require_non_empty("server", server)?;
    validate_no_control("server", server)?;
    require_non_empty("transport_identity", transport_identity)?;
    validate_no_control("transport_identity", transport_identity)?;
    require_non_empty("tool", tool)?;
    validate_no_control("tool", tool)?;
    if tool_schema_version == 0 {
        return Err("tool_schema_version must be at least 1".to_string());
    }
    Ok(())
}

pub(crate) fn parse_expiry(text: &str) -> Result<SystemTime, String> {
    let parsed = chrono::DateTime::parse_from_rfc3339(text)
        .map_err(|error| format!("invalid expires_at {text:?}: {error}"))?;
    Ok(parsed.with_timezone(&chrono::Utc).into())
}

pub(crate) fn format_expiry(time: SystemTime) -> String {
    let datetime: chrono::DateTime<chrono::Utc> = time.into();
    datetime.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
