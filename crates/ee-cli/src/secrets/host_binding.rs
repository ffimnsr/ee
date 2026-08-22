//! Host binding (phase 2): versioned SHA-256 digest of the canonical platform
//! machine identifier.
//!
//! The digest is authenticated binding data, not a cryptographic secret, and
//! is never used to derive encryption material. Only the digest (never the raw
//! machine identifier) is stored in vault metadata; domain separation is
//! applied before hashing.

#[cfg(target_os = "linux")]
use std::path::Path;

use sha2::{Digest as _, Sha256};

use super::SecretStoreError;

/// Fixed domain separator for host-binding digests.
pub const HOST_BINDING_DOMAIN_SEPARATOR: &[u8] = b"ee-secrets-host-binding-v1";

/// Path of the Linux machine identifier.
#[cfg(target_os = "linux")]
const LINUX_MACHINE_ID_PATH: &str = "/etc/machine-id";

/// Authenticated host binding for the secrets vault.
///
/// Holds the versioned SHA-256 digest of the canonical platform identifier.
/// Constructed from the current machine's identifier via
/// [`HostBinding::current`] or from canonical identifier bytes via
/// [`HostBinding::from_identifier_bytes`] (test doubles and fake sources).
#[derive(Clone, PartialEq, Eq)]
pub struct HostBinding {
    digest: [u8; 32],
}

impl HostBinding {
    /// Reads the current platform machine identifier and derives the binding
    /// digest.
    ///
    /// Fails closed with [`SecretStoreError::HostBindingUnavailable`] when the
    /// platform identity cannot be read or is empty; no recovery, fingerprint
    /// override, or host-migration bypass exists.
    pub fn current() -> Result<Self, SecretStoreError> {
        let identifier = read_current_machine_identifier()?;
        Self::from_identifier_bytes(&identifier)
    }

    /// Derives a binding from canonical identifier bytes (empty bytes are an
    /// unavailable identity).
    pub(crate) fn from_identifier_bytes(identifier: &[u8]) -> Result<Self, SecretStoreError> {
        if identifier.is_empty() {
            return Err(SecretStoreError::HostBindingUnavailable);
        }
        Ok(Self::from_digest(sha256_digest(identifier)))
    }

    /// Constructs a binding from its 32-byte SHA-256 digest.
    pub(crate) fn from_digest(digest: [u8; 32]) -> Self {
        Self { digest }
    }

    /// The raw binding digest bytes.
    pub(crate) fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}

impl std::fmt::Debug for HostBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hex = self.digest.map(|b| format!("{b:02x}")).join("");
        f.debug_struct("HostBinding").field("digest_sha256_hex", &hex).finish()
    }
}

/// SHA-256 digest of the canonical identifier with the fixed domain separator
/// prefixed (`ee-secrets-host-binding-v1`). The version lives in the separator
/// so future versions change the digest space.
pub(crate) fn sha256_digest(identifier: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(HOST_BINDING_DOMAIN_SEPARATOR);
    hasher.update(identifier);
    hasher.finalize().into()
}

/// Strips trailing platform line-ending bytes (`\n`, `\r\n`, lone `\r`) only;
/// every other byte is preserved exactly, including interior or leading
/// whitespace.
/// Strips one trailing platform line-ending sequence (`\n`, `\r\n`, or lone
/// `\r`) only; every other byte is preserved exactly, including interior or
/// leading whitespace and any further line endings.
fn trim_line_endings(mut raw: Vec<u8>) -> Vec<u8> {
    if raw.ends_with(b"\r\n") {
        raw.truncate(raw.len() - 2);
    } else if raw.last() == Some(&b'\n') || raw.last() == Some(&b'\r') {
        raw.pop();
    }
    raw
}

// ── Vault metadata ───────────────────────────────────────────────────────────

/// Vault metadata relevant to host binding: format version and the SHA-256
/// host-binding digest.
///
/// Phase 3 persists and AEAD-authenticates this metadata; callers must only
/// verify metadata that passed authentication (the AEAD decryption result).
/// It holds no raw machine identifier and no secret material.
#[derive(Clone, PartialEq, Eq)]
pub struct VaultMetadata {
    version: u32,
    host_binding_digest: [u8; 32],
}

