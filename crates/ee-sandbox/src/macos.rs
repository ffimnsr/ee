//! macOS Seatbelt profile and command construction.

use std::path::{Path, PathBuf};

use crate::spec::{NetworkAccess, ReadAccess, SandboxCommand, SandboxError, SandboxSpec};
use crate::{DEFAULT_SANDBOX_PATH, SANDBOX_TEMP_DIR};

/// Mach services needed by ordinary developer tooling. LaunchServices,
/// launchd, the pasteboard, and audio services are intentionally absent.
const DEFAULT_MACH_SERVICES: &[&str] = &[
    "com.apple.system.logger",
    "com.apple.system.notification_center",
    "com.apple.cfprefsd.agent",
    "com.apple.SecurityServer",
];

/// Builds one Seatbelt command (`sandbox-exec -p <profile> sh -c ...`).
pub fn seatbelt_command(
    sandbox_exec: &Path,
    spec: &SandboxSpec,
    command: &str,
    env: &[(String, String)],
    temp_dir: &Path,
) -> Result<SandboxCommand, SandboxError> {
    let workspace = std::fs::canonicalize(&spec.workspace).map_err(|error| {
        SandboxError::Invalid(format!(
            "workspace {} cannot be resolved: {error}",
            spec.workspace.display()
        ))
    })?;
    let cwd = std::fs::canonicalize(&spec.cwd).map_err(|error| {
        SandboxError::Invalid(format!("cwd {} cannot be resolved: {error}", spec.cwd.display()))
    })?;
    if !cwd.starts_with(&workspace) {
        return Err(SandboxError::Invalid(format!(
            "sandbox cwd {} escapes workspace {}",
            cwd.display(),
            workspace.display()
        )));
    }
    let profile = profile(&workspace, spec, temp_dir)?;

    let mut args = vec![String::from("-p"), profile, String::from("sh"), String::from("-c")];
    args.push(command.to_string());
    let mut command_env = env.to_vec();
    set_default(&mut command_env, "PATH", DEFAULT_SANDBOX_PATH);
    set_default(&mut command_env, "TMPDIR", &temp_dir.to_string_lossy());
    set_default(&mut command_env, "TMP", &temp_dir.to_string_lossy());
    set_default(&mut command_env, "TEMP", &temp_dir.to_string_lossy());
    set_default(&mut command_env, "HOME", &temp_dir.to_string_lossy());
    Ok(SandboxCommand { program: sandbox_exec.to_path_buf(), args, env: command_env })
}

/// Builds one Seatbelt direct-executable command.
pub fn seatbelt_direct_command(
    sandbox_exec: &Path,
    spec: &SandboxSpec,
    program: &str,
    program_args: &[String],
    env: &[(String, String)],
    temp_dir: &Path,
) -> Result<SandboxCommand, SandboxError> {
    let workspace = std::fs::canonicalize(&spec.workspace).map_err(|error| {
        SandboxError::Invalid(format!(
            "workspace {} cannot be resolved: {error}",
            spec.workspace.display()
        ))
    })?;
    let cwd = std::fs::canonicalize(&spec.cwd).map_err(|error| {
        SandboxError::Invalid(format!("cwd {} cannot be resolved: {error}", spec.cwd.display()))
    })?;
    if !cwd.starts_with(&workspace) {
        return Err(SandboxError::Invalid(format!(
            "sandbox cwd {} escapes workspace {}",
            cwd.display(),
            workspace.display()
        )));
    }
    let profile = profile(&workspace, spec, temp_dir)?;
    let mut args = vec![String::from("-p"), profile, program.to_string()];
    args.extend(program_args.iter().cloned());
    let command_env = default_env(env, temp_dir);
    Ok(SandboxCommand { program: sandbox_exec.to_path_buf(), args, env: command_env })
}

fn default_env(env: &[(String, String)], temp_dir: &Path) -> Vec<(String, String)> {
    let mut command_env = env.to_vec();
    let temp = temp_dir.to_string_lossy();
    set_default(&mut command_env, "PATH", DEFAULT_SANDBOX_PATH);
    set_default(&mut command_env, "TMPDIR", &temp);
    set_default(&mut command_env, "TMP", &temp);
    set_default(&mut command_env, "TEMP", &temp);
    set_default(&mut command_env, "HOME", &temp);
    command_env
}
fn profile(
    workspace_path: &Path,
    spec: &SandboxSpec,
    temp_dir: &Path,
) -> Result<String, SandboxError> {
    let workspace = seatbelt_path(workspace_path)?;
    let temp = seatbelt_path(temp_dir)?;
    let mut profile = String::from(
        "(version 1)\n\
         (deny default)\n\
         (allow process*)\n\
         (allow sysctl-read)\n",
    );
    if spec.read == ReadAccess::Broad {
        profile.push_str("(allow file-read*)\n");
    } else {
        profile.push_str("(allow file-read*");
        for path in ["/usr", "/System", "/Library", "/private/etc", "/bin", "/sbin"] {
            if Path::new(path).exists() {
                profile.push_str(&format!(" (subpath \"{}\")", seatbelt_path(Path::new(path))?));
            }
        }
        profile.push_str(&format!(" (subpath \"{workspace}\"))\n"));
    }
    profile
        .push_str(&format!("(allow file-write* (subpath \"{workspace}\") (subpath \"{temp}\"))\n"));
    profile.push_str(&format!("(allow file-read* (subpath \"{temp}\"))\n"));
    if let Some(git) = crate::git_metadata_path(workspace_path) {
        profile.push_str(&format!("(deny file-write* (subpath \"{}\"))\n", seatbelt_path(&git)?));
    }
    for path in &spec.write_paths {
        let path = std::fs::canonicalize(path).map_err(|error| {
            SandboxError::Invalid(format!(
                "write path {} cannot be resolved: {error}",
                path.display()
            ))
        })?;
        profile.push_str(&format!("(allow file-write* (subpath \"{}\"))\n", seatbelt_path(&path)?));
    }
    if spec.network == NetworkAccess::Allow {
        profile.push_str("(allow network*)\n");
    } else {
        profile.push_str("(deny network*)\n");
    }
    profile.push_str("(allow mach-lookup");
    for service in DEFAULT_MACH_SERVICES {
        profile.push_str(&format!(" (global-name \"{service}\")"));
    }
    profile.push_str(")\n");
    Ok(profile)
}

