//! `ee do secrets` command tests (phase 4): parser coverage, safe input,
//! stdout/stderr separation, stable exit codes, and secret redaction.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use clap::Parser as _;
use zeroize::Zeroizing;

use crate::secrets::cli::{
    EXIT_SECRETS_HOST_BINDING, EXIT_SECRETS_HOST_MISMATCH, EXIT_SECRETS_IO, EXIT_SECRETS_KEYCHAIN,
    EXIT_SECRETS_NOT_FOUND, EXIT_SECRETS_UNSUPPORTED_VERSION, EXIT_SECRETS_USER_INPUT,
    EXIT_SECRETS_VAULT_CORRUPTION, HiddenTerminalSecretSource, SecretValueSource, SecretsCliError,
    StdinSecretSource, exit_code, run_secrets_delete, run_secrets_get, run_secrets_list,
    run_secrets_reset, run_secrets_set, run_secrets_status,
};
use crate::secrets::test_support::StoredKeychain;
use crate::secrets::{
    HostBinding, SecretName, SecretNameError, SecretReferenceError, SecretStore, SecretStoreError,
    cli,
};

const SEEDED_SECRET: &str = "sk-secret-OPENROUTER-9876";

fn name(s: &str) -> SecretName {
    SecretName::new(s).expect("valid test name")
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

fn capture() -> Vec<u8> {
    Vec::new()
}

fn output(out: &[u8]) -> String {
    String::from_utf8(out.to_vec()).expect("utf-8 output")
}

// ── Parser coverage ──────────────────────────────────────────────────────────

#[test]
fn secrets_command_subcommands_parse_with_all_flags() {
    let cli =
        crate::Cli::try_parse_from(["ee", "do", "secrets", "set", "api-key", "--stdin"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do {
            command: crate::DoCommands::Secrets {
                command: crate::SecretsCommands::Set { stdin: true, .. }
            }
        })
    ));

    let cli = crate::Cli::try_parse_from(["ee", "do", "secrets", "set", "api-key"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do {
            command: crate::DoCommands::Secrets {
                command: crate::SecretsCommands::Set { stdin: false, .. }
            }
        })
    ));

    let cli =
        crate::Cli::try_parse_from(["ee", "do", "secrets", "get", "api-key", "--force"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do {
            command: crate::DoCommands::Secrets {
                command: crate::SecretsCommands::Get { force: true, .. }
            }
        })
    ));

    let cli = crate::Cli::try_parse_from(["ee", "do", "secrets", "get", "api-key"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do {
            command: crate::DoCommands::Secrets {
                command: crate::SecretsCommands::Get { force: false, .. }
            }
        })
    ));

    let cli = crate::Cli::try_parse_from(["ee", "do", "secrets", "list"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do {
            command: crate::DoCommands::Secrets { command: crate::SecretsCommands::List }
        })
    ));

    let cli = crate::Cli::try_parse_from(["ee", "do", "secrets", "delete", "api-key"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do {
            command: crate::DoCommands::Secrets { command: crate::SecretsCommands::Delete { .. } }
        })
    ));

    let cli = crate::Cli::try_parse_from(["ee", "do", "secrets", "reset"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do {
            command: crate::DoCommands::Secrets { command: crate::SecretsCommands::Reset }
        })
    ));

    let cli = crate::Cli::try_parse_from(["ee", "do", "secrets", "status"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(crate::Commands::Do {
            command: crate::DoCommands::Secrets { command: crate::SecretsCommands::Status }
        })
    ));
}

#[test]
fn secrets_command_rejects_secret_value_as_cli_argument() {
    // No secret value may ever arrive through argument parsing.
    assert!(
        crate::Cli::try_parse_from(["ee", "do", "secrets", "set", "api-key", "sk-literal-value",])
            .is_err()
    );
    assert!(
        crate::Cli::try_parse_from([
            "ee",
            "do",
            "secrets",
            "get",
            "api-key",
            "--force",
            "sk-literal-value",
        ])
        .is_err()
    );
}

#[test]
fn secrets_command_rejects_invalid_flag_combinations() {
    // `--stdin` belongs to `set` only.
    assert!(
        crate::Cli::try_parse_from(["ee", "do", "secrets", "get", "api-key", "--stdin"]).is_err()
    );
    assert!(crate::Cli::try_parse_from(["ee", "do", "secrets", "list", "--force"]).is_err());
    assert!(crate::Cli::try_parse_from(["ee", "do", "secrets", "status", "--stdin"]).is_err());
    // Names are required where commands need them.
    assert!(crate::Cli::try_parse_from(["ee", "do", "secrets", "set"]).is_err());
    assert!(crate::Cli::try_parse_from(["ee", "do", "secrets", "get"]).is_err());
    assert!(crate::Cli::try_parse_from(["ee", "do", "secrets", "delete"]).is_err());
    // Unknown subcommands fail.
    assert!(crate::Cli::try_parse_from(["ee", "do", "secrets", "export"]).is_err());
}

