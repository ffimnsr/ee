//! Protected-path classification for persistent read trust (Phase 4).
//!
//! Read rules require canonical in-workspace paths; this classifier marks
//! path classes that persistent trust must never cover: hidden entries
//! (`.env`, `.env.*`, `.git`, `.ssh`), credential/secret-store directories,
//! SSH material, and private-key suffixes.  It runs at normalization time —
//! before trust matching and before any persistent-option display — so a
//! protected path normalizes to an unknown operation and can never match a
//! rule.

/// Private-key suffixes that persistent read trust never covers.
const PRIVATE_KEY_SUFFIXES: [&str; 6] = [".pem", ".key", ".p12", ".pfx", ".p8", ".der"];

/// SSH private-key file names that persistent read trust never covers.
const SSH_PRIVATE_KEYS: [&str; 4] = ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"];

/// Whether one canonical workspace-relative path segment is a protected
/// class.
pub(crate) fn is_protected_segment(segment: &str) -> bool {
    let lower = segment.to_ascii_lowercase();
    segment.starts_with('.')
        || matches!(lower.as_str(), "secrets" | "credential" | "credentials" | "vault")
        // Secret-like file names (`secret.json`, `secret.env`, …).
        || lower == "secret"
        || lower.starts_with("secret.")
        || SSH_PRIVATE_KEYS.contains(&lower.as_str())
        || PRIVATE_KEY_SUFFIXES.iter().any(|suffix| lower.ends_with(suffix))
}

/// Whether a canonical workspace-relative slash-joined path is a protected
/// class.  Protected reads never match a persistent read rule.
pub(crate) fn is_protected_relative_path(relative: &str) -> bool {
    !relative.is_empty() && relative.split('/').any(is_protected_segment)
}
