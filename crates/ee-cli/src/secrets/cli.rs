//! `ee do secrets` command surface (phase 4).
//!
//! Safe, scriptable commands over the [`SecretStore`]: `set`, `get`, `list`,
//! `delete`, `reset`, and `status`. Secret values never appear as CLI arguments, in
//! stdout beyond the explicitly permitted raw `get` output, or in any
//! diagnostic or error text. Interactive values are read with terminal echo
//! disabled (crossterm raw mode disables echo on Unix and Windows); `--stdin`
//! is the explicit opt-in for automated workflows, capped at 64 KiB.

use std::fmt;
use std::io::{self, Read, Write};
use std::path::Path;

use zeroize::Zeroizing;

use super::keychain::{KEYCHAIN_ACCOUNT, KEYCHAIN_SERVICE};
use super::vault::reset_vault_file;
use super::{SecretName, SecretStore, SecretStoreError};

/// Maximum secret value size accepted from any input source.
pub(crate) const MAX_SECRET_VALUE_BYTES: usize = 64 * 1024;

// ── Exit codes ───────────────────────────────────────────────────────────────

pub(crate) const EXIT_SECRETS_NOT_FOUND: i32 = 20;
pub(crate) const EXIT_SECRETS_USER_INPUT: i32 = 21;
pub(crate) const EXIT_SECRETS_KEYCHAIN: i32 = 22;
pub(crate) const EXIT_SECRETS_HOST_BINDING: i32 = 23;
pub(crate) const EXIT_SECRETS_VAULT_CORRUPTION: i32 = 24;
pub(crate) const EXIT_SECRETS_HOST_MISMATCH: i32 = 25;
pub(crate) const EXIT_SECRETS_IO: i32 = 26;
pub(crate) const EXIT_SECRETS_UNSUPPORTED_VERSION: i32 = 27;

/// CLI-level failure: store errors plus safe user-input classifications.
///
/// Messages never contain secret values, ciphertext, keys, host digests, or
/// raw machine identifiers.
#[derive(Debug)]
pub(crate) enum SecretsCliError {
    Store(SecretStoreError),
    /// `get` refused because stdout is a terminal and `--force` is absent.
    RefusedTerminalOutput,
    /// The secret value was empty after normalization.
    EmptySecret,
    /// The secret value exceeded [`MAX_SECRET_VALUE_BYTES`].
    SecretTooLarge,
    /// The secret value is not valid UTF-8.
    InvalidUtf8,
    /// Interactive input was cancelled.
    Cancelled,
}

impl fmt::Display for SecretsCliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(e) => write!(f, "{e}"),
            Self::RefusedTerminalOutput => write!(
                f,
                "refusing to print a secret to a terminal; redirect stdout or pass --force"
            ),
            Self::EmptySecret => write!(f, "secret value must not be empty"),
            Self::SecretTooLarge => {
                write!(f, "secret value exceeds {MAX_SECRET_VALUE_BYTES} bytes")
            }
            Self::InvalidUtf8 => write!(f, "secret value is not valid UTF-8"),
            Self::Cancelled => write!(f, "secret input cancelled"),
        }
    }
}

impl std::error::Error for SecretsCliError {}

impl From<SecretStoreError> for SecretsCliError {
    fn from(err: SecretStoreError) -> Self {
        Self::Store(err)
    }
}

/// Stable non-zero exit code per failure class.
pub(crate) fn exit_code(err: &SecretsCliError) -> i32 {
    match err {
        SecretsCliError::Store(e) => match e {
            SecretStoreError::NotFound => EXIT_SECRETS_NOT_FOUND,
            SecretStoreError::InvalidName(_) | SecretStoreError::InvalidReference(_) => {
                EXIT_SECRETS_USER_INPUT
            }
            SecretStoreError::KeychainUnavailable
            | SecretStoreError::KeychainCorruption
            | SecretStoreError::CspRngUnavailable => EXIT_SECRETS_KEYCHAIN,
            SecretStoreError::HostBindingUnavailable => EXIT_SECRETS_HOST_BINDING,
            SecretStoreError::HostBindingMismatch { .. } => EXIT_SECRETS_HOST_MISMATCH,
            SecretStoreError::UnsupportedVersion { .. } => EXIT_SECRETS_UNSUPPORTED_VERSION,
            SecretStoreError::VaultCorruption => EXIT_SECRETS_VAULT_CORRUPTION,
            SecretStoreError::DataDirUnavailable | SecretStoreError::Io(_) => EXIT_SECRETS_IO,
        },
        SecretsCliError::RefusedTerminalOutput
        | SecretsCliError::EmptySecret
        | SecretsCliError::SecretTooLarge
        | SecretsCliError::InvalidUtf8
        | SecretsCliError::Cancelled => EXIT_SECRETS_USER_INPUT,
    }
}