// ── Command behavior ─────────────────────────────────────────────────────────

#[test]
fn secrets_command_set_stdin_stores_and_acknowledges() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (store, _keychain) = store_in(dir.path());
    let mut out = capture();
    let mut stdin = Cursor::new(format!("{SEEDED_SECRET}\n").into_bytes());
    let mut terminal = HiddenTerminalSecretSource;

    run_secrets_set(&store, &name("openrouter-key"), true, &mut stdin, &mut terminal, &mut out)
        .expect("set");

    assert_eq!(output(&out), "set secret openrouter-key\n");
    assert_eq!(store.get(&name("openrouter-key")).expect("stored").as_str(), SEEDED_SECRET);
}

#[test]
fn secrets_command_set_rejects_empty_stdin_value() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (store, _keychain) = store_in(dir.path());
    let mut out = capture();
    let mut stdin = Cursor::new(b"\n".to_vec());
    let mut terminal = HiddenTerminalSecretSource;

    let err = run_secrets_set(&store, &name("a"), true, &mut stdin, &mut terminal, &mut out)
        .expect_err("empty value rejected");
    assert!(matches!(err, SecretsCliError::EmptySecret));
    assert_eq!(exit_code(&err), EXIT_SECRETS_USER_INPUT);
    assert!(output(&out).is_empty(), "no acknowledgement on failure");
    assert!(!vault_path(dir.path()).exists(), "nothing written");
}

#[test]
fn secrets_command_set_rejects_oversize_stdin_value() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (store, _keychain) = store_in(dir.path());
    let mut out = capture();
    let oversize = vec![b'x'; cli::MAX_SECRET_VALUE_BYTES + 1];
    let mut stdin = Cursor::new(oversize);
    let mut terminal = HiddenTerminalSecretSource;

    let err = run_secrets_set(&store, &name("a"), true, &mut stdin, &mut terminal, &mut out)
        .expect_err("oversize rejected");
    assert!(matches!(err, SecretsCliError::SecretTooLarge));
    assert_eq!(exit_code(&err), EXIT_SECRETS_USER_INPUT);
}

#[test]
fn secrets_command_get_piped_emits_raw_value_only() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (store, _keychain) = store_in(dir.path());
    store.set(&name("api-key"), &Zeroizing::new(SEEDED_SECRET.to_owned())).expect("set");

    let mut out = capture();
    run_secrets_get(&store, &name("api-key"), false, false, &mut out).expect("piped get");
    assert_eq!(output(&out), format!("{SEEDED_SECRET}\n"), "raw value plus one newline only");
}

#[test]
fn secrets_command_get_refused_on_terminal_without_force() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (store, _keychain) = store_in(dir.path());
    store.set(&name("api-key"), &Zeroizing::new(SEEDED_SECRET.to_owned())).expect("set");

    let mut out = capture();
    let err = run_secrets_get(&store, &name("api-key"), false, true, &mut out)
        .expect_err("terminal get refused");
    assert!(matches!(err, SecretsCliError::RefusedTerminalOutput));
    assert_eq!(exit_code(&err), EXIT_SECRETS_USER_INPUT);
    assert!(output(&out).is_empty(), "no secret leaked into stdout");

    // `--force` permits terminal output.
    run_secrets_get(&store, &name("api-key"), true, true, &mut out).expect("forced get");
    assert_eq!(output(&out), format!("{SEEDED_SECRET}\n"));
}

#[test]
fn secrets_command_get_missing_is_not_found() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (store, _keychain) = store_in(dir.path());
    let mut out = capture();
    let err = run_secrets_get(&store, &name("nope"), false, false, &mut out).expect_err("missing");
    assert!(matches!(err, SecretsCliError::Store(SecretStoreError::NotFound)));
    assert_eq!(exit_code(&err), EXIT_SECRETS_NOT_FOUND);
    assert!(output(&out).is_empty());
}

#[test]
fn secrets_command_list_prints_sorted_names_only() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (store, _keychain) = store_in(dir.path());
    store.set(&name("zeta"), &Zeroizing::new("1".into())).expect("set zeta");
    store.set(&name("alpha"), &Zeroizing::new("2".into())).expect("set alpha");
    store.set(&name("mid"), &Zeroizing::new("3".into())).expect("set mid");

    let mut out = capture();
    run_secrets_list(&store, &mut out).expect("list");
    assert_eq!(output(&out), "alpha\nmid\nzeta\n");
    assert!(!output(&out).contains(SEEDED_SECRET));
}

