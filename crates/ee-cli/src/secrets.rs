//! Host-bound encrypted secrets store — phases 1-2: contract, dependency
//! boundary, host binding, and OS-secure vault-key lifecycle.
//!
//! Defines the frontend-local secrets API: typed [`SecretName`]s, opaque
//! [`SecretReference`]s (`secret://<name>`), the [`SecretStore`] shape, the
//! [`host_binding`] module (host fingerprint digest), and the [`keychain`]
//! module (OS secure-storage boundary and vault key).
//!
//! Boundary rules (phases 1-6 of the "Host-Bound Encrypted Secrets Store"
//! issue in `ISSUES.md`):
//!
//! - All secret-store code lives in `ee-cli`; `xi-core-lib` stays config- and
//!   frontend-agnostic.
//! - A random 256-bit vault key lives in OS secure storage; a host fingerprint
//!   is only authenticated binding data, never encryption material.
//! - Secret values are never positional CLI arguments, config plaintext
//!   expansions, log fields, debug output, or error interpolation.
//!   Plaintext-bearing methods are crate-private and expose values only as
//!   [`Zeroizing`] buffers.
//! - References are exact `secret://<name>` strings: no interpolation,
//!   concatenation, environment expansion, percent-decoding, or recursion.
//! - Parsing, rendering, and store construction never touch a real keychain,
//!   host-identity source, or filesystem; tests run on test doubles only.
//!
//! [`SecretStore`] intentionally has no `Debug` impl: it holds vault-key
//! material from phase 2 onward.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

pub(crate) mod cli;
mod host_binding;
mod keychain;
pub(crate) mod resolve;
mod vault;

pub(crate) use host_binding::{HostBinding, VaultMetadata};
pub(crate) use keychain::{Keychain, VaultKey};

/// Maximum canonical secret-name length in bytes.
pub const MAX_SECRET_NAME_BYTES: usize = 128;

/// Exact literal prefix of every accepted secret reference.
pub const SECRET_REFERENCE_PREFIX: &str = "secret://";

/// Whether a config value is an exact `secret://` reference candidate.
///
/// Only values that *start* with the prefix are candidates; strings merely
/// containing `secret://` stay literals.
pub(crate) fn is_secret_reference_text(value: &str) -> bool {
    value.starts_with(SECRET_REFERENCE_PREFIX)
}

// ── Errors ──────────────────────────────────────────────────────────────────

/// Why a [`SecretName`] was rejected.
///
/// Carries only the offending input position/character; names are not secret
/// material, so these messages are safe to surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretNameError {
    /// The name is empty.
    Empty,
    /// The name exceeds [`MAX_SECRET_NAME_BYTES`] bytes.
    TooLong { len: usize },
    /// The first character is not an ASCII letter or digit.
    InvalidFirstCharacter(char),
    /// A later character is outside `[A-Za-z0-9._-]`.
    InvalidCharacter { index: usize, ch: char },
    /// The name contains the `..` sequence.
    DoubleDot,
}

impl fmt::Display for SecretNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "secret name must not be empty"),
            Self::TooLong { len } => {
                write!(f, "secret name must be at most {MAX_SECRET_NAME_BYTES} bytes, got {len}")
            }
            Self::InvalidFirstCharacter(ch) => {
                write!(f, "secret name must start with an ASCII letter or digit, got {ch:?}")
            }
            Self::InvalidCharacter { index, ch } => {
                write!(f, "secret name contains disallowed character {ch:?} at byte {index}")
            }
            Self::DoubleDot => write!(f, "secret name must not contain `..`"),
        }
    }
}

impl std::error::Error for SecretNameError {}

/// Why a [`SecretReference`] was rejected.
///
/// Structural violations (extra segments, query, fragment, percent-encoding)
/// are reported before name validation so malformed references always fail
/// closed with a precise cause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretReferenceError {
    /// Input does not start with the exact `secret://` prefix.
    InvalidPrefix,
    /// More than one path segment after `secret://`.
    MultiplePathSegments,
    /// A query string (`?`) is present.
    QueryString,
    /// A fragment (`#`) is present.
    Fragment,
    /// Percent-encoding (`%`) is present; references are never decoded.
    PercentEncoding,
    /// The remaining path is not a valid canonical secret name.
    InvalidName(SecretNameError),
}

