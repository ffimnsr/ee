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

pub(crate) fn cmd_completions(shell: Shell, install: bool) {
    let mut cmd = Cli::command();
    if !install {
        generate(shell, &mut cmd, "ee", &mut io::stdout());
        return;
    }
    let Some(home) = dirs::home_dir() else {
        eprintln!("ee: cannot resolve home directory for completion install");
        std::process::exit(1);
    };
    match install_completion(shell, &mut cmd, &home) {
        Ok(message) => println!("{message}"),
        Err(error) => {
            eprintln!("ee: completion install failed: {error}");
            std::process::exit(1);
        }
    }
}

/// Marker that identifies ee-managed blocks in append-style targets so
/// repeated installs stay idempotent.
const MANAGED_MARKER: &str = "# ee: managed shell completions (`ee do completions --install`)\n";

/// Whether a target is a standalone completion file or an init script that
/// must be appended to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CompletionTarget {
    WriteFile(PathBuf),
    AppendTo(PathBuf),
}

/// Userspace destination for each shell's completion script. Locations follow
/// the established per-shell conventions so no sudo or init-file edits are
/// needed (zsh and bash still print their one-time activation hints).
fn completion_target(shell: Shell, home: &Path) -> CompletionTarget {
    match shell {
        // bash-completion project's user dir: sourced automatically for every
        // command when the bash-completion package is installed.
        Shell::Bash => {
            CompletionTarget::WriteFile(home.join(".local/share/bash-completion/completions/ee"))
        }
        // Homebrew-style userspace fpath directory; needs fpath+compinit.
        Shell::Zsh => CompletionTarget::WriteFile(home.join(".local/share/zsh/site-functions/_ee")),
        // Fish sources every file under its completions directory.
        Shell::Fish => CompletionTarget::WriteFile(home.join(".config/fish/completions/ee.fish")),
        // PowerShell has no auto-loaded completion directory; the generated
        // Register-ArgumentCompleter block runs from the per-user profile.
        Shell::PowerShell => CompletionTarget::AppendTo(powershell_profile_path(home)),
        // Elvish autoloads only rc.elv; the generated script is eval'd there.
        Shell::Elvish => CompletionTarget::AppendTo(home.join(".config/elvish/rc.elv")),
        // Future shells fall back to a plain text-file target under the
        // userspace config tree so `--install` never fails silently.
        _ => CompletionTarget::WriteFile(home.join(format!(".config/ee/completions/{shell}"))),
    }
}

#[cfg(windows)]
fn powershell_profile_path(home: &Path) -> PathBuf {
    home.join("Documents").join("PowerShell").join("Microsoft.PowerShell_profile.ps1")
}

#[cfg(not(windows))]
fn powershell_profile_path(home: &Path) -> PathBuf {
    home.join(".config").join("powershell").join("Microsoft.PowerShell_profile.ps1")
}

fn render_completion(shell: Shell, cmd: &mut clap::Command) -> String {
    let mut bytes = Vec::new();
    generate(shell, cmd, "ee", &mut bytes);
    String::from_utf8(bytes).expect("completion script is UTF-8")
}

fn install_completion(
    shell: Shell,
    cmd: &mut clap::Command,
    home: &Path,
) -> Result<String, String> {
    let script = render_completion(shell, cmd);
    let (path, hint) = match completion_target(shell, home) {
        CompletionTarget::WriteFile(path) => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
            }
            fs::write(&path, &script)
                .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
            let hint = match shell {
                Shell::Bash => String::from(
                    "bash-completion sources it automatically when the package is installed; \
                     otherwise source the file from your ~/.bashrc",
                ),
                Shell::Zsh => String::from(
                    "add to ~/.zshrc: fpath+=($HOME/.local/share/zsh/site-functions); \
                     autoload -Uz compinit && compinit",
                ),
                _ => String::from("the shell picks it up automatically"),
            };
            (path, hint)
        }
        CompletionTarget::AppendTo(path) => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
            }
            let existing = fs::read_to_string(&path).unwrap_or_default();
            if !existing.contains(MANAGED_MARKER) {
                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
                write!(&mut file, "\n{MANAGED_MARKER}{script}")
                    .map_err(|error| format!("cannot append {}: {error}", path.display()))?;
            }
            let hint = match shell {
                Shell::PowerShell => String::from("restart PowerShell or dot-source your profile"),
                _ => String::from("restart the shell"),
            };
            (path, hint)
        }
    };
    Ok(format!("Installed {shell} completion to {}.\nNote: {hint}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_targets_use_userspace_paths() {
        let home = Path::new("/home/user");
        assert_eq!(
            completion_target(Shell::Bash, home),
            CompletionTarget::WriteFile(PathBuf::from(
                "/home/user/.local/share/bash-completion/completions/ee"
            ))
        );
        assert_eq!(
            completion_target(Shell::Zsh, home),
            CompletionTarget::WriteFile(PathBuf::from(
                "/home/user/.local/share/zsh/site-functions/_ee"
            ))
        );
        assert_eq!(
            completion_target(Shell::Fish, home),
            CompletionTarget::WriteFile(PathBuf::from(
                "/home/user/.config/fish/completions/ee.fish"
            ))
        );
        assert_eq!(
            completion_target(Shell::PowerShell, home),
            CompletionTarget::AppendTo(PathBuf::from(
                "/home/user/.config/powershell/Microsoft.PowerShell_profile.ps1"
            ))
        );
        assert_eq!(
            completion_target(Shell::Elvish, home),
            CompletionTarget::AppendTo(PathBuf::from("/home/user/.config/elvish/rc.elv"))
        );
    }

    #[test]
    fn install_writes_completion_file_and_creates_directories() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let mut cmd = super::super::args::Cli::command();
        let message = install_completion(Shell::Fish, &mut cmd, &home).expect("install fish");
        let path = home.join(".config/fish/completions/ee.fish");
        assert!(path.exists(), "{message}");
        let content = fs::read_to_string(&path).expect("read fish completion");
        assert!(content.contains("complete"), "{content}");
    }

    #[test]
    fn append_targets_are_idempotent_and_preserve_user_content() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let rc = home.join(".config/elvish/rc.elv");
        fs::create_dir_all(rc.parent().unwrap()).unwrap();
        fs::write(&rc, "set edit:prompt = '> '\n").unwrap();

        let mut cmd = super::super::args::Cli::command();
        install_completion(Shell::Elvish, &mut cmd, &home).expect("first install");
        install_completion(Shell::Elvish, &mut cmd, &home).expect("second install");

        let content = fs::read_to_string(&rc).expect("read rc.elv");
        assert!(content.starts_with("set edit:prompt = '> '\n"), "user content first: {content}");
        assert_eq!(content.matches(MANAGED_MARKER).count(), 1, "single managed block: {content}");
    }
}