#[test]
fn secrets_command_reset_removes_vault_and_preserves_key_for_fresh_storage() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (store, keychain) = store_in(dir.path());
    store.set(&name("api-key"), &Zeroizing::new(SEEDED_SECRET.into())).expect("set secret");
    assert!(vault_path(dir.path()).is_file(), "vault exists before reset");

    let mut out = capture();
    run_secrets_reset(store.vault_path(), &mut out).expect("reset vault");
    assert_eq!(output(&out), "reset encrypted secrets vault\n");
    assert!(!vault_path(dir.path()).exists(), "vault file removed");
    assert!(matches!(store.get(&name("api-key")), Err(SecretStoreError::NotFound)));

    store.set(&name("replacement"), &Zeroizing::new("fresh-value".into())).expect("fresh set");
    assert_eq!(store.get(&name("replacement")).expect("fresh get").as_str(), "fresh-value");
    assert_eq!(keychain.store_calls(), 1, "reset preserves existing keychain key");

    let mut second_out = capture();
    run_secrets_reset(store.vault_path(), &mut second_out).expect("second reset is idempotent");
    assert_eq!(output(&second_out), "reset encrypted secrets vault\n");
}

#[test]
fn secrets_command_delete_acknowledges_and_removes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (store, _keychain) = store_in(dir.path());
    store.set(&name("a"), &Zeroizing::new("1".into())).expect("set a");

    let mut out = capture();
    run_secrets_delete(&store, &name("a"), &mut out).expect("delete");
    assert_eq!(output(&out), "deleted secret a\n");
    assert!(matches!(store.get(&name("a")), Err(SecretStoreError::NotFound)));

    let mut out2 = capture();
    let err = run_secrets_delete(&store, &name("a"), &mut out2).expect_err("absent delete");
    assert!(matches!(err, SecretsCliError::Store(SecretStoreError::NotFound)));
    assert_eq!(exit_code(&err), EXIT_SECRETS_NOT_FOUND);
}

#[test]
fn secrets_command_status_reports_safe_state_only() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (store, _keychain) = store_in(dir.path());

    let mut out = capture();
    run_secrets_status(&store, &mut out).expect("status");
    let before = output(&out);
    assert!(before.contains(&format!("vault path: {}", vault_path(dir.path()).display())));
    assert!(before.contains("vault present: no"));
    assert!(before.contains("keychain available: yes"));
    assert!(before.contains("host binding verified: yes"));
    assert!(!before.contains(SEEDED_SECRET));

    store.set(&name("api-key"), &Zeroizing::new(SEEDED_SECRET.to_owned())).expect("set");

    let mut out2 = capture();
    run_secrets_status(&store, &mut out2).expect("status after set");
    let after = output(&out2);
    assert!(after.contains("vault present: yes"));
    assert!(after.contains("record count: 1"));
    assert!(!after.contains(SEEDED_SECRET), "status never prints values");
}

#[test]
fn secrets_command_errors_redact_seeded_secret_values() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (store, keychain) = store_in(dir.path());
    store.set(&name("api-key"), &Zeroizing::new(SEEDED_SECRET.to_owned())).expect("set");

    // Corrupt the ciphertext: reads fail with corruption, never the value.
    let text = std::fs::read_to_string(vault_path(dir.path())).expect("vault file");
    let mut json: serde_json::Value = serde_json::from_str(&text).expect("valid json");
    let corrupted = base64_engine_encode(&[0u8; 16]);
    json["records"][0]["ciphertext"] = serde_json::Value::String(corrupted);
    std::fs::write(vault_path(dir.path()), serde_json::to_string(&json).expect("serialize"))
        .expect("write");

    let mut out = capture();
    let err =
        run_secrets_get(&store, &name("api-key"), false, false, &mut out).expect_err("corrupt");
    assert!(matches!(err, SecretsCliError::Store(SecretStoreError::VaultCorruption)));
    assert_eq!(exit_code(&err), EXIT_SECRETS_VAULT_CORRUPTION);
    assert!(!err.to_string().contains(SEEDED_SECRET), "error redacts the value");
    assert!(!output(&out).contains(SEEDED_SECRET), "stdout redacts the value");

    // Host-mismatch errors also redact the value.
    let host_b = HostBinding::from_identifier_bytes(b"other-machine-id").expect("valid");
    let other_store = SecretStore::new(Box::new(keychain.clone()), host_b, vault_path(dir.path()));
    // Restore the original vault first (the tampered one is unreadable).
    std::fs::write(vault_path(dir.path()), text).expect("restore vault");
    let mut out2 = capture();
    let err2 = run_secrets_get(&other_store, &name("api-key"), false, false, &mut out2)
        .expect_err("mismatch");
    assert!(matches!(
        err2,
        SecretsCliError::Store(SecretStoreError::HostBindingMismatch { version: 1 })
    ));
    assert_eq!(exit_code(&err2), EXIT_SECRETS_HOST_MISMATCH);
    assert!(!err2.to_string().contains(SEEDED_SECRET));
    assert!(!output(&out2).contains(SEEDED_SECRET));
}