fn seatbelt_path(path: &Path) -> Result<String, SandboxError> {
    path.to_str().map(|value| value.replace('\\', "\\\\").replace('"', "\\\"")).ok_or_else(|| {
        SandboxError::Invalid(format!("sandbox path is not valid UTF-8: {}", path.display()))
    })
}

fn set_default(env: &mut Vec<(String, String)>, name: &str, value: &str) {
    if !env.iter().any(|(key, _)| key == name) {
        env.push((name.to_string(), value.to_string()));
    }
}

/// Default temporary root used by the macOS adapter when it cannot create a
/// per-command directory.
#[must_use]
pub fn default_temp_root() -> PathBuf {
    PathBuf::from(SANDBOX_TEMP_DIR)
}

#[cfg(target_os = "macos")]
#[cfg(test)]
mod integration_tests {
    use super::*;

    #[test]
    fn real_seatbelt_contains_workspace_and_temp_writes() {
        let probe = crate::cached_probe();
        if !probe.available {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join(".git")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let spec = SandboxSpec::new(workspace.path(), workspace.path()).unwrap();
        let program = probe.program.as_ref().unwrap();

        let inside = workspace.path().join("inside-marker");
        let command = seatbelt_command(
            program,
            &spec,
            &format!("printf ok > '{}'", inside.display()),
            &[],
            temp.path(),
        )
        .unwrap();
        let status = std::process::Command::new(&command.program)
            .args(&command.args)
            .env_clear()
            .envs(command.env.iter().cloned())
            .status()
            .unwrap();
        assert!(status.success(), "workspace write should succeed");
        assert!(inside.exists());

        let outside_marker = outside.path().join("outside-marker");
        let command = seatbelt_command(
            program,
            &spec,
            &format!("printf bad > '{}'", outside_marker.display()),
            &[],
            temp.path(),
        )
        .unwrap();
        let status = std::process::Command::new(&command.program)
            .args(&command.args)
            .env_clear()
            .envs(command.env.iter().cloned())
            .status()
            .unwrap();
        assert!(!status.success(), "outside write must fail");
        assert!(!outside_marker.exists());

        let command =
            seatbelt_command(program, &spec, "printf bad > .git/pwned", &[], temp.path()).unwrap();
        let status = std::process::Command::new(&command.program)
            .args(&command.args)
            .env_clear()
            .envs(command.env.iter().cloned())
            .status()
            .unwrap();
        assert!(!status.success(), ".git write must fail");
        assert!(!workspace.path().join(".git/pwned").exists());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> (tempfile::TempDir, tempfile::TempDir, SandboxSpec) {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join(".git")).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let spec = SandboxSpec::new(workspace.path(), workspace.path()).unwrap();
        (workspace, temp, spec)
    }

    #[test]
    fn profile_denies_network_git_writes_and_launch_services() {
        let (workspace, temp, spec) = spec();
        let command =
            seatbelt_command(Path::new("/usr/bin/sandbox-exec"), &spec, "true", &[], temp.path())
                .unwrap();
        let profile = &command.args[1];
        assert!(profile.contains("(deny network*)"));
        assert!(profile.contains("(deny file-write*"));
        assert!(profile.contains(".git"));
        assert!(profile.contains("(allow file-write*"));
        assert!(!profile.contains("launchd"));
        assert!(!profile.contains("LaunchServices"));
        assert!(profile.contains(&temp.path().display().to_string()));
        assert_eq!(command.args.last().map(String::as_str), Some("true"));
        assert!(
            command.env.iter().any(
                |(name, value)| name == "TMPDIR" && value == &temp.path().display().to_string()
            )
        );
        assert!(workspace.path().exists());
    }

    #[test]
    fn network_allow_and_workspace_read_shape() {
        let (_workspace, temp, mut spec) = spec();
        spec.network = NetworkAccess::Allow;
        spec.read = ReadAccess::Workspace;
        let command =
            seatbelt_command(Path::new("/usr/bin/sandbox-exec"), &spec, "true", &[], temp.path())
                .unwrap();
        let profile = &command.args[1];
        assert!(profile.contains("(allow network*)"));
        assert!(!profile.contains("(allow file-read*)"));
        assert!(profile.contains("(allow file-read*"));
    }
}
