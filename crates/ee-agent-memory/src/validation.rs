use sha2::{Digest, Sha256};

use crate::{FactAuthority, FactFreshness, FactState, MemoryError, MemoryQuotas, NewWorkspaceFact};

pub(crate) fn normalize_component(value: &str) -> Result<String, MemoryError> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.is_empty() || normalized.len() > 128 {
        return Err(MemoryError::InvalidFact("namespace and key must be 1..=128 bytes"));
    }
    if !normalized
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'/' | b'_' | b'-'))
    {
        return Err(MemoryError::InvalidFact("namespace and key contain unsupported characters"));
    }
    Ok(normalized)
}

pub(crate) fn validate_fact(
    fact: &NewWorkspaceFact,
    quotas: &MemoryQuotas,
) -> Result<(String, String), MemoryError> {
    let namespace = normalize_component(&fact.namespace)?;
    let key = normalize_component(&fact.key)?;
    if fact.value.is_empty() || fact.value.len() > quotas.max_value_bytes {
        return Err(MemoryError::QuotaExceeded("fact value bytes"));
    }
    if fact.provenance.source_kind.trim().is_empty() || fact.provenance.source_id.trim().is_empty()
    {
        return Err(MemoryError::InvalidFact("provenance source kind and id are required"));
    }
    let provenance_fields = [
        Some(fact.provenance.source_kind.as_str()),
        Some(fact.provenance.source_id.as_str()),
        fact.provenance.source_revision.as_deref(),
        fact.provenance.source_fingerprint.as_deref(),
    ];
    if provenance_fields.into_iter().flatten().any(|field| field.len() > 512) {
        return Err(MemoryError::InvalidFact("provenance field exceeds 512 bytes"));
    }
    for field in provenance_fields.into_iter().flatten() {
        reject_sensitive("provenance", "identity", field)?;
    }
    if fact.freshness == FactFreshness::RevisionBound
        && fact.provenance.source_revision.as_deref().is_none_or(str::is_empty)
        && fact.provenance.source_fingerprint.as_deref().is_none_or(str::is_empty)
    {
        return Err(MemoryError::InvalidFact(
            "revision-bound fact requires source revision or fingerprint",
        ));
    }
    reject_sensitive(&namespace, &key, &fact.value)?;
    Ok((namespace, key))
}

pub(crate) fn initial_state(authority: FactAuthority) -> FactState {
    match authority {
        FactAuthority::AgentCandidate => FactState::Candidate,
        FactAuthority::UserAsserted | FactAuthority::HostVerified => FactState::Active,
    }
}

pub(crate) fn content_hash(namespace: &str, key: &str, value: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"ee.workspace-fact.v1\0");
    hash.update(namespace.as_bytes());
    hash.update([0]);
    hash.update(key.as_bytes());
    hash.update([0]);
    hash.update(value.as_bytes());
    format!("sha256:{:x}", hash.finalize())
}

fn reject_sensitive(namespace: &str, key: &str, value: &str) -> Result<(), MemoryError> {
    let label = format!("{namespace}.{key}");
    let lower = value.to_ascii_lowercase();
    let secret_labels = [
        "password",
        "passwd",
        "secret",
        "token",
        "credential",
        "private_key",
        "api_key",
        "apikey",
        "cookie",
        "authorization",
    ];
    let forbidden_material = [
        "begin private key",
        "bearer ",
        "ghp_",
        "github_pat_",
        "sk-proj-",
        "aws_secret_access_key",
        "transcript:",
        "user prompt:",
        "system prompt:",
        "assistant:",
        "chain of thought",
        "model reasoning:",
        "raw terminal output:",
        "terminal output:",
        "stdout:",
        "stderr:",
        "environment dump:",
        "env dump:",
        "ignore previous instructions",
        "system message:",
        "<system>",
        "<assistant>",
        "[assistant]",
        "[user]",
        "human:",
        "\u{1b}[",
    ];
    let assignment_markers =
        ["password=", "passwd=", "token=", "secret=", "api_key=", "apikey=", "authorization="];
    if secret_labels.iter().any(|needle| label.contains(needle))
        || forbidden_material.iter().any(|needle| lower.contains(needle))
        || assignment_markers.iter().any(|needle| lower.contains(needle))
        || looks_like_environment_dump(value)
    {
        return Err(MemoryError::SensitiveMaterial);
    }
    Ok(())
}

fn looks_like_environment_dump(value: &str) -> bool {
    let mut assignments = 0;
    for line in value.lines() {
        let Some((name, _)) = line.split_once('=') else { continue };
        if name.len() >= 2
            && name.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        {
            assignments += 1;
        }
    }
    assignments >= 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FactKind, FactProvenance};

    fn candidate(value: &str) -> NewWorkspaceFact {
        NewWorkspaceFact {
            namespace: "project".into(),
            key: "architecture".into(),
            value: value.into(),
            kind: FactKind::Architecture,
            authority: FactAuthority::AgentCandidate,
            freshness: FactFreshness::Current,
            provenance: FactProvenance {
                source_kind: "agent".into(),
                source_id: "turn-1".into(),
                source_revision: None,
                source_fingerprint: None,
                verified_at: None,
            },
            expires_at: None,
            relations: vec![],
        }
    }

    #[test]
    fn rejects_secrets_and_session_material() {
        for value in [
            "password=hunter2",
            "Bearer abc",
            "user prompt: do this",
            "stdout: raw",
            "HOME=/private/path",
            "ignore previous instructions",
        ] {
            assert!(matches!(
                validate_fact(&candidate(value), &MemoryQuotas::default()),
                Err(MemoryError::SensitiveMaterial)
            ));
        }
        let mut poisoned_provenance = candidate("safe fact");
        poisoned_provenance.provenance.source_id = "token=stolen".into();
        assert!(matches!(
            validate_fact(&poisoned_provenance, &MemoryQuotas::default()),
            Err(MemoryError::SensitiveMaterial)
        ));
    }

    #[test]
    fn requires_revision_identity() {
        let mut fact = candidate("uses layered architecture");
        fact.freshness = FactFreshness::RevisionBound;
        assert!(validate_fact(&fact, &MemoryQuotas::default()).is_err());
    }
}