impl fmt::Display for SecretReferenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPrefix => {
                write!(f, "secret reference must be exactly `{SECRET_REFERENCE_PREFIX}<name>`")
            }
            Self::MultiplePathSegments => write!(
                f,
                "secret reference must contain exactly one path segment after `{SECRET_REFERENCE_PREFIX}`"
            ),
            Self::QueryString => {
                write!(f, "secret reference must not contain a query string (`?`)")
            }
            Self::Fragment => write!(f, "secret reference must not contain a fragment (`#`)"),
            Self::PercentEncoding => write!(
                f,
                "secret reference must not contain percent-encoding (`%`); references are never decoded"
            ),
            Self::InvalidName(e) => write!(f, "invalid secret name in reference: {e}"),
        }
    }
}

impl std::error::Error for SecretReferenceError {}

/// Typed failure for every secrets-store operation.
///
/// Messages never include secret values, vault keys, ciphertext, host
/// digests, or raw machine identifiers.
#[derive(Debug)]
pub enum SecretStoreError {
    /// A [`SecretName`] violates the canonical grammar.
    InvalidName(SecretNameError),
    /// A [`SecretReference`] violates the exact `secret://<name>` grammar.
    InvalidReference(SecretReferenceError),
    /// The requested secret does not exist.
    NotFound,
    /// OS secure storage is unavailable.
    KeychainUnavailable,
    /// OS secure storage content is malformed.
    KeychainCorruption,
    /// Operating-system randomness is unavailable.
    CspRngUnavailable,
    /// Host identity cannot be determined.
    HostBindingUnavailable,
    /// The vault is bound to a different host.
    HostBindingMismatch { version: u32 },
    /// The vault format version is not supported.
    UnsupportedVersion { version: u32 },
    /// The user data directory cannot be resolved.
    DataDirUnavailable,
    /// Vault content is malformed or failed authentication.
    VaultCorruption,
    /// Underlying filesystem or I/O failure.
    Io(io::Error),
}

impl fmt::Display for SecretStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(e) => write!(f, "invalid secret name: {e}"),
            Self::InvalidReference(e) => write!(f, "invalid secret reference: {e}"),
            Self::NotFound => write!(f, "secret not found"),
            Self::KeychainUnavailable => write!(f, "OS secure storage is unavailable"),
            Self::KeychainCorruption => write!(f, "OS secure storage content is corrupt"),
            Self::CspRngUnavailable => write!(f, "operating-system randomness is unavailable"),
            Self::HostBindingUnavailable => {
                write!(f, "host identity is unavailable; cannot bind secrets to this machine")
            }
            Self::HostBindingMismatch { version } => write!(
                f,
                "vault is bound to a different host (vault version {version}); \
                 open the vault on its original host or initialize a new vault here"
            ),
            Self::UnsupportedVersion { version } => {
                write!(f, "vault format version {version} is not supported (supported: 1)")
            }
            Self::DataDirUnavailable => write!(f, "user data directory is unavailable"),
            Self::VaultCorruption => write!(f, "vault content is corrupt or failed authentication"),
            Self::Io(e) => write!(f, "secrets store I/O failure: {e}"),
        }
    }
}

impl std::error::Error for SecretStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidName(e) => Some(e),
            Self::InvalidReference(e) => Some(e),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

// `io::Error` has no `PartialEq`, so equality is implemented per variant; I/O
// errors compare by kind only. Useful for deterministic tests.
impl PartialEq for SecretStoreError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::InvalidName(a), Self::InvalidName(b)) => a == b,
            (Self::InvalidReference(a), Self::InvalidReference(b)) => a == b,
            (Self::NotFound, Self::NotFound) => true,
            (Self::KeychainUnavailable, Self::KeychainUnavailable) => true,
            (Self::KeychainCorruption, Self::KeychainCorruption) => true,
            (Self::CspRngUnavailable, Self::CspRngUnavailable) => true,
            (Self::HostBindingUnavailable, Self::HostBindingUnavailable) => true,
            (
                Self::HostBindingMismatch { version: a },
                Self::HostBindingMismatch { version: b },
            ) => a == b,
            (Self::UnsupportedVersion { version: a }, Self::UnsupportedVersion { version: b }) => {
                a == b
            }
            (Self::DataDirUnavailable, Self::DataDirUnavailable) => true,
            (Self::VaultCorruption, Self::VaultCorruption) => true,
            (Self::Io(a), Self::Io(b)) => a.kind() == b.kind(),
            _ => false,
        }
    }
}

