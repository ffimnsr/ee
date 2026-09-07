//! `ee do config` commands.
use super::*;

pub(crate) fn cmd_config_init(scope: config::ConfigScope) {
    match config::init_config(scope) {
        Ok(path) => println!("created config template: {}", path.display()),
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    }
}

pub(crate) fn cmd_config_show() {
    match config::merged_config_document(None) {
        Ok(text) => print!("{text}"),
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    }
}

pub(crate) fn cmd_config_get(scope: config::ConfigScope, key: &str) {
    match config::get_config_value(scope, key) {
        Ok(Some(value)) => println!("{value}"),
        Ok(None) => {
            eprintln!("config key `{key}` not found");
            std::process::exit(1);
        }
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    }
}

pub(crate) fn cmd_config_set(scope: config::ConfigScope, key: &str, value: &str) {
    match config::set_config_value(scope, key, value) {
        Ok(path) => println!("set {key} in {}", path.display()),
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    }
}

#[cfg(feature = "agents")]
pub(crate) fn cmd_config_setup_agent() {
    if let Err(error) = agent_setup::run() {
        eprintln!("agent setup failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(feature = "agents"))]
pub(crate) fn cmd_config_setup_agent() {
    eprintln!("agent setup unavailable: rebuild ee with `--features agents`");
    std::process::exit(1);
}
