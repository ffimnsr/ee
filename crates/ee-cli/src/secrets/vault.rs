//! Authenticated encrypted vault format and durable persistence (phase 3).
//!
//! A single version-1 vault file holds sorted records of XChaCha20-Poly1305
//! ciphertext. Every record is bound to the vault version, the host-binding
//! digest, and its canonical name through AEAD associated data; each write
//! re-encrypts every record with a fresh 24-byte nonce and replaces the file
//! atomically through a same-directory temporary file.
//!
//! Security properties:
//! - The vault JSON never contains plaintext, the encryption key, or raw host
//!   identifiers — only ciphertext, nonces, version, and the digest.
//! - Tampering with ciphertext, nonce, record name, or host digest fails
//!   AEAD verification and surfaces as [`SecretStoreError::VaultCorruption`]
//!   without revealing whether a record existed.
//! - A genuine vault copied to another host decrypts (its records are
//!   internally consistent) and then fails the host-binding comparison with
//!   [`SecretStoreError::HostBindingMismatch`], before any mutation.
//! - Plaintext lives in exactly one [`Zeroizing`] buffer: AEAD operations are
//!   in-place. The cipher holds a transient key copy that is dropped after
//!   each call; the source key and all plaintext buffers are zeroized.
//!
//! The vault path is `dirs::data_dir()/ee/secrets/v1.json`. On Unix, parent
//! directories created by this module get mode `0700` and the vault file
//! `0600`.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chacha20poly1305::aead::AeadInPlace;
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

use super::host_binding::VaultMetadata;
use super::keychain::{VaultKey, decode_hex32, encode_hex32, random_bytes};
use super::{HostBinding, SecretName, SecretStore, SecretStoreError};

/// Vault format version written and accepted by this implementation.
pub const VAULT_VERSION: u32 = 1;

/// Minimum ciphertext byte length (16-byte Poly1305 tag included).
const MIN_CIPHERTEXT_BYTES: usize = 16;

/// Nonce byte length required by XChaCha20-Poly1305.
const NONCE_BYTES: usize = 24;

// ── Wire format ──────────────────────────────────────────────────────────────

/// Strict version-1 vault wire format. Unknown fields are rejected; no `Debug`
/// is derived — these types hold ciphertext, and the decoded form holds
/// plaintext.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VaultFileV1 {
    version: u32,
    /// Lowercase-hex host-binding digest (64 chars), never a raw machine ID.
    host_binding_digest: String,
    records: Vec<SecretRecordV1>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretRecordV1 {
    /// Exact canonical secret name.
    name: String,
    /// Standard base64, exactly 24 bytes when decoded.
    nonce: String,
    /// Standard base64, at least 16 bytes when decoded.
    ciphertext: String,
}

/// Validated, decoded in-memory vault.
pub(crate) struct DecodedVault {
    pub(crate) version: u32,
    pub(crate) host_binding_digest: [u8; 32],
    pub(crate) records: Vec<DecodedRecord>,
}

impl DecodedVault {
    /// Verifies the current host binding against this vault's digest.
    fn verify_binding(&self, binding: &HostBinding) -> Result<(), SecretStoreError> {
        VaultMetadata::new(self.version, self.host_binding_digest).verify_host_binding(binding)
    }
}

/// A validated vault record: canonical name, 24-byte nonce, ciphertext.
pub(crate) struct DecodedRecord {
    pub(crate) name: SecretName,
    pub(crate) nonce: [u8; NONCE_BYTES],
    pub(crate) ciphertext: Vec<u8>,
}

impl VaultFileV1 {
    /// Strictly parses and validates vault JSON.
    ///
    /// Rejects: unknown fields, trailing content, non-object input, future
    /// versions, malformed base64, wrong nonce/ciphertext sizes, invalid
    /// record names, invalid digest encoding, and duplicate names. No AEAD is
    /// performed here.
    pub(crate) fn decode(json_bytes: &[u8]) -> Result<DecodedVault, SecretStoreError> {
        let file: VaultFileV1 =
            serde_json::from_slice(json_bytes).map_err(|_| SecretStoreError::VaultCorruption)?;
        if file.version != VAULT_VERSION {
            return Err(SecretStoreError::UnsupportedVersion { version: file.version });
        }
        let host_binding_digest = decode_hex32(file.host_binding_digest.as_bytes())
            .ok_or(SecretStoreError::VaultCorruption)?;
        let mut seen: HashSet<SecretName> = HashSet::new();
        let mut records = Vec::with_capacity(file.records.len());
        for record in file.records {
            let name =
                SecretName::new(&record.name).map_err(|_| SecretStoreError::VaultCorruption)?;
            if !seen.insert(name.clone()) {
                return Err(SecretStoreError::VaultCorruption);
            }
            let nonce_bytes = STANDARD
                .decode(record.nonce.as_bytes())
                .map_err(|_| SecretStoreError::VaultCorruption)?;
            let nonce: [u8; NONCE_BYTES] =
                nonce_bytes.try_into().map_err(|_| SecretStoreError::VaultCorruption)?;
            let ciphertext = STANDARD
                .decode(record.ciphertext.as_bytes())
                .map_err(|_| SecretStoreError::VaultCorruption)?;
            if ciphertext.len() < MIN_CIPHERTEXT_BYTES {
                return Err(SecretStoreError::VaultCorruption);
            }
            records.push(DecodedRecord { name, nonce, ciphertext });
        }
        Ok(DecodedVault { version: file.version, host_binding_digest, records })
    }
}