// ── Secret names ─────────────────────────────────────────────────────────────

fn validate_name(name: &str) -> Result<(), SecretNameError> {
    if name.is_empty() {
        return Err(SecretNameError::Empty);
    }
    // Length is checked before the character set so oversize names always
    // fail with the size cause regardless of their contents.
    if name.len() > MAX_SECRET_NAME_BYTES {
        return Err(SecretNameError::TooLong { len: name.len() });
    }
    let first = name.chars().next().expect("non-empty name has a first char");
    if !first.is_ascii_alphanumeric() {
        return Err(SecretNameError::InvalidFirstCharacter(first));
    }
    let mut previous = first;
    for (index, ch) in name.char_indices().skip(1) {
        if !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')) {
            return Err(SecretNameError::InvalidCharacter { index, ch });
        }
        if ch == '.' && previous == '.' {
            return Err(SecretNameError::DoubleDot);
        }
        previous = ch;
    }
    Ok(())
}

/// Canonical secret name: 1-128 bytes of `[A-Za-z0-9._-]`, starting with an
/// ASCII letter or digit, never containing `..`.
///
/// A value of this type is guaranteed valid, so store operations accept it
/// directly: invalid names fail at this type boundary, before any keychain or
/// filesystem operation can observe them.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SecretName(String);

impl SecretName {
    /// Validates `name` against the canonical grammar.
    pub fn new(name: &str) -> Result<Self, SecretNameError> {
        validate_name(name)?;
        Ok(Self(name.to_owned()))
    }

    /// The canonical name text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SecretName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// Names are not secret material; Debug renders the plain name text.
impl fmt::Debug for SecretName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for SecretName {
    type Err = SecretNameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<&str> for SecretName {
    type Error = SecretNameError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for SecretName {
    type Error = SecretNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        validate_name(&value)?;
        Ok(Self(value))
    }
}

// ── Secret references ────────────────────────────────────────────────────────

/// Exact `secret://<name>` reference.
///
/// Parsing is structural and strict: lowercase scheme `secret`, literal `//`,
/// exactly one non-empty path segment matching the [`SecretName`] grammar.
/// Authority components, query strings, fragments, percent-encoding,
/// whitespace, and embedded secret URIs are all rejected; rendering always
/// produces the canonical `secret://<name>` form.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SecretReference {
    name: SecretName,
}

impl SecretReference {
    /// Parses an exact `secret://<name>` reference.
    pub fn parse(reference: &str) -> Result<Self, SecretReferenceError> {
        let path = reference
            .strip_prefix(SECRET_REFERENCE_PREFIX)
            .ok_or(SecretReferenceError::InvalidPrefix)?;
        if path.contains('/') {
            return Err(SecretReferenceError::MultiplePathSegments);
        }
        if path.contains('?') {
            return Err(SecretReferenceError::QueryString);
        }
        if path.contains('#') {
            return Err(SecretReferenceError::Fragment);
        }
        if path.contains('%') {
            return Err(SecretReferenceError::PercentEncoding);
        }
        let name = SecretName::new(path).map_err(SecretReferenceError::InvalidName)?;
        Ok(Self { name })
    }

    /// Builds a reference from an already-validated name.
    pub fn from_name(name: SecretName) -> Self {
        Self { name }
    }

    /// The canonical name of the referenced secret.
    pub fn name(&self) -> &SecretName {
        &self.name
    }
}

impl fmt::Display for SecretReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{SECRET_REFERENCE_PREFIX}{}", self.name)
    }
}

