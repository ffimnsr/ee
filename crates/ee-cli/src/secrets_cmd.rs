//! `ee do secrets` command.
use super::args::SecretsCommands;
use super::*;

pub(crate) fn parse_secret_name(raw: &str) -> secrets::SecretName {
    match secrets::SecretName::new(raw) {
        Ok(name) => name,
        Err(e) => {
            eprintln!("error: invalid secret name: {e}");
            std::process::exit(secrets::cli::EXIT_SECRETS_USER_INPUT);
        }
    }
}

pub(crate) fn exit_secrets_error(err: secrets::SecretStoreError) {
    eprintln!("error: {err}");
    std::process::exit(secrets::cli::exit_code(&secrets::cli::SecretsCliError::Store(err)));
}

pub(crate) fn cmd_secrets(command: SecretsCommands) {
    let result = match command {
        SecretsCommands::Set { name, stdin } => {
            let name = parse_secret_name(&name);
            let store = match secrets::SecretStore::default() {
                Ok(store) => store,
                Err(err) => return exit_secrets_error(err),
            };
            secrets::cli::run_secrets_set(
                &store,
                &name,
                stdin,
                &mut io::stdin().lock(),
                &mut secrets::cli::HiddenTerminalSecretSource,
                &mut io::stdout(),
            )
        }
        SecretsCommands::Get { name, force } => {
            let name = parse_secret_name(&name);
            let store = match secrets::SecretStore::default() {
                Ok(store) => store,
                Err(err) => return exit_secrets_error(err),
            };
            secrets::cli::run_secrets_get(
                &store,
                &name,
                force,
                io::stdout().is_terminal(),
                &mut io::stdout(),
            )
        }
        SecretsCommands::List => {
            let store = match secrets::SecretStore::default() {
                Ok(store) => store,
                Err(err) => return exit_secrets_error(err),
            };
            secrets::cli::run_secrets_list(&store, &mut io::stdout())
        }
        SecretsCommands::Delete { name } => {
            let name = parse_secret_name(&name);
            let store = match secrets::SecretStore::default() {
                Ok(store) => store,
                Err(err) => return exit_secrets_error(err),
            };
            secrets::cli::run_secrets_delete(&store, &name, &mut io::stdout())
        }
        SecretsCommands::Reset => {
            let vault_path = match secrets::default_vault_path() {
                Ok(path) => path,
                Err(err) => return exit_secrets_error(err),
            };
            secrets::cli::run_secrets_reset(&vault_path, &mut io::stdout())
        }
        SecretsCommands::Status => {
            let store = match secrets::SecretStore::default() {
                Ok(store) => store,
                Err(err) => return exit_secrets_error(err),
            };
            secrets::cli::run_secrets_status(&store, &mut io::stdout())
        }
    };
    if let Err(err) = result {
        eprintln!("error: {err}");
        std::process::exit(secrets::cli::exit_code(&err));
    }
}