/// Encodes a vault to canonical JSON with records sorted by canonical name.
fn encode_vault(
    version: u32,
    host_binding_digest: &[u8; 32],
    records: &[(SecretName, [u8; NONCE_BYTES], Vec<u8>)],
) -> Result<Vec<u8>, SecretStoreError> {
    let mut records: Vec<SecretRecordV1> = records
        .iter()
        .map(|(name, nonce, ciphertext)| SecretRecordV1 {
            name: name.to_string(),
            nonce: STANDARD.encode(nonce),
            ciphertext: STANDARD.encode(ciphertext),
        })
        .collect();
    records.sort_by(|a, b| a.name.cmp(&b.name));
    let file =
        VaultFileV1 { version, host_binding_digest: encode_hex32(host_binding_digest), records };
    serde_json::to_vec(&file).map_err(|e| SecretStoreError::Io(io::Error::other(e)))
}

/// A vault's plaintext records in memory (canonical name + zeroizing value).
type PlaintextRecords = Vec<(SecretName, Zeroizing<Vec<u8>>)>;

// ── AEAD ─────────────────────────────────────────────────────────────────────

/// Canonical associated data: little-endian vault version, host-binding
/// digest, then the exact canonical name bytes. All prefixes have fixed
/// lengths, so the encoding is unambiguous.
fn build_aad(version: u32, host_binding_digest: &[u8; 32], name: &SecretName) -> Vec<u8> {
    let mut aad = Vec::with_capacity(4 + 32 + name.as_str().len());
    aad.extend_from_slice(&version.to_le_bytes());
    aad.extend_from_slice(host_binding_digest);
    aad.extend_from_slice(name.as_str().as_bytes());
    aad
}

/// Authenticated encryption in place: `buffer` holds plaintext on entry and
/// ciphertext plus tag on exit. The fresh nonce comes from the caller.
///
/// Authentication failure is impossible here (the tag is appended, not
/// verified), but errors are still mapped to [`SecretStoreError::VaultCorruption`]
/// so callers never see raw AEAD details.
fn encrypt_record(
    key: &[u8; 32],
    version: u32,
    host_binding_digest: &[u8; 32],
    name: &SecretName,
    nonce: &[u8; NONCE_BYTES],
    buffer: &mut Vec<u8>,
) -> Result<(), SecretStoreError> {
    let cipher = XChaCha20Poly1305::new_from_slice(key).expect("32-byte key");
    let aad = build_aad(version, host_binding_digest, name);
    cipher
        .encrypt_in_place(XNonce::from_slice(nonce), &aad, buffer)
        .map_err(|_| SecretStoreError::VaultCorruption)
}

/// Authenticated decryption in place: `buffer` holds ciphertext plus tag on
/// entry and plaintext on exit (auth failure leaves it undefined; the caller's
/// [`Zeroizing`] wrapper still scrubs it on drop).
fn decrypt_record(
    key: &[u8; 32],
    version: u32,
    host_binding_digest: &[u8; 32],
    name: &SecretName,
    nonce: &[u8; NONCE_BYTES],
    buffer: &mut Vec<u8>,
) -> Result<(), SecretStoreError> {
    let cipher = XChaCha20Poly1305::new_from_slice(key).expect("32-byte key");
    let aad = build_aad(version, host_binding_digest, name);
    cipher
        .decrypt_in_place(XNonce::from_slice(nonce), &aad, buffer)
        .map_err(|_| SecretStoreError::VaultCorruption)
}

// ── Path resolution and permissions ──────────────────────────────────────────

/// The vault path under a data directory: `<data_dir>/ee/secrets/v1.json`.
pub(crate) fn vault_path_from(data_dir: &Path) -> PathBuf {
    data_dir.join("ee").join("secrets").join("v1.json")
}

/// The default vault path under [`dirs::data_dir`].
pub(crate) fn default_vault_path() -> Result<PathBuf, SecretStoreError> {
    let data_dir = dirs::data_dir().ok_or(SecretStoreError::DataDirUnavailable)?;
    Ok(vault_path_from(&data_dir))
}

/// Removes one encrypted vault file without reading its contents, host binding,
/// or keychain key. Returns whether a file was removed.
///
/// This recovery operation is intentionally idempotent so a missing vault is
/// already reset. It preserves the OS-keychain key: a later `set` creates a
/// fresh vault without requiring keychain mutation.
pub(crate) fn reset_vault_file(path: &Path) -> Result<bool, SecretStoreError> {
    match fs::remove_file(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                sync_parent_dir(parent);
            }
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(SecretStoreError::Io(error)),
    }
}