// References are not secret material; Debug renders the canonical form.
impl fmt::Debug for SecretReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl FromStr for SecretReference {
    type Err = SecretReferenceError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

// ── Secret store ─────────────────────────────────────────────────────────────

/// Frontend-local encrypted secret store.
///
/// Holds the OS-keychain boundary and host binding; vault persistence and the
/// `set`/`get`/`list`/`delete` operations arrive in phase 3. Construction
/// performs no keychain, host-identity, or filesystem work, so building a
/// store has no platform side effects.
pub struct SecretStore {
    keychain: Box<dyn Keychain>,
    binding: HostBinding,
    vault_path: PathBuf,
}

impl SecretStore {
    /// Builds a store over the given dependencies.
    ///
    /// No keychain entry, host-identity source, or file is touched here; all
    /// platform interaction happens inside operations.
    pub fn new(keychain: Box<dyn Keychain>, binding: HostBinding, vault_path: PathBuf) -> Self {
        Self { keychain, binding, vault_path }
    }

    /// The injected keychain boundary.
    pub(crate) fn keychain(&self) -> &dyn Keychain {
        self.keychain.as_ref()
    }

    /// The host binding the vault is bound to.
    pub(crate) fn binding(&self) -> &HostBinding {
        &self.binding
    }

    /// The vault file path.
    pub(crate) fn vault_path(&self) -> &Path {
        &self.vault_path
    }