impl VaultMetadata {
    /// Builds metadata for `version` bound to `host_binding_digest`.
    pub(crate) fn new(version: u32, host_binding_digest: [u8; 32]) -> Self {
        Self { version, host_binding_digest }
    }

    /// The vault format version.
    pub(crate) fn version(&self) -> u32 {
        self.version
    }

    /// The host-binding digest recorded in the vault metadata.
    pub(crate) fn host_binding_digest(&self) -> &[u8; 32] {
        &self.host_binding_digest
    }

    /// Verifies that `binding` matches the digest recorded in the vault.
    ///
    /// Returns [`SecretStoreError::HostBindingMismatch`] (carrying the vault
    /// version and safe remediation text, never the digest, key, ciphertext,
    /// or plaintext) when the digests differ.
    pub(crate) fn verify_host_binding(
        &self,
        binding: &HostBinding,
    ) -> Result<(), SecretStoreError> {
        if self.host_binding_digest == *binding.digest() {
            Ok(())
        } else {
            Err(SecretStoreError::HostBindingMismatch { version: self.version })
        }
    }
}

// ── Platform identifier readers ──────────────────────────────────────────────

/// Reads the canonical machine identifier for the current platform.
///
/// Linux: `/etc/machine-id`. macOS: hardware UUID via `sysctl hw.uuid`.
/// Windows: `MachineGuid` registry value. Unsupported platforms fail closed.
fn read_current_machine_identifier() -> Result<Vec<u8>, SecretStoreError> {
    #[cfg(target_os = "linux")]
    {
        read_machine_id_file(Path::new(LINUX_MACHINE_ID_PATH))
    }
    #[cfg(target_os = "macos")]
    {
        read_macos_hardware_uuid()
    }
    #[cfg(target_os = "windows")]
    {
        read_windows_machine_guid()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Err(SecretStoreError::HostBindingUnavailable)
    }
}

/// Reads a machine-id file and trims its trailing line ending.
#[cfg(target_os = "linux")]
fn read_machine_id_file(path: &Path) -> Result<Vec<u8>, SecretStoreError> {
    let raw = std::fs::read(path).map_err(|_| SecretStoreError::HostBindingUnavailable)?;
    Ok(trim_line_endings(raw))
}

/// Reads the macOS hardware UUID via `sysctl(3) hw.uuid` (the same value as
/// IOKit `IOPlatformUUID`).
#[cfg(target_os = "macos")]
fn read_macos_hardware_uuid() -> Result<Vec<u8>, SecretStoreError> {
    let name = b"hw.uuid\0".as_ptr().cast::<libc::c_char>();
    let mut size: libc::size_t = 0;
    // First call queries the required buffer size.
    if unsafe { libc::sysctlbyname(name, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0) }
        != 0
    {
        return Err(SecretStoreError::HostBindingUnavailable);
    }
    let mut buffer = vec![0u8; size];
    if unsafe {
        libc::sysctlbyname(name, buffer.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0)
    } != 0
    {
        return Err(SecretStoreError::HostBindingUnavailable);
    }
    buffer.truncate(size);
    // sysctl strings are NUL-terminated; the NUL is not identifier data.
    while buffer.last() == Some(&0) {
        buffer.pop();
    }
    Ok(trim_line_endings(buffer))
}