/// Creates the vault parent directory (mode `0700` on Unix when newly created)
/// and reports whether this call created it.
#[cfg(unix)]
fn ensure_parent_dir(dir: &Path) -> Result<bool, SecretStoreError> {
    use std::os::unix::fs::PermissionsExt;
    if dir.exists() {
        return Ok(false);
    }
    fs::create_dir_all(dir).map_err(SecretStoreError::Io)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(SecretStoreError::Io)?;
    Ok(true)
}

#[cfg(not(unix))]
fn ensure_parent_dir(dir: &Path) -> Result<bool, SecretStoreError> {
    fs::create_dir_all(dir).map_err(SecretStoreError::Io)?;
    Ok(true)
}

/// A unique temporary sibling of the vault file (pid plus OS-random bytes).
fn unique_temp_path(dir: &Path) -> Result<PathBuf, SecretStoreError> {
    let random = random_bytes::<8>()?;
    let random_hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
    Ok(dir.join(format!(".v1.tmp-{}-{random_hex}", std::process::id())))
}

/// Replaces `path` atomically: write a unique same-directory temp file, flush
/// it, rename over the target, then best-effort flush the parent directory.
/// Failed writes remove the temp file and leave any previous vault intact.
fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), SecretStoreError> {
    let dir = path.parent().ok_or_else(|| {
        SecretStoreError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "vault path has no parent directory",
        ))
    })?;
    ensure_parent_dir(dir)?;
    let temp = unique_temp_path(dir)?;

    let result = (|| -> Result<(), SecretStoreError> {
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)
                .map_err(SecretStoreError::Io)?
        };
        #[cfg(not(unix))]
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(SecretStoreError::Io)?;
        file.write_all(contents).map_err(SecretStoreError::Io)?;
        file.sync_all().map_err(SecretStoreError::Io)?;
        drop(file);
        fs::rename(&temp, path).map_err(SecretStoreError::Io)?;
        #[cfg(unix)]
        enforce_private_file_mode(path)?;
        sync_parent_dir(dir);
        Ok(())
    })();
    if result.is_err() {
        // Best-effort cleanup: never leave a partial replacement behind.
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Re-asserts owner-only file mode after replacement.
#[cfg(unix)]
fn enforce_private_file_mode(path: &Path) -> Result<(), SecretStoreError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(SecretStoreError::Io)
}