    /// Loads or creates the vault key after verifying the current host binding
    /// against vault metadata.
    ///
    /// `metadata` must be the vault's authenticated metadata (phase 3: the
    /// AEAD decryption result); a binding mismatch fails closed here, before
    /// any key material is loaded or created. No recovery, export, fingerprint
    /// override, or host-migration bypass exists.
    pub(crate) fn open_vault(
        &self,
        metadata: &VaultMetadata,
    ) -> Result<VaultKey, SecretStoreError> {
        metadata.verify_host_binding(&self.binding)?;
        VaultKey::load_or_create(self.keychain())
    }
}

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod tests {
    use super::test_support::{CountingKeychain, test_binding};
    use super::*;
    use std::sync::atomic::Ordering;

    // ── Secret name grammar ──────────────────────────────────────────────────

    #[test]
    fn name_accepts_canonical_grammar() {
        for name in [
            "a",
            "A",
            "0",
            "9",
            "z9",
            "api-key",
            "api_key",
            "api.key",
            "Api.KEY-9_x",
            "a.b-c_d",
            "x",
            "single-word",
        ] {
            let parsed =
                SecretName::new(name).unwrap_or_else(|e| panic!("{name:?} should parse: {e}"));
            assert_eq!(parsed.as_str(), name);
        }
        // Every allowed character is accepted in one name.
        let all_chars: String =
            "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-".chars().collect();
        let parsed = SecretName::new(&all_chars)
            .unwrap_or_else(|e| panic!("full charset should parse: {e}"));
        assert_eq!(parsed.as_str(), all_chars);
        // Trailing separators are fine as long as `..` never appears.
        for name in ["a.", "a-", "a_"] {
            let parsed =
                SecretName::new(name).unwrap_or_else(|e| panic!("{name:?} should parse: {e}"));
            assert_eq!(parsed.as_str(), name);
        }
    }

    #[test]
    fn name_length_boundaries_are_exact() {
        for len in [1, 2, 64, 127, MAX_SECRET_NAME_BYTES] {
            let name = "a".repeat(len);
            let parsed =
                SecretName::new(&name).unwrap_or_else(|e| panic!("{len} bytes should parse: {e}"));
            assert_eq!(parsed.as_str(), name);
        }
        let over = "a".repeat(MAX_SECRET_NAME_BYTES + 1);
        assert_eq!(
            SecretName::new(&over),
            Err(SecretNameError::TooLong { len: MAX_SECRET_NAME_BYTES + 1 })
        );
        // Byte length, not char count: 65 two-byte chars exceed 128 bytes.
        let wide = "é".repeat(65);
        assert_eq!(SecretName::new(&wide), Err(SecretNameError::TooLong { len: 130 }));
    }

    #[test]
    fn name_rejection_kinds_are_exact_and_table_driven() {
        let cases: &[(&str, SecretNameError)] = &[
            ("", SecretNameError::Empty),
            (".a", SecretNameError::InvalidFirstCharacter('.')),
            ("-a", SecretNameError::InvalidFirstCharacter('-')),
            ("_a", SecretNameError::InvalidFirstCharacter('_')),
            (".", SecretNameError::InvalidFirstCharacter('.')),
            ("-", SecretNameError::InvalidFirstCharacter('-')),
            ("_", SecretNameError::InvalidFirstCharacter('_')),
            ("é", SecretNameError::InvalidFirstCharacter('é')),
            ("a/b", SecretNameError::InvalidCharacter { index: 1, ch: '/' }),
            ("a\\b", SecretNameError::InvalidCharacter { index: 1, ch: '\\' }),
            ("a:b", SecretNameError::InvalidCharacter { index: 1, ch: ':' }),
            ("a b", SecretNameError::InvalidCharacter { index: 1, ch: ' ' }),
            ("a\tb", SecretNameError::InvalidCharacter { index: 1, ch: '\t' }),
            ("a\n", SecretNameError::InvalidCharacter { index: 1, ch: '\n' }),
            ("a\r", SecretNameError::InvalidCharacter { index: 1, ch: '\r' }),
            ("a\u{0}", SecretNameError::InvalidCharacter { index: 1, ch: '\u{0}' }),
            ("a\u{1f}", SecretNameError::InvalidCharacter { index: 1, ch: '\u{1f}' }),
            ("a\u{7f}", SecretNameError::InvalidCharacter { index: 1, ch: '\u{7f}' }),
            ("a@b", SecretNameError::InvalidCharacter { index: 1, ch: '@' }),
            ("a?b", SecretNameError::InvalidCharacter { index: 1, ch: '?' }),
            ("a#b", SecretNameError::InvalidCharacter { index: 1, ch: '#' }),
            ("a%b", SecretNameError::InvalidCharacter { index: 1, ch: '%' }),
            ("a b.c", SecretNameError::InvalidCharacter { index: 1, ch: ' ' }),
            ("café", SecretNameError::InvalidCharacter { index: 3, ch: 'é' }),
            ("秘密", SecretNameError::InvalidFirstCharacter('秘')),
            ("a..b", SecretNameError::DoubleDot),
            ("a..", SecretNameError::DoubleDot),
            ("a...", SecretNameError::DoubleDot),
            ("a.b..c", SecretNameError::DoubleDot),
            ("..", SecretNameError::InvalidFirstCharacter('.')),
            ("...", SecretNameError::InvalidFirstCharacter('.')),
        ];
        for (input, expected) in cases {
            assert_eq!(SecretName::new(input), Err(*expected), "input {input:?}");
        }
    }

    #[test]
    fn name_parses_through_fromstr_and_tryfrom() {
        let via_fromstr: SecretName = "api-key".parse().expect("FromStr parse");
        assert_eq!(via_fromstr.as_str(), "api-key");
        let via_tryfrom: SecretName = "api-key".try_into().expect("TryFrom<&str>");
        assert_eq!(via_tryfrom, via_fromstr);
        let owned = String::from("api-key");
        let via_owned: SecretName = owned.try_into().expect("TryFrom<String>");
        assert_eq!(via_owned, via_fromstr);
        assert!("bad name".parse::<SecretName>().is_err());
    }

    #[test]
    fn name_equality_is_byte_exact_and_case_sensitive() {
        assert_eq!(SecretName::new("Foo").unwrap(), SecretName::new("Foo").unwrap());
        assert_ne!(SecretName::new("Foo").unwrap(), SecretName::new("foo").unwrap());
    }

    // ── Secret reference grammar ─────────────────────────────────────────────

    #[test]
    fn reference_parses_canonical_form() {
        for name in ["a", "0", "api-key", "Api.KEY-9_x", "a.b-c_d"] {
            let input = format!("{SECRET_REFERENCE_PREFIX}{name}");
            let reference = SecretReference::parse(&input)
                .unwrap_or_else(|e| panic!("{input:?} should parse: {e}"));
            assert_eq!(reference.name().as_str(), name);
        }
        // `FromStr` mirrors `parse`.
        let reference: SecretReference = "secret://api-key".parse().expect("FromStr parse");
        assert_eq!(reference.name().as_str(), "api-key");
    }

    #[test]
    fn parsed_references_round_trip_to_one_canonical_string() {
        for name in ["a", "0", "api-key", "Api.KEY-9_x", "a.b-c_d"] {
            let canonical = format!("{SECRET_REFERENCE_PREFIX}{name}");
            let reference = SecretReference::parse(&canonical).expect("canonical reference");
            assert_eq!(reference.to_string(), canonical, "Display is canonical");
            assert_eq!(format!("{reference:?}"), canonical, "Debug is canonical");
            let reparsed =
                SecretReference::parse(&reference.to_string()).expect("round trip stays parseable");
            assert_eq!(reparsed, reference);
            assert_eq!(reparsed.name(), reference.name());
        }
    }

    #[test]
    fn reference_from_name_renders_canonical_form() {
        let name = SecretName::new("openrouter-api-key").expect("valid name");
        let reference = SecretReference::from_name(name);
        assert_eq!(reference.to_string(), "secret://openrouter-api-key");
        assert_eq!(SecretReference::parse(&reference.to_string()).unwrap(), reference);
    }

    #[test]
    fn reference_rejects_malformed_forms_table_driven() {
        let mut cases: Vec<(String, SecretReferenceError)> = vec![
            ("".into(), SecretReferenceError::InvalidPrefix),
            ("secret".into(), SecretReferenceError::InvalidPrefix),
            ("secret:".into(), SecretReferenceError::InvalidPrefix),
            ("secret:/x".into(), SecretReferenceError::InvalidPrefix),
            ("Secret://x".into(), SecretReferenceError::InvalidPrefix),
            ("SECRET://x".into(), SecretReferenceError::InvalidPrefix),
            ("secret://x/".into(), SecretReferenceError::MultiplePathSegments),
            ("secret://x/y".into(), SecretReferenceError::MultiplePathSegments),
            ("secret://x/y/z".into(), SecretReferenceError::MultiplePathSegments),
            ("secret:///x".into(), SecretReferenceError::MultiplePathSegments),
            ("secret://x//y".into(), SecretReferenceError::MultiplePathSegments),
            ("secret://x?q=1".into(), SecretReferenceError::QueryString),
            ("secret://x?secret://y".into(), SecretReferenceError::MultiplePathSegments),
            ("secret://x#frag".into(), SecretReferenceError::Fragment),
            ("secret://x#secret://y".into(), SecretReferenceError::MultiplePathSegments),
            ("secret://x%20y".into(), SecretReferenceError::PercentEncoding),
            ("secret://x%".into(), SecretReferenceError::PercentEncoding),
            ("%61".into(), SecretReferenceError::InvalidPrefix),
            (" secret://x".into(), SecretReferenceError::InvalidPrefix),
            ("\tsecret://x".into(), SecretReferenceError::InvalidPrefix),
            ("xsecret://a".into(), SecretReferenceError::InvalidPrefix),
            ("http://x".into(), SecretReferenceError::InvalidPrefix),
            (
                "secret://x ".into(),
                SecretReferenceError::InvalidName(SecretNameError::InvalidCharacter {
                    index: 1,
                    ch: ' ',
                }),
            ),
            (
                "secret://x\n".into(),
                SecretReferenceError::InvalidName(SecretNameError::InvalidCharacter {
                    index: 1,
                    ch: '\n',
                }),
            ),
            ("secret://".into(), SecretReferenceError::InvalidName(SecretNameError::Empty)),
            (
                "secret://..".into(),
                SecretReferenceError::InvalidName(SecretNameError::InvalidFirstCharacter('.')),
            ),
            ("secret://a..b".into(), SecretReferenceError::InvalidName(SecretNameError::DoubleDot)),
            (
                "secret://a:b".into(),
                SecretReferenceError::InvalidName(SecretNameError::InvalidCharacter {
                    index: 1,
                    ch: ':',
                }),
            ),
            (
                "secret://a\\b".into(),
                SecretReferenceError::InvalidName(SecretNameError::InvalidCharacter {
                    index: 1,
                    ch: '\\',
                }),
            ),
            ("secret://user@host/x".into(), SecretReferenceError::MultiplePathSegments),
            ("secret://host:443/x".into(), SecretReferenceError::MultiplePathSegments),
            ("secret://a/secret://b".into(), SecretReferenceError::MultiplePathSegments),
            ("secret://secret://x".into(), SecretReferenceError::MultiplePathSegments),
            (
                "secret://-x".into(),
                SecretReferenceError::InvalidName(SecretNameError::InvalidFirstCharacter('-')),
            ),
            (
                "secret://.x".into(),
                SecretReferenceError::InvalidName(SecretNameError::InvalidFirstCharacter('.')),
            ),
            (
                "secret://x y".into(),
                SecretReferenceError::InvalidName(SecretNameError::InvalidCharacter {
                    index: 1,
                    ch: ' ',
                }),
            ),
            (
                "secret://é".into(),
                SecretReferenceError::InvalidName(SecretNameError::InvalidFirstCharacter('é')),
            ),
        ];
        cases.push((
            format!("{SECRET_REFERENCE_PREFIX}{}", "a".repeat(MAX_SECRET_NAME_BYTES + 1)),
            SecretReferenceError::InvalidName(SecretNameError::TooLong {
                len: MAX_SECRET_NAME_BYTES + 1,
            }),
        ));
        for (input, expected) in cases {
            assert_eq!(SecretReference::parse(&input), Err(expected), "input {input:?}");
        }
    }

    #[test]
    fn reference_equality_is_byte_exact_and_case_sensitive() {
        assert_eq!(
            SecretReference::parse("secret://Foo").unwrap(),
            SecretReference::parse("secret://Foo").unwrap()
        );
        assert_ne!(
            SecretReference::parse("secret://Foo").unwrap(),
            SecretReference::parse("secret://foo").unwrap()
        );
    }

    // ── Dependency boundary ──────────────────────────────────────────────────

    #[test]
    fn invalid_names_and_references_never_touch_keychain_or_filesystem() {
        let (keychain, loads, stores) = CountingKeychain::new();
        let dir = tempfile::tempdir().expect("temp dir");
        // The store exists with the fake keychain injected; invalid inputs
        // below are rejected at the pure type boundary, so neither the
        // keychain nor the vault filesystem is ever touched. Phase 1 store
        // exposes no operations; future operations accept only validated
        // `SecretName`/`SecretReference` values.
        let _store =
            SecretStore::new(Box::new(keychain), test_binding(), dir.path().join("v1.json"));
        for input in ["", "a b", "a/b", "a..b", "é", "a".repeat(MAX_SECRET_NAME_BYTES + 1).as_str()]
        {
            assert!(SecretName::new(input).is_err(), "input {input:?} must fail name validation");
            let reference = format!("{SECRET_REFERENCE_PREFIX}{input}");
            assert!(
                SecretReference::parse(&reference).is_err(),
                "reference {reference:?} must fail"
            );
        }
        for malformed in [
            "secret",
            "secret:/x",
            "Secret://x",
            "secret://x/y",
            "secret://x?q=1",
            "secret://x#f",
            "secret://x%20",
            "secret://x ",
        ] {
            assert!(
                SecretReference::parse(malformed).is_err(),
                "reference {malformed:?} must fail"
            );
        }

        assert_eq!(loads.load(Ordering::Relaxed), 0, "zero keychain loads");
        assert_eq!(stores.load(Ordering::Relaxed), 0, "zero keychain stores");
        assert_eq!(
            dir.path().read_dir().expect("read dir").count(),
            0,
            "zero vault filesystem entries"
        );
    }

    #[test]
    fn store_construction_touches_no_platform_backend() {
        // Compile-backed construction with test doubles: no real keyring
        // backend, host-identity source, or filesystem code is reachable from
        // building a store.
        let (keychain, loads, stores) = CountingKeychain::new();
        let dir = tempfile::tempdir().expect("temp dir");
        let vault_path = dir.path().join("v1.json");
        let binding = test_binding();
        let store = SecretStore::new(Box::new(keychain), binding.clone(), vault_path.clone());

        assert_eq!(store.binding(), &binding);
        assert_eq!(store.vault_path(), vault_path);
        assert_eq!(loads.load(Ordering::Relaxed), 0, "construction loads nothing");
        assert_eq!(stores.load(Ordering::Relaxed), 0, "construction stores nothing");
        assert_eq!(
            dir.path().read_dir().expect("read dir").count(),
            0,
            "construction writes nothing"
        );
    }
}
