//! First-run config bootstrap for external registry agents.
//!
//! The ACP registry schema carries no setup/auth metadata, so a few agents
//! hard-require a config file before their launcher starts (for example Google
//! Antigravity refuses to start its ACP server without an auth record). This
//! module is compiled into both the library and the binary targets so setup
//! (binary) and agent launch (library) share one healing path. Existing files
//! are never overwritten and no secrets are ever written.

use std::fs;
use std::path::{Path, PathBuf};

/// Result of a first-run config bootstrap for a registry agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FirstRunBootstrap {
    /// Config file was created.
    Created(PathBuf),
    /// Config file already existed and was left untouched.
    Existing(PathBuf),
    /// No bootstrap is defined for this agent.
    NotApplicable,
}

/// Ensures the known first-run config for `agent_id` exists under `home`.
pub(crate) fn bootstrap_first_run_config_for_id(
    agent_id: &str,
    home: &Path,
) -> Result<FirstRunBootstrap, String> {
    let (directory, file_name, contents) = match agent_id {
        // Google Antigravity's ACP launcher fails at startup when the auth
        // record is missing; `oauth-personal` opens the user sign-in flow on
        // first use instead of erroring during onboarding.
        "antigravity-acp" => (
            PathBuf::from(".gemini").join("antigravity-acp"),
            String::from("settings.json"),
            String::from("{\n  \"auth\": {\n    \"type\": \"oauth-personal\"\n  }\n}\n"),
        ),
        _ => return Ok(FirstRunBootstrap::NotApplicable),
    };
    let directory = home.join(directory);
    let path = directory.join(file_name);
    if path.exists() {
        return Ok(FirstRunBootstrap::Existing(path));
    }
    fs::create_dir_all(&directory)
        .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
    use std::io::Write as _;
    file.write_all(contents.as_bytes())
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    Ok(FirstRunBootstrap::Created(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn antigravity_first_run_config_is_created_once_and_never_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");

        let path =
            match bootstrap_first_run_config_for_id("antigravity-acp", &home).expect("bootstrap") {
                FirstRunBootstrap::Created(path) => path,
                other => panic!("expected created bootstrap, got {other:?}"),
            };
        assert_eq!(path, home.join(".gemini/antigravity-acp/settings.json"));
        let content = fs::read_to_string(&path).expect("read antigravity settings.json");
        assert!(content.contains("\"type\": \"oauth-personal\""), "{content}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }

        // An existing (possibly user-edited) file must be preserved verbatim.
        fs::write(&path, "{\"auth\":{\"type\":\"oauth-personal\",\"custom\":true}}\n").unwrap();
        assert_eq!(
            bootstrap_first_run_config_for_id("antigravity-acp", &home).expect("second bootstrap"),
            FirstRunBootstrap::Existing(path.clone())
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\"auth\":{\"type\":\"oauth-personal\",\"custom\":true}}\n"
        );
    }

    #[test]
    fn unknown_agents_are_not_bootstrapped() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(
            bootstrap_first_run_config_for_id("claude-acp", temp.path()).expect("no error"),
            FirstRunBootstrap::NotApplicable
        );
        assert!(!temp.path().join(".gemini").exists());
    }
}