/// Best-effort directory flush so the rename is durable where supported.
#[cfg(unix)]
fn sync_parent_dir(dir: &Path) {
    if let Ok(dir_file) = fs::File::open(dir) {
        let _ = dir_file.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_parent_dir(_dir: &Path) {}

// ── Store operations ─────────────────────────────────────────────────────────

impl SecretStore {
    /// Builds the default store: real OS keychain, current host binding, and
    /// the default vault path under the user data directory.
    pub(crate) fn default() -> Result<Self, SecretStoreError> {
        Ok(Self::new(
            Box::new(super::keychain::KeyringKeychain),
            HostBinding::current()?,
            default_vault_path()?,
        ))
    }

    /// Status-diagnostic: compares the stored host digest with the current
    /// binding without decrypting anything. `Ok` also when no vault exists.
    /// Not a substitute for authenticated verification on data paths.
    pub(crate) fn verify_vault_binding_digest(&self) -> Result<(), SecretStoreError> {
        let Some(vault) = self.read_vault()? else {
            return Ok(());
        };
        vault.verify_binding(&self.binding)
    }

    /// Reads and validates the vault file; `None` when no vault exists yet.
    fn read_vault(&self) -> Result<Option<DecodedVault>, SecretStoreError> {
        let bytes = match fs::read(&self.vault_path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(SecretStoreError::Io(e)),
        };
        Ok(Some(VaultFileV1::decode(&bytes)?))
    }

    /// Decrypts and authenticates every record, then — and only then — verifies
    /// the host binding against the now-trusted metadata digest.
    fn authenticated_records(
        &self,
        key: &VaultKey,
        vault: &DecodedVault,
    ) -> Result<PlaintextRecords, SecretStoreError> {
        let mut records = Vec::with_capacity(vault.records.len());
        for record in &vault.records {
            let mut plaintext = Zeroizing::new(record.ciphertext.clone());
            decrypt_record(
                key.bytes(),
                vault.version,
                &vault.host_binding_digest,
                &record.name,
                &record.nonce,
                &mut plaintext,
            )?;
            records.push((record.name.clone(), plaintext));
        }
        vault.verify_binding(&self.binding)?;
        Ok(records)
    }

    /// Re-encrypts every record with a fresh nonce and atomically replaces the
    /// vault file, bound to the current host digest.
    fn write_vault(
        &self,
        key: &VaultKey,
        records: &PlaintextRecords,
    ) -> Result<(), SecretStoreError> {
        let mut records: Vec<(SecretName, Zeroizing<Vec<u8>>)> = records.to_vec();
        records.sort_by(|a, b| a.0.cmp(&b.0));
        let mut encoded = Vec::with_capacity(records.len());
        for (name, plaintext) in &records {
            let nonce = random_bytes::<NONCE_BYTES>()?;
            let mut buffer = plaintext.clone();
            encrypt_record(
                key.bytes(),
                VAULT_VERSION,
                self.binding.digest(),
                name,
                &nonce,
                &mut buffer,
            )?;
            encoded.push((name.clone(), nonce, buffer.to_vec()));
        }
        let json = encode_vault(VAULT_VERSION, self.binding.digest(), &encoded)?;
        atomic_write(&self.vault_path, &json)
    }

    /// The vault key for an existing vault; read-only so `get`/`list`/`delete`
    /// never create keychain entries. A vault without its key is unrecoverable
    /// and treated as corruption.
    fn existing_vault_key(&self) -> Result<VaultKey, SecretStoreError> {
        match VaultKey::load(self.keychain())? {
            Some(key) => Ok(key),
            None => Err(SecretStoreError::VaultCorruption),
        }
    }

    /// Sets (creating or replacing) the secret `name` to `value`.
    ///
    /// Replaces only the exact canonical name; unrelated records are retained
    /// byte-semantically (decrypted and re-encrypted unchanged). Every record
    /// in the rewritten vault gets a fresh nonce.
    pub(crate) fn set(
        &self,
        name: &SecretName,
        value: &Zeroizing<String>,
    ) -> Result<(), SecretStoreError> {
        let existing = self.read_vault()?;
        let key = match &existing {
            Some(_) => self.existing_vault_key()?,
            None => VaultKey::load_or_create(self.keychain())?,
        };
        let mut records = match &existing {
            Some(vault) => self.authenticated_records(&key, vault)?,
            None => Vec::new(),
        };
        let new_value = Zeroizing::new(value.as_bytes().to_vec());
        if let Some(entry) = records.iter_mut().find(|(n, _)| n == name) {
            entry.1 = new_value;
        } else {
            records.push((name.clone(), new_value));
        }
        self.write_vault(&key, &records)
    }

    /// Returns the plaintext of `name`, or [`SecretStoreError::NotFound`].
    ///
    /// Never creates vault files or keychain entries. Authentication failure
    /// surfaces as corruption without revealing whether the record existed.
    pub(crate) fn get(&self, name: &SecretName) -> Result<Zeroizing<String>, SecretStoreError> {
        let Some(vault) = self.read_vault()? else {
            return Err(SecretStoreError::NotFound);
        };
        let key = self.existing_vault_key()?;
        let Some(record) = vault.records.iter().find(|r| &r.name == name) else {
            return Err(SecretStoreError::NotFound);
        };
        let mut plaintext = Zeroizing::new(record.ciphertext.clone());
        decrypt_record(
            key.bytes(),
            vault.version,
            &vault.host_binding_digest,
            &record.name,
            &record.nonce,
            &mut plaintext,
        )?;
        // The binding is verified only after authenticated decryption.
        vault.verify_binding(&self.binding)?;
        let text =
            std::str::from_utf8(&plaintext).map_err(|_| SecretStoreError::VaultCorruption)?;
        Ok(Zeroizing::new(text.to_owned()))
    }

    /// Lists canonical names in sorted order. Decrypts no record plaintext;
    /// a missing vault is an empty list.
    pub(crate) fn list(&self) -> Result<Vec<SecretName>, SecretStoreError> {
        let Some(vault) = self.read_vault()? else {
            return Ok(Vec::new());
        };
        vault.verify_binding(&self.binding)?;
        let mut names: Vec<SecretName> = vault.records.iter().map(|r| r.name.clone()).collect();
        names.sort();
        Ok(names)
    }

    /// Deletes exactly `name`; absent names return
    /// [`SecretStoreError::NotFound`] without rewriting the vault.
    pub(crate) fn delete(&self, name: &SecretName) -> Result<(), SecretStoreError> {
        let Some(vault) = self.read_vault()? else {
            return Err(SecretStoreError::NotFound);
        };
        if !vault.records.iter().any(|r| &r.name == name) {
            return Err(SecretStoreError::NotFound);
        }
        let key = self.existing_vault_key()?;
        let records = self.authenticated_records(&key, &vault)?;
        let remaining: Vec<_> = records.into_iter().filter(|(n, _)| n != name).collect();
        self.write_vault(&key, &remaining)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::test_support::StoredKeychain;
    use crate::secrets::{HostBinding, SecretStore};
    use std::path::PathBuf;

    fn name(s: &str) -> SecretName {
        SecretName::new(s).expect("valid test name")
    }

    fn secret(s: &str) -> Zeroizing<String> {
        Zeroizing::new(s.to_owned())
    }

    fn test_binding() -> HostBinding {
        HostBinding::from_identifier_bytes(b"test-machine-id\n").expect("valid identifier")
    }

    fn store_in(dir: &Path) -> (SecretStore, StoredKeychain) {
        let keychain = StoredKeychain::new();
        let store = SecretStore::new(
            Box::new(keychain.clone()),
            test_binding(),
            dir.join("ee").join("secrets").join("v1.json"),
        );
        (store, keychain)
    }

    fn vault_path(dir: &Path) -> PathBuf {
        dir.join("ee").join("secrets").join("v1.json")
    }

    fn read_vault_json(dir: &Path) -> serde_json::Value {
        let text = fs::read_to_string(vault_path(dir)).expect("vault file exists");
        serde_json::from_str(&text).expect("vault parses as JSON")
    }

    fn rewrite_vault_file(dir: &Path, json: &str) {
        fs::write(vault_path(dir), json).expect("rewrite vault file");
    }

    /// Mutates one byte inside a base64 field while keeping it valid base64.
    fn tamper_b64_field(dir: &Path, field_path: &str, flip_byte_at: usize) {
        let mut value = read_vault_json(dir);
        let field = value
            .pointer_mut(field_path)
            .expect("field exists")
            .as_str()
            .expect("field is a string");
        let mut bytes = STANDARD.decode(field).expect("valid base64");
        bytes[flip_byte_at] ^= 0x01;
        *value.pointer_mut(field_path).expect("field exists") =
            serde_json::Value::String(STANDARD.encode(&bytes));
        rewrite_vault_file(dir, &serde_json::to_string(&value).expect("serialize"));
    }

    // ── Store operations ─────────────────────────────────────────────────────

    #[test]
    fn set_then_get_round_trips_value() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_in(dir.path());
        store.set(&name("api-key"), &secret("sk-live-123")).expect("set");
        assert_eq!(store.get(&name("api-key")).expect("get").as_str(), "sk-live-123");
    }

    #[test]
    fn set_replaces_exact_name_and_retains_others_byte_semantically() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_in(dir.path());
        store.set(&name("a"), &secret("first-a")).expect("set a");
        store.set(&name("b"), &secret("first-b")).expect("set b");
        store.set(&name("a"), &secret("second-a")).expect("replace a");

        assert_eq!(store.get(&name("a")).expect("get a").as_str(), "second-a");
        assert_eq!(store.get(&name("b")).expect("get b").as_str(), "first-b");
        assert_eq!(store.list().expect("list"), vec![name("a"), name("b")]);
    }

    #[test]
    fn get_missing_returns_not_found_without_creating_entries() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, keychain) = store_in(dir.path());
        assert!(matches!(store.get(&name("nope")), Err(SecretStoreError::NotFound)));
        assert_eq!(keychain.load_calls(), 0, "no keychain load");
        assert_eq!(keychain.store_calls(), 0, "no keychain store");
        assert!(!vault_path(dir.path()).exists(), "no vault file created");
    }

    #[test]
    fn delete_absent_does_not_rewrite_or_create() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, keychain) = store_in(dir.path());
        assert!(matches!(store.delete(&name("nope")), Err(SecretStoreError::NotFound)));
        assert_eq!(keychain.load_calls(), 0);
        assert_eq!(keychain.store_calls(), 0);
        assert!(!vault_path(dir.path()).exists());
    }

    #[test]
    fn delete_removes_exact_name_and_absent_second_delete_does_not_rewrite() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, keychain) = store_in(dir.path());
        store.set(&name("a"), &secret("1")).expect("set a");
        store.set(&name("b"), &secret("2")).expect("set b");

        store.delete(&name("a")).expect("delete a");
        assert_eq!(store.list().expect("list"), vec![name("b")]);
        assert!(matches!(store.get(&name("a")), Err(SecretStoreError::NotFound)));

        let stores_before = keychain.store_calls();
        assert!(matches!(store.delete(&name("a")), Err(SecretStoreError::NotFound)));
        assert_eq!(keychain.store_calls(), stores_before, "absent delete does not rewrite");
    }

    #[test]
    fn list_returns_sorted_names_and_decrypts_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_in(dir.path());
        store.set(&name("c"), &secret("3")).expect("set c");
        store.set(&name("a"), &secret("1")).expect("set a");
        store.set(&name("b"), &secret("2")).expect("set b");

        // Corrupt one ciphertext (valid base64, wrong bytes): parse still
        // accepts the shape, list must not decrypt and therefore still works.
        tamper_b64_field(dir.path(), "/records/0/ciphertext", 0);
        assert_eq!(store.list().expect("list"), vec![name("a"), name("b"), name("c")]);
        assert!(matches!(store.get(&name("a")), Err(SecretStoreError::VaultCorruption)));
    }

    #[test]
    fn missing_vault_lists_empty() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_in(dir.path());
        assert_eq!(store.list().expect("list"), Vec::<SecretName>::new());
    }

    #[test]
    fn deleting_every_record_leaves_an_empty_vault() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_in(dir.path());
        store.set(&name("a"), &secret("1")).expect("set a");
        store.delete(&name("a")).expect("delete a");
        assert_eq!(store.list().expect("list"), Vec::<SecretName>::new());
        assert!(matches!(store.get(&name("a")), Err(SecretStoreError::NotFound)));
    }

    #[test]
    fn set_serializes_canonical_vault_shape() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_in(dir.path());
        store.set(&name("b"), &secret("2")).expect("set b");
        store.set(&name("a"), &secret("1")).expect("set a");

        let json = read_vault_json(dir.path());
        assert_eq!(json["version"], 1);
        assert_eq!(json["host_binding_digest"], encode_hex32(test_binding().digest()));
        let records = json["records"].as_array().expect("records array");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["name"], "a");
        assert_eq!(records[1]["name"], "b");
        for record in records {
            assert!(record.get("name").is_some());
            assert!(record.get("nonce").is_some());
            assert!(record.get("ciphertext").is_some());
            assert_eq!(STANDARD.decode(record["nonce"].as_str().unwrap()).unwrap().len(), 24);
            assert!(STANDARD.decode(record["ciphertext"].as_str().unwrap()).unwrap().len() >= 16);
        }
    }

    #[test]
    fn vault_json_contains_no_plaintext_or_raw_identifiers() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_in(dir.path());
        store.set(&name("openrouter-key"), &secret("sk-secret-OPENROUTER-9876")).expect("set");

        let text = fs::read_to_string(vault_path(dir.path())).expect("vault file");
        assert!(!text.contains("sk-secret-OPENROUTER-9876"), "no plaintext in vault JSON");
        assert!(!text.contains("test-machine-id"), "no raw machine identifier");
        // Only the digest (hex) and base64 nonce/ciphertext appear.
        assert!(text.contains(&encode_hex32(test_binding().digest())));
    }

    #[test]
    fn nonces_are_fresh_per_write_and_per_record() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_in(dir.path());
        store.set(&name("a"), &secret("1")).expect("first set");
        let first = read_vault_json(dir.path());
        let nonce_first = first["records"][0]["nonce"].as_str().unwrap().to_owned();

        store.set(&name("a"), &secret("2")).expect("replace set");
        let second = read_vault_json(dir.path());
        let nonce_replaced = second["records"][0]["nonce"].as_str().unwrap().to_owned();
        assert_ne!(nonce_replaced, nonce_first, "replacement gets a fresh nonce");

        store.set(&name("b"), &secret("3")).expect("add b");
        let third = read_vault_json(dir.path());
        let nonce_b = third["records"][1]["nonce"].as_str().unwrap().to_owned();
        let nonce_a_again = third["records"][0]["nonce"].as_str().unwrap().to_owned();
        assert_ne!(nonce_a_again, nonce_first, "every rewrite refreshes all nonces");
        assert_ne!(nonce_b, nonce_a_again, "different records get different nonces");
    }

    // ── Tamper detection ─────────────────────────────────────────────────────

    #[test]
    fn modified_ciphertext_is_detected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_in(dir.path());
        store.set(&name("a"), &secret("1")).expect("set");
        tamper_b64_field(dir.path(), "/records/0/ciphertext", 0);
        assert!(matches!(store.get(&name("a")), Err(SecretStoreError::VaultCorruption)));
    }

    #[test]
    fn modified_nonce_is_detected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_in(dir.path());
        store.set(&name("a"), &secret("1")).expect("set");
        tamper_b64_field(dir.path(), "/records/0/nonce", 0);
        assert!(matches!(store.get(&name("a")), Err(SecretStoreError::VaultCorruption)));
    }

    #[test]
    fn swapped_record_names_are_detected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_in(dir.path());
        store.set(&name("a"), &secret("1")).expect("set a");
        store.set(&name("b"), &secret("2")).expect("set b");

        let mut json = read_vault_json(dir.path());
        let names = ["a".to_owned(), "b".to_owned()];
        let swapped: Vec<String> = names.iter().rev().cloned().collect();
        for (record, swapped_name) in
            json["records"].as_array_mut().unwrap().iter_mut().zip(swapped)
        {
            record["name"] = serde_json::Value::String(swapped_name);
        }
        rewrite_vault_file(dir.path(), &serde_json::to_string(&json).expect("serialize"));

        assert!(matches!(store.get(&name("a")), Err(SecretStoreError::VaultCorruption)));
        assert!(matches!(store.get(&name("b")), Err(SecretStoreError::VaultCorruption)));
        // set validates the whole vault before mutating anything.
        assert!(matches!(
            store.set(&name("c"), &secret("3")),
            Err(SecretStoreError::VaultCorruption)
        ));
    }

    #[test]
    fn modified_host_digest_is_detected_as_tampering() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_in(dir.path());
        store.set(&name("a"), &secret("1")).expect("set");

        let mut json = read_vault_json(dir.path());
        let other_digest = HostBinding::from_identifier_bytes(b"other-machine-id").expect("valid");
        json["host_binding_digest"] =
            serde_json::Value::String(encode_hex32(other_digest.digest()));
        rewrite_vault_file(dir.path(), &serde_json::to_string(&json).expect("serialize"));

        // The records no longer authenticate against the tampered digest.
        assert!(matches!(store.get(&name("a")), Err(SecretStoreError::VaultCorruption)));
    }

    #[test]
    fn copied_vault_on_other_host_fails_closed_with_mismatch() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store_a, keychain) = store_in(dir.path());
        store_a.set(&name("a"), &secret("top-secret-value")).expect("set on host A");

        // Same vault file and keychain content, different host binding.
        let host_b = HostBinding::from_identifier_bytes(b"other-machine-id").expect("valid");
        let store_b = SecretStore::new(Box::new(keychain.clone()), host_b, vault_path(dir.path()));
        assert!(matches!(
            store_b.get(&name("a")),
            Err(SecretStoreError::HostBindingMismatch { version: 1 })
        ));
        assert!(matches!(
            store_b.set(&name("b"), &secret("x")),
            Err(SecretStoreError::HostBindingMismatch { version: 1 })
        ));
        assert!(matches!(
            store_b.list(),
            Err(SecretStoreError::HostBindingMismatch { version: 1 })
        ));
        // No plaintext leaked into any error text.
        let err = store_b.get(&name("a")).expect_err("must fail");
        assert!(!err.to_string().contains("top-secret-value"));
    }

    #[test]
    fn future_version_is_rejected() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_in(dir.path());
        store.set(&name("a"), &secret("1")).expect("set");
        let mut json = read_vault_json(dir.path());
        json["version"] = serde_json::Value::Number(2.into());
        rewrite_vault_file(dir.path(), &serde_json::to_string(&json).expect("serialize"));

        assert!(matches!(
            store.get(&name("a")),
            Err(SecretStoreError::UnsupportedVersion { version: 2 })
        ));
        assert!(matches!(store.list(), Err(SecretStoreError::UnsupportedVersion { version: 2 })));
    }

    #[test]
    fn vault_without_keychain_key_fails_closed() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_in(dir.path());
        store.set(&name("a"), &secret("1")).expect("set with key");

        // A second store on the same vault but with an empty keychain (the
        // key entry was lost): content is unrecoverable, fail closed.
        let empty_keychain = StoredKeychain::new();
        let orphan =
            SecretStore::new(Box::new(empty_keychain), test_binding(), vault_path(dir.path()));
        assert!(matches!(orphan.get(&name("a")), Err(SecretStoreError::VaultCorruption)));
        assert!(matches!(
            orphan.set(&name("b"), &secret("2")),
            Err(SecretStoreError::VaultCorruption)
        ));
    }

    // ── Parse rejections ─────────────────────────────────────────────────────

    #[test]
    fn decode_rejects_malformed_vaults_table_driven() {
        let digest_hex = encode_hex32(test_binding().digest());
        let valid_nonce = STANDARD.encode([0u8; 24]);
        let valid_ct = STANDARD.encode([0u8; 16]);
        let mut cases: Vec<(&str, serde_json::Value, SecretStoreError)> = vec![
            (
                "unknown top-level field",
                serde_json::json!({
                    "version": 1,
                    "host_binding_digest": digest_hex,
                    "records": [],
                    "extra": true,
                }),
                SecretStoreError::VaultCorruption,
            ),
            (
                "unknown record field",
                serde_json::json!({
                    "version": 1,
                    "host_binding_digest": digest_hex,
                    "records": [{ "name": "a", "nonce": valid_nonce, "ciphertext": valid_ct, "extra": 1 }],
                }),
                SecretStoreError::VaultCorruption,
            ),
            (
                "duplicate names",
                serde_json::json!({
                    "version": 1,
                    "host_binding_digest": digest_hex,
                    "records": [
                        { "name": "a", "nonce": valid_nonce, "ciphertext": valid_ct },
                        { "name": "a", "nonce": valid_nonce, "ciphertext": valid_ct },
                    ],
                }),
                SecretStoreError::VaultCorruption,
            ),
            (
                "malformed nonce base64",
                serde_json::json!({
                    "version": 1,
                    "host_binding_digest": digest_hex,
                    "records": [{ "name": "a", "nonce": "!!!", "ciphertext": valid_ct }],
                }),
                SecretStoreError::VaultCorruption,
            ),
            (
                "malformed ciphertext base64",
                serde_json::json!({
                    "version": 1,
                    "host_binding_digest": digest_hex,
                    "records": [{ "name": "a", "nonce": valid_nonce, "ciphertext": "###" }],
                }),
                SecretStoreError::VaultCorruption,
            ),
            (
                "nonce wrong length",
                serde_json::json!({
                    "version": 1,
                    "host_binding_digest": digest_hex,
                    "records": [{ "name": "a", "nonce": STANDARD.encode([0u8; 16]), "ciphertext": valid_ct }],
                }),
                SecretStoreError::VaultCorruption,
            ),
            (
                "ciphertext too short",
                serde_json::json!({
                    "version": 1,
                    "host_binding_digest": digest_hex,
                    "records": [{ "name": "a", "nonce": valid_nonce, "ciphertext": STANDARD.encode([0u8; 4]) }],
                }),
                SecretStoreError::VaultCorruption,
            ),
            (
                "invalid digest hex",
                serde_json::json!({
                    "version": 1,
                    "host_binding_digest": "zz".repeat(32),
                    "records": [],
                }),
                SecretStoreError::VaultCorruption,
            ),
            (
                "invalid digest length",
                serde_json::json!({
                    "version": 1,
                    "host_binding_digest": "ab",
                    "records": [],
                }),
                SecretStoreError::VaultCorruption,
            ),
            (
                "invalid record name",
                serde_json::json!({
                    "version": 1,
                    "host_binding_digest": digest_hex,
                    "records": [{ "name": "bad name", "nonce": valid_nonce, "ciphertext": valid_ct }],
                }),
                SecretStoreError::VaultCorruption,
            ),
            (
                "trailing garbage",
                serde_json::json!({
                    "version": 1,
                    "host_binding_digest": digest_hex,
                    "records": [],
                }),
                SecretStoreError::VaultCorruption,
            ),
            ("non-object json", serde_json::json!([1, 2, 3]), SecretStoreError::VaultCorruption),
        ];
        // Future version (otherwise valid).
        cases.push((
            "future version",
            serde_json::json!({
                "version": 2,
                "host_binding_digest": digest_hex,
                "records": [],
            }),
            SecretStoreError::UnsupportedVersion { version: 2 },
        ));

        for (label, mut value, expected) in cases {
            let json_text = if label == "trailing garbage" {
                format!("{}{}", serde_json::to_string(&value).expect("serialize"), " trailing")
            } else if label == "non-object json" {
                serde_json::to_string(&value).expect("serialize")
            } else {
                let _ = &mut value;
                serde_json::to_string(&value).expect("serialize")
            };
            // `DecodedVault` deliberately has no `Debug`; compare the error only.
            match VaultFileV1::decode(json_text.as_bytes()) {
                Err(err) => assert_eq!(err, expected, "case {label}"),
                Ok(_) => panic!("case {label}: expected {expected:?}, got a decoded vault"),
            }
        }
    }

    #[test]
    fn unsorted_records_are_accepted_and_reserialized_sorted() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_in(dir.path());
        store.set(&name("b"), &secret("2")).expect("set b");
        store.set(&name("a"), &secret("1")).expect("set a");
        let mut json = read_vault_json(dir.path());
        // Deliberately unsort the stored records.
        let records = json["records"].take();
        json["records"] = serde_json::json!([records[1].clone(), records[0].clone()]);
        rewrite_vault_file(dir.path(), &serde_json::to_string(&json).expect("serialize"));

        // Decryption still works (AAD is name-bound, not order-bound)...
        assert_eq!(store.get(&name("a")).expect("get a").as_str(), "1");
        // ...and the next write reserializes sorted.
        store.set(&name("c"), &secret("3")).expect("set c");
        let records = read_vault_json(dir.path());
        let names: Vec<&str> = records["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    // ── Paths, permissions, atomicity ────────────────────────────────────────

    #[test]
    fn path_construction_is_canonical() {
        let data_dir = Path::new("/tmp/fake-data-dir");
        assert_eq!(
            vault_path_from(data_dir),
            PathBuf::from("/tmp/fake-data-dir/ee/secrets/v1.json")
        );
        assert_eq!(
            default_vault_path().expect("default path"),
            vault_path_from(&dirs::data_dir().expect("data dir in test env"))
        );
    }

    #[test]
    fn first_write_creates_parent_directories() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_in(dir.path());
        store.set(&name("a"), &secret("1")).expect("set");
        assert!(vault_path(dir.path()).is_file());
    }

    #[cfg(unix)]
    #[test]
    fn unix_vault_permissions_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_in(dir.path());
        store.set(&name("a"), &secret("1")).expect("first set");
        store.set(&name("a"), &secret("2")).expect("replacement set");

        let file_mode =
            fs::metadata(vault_path(dir.path())).expect("vault metadata").permissions().mode();
        assert_eq!(file_mode & 0o777, 0o600, "vault file is owner-only");
        let parent_mode = fs::metadata(vault_path(dir.path()).parent().unwrap())
            .expect("parent metadata")
            .permissions()
            .mode();
        assert_eq!(parent_mode & 0o777, 0o700, "vault parent dir is owner-only");
    }

    #[cfg(unix)]
    #[test]
    fn interrupted_write_preserves_previous_vault() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temp dir");
        let (store, _keychain) = store_in(dir.path());
        store.set(&name("a"), &secret("first")).expect("set first");

        // Block writes to the vault directory: the temp file cannot be created
        // and the previous vault must survive untouched.
        let path = vault_path(dir.path());
        let parent = path.parent().unwrap();
        fs::set_permissions(parent, fs::Permissions::from_mode(0o500)).expect("lock dir");
        assert!(matches!(store.set(&name("b"), &secret("second")), Err(SecretStoreError::Io(_))));
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).expect("unlock dir");

        assert_eq!(store.get(&name("a")).expect("old vault readable").as_str(), "first");
        assert!(matches!(store.get(&name("b")), Err(SecretStoreError::NotFound)));
        // No partial replacement or temp leftovers.
        let entries: Vec<_> = fs::read_dir(parent)
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["v1.json".to_owned()], "only the vault file remains");

        // The store recovers once the directory is writable again.
        store.set(&name("b"), &secret("second")).expect("write after recovery");
        assert_eq!(store.get(&name("b")).expect("get b").as_str(), "second");
    }
}
