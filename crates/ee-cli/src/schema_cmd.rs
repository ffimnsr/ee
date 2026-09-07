//! `ee do validate`, `schema`, and shell-completion commands.
use super::args::Cli;
use super::*;

pub(crate) fn cmd_validate(config_path: Option<&PathBuf>) {
    let paths = if let Some(path) = config_path.cloned() {
        vec![path]
    } else {
        config::default_config_layers(None).into_iter().map(|layer| layer.path).collect::<Vec<_>>()
    };

    if paths.is_empty() {
        eprintln!("No config files found in layered default search path.");
        std::process::exit(1);
    }

    for path in paths {
        if !path.exists() {
            eprintln!("Config file not found: {path:?}");
            std::process::exit(1);
        }
        if let Err(err) = config::validate_config_file(&path) {
            eprintln!("{err}");
            std::process::exit(1);
        }
        println!("Config {path:?} is valid.");
    }
}

pub(crate) fn cmd_schema_generate(output: &Path) {
    if let Err(err) = config::write_config_schema(output) {
        eprintln!("{err}");
        std::process::exit(1);
    }
    println!("Generated config schema: {}", output.display());
}

pub(crate) fn cmd_schema_check(schema: &Path) {
    if let Err(err) = config::check_config_schema(schema) {
        eprintln!("{err}");
        std::process::exit(1);
    }
    println!("Config schema is up to date: {}", schema.display());
}

pub(crate) fn cmd_completions(shell: Shell) {
    let mut cmd = Cli::command();
    generate(shell, &mut cmd, "ee", &mut io::stdout());
}