/// Reads the Windows `MachineGuid` registry value
/// (`HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid`).
#[cfg(target_os = "windows")]
fn read_windows_machine_guid() -> Result<Vec<u8>, SecretStoreError> {
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};

    const MACHINE_GUID_KEY: &str = r"SOFTWARE\Microsoft\Cryptography";
    const MACHINE_GUID_VALUE: &str = "MachineGuid";

    let key: Vec<u16> = MACHINE_GUID_KEY.encode_utf16().chain(std::iter::once(0)).collect();
    let value: Vec<u16> = MACHINE_GUID_VALUE.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buffer = [0u16; 128];
    let mut size: u32 = (buffer.len() * std::mem::size_of::<u16>()) as u32;

    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status != 0 || size == 0 || size as usize > buffer.len() * std::mem::size_of::<u16>() {
        return Err(SecretStoreError::HostBindingUnavailable);
    }
    let mut wide = &buffer[..size as usize / 2];
    while wide.last() == Some(&0) {
        wide = &wide[..wide.len() - 1];
    }
    let text = String::from_utf16(wide).map_err(|_| SecretStoreError::HostBindingUnavailable)?;
    Ok(text.into_bytes())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::secrets::test_support::StoredKeychain;
    use crate::secrets::{SecretStore, keychain::encode_hex32};

    // ── Digest derivation ────────────────────────────────────────────────────

    #[test]
    fn equal_identifiers_yield_equal_digests() {
        let a = HostBinding::from_identifier_bytes(b"01234567-89ab-cdef-0123-456789abcdef\n")
            .expect("valid identifier");
        let b = HostBinding::from_identifier_bytes(b"01234567-89ab-cdef-0123-456789abcdef\n")
            .expect("valid identifier");
        assert_eq!(a, b);
        assert_eq!(a.digest(), b.digest());
    }

    #[test]
    fn distinct_identifiers_yield_distinct_digests() {
        let a = HostBinding::from_identifier_bytes(b"01234567-89ab-cdef-0123-456789abcdef")
            .expect("valid identifier");
        let b = HostBinding::from_identifier_bytes(b"76543210-89ab-cdef-0123-456789abcdef")
            .expect("valid identifier");
        assert_ne!(a, b);
        assert_ne!(a.digest(), b.digest());
    }

    #[test]
    fn digest_is_domain_separated() {
        let identifier = b"01234567-89ab-cdef-0123-456789abcdef";
        let binding = HostBinding::from_identifier_bytes(identifier).expect("valid identifier");
        // A plain (non-domain-separated) SHA-256 of the identifier must differ.
        let mut plain = Sha256::new();
        plain.update(identifier);
        let plain: [u8; 32] = plain.finalize().into();
        assert_ne!(binding.digest(), &plain);
        // The separator prefix is the fixed versioned constant.
        let mut expected = Sha256::new();
        expected.update(HOST_BINDING_DOMAIN_SEPARATOR);
        expected.update(identifier);
        let expected: [u8; 32] = expected.finalize().into();
        assert_eq!(binding.digest(), &expected);
    }

    #[test]
    fn empty_identifier_is_unavailable() {
        assert_eq!(
            HostBinding::from_identifier_bytes(b""),
            Err(SecretStoreError::HostBindingUnavailable)
        );
    }

    #[test]
    fn line_endings_are_trimmed_but_other_bytes_preserved() {
        // Exactly one platform line ending is stripped; nothing else changes.
        assert_eq!(trim_line_endings(b"machine-id\n".to_vec()), b"machine-id");
        assert_eq!(trim_line_endings(b"machine-id\r\n".to_vec()), b"machine-id");
        assert_eq!(trim_line_endings(b"machine-id\r".to_vec()), b"machine-id");
        assert_eq!(trim_line_endings(b"machine-id".to_vec()), b"machine-id");
        // A second line ending is data, not the file's single line ending.
        assert_eq!(trim_line_endings(b"machine-id\n\n".to_vec()), b"machine-id\n");
        // Interior/leading whitespace and other bytes are preserved exactly.
        assert_eq!(trim_line_endings(b"  machine-id\n".to_vec()), b"  machine-id");

        // Trimmed identifiers hash identically regardless of line ending.
        let plain = HostBinding::from_identifier_bytes(b"machine-id").expect("valid");
        let lf = HostBinding::from_identifier_bytes(&trim_line_endings(b"machine-id\n".to_vec()))
            .expect("valid");
        let crlf =
            HostBinding::from_identifier_bytes(&trim_line_endings(b"machine-id\r\n".to_vec()))
                .expect("valid");
        assert_eq!(plain, lf);
        assert_eq!(plain, crlf);

        let double =
            HostBinding::from_identifier_bytes(&trim_line_endings(b"machine-id\n\n".to_vec()))
                .expect("valid");
        assert_ne!(plain, double);
        let padded =
            HostBinding::from_identifier_bytes(&trim_line_endings(b"  machine-id\n".to_vec()))
                .expect("valid");
        assert_ne!(plain, padded);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_machine_id_file_fails_closed() {
        let missing = Path::new("/definitely/not/a/machine-id-file");
        assert_eq!(read_machine_id_file(missing), Err(SecretStoreError::HostBindingUnavailable));
    }

    // ── Binding verification ─────────────────────────────────────────────────

    #[test]
    fn metadata_verifies_when_binding_matches() {
        let binding = HostBinding::from_identifier_bytes(b"host-a").expect("valid");
        let metadata = VaultMetadata::new(1, *binding.digest());
        assert!(metadata.verify_host_binding(&binding).is_ok());
    }

    #[test]
    fn metadata_mismatch_error_is_safe_and_versioned() {
        let binding = HostBinding::from_identifier_bytes(b"host-a").expect("valid");
        let other = HostBinding::from_identifier_bytes(b"host-b").expect("valid");
        let metadata = VaultMetadata::new(1, *binding.digest());

        let err = metadata.verify_host_binding(&other).expect_err("mismatch");
        assert!(matches!(err, SecretStoreError::HostBindingMismatch { version: 1 }));

        let text = err.to_string();
        assert!(text.contains("vault version 1"), "error carries vault version");
        assert!(text.contains("original host"), "error carries remediation text");
        // No raw identifiers, digest bytes, or hex digests leak into the error.
        assert!(!text.contains("host-a") && !text.contains("host-b"));
        let digest_hex = binding.digest().map(|b| format!("{b:02x}")).join("");
        assert!(!text.contains(&digest_hex));
    }

    #[test]
    fn copied_vault_with_different_binding_cannot_be_opened_even_with_same_key() {
        // Host A: vault key created and bound to host A.
        let keychain = StoredKeychain::new();
        let host_a = HostBinding::from_identifier_bytes(b"host-a-machine-id").expect("valid");
        let store_a = SecretStore::new(
            Box::new(keychain.clone()),
            host_a.clone(),
            Path::new("unused").into(),
        );
        let metadata = VaultMetadata::new(1, *host_a.digest());
        let key_a = store_a.open_vault(&metadata).expect("host A opens its vault");
        assert_eq!(keychain.load_calls(), 1, "one load");
        assert_eq!(keychain.store_calls(), 1, "key created exactly once");

        // The vault is copied to host B: same keychain content, different host.
        let host_b = HostBinding::from_identifier_bytes(b"host-b-machine-id").expect("valid");
        let store_b = SecretStore::new(
            Box::new(keychain.clone()),
            host_b.clone(),
            Path::new("unused").into(),
        );
        // `VaultKey` deliberately has no `Debug`; match instead of `expect_err`.
        let err = match store_b.open_vault(&metadata) {
            Ok(_) => panic!("host B must fail closed"),
            Err(e) => e,
        };
        assert!(matches!(err, SecretStoreError::HostBindingMismatch { version: 1 }));
        // The failure happens before any keychain interaction on host B.
        assert_eq!(keychain.load_calls(), 1, "no additional keychain load");
        assert_eq!(keychain.store_calls(), 1, "no additional keychain store");

        // Error text exposes neither identifier, digest, nor key bytes.
        let text = err.to_string();
        assert!(!text.contains("host-a-machine-id") && !text.contains("host-b-machine-id"));
        let digest_hex = host_a.digest().map(|b| format!("{b:02x}")).join("");
        assert!(!text.contains(&digest_hex));
        let key_hex = encode_hex32(key_a.bytes());
        assert!(!text.contains(&key_hex), "no key bytes in error text");

        // Control: host A still opens the same stored key.
        let key_a_again = store_a.open_vault(&metadata).expect("host A still opens");
        assert_eq!(key_a.bytes(), key_a_again.bytes());
        assert_eq!(keychain.store_calls(), 1, "no new key after reload");
    }
}