// ── Secret input ─────────────────────────────────────────────────────────────

/// A source of a secret value: hidden terminal prompt or stdin.
pub(crate) trait SecretValueSource {
    fn read_value(&mut self) -> Result<Zeroizing<String>, SecretsCliError>;
}

/// Stdin source: at most [`MAX_SECRET_VALUE_BYTES`] bytes, exactly one final
/// line ending removed, all other bytes preserved.
pub(crate) struct StdinSecretSource<'a> {
    pub(crate) reader: &'a mut dyn Read,
}

impl SecretValueSource for StdinSecretSource<'_> {
    fn read_value(&mut self) -> Result<Zeroizing<String>, SecretsCliError> {
        let mut bytes = Vec::with_capacity(MAX_SECRET_VALUE_BYTES);
        let mut chunk = [0u8; 4096];
        loop {
            let n = self
                .reader
                .read(&mut chunk)
                .map_err(|e| SecretsCliError::Store(SecretStoreError::Io(e)))?;
            if n == 0 {
                break;
            }
            if bytes.len() + n > MAX_SECRET_VALUE_BYTES {
                return Err(SecretsCliError::SecretTooLarge);
            }
            bytes.extend_from_slice(&chunk[..n]);
        }
        normalize_stdin_bytes(bytes)
    }
}

/// Removes exactly one final line ending (`\n`, `\r\n`, or lone `\r`) and
/// preserves every other byte, then rejects empty values.
fn normalize_stdin_bytes(mut bytes: Vec<u8>) -> Result<Zeroizing<String>, SecretsCliError> {
    if bytes.ends_with(b"\r\n") {
        bytes.truncate(bytes.len() - 2);
    } else if bytes.last() == Some(&b'\n') || bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    if bytes.is_empty() {
        return Err(SecretsCliError::EmptySecret);
    }
    let text = String::from_utf8(bytes).map_err(|_| SecretsCliError::InvalidUtf8)?;
    Ok(Zeroizing::new(text))
}

/// Hidden terminal source: crossterm raw mode disables echo on every
/// supported platform, so typed characters never appear on screen.
pub(crate) struct HiddenTerminalSecretSource;

impl SecretValueSource for HiddenTerminalSecretSource {
    fn read_value(&mut self) -> Result<Zeroizing<String>, SecretsCliError> {
        read_hidden_terminal_secret()
    }
}