fn base64_engine_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[test]
fn secrets_command_exit_codes_are_stable_per_class() {
    let cases: &[(SecretsCliError, i32)] = &[
        (SecretsCliError::Store(SecretStoreError::NotFound), EXIT_SECRETS_NOT_FOUND),
        (
            SecretsCliError::Store(SecretStoreError::InvalidName(SecretNameError::Empty)),
            EXIT_SECRETS_USER_INPUT,
        ),
        (
            SecretsCliError::Store(SecretStoreError::InvalidReference(
                SecretReferenceError::InvalidPrefix,
            )),
            EXIT_SECRETS_USER_INPUT,
        ),
        (SecretsCliError::Store(SecretStoreError::KeychainUnavailable), EXIT_SECRETS_KEYCHAIN),
        (SecretsCliError::Store(SecretStoreError::KeychainCorruption), EXIT_SECRETS_KEYCHAIN),
        (SecretsCliError::Store(SecretStoreError::CspRngUnavailable), EXIT_SECRETS_KEYCHAIN),
        (
            SecretsCliError::Store(SecretStoreError::HostBindingUnavailable),
            EXIT_SECRETS_HOST_BINDING,
        ),
        (
            SecretsCliError::Store(SecretStoreError::HostBindingMismatch { version: 1 }),
            EXIT_SECRETS_HOST_MISMATCH,
        ),
        (SecretsCliError::Store(SecretStoreError::VaultCorruption), EXIT_SECRETS_VAULT_CORRUPTION),
        (
            SecretsCliError::Store(SecretStoreError::UnsupportedVersion { version: 2 }),
            EXIT_SECRETS_UNSUPPORTED_VERSION,
        ),
        (
            SecretsCliError::Store(SecretStoreError::Io(std::io::Error::other("boom"))),
            EXIT_SECRETS_IO,
        ),
        (SecretsCliError::EmptySecret, EXIT_SECRETS_USER_INPUT),
        (SecretsCliError::SecretTooLarge, EXIT_SECRETS_USER_INPUT),
        (SecretsCliError::InvalidUtf8, EXIT_SECRETS_USER_INPUT),
        (SecretsCliError::Cancelled, EXIT_SECRETS_USER_INPUT),
        (SecretsCliError::RefusedTerminalOutput, EXIT_SECRETS_USER_INPUT),
    ];
    for (err, expected) in cases {
        assert_eq!(exit_code(err), *expected, "{err:?}");
    }
}

#[test]
fn secrets_command_keychain_failure_classifies_and_redacts() {
    let dir = tempfile::tempdir().expect("temp dir");
    let (store, _keychain) = store_in(dir.path());
    store.set(&name("api-key"), &Zeroizing::new(SEEDED_SECRET.to_owned())).expect("set");

    // A store whose keychain is unavailable cannot open the vault.
    let failing = crate::secrets::test_support::ScriptedKeychain::new(
        vec![Err(SecretStoreError::KeychainUnavailable)],
        vec![],
    );
    let blocked = SecretStore::new(Box::new(failing), test_binding(), vault_path(dir.path()));

    let mut out = capture();
    let err = run_secrets_get(&blocked, &name("api-key"), false, false, &mut out)
        .expect_err("keychain failure");
    assert!(matches!(err, SecretsCliError::Store(SecretStoreError::KeychainUnavailable)));
    assert_eq!(exit_code(&err), EXIT_SECRETS_KEYCHAIN);
    assert!(!err.to_string().contains(SEEDED_SECRET));
}

// ── Stdin source coverage via the public source type ─────────────────────────

#[test]
fn secrets_command_stdin_source_preserves_interior_bytes() {
    let mut reader = Cursor::new(b"prefix-mid\t\n".to_vec());
    let value = StdinSecretSource { reader: &mut reader }.read_value().expect("value");
    assert_eq!(value.as_str(), "prefix-mid\t");
}