fn read_hidden_terminal_secret() -> Result<Zeroizing<String>, SecretsCliError> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

    enable_raw_mode().map_err(|e| SecretsCliError::Store(SecretStoreError::Io(e)))?;
    let result = (|| {
        eprint!("secret value: ");
        io::stderr().flush().map_err(|e| SecretsCliError::Store(SecretStoreError::Io(e)))?;
        let mut value = String::new();
        loop {
            match read().map_err(|e| SecretsCliError::Store(SecretStoreError::Io(e)))? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Enter => break,
                    KeyCode::Backspace => {
                        value.pop();
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Err(SecretsCliError::Cancelled);
                    }
                    KeyCode::Char(c) if !c.is_control() => {
                        value.push(c);
                        if value.len() > MAX_SECRET_VALUE_BYTES {
                            return Err(SecretsCliError::SecretTooLarge);
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        if value.is_empty() {
            return Err(SecretsCliError::EmptySecret);
        }
        Ok(Zeroizing::new(value))
    })();
    let _ = disable_raw_mode();
    eprintln!();
    result
}

/// Reads a secret value from the selected source. `from_stdin` selects the
/// stdin path; otherwise the terminal source is used.
pub(crate) fn read_secret_value(
    from_stdin: bool,
    stdin: &mut dyn Read,
    terminal_source: &mut dyn SecretValueSource,
) -> Result<Zeroizing<String>, SecretsCliError> {
    if from_stdin {
        StdinSecretSource { reader: stdin }.read_value()
    } else {
        terminal_source.read_value()
    }
}

// ── Command dispatch helpers ─────────────────────────────────────────────────

fn write_out(out: &mut dyn Write, text: &str) -> Result<(), SecretsCliError> {
    out.write_all(text.as_bytes()).map_err(|e| SecretsCliError::Store(SecretStoreError::Io(e)))
}

fn write_line(out: &mut dyn Write, text: &str) -> Result<(), SecretsCliError> {
    write_out(out, &format!("{text}\n"))
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

/// `set <name>`: reads the value from the selected input source and stores it.
/// Prints only an acknowledgement.
pub(crate) fn run_secrets_set(
    store: &SecretStore,
    name: &SecretName,
    from_stdin: bool,
    stdin: &mut dyn Read,
    terminal_source: &mut dyn SecretValueSource,
    out: &mut dyn Write,
) -> Result<(), SecretsCliError> {
    let value = read_secret_value(from_stdin, stdin, terminal_source)?;
    store.set(name, &value)?;
    write_line(out, &format!("set secret {name}"))
}

/// `get <name>`: prints the exact raw value plus one newline, only when
/// stdout is not a terminal or `--force` was passed.
pub(crate) fn run_secrets_get(
    store: &SecretStore,
    name: &SecretName,
    force: bool,
    stdout_is_terminal: bool,
    out: &mut dyn Write,
) -> Result<(), SecretsCliError> {
    if stdout_is_terminal && !force {
        return Err(SecretsCliError::RefusedTerminalOutput);
    }
    let value = store.get(name)?;
    write_out(out, &value)?;
    write_out(out, "\n")
}

/// `list`: prints sorted names only, one per line. Decrypts nothing.
pub(crate) fn run_secrets_list(
    store: &SecretStore,
    out: &mut dyn Write,
) -> Result<(), SecretsCliError> {
    for name in store.list()? {
        write_line(out, &name.to_string())?;
    }
    Ok(())
}

/// `delete <name>`: removes exactly the named secret; prints only an
/// acknowledgement.
pub(crate) fn run_secrets_delete(
    store: &SecretStore,
    name: &SecretName,
    out: &mut dyn Write,
) -> Result<(), SecretsCliError> {
    store.delete(name)?;
    write_line(out, &format!("deleted secret {name}"))
}

/// `reset`: removes the encrypted vault file. It is idempotent and does not
/// touch the OS-keychain key, so a future `set` creates a fresh vault.
pub(crate) fn run_secrets_reset(
    vault_path: &Path,
    out: &mut dyn Write,
) -> Result<(), SecretsCliError> {
    let _removed = reset_vault_file(vault_path)?;
    write_line(out, "reset encrypted secrets vault")
}

/// `status`: reports safe state only — vault path, presence, record count
/// when readable, keychain availability, and host-binding verification state.
/// Exits successfully even when individual state probes fail; each probe
/// reports its own result.
pub(crate) fn run_secrets_status(
    store: &SecretStore,
    out: &mut dyn Write,
) -> Result<(), SecretsCliError> {
    write_line(out, &format!("vault path: {}", store.vault_path().display()))?;
    let present = store.vault_path().is_file();
    write_line(out, &format!("vault present: {}", yes_no(present)))?;
    if present {
        match store.list() {
            Ok(names) => write_line(out, &format!("record count: {}", names.len()))?,
            Err(_) => write_line(out, "record count: unavailable")?,
        }
    }
    match store.keychain().load(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT) {
        Ok(_) => write_line(out, "keychain available: yes")?,
        Err(_) => write_line(out, "keychain available: no")?,
    }
    match store.verify_vault_binding_digest() {
        Ok(()) => write_line(out, "host binding verified: yes")?,
        Err(SecretStoreError::HostBindingMismatch { .. }) => {
            write_line(out, "host binding verified: no")?
        }
        Err(_) => write_line(out, "host binding verified: unavailable")?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn read_stdin(bytes: &[u8]) -> Result<Zeroizing<String>, SecretsCliError> {
        StdinSecretSource { reader: &mut Cursor::new(bytes) }.read_value()
    }

    #[test]
    fn stdin_removes_exactly_one_final_line_ending() {
        assert_eq!(read_stdin(b"abc\n").expect("lf").as_str(), "abc");
        assert_eq!(read_stdin(b"abc\r\n").expect("crlf").as_str(), "abc");
        assert_eq!(read_stdin(b"abc\r").expect("cr").as_str(), "abc");
        // A second line ending is data, not the input's single line ending.
        assert_eq!(read_stdin(b"abc\n\n").expect("double").as_str(), "abc\n");
        // No trailing line ending: everything preserved.
        assert_eq!(read_stdin(b"abc").expect("none").as_str(), "abc");
        // Interior and other trailing bytes are preserved exactly.
        assert_eq!(read_stdin(b"abc\n ").expect("trailing space").as_str(), "abc\n ");
        assert_eq!(read_stdin(b"  abc\n").expect("leading space").as_str(), "  abc");
    }

    #[test]
    fn stdin_rejects_empty_after_normalization() {
        assert!(matches!(read_stdin(b""), Err(SecretsCliError::EmptySecret)));
        assert!(matches!(read_stdin(b"\n"), Err(SecretsCliError::EmptySecret)));
        assert!(matches!(read_stdin(b"\r\n"), Err(SecretsCliError::EmptySecret)));
    }

    #[test]
    fn stdin_rejects_oversize_input() {
        let over = vec![b'a'; MAX_SECRET_VALUE_BYTES + 1];
        assert!(matches!(read_stdin(&over), Err(SecretsCliError::SecretTooLarge)));
        // Exactly the cap (plus line ending) is accepted.
        let at_cap = vec![b'a'; MAX_SECRET_VALUE_BYTES];
        assert_eq!(read_stdin(&at_cap).expect("at cap").as_str().len(), MAX_SECRET_VALUE_BYTES);
    }

    #[test]
    fn stdin_rejects_non_utf8() {
        assert!(matches!(read_stdin(b"\xff\xfe secret"), Err(SecretsCliError::InvalidUtf8)));
    }

    #[test]
    fn stdin_and_terminal_sources_are_selected_by_flag() {
        struct CountingTerminalSource {
            calls: std::cell::Cell<usize>,
        }
        impl SecretValueSource for CountingTerminalSource {
            fn read_value(&mut self) -> Result<Zeroizing<String>, SecretsCliError> {
                self.calls.set(self.calls.get() + 1);
                Ok(Zeroizing::new(String::from("terminal-value")))
            }
        }

        let mut stdin = Cursor::new(b"stdin-value\n");
        let mut terminal = CountingTerminalSource { calls: std::cell::Cell::new(0) };

        let via_stdin = read_secret_value(true, &mut stdin, &mut terminal).expect("stdin path");
        assert_eq!(via_stdin.as_str(), "stdin-value");
        assert_eq!(terminal.calls.get(), 0, "stdin flag must not touch the terminal");

        let mut stdin2 = Cursor::new(b"ignored\n");
        let via_terminal =
            read_secret_value(false, &mut stdin2, &mut terminal).expect("terminal path");
        assert_eq!(via_terminal.as_str(), "terminal-value");
        assert_eq!(terminal.calls.get(), 1, "terminal source used without --stdin");
    }

    #[test]
    fn error_messages_never_carry_seeded_values() {
        let seeded = "sk-secret-OPENROUTER-9876";
        for message in [
            SecretsCliError::RefusedTerminalOutput.to_string(),
            SecretsCliError::EmptySecret.to_string(),
            SecretsCliError::SecretTooLarge.to_string(),
            SecretsCliError::InvalidUtf8.to_string(),
            SecretsCliError::Cancelled.to_string(),
            SecretsCliError::Store(SecretStoreError::NotFound).to_string(),
        ] {
            assert!(!message.contains(seeded), "no seeded value in {message:?}");
        }
    }
}
