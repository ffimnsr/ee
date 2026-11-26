//! Linux bubblewrap argv construction.

use std::path::Path;

use crate::spec::{NetworkAccess, ReadAccess, SandboxCommand, SandboxError, SandboxSpec};
use crate::{DEFAULT_SANDBOX_PATH, SANDBOX_TEMP_DIR};

/// Builds one sandboxed `sh -c` command using bubblewrap.
pub fn bwrap_command(
    bwrap: &Path,
    spec: &SandboxSpec,
    command: &str,
    env: &[(String, String)],
) -> Result<SandboxCommand, SandboxError> {
    let mut args = base_args(spec, env)?;
    args.push(String::from("--"));
    args.push(String::from("sh"));
    args.push(String::from("-c"));
    args.push(command.to_string());
    Ok(SandboxCommand { program: bwrap.to_path_buf(), args, env: Vec::new() })
}

/// Builds one sandboxed direct executable command using bubblewrap.
pub fn bwrap_direct_command(
    bwrap: &Path,
    spec: &SandboxSpec,
    program: &str,
    program_args: &[String],
    env: &[(String, String)],
) -> Result<SandboxCommand, SandboxError> {
    let mut args = base_args(spec, env)?;
    args.push(String::from("--"));
    args.push(program.to_string());
    args.extend(program_args.iter().cloned());
    Ok(SandboxCommand { program: bwrap.to_path_buf(), args, env: Vec::new() })
}

fn base_args(spec: &SandboxSpec, env: &[(String, String)]) -> Result<Vec<String>, SandboxError> {
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

    let mut args = vec![
        String::from("--die-with-parent"),
        String::from("--new-session"),
        String::from("--unshare-user"),
        String::from("--unshare-pid"),
        String::from("--unshare-ipc"),
        String::from("--unshare-uts"),
        String::from("--unshare-cgroup"),
        String::from("--cap-drop"),
        String::from("ALL"),
    ];
    if spec.network == NetworkAccess::Deny {
        args.push(String::from("--unshare-net"));
    }

    if spec.read == ReadAccess::Broad {
        args.extend(["--ro-bind", "/", "/"].map(String::from));
    } else {
        for path in ["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/opt", "/nix/store"] {
            if Path::new(path).exists() {
                args.extend([String::from("--ro-bind"), path.to_string(), path.to_string()]);
            }
        }
        for path in ["/home", "/root", "/media", "/mnt"] {
            if Path::new(path).exists() {
                args.extend([String::from("--tmpfs"), path.to_string()]);
            }
        }
    }

    // Fresh temp and no host IPC/runtime sockets. These mounts must precede
    // the workspace bind because the workspace may live under /tmp or /home.
    args.extend([String::from("--tmpfs"), String::from(SANDBOX_TEMP_DIR)]);
    args.extend([String::from("--tmpfs"), String::from("/run")]);

    // Later mounts override earlier ones, so Git metadata stays read-only even
    // after the workspace writable bind.
    args.extend([
        String::from("--bind"),
        workspace.to_string_lossy().into_owned(),
        workspace.to_string_lossy().into_owned(),
    ]);
    for path in &spec.write_paths {
        let canonical = std::fs::canonicalize(path).map_err(|error| {
            SandboxError::Invalid(format!(
                "write path {} cannot be resolved: {error}",
                path.display()
            ))
        })?;
        let value = canonical.to_string_lossy().into_owned();
        args.extend([String::from("--bind"), value.clone(), value]);
    }
    if let Some(git) = crate::git_metadata_path(&workspace) {
        let source = git.to_string_lossy().into_owned();
        let destination = workspace.join(".git").to_string_lossy().into_owned();
        args.extend([String::from("--ro-bind"), source, destination]);
    }

    args.extend([String::from("--dev"), String::from("/dev")]);
    args.extend([String::from("--proc"), String::from("/proc")]);
    args.extend([
        String::from("--chdir"),
        cwd.to_string_lossy().into_owned(),
        String::from("--clearenv"),
    ]);

    let mut sandbox_env = env.to_vec();
    set_default(&mut sandbox_env, "PATH", DEFAULT_SANDBOX_PATH);
    set_default(&mut sandbox_env, "TMPDIR", SANDBOX_TEMP_DIR);
    set_default(&mut sandbox_env, "TMP", SANDBOX_TEMP_DIR);
    set_default(&mut sandbox_env, "TEMP", SANDBOX_TEMP_DIR);
    set_default(&mut sandbox_env, "HOME", SANDBOX_TEMP_DIR);
    for (name, value) in &sandbox_env {
        args.extend([String::from("--setenv"), name.clone(), value.clone()]);
    }
    Ok(args)
}

fn set_default(env: &mut Vec<(String, String)>, name: &str, value: &str) {
    if !env.iter().any(|(key, _)| key == name) {
        env.push((name.to_string(), value.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_spec() -> (tempfile::TempDir, SandboxSpec) {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        std::fs::create_dir(temp.path().join("src")).unwrap();
        let spec = SandboxSpec::new(temp.path(), &temp.path().join("src")).unwrap();
        (temp, spec)
    }

    fn contains(args: &[String], needle: &str) -> bool {
        args.iter().any(|arg| arg == needle)
    }

    #[test]
    fn profile_contains_workspace_git_and_private_runtime_mounts() {
        let (temp, spec) = temp_spec();
        let command = bwrap_command(
            Path::new("/usr/bin/bwrap"),
            &spec,
            "printf ok",
            &[("PATH".into(), DEFAULT_SANDBOX_PATH.into())],
        )
        .unwrap();
        assert!(contains(&command.args, "--unshare-net"));
        assert!(contains(&command.args, "--tmpfs"));
        assert!(command.args.iter().any(|arg| arg == "/run"));
        assert!(command.args.iter().any(|arg| arg == "/tmp"));
        assert!(command.args.windows(2).any(|pair| {
            pair[0] == "--ro-bind" && pair[1] == temp.path().join(".git").to_string_lossy().as_ref()
        }));
        assert!(command.args.windows(3).any(|window| {
            window[0] == "--chdir"
                && window[1]
                    == std::fs::canonicalize(temp.path().join("src"))
                        .unwrap()
                        .to_string_lossy()
                        .as_ref()
        }));
        assert_eq!(command.args.last().map(String::as_str), Some("printf ok"));
        assert!(command.args.windows(3).any(|window| {
            window[0] == "--setenv" && window[1] == "TMPDIR" && window[2] == "/tmp"
        }));
    }

    #[test]
    fn network_allow_omits_network_namespace() {
        let (_temp, mut spec) = temp_spec();
        spec.network = NetworkAccess::Allow;
        let command = bwrap_command(Path::new("/usr/bin/bwrap"), &spec, "true", &[]).unwrap();
        assert!(!contains(&command.args, "--unshare-net"));
    }

    #[test]
    fn workspace_read_masks_home_trees() {
        let (_temp, mut spec) = temp_spec();
        spec.read = ReadAccess::Workspace;
        let command = bwrap_command(Path::new("/usr/bin/bwrap"), &spec, "true", &[]).unwrap();
        assert!(command.args.windows(2).any(|pair| pair[0] == "--tmpfs" && pair[1] == "/home"));
        assert!(!command.args.windows(2).any(|pair| pair[0] == "--ro-bind" && pair[1] == "/"));
    }

    #[test]
    fn write_path_grants_are_bound() {
        let (_temp, mut spec) = temp_spec();
        spec.write_paths.push(std::path::PathBuf::from("/tmp"));
        let command = bwrap_command(Path::new("/usr/bin/bwrap"), &spec, "true", &[]).unwrap();
        assert!(
            command.args.windows(3).any(|window| {
                window[0] == "--bind" && window[1] == "/tmp" && window[2] == "/tmp"
            })
        );
    }

    #[test]
    fn direct_command_keeps_argv_without_shell() {
        let (_temp, spec) = temp_spec();
        let command = bwrap_direct_command(
            Path::new("/usr/bin/bwrap"),
            &spec,
            "agent-bin",
            &["serve".into(), "--flag".into()],
            &[],
        )
        .unwrap();
        assert_eq!(
            command.args[command.args.len() - 3..],
            [String::from("agent-bin"), String::from("serve"), String::from("--flag")]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn real_bwrap_contains_workspace_git_and_temp_writes() {
        let probe = crate::cached_probe();
        if !probe.available {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join(".git")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let spec = SandboxSpec::new(workspace.path(), workspace.path()).unwrap();
        let program = probe.program.as_ref().unwrap();

        let inside = workspace.path().join("inside-marker");
        let command =
            bwrap_command(program, &spec, &format!("printf ok > '{}'", inside.display()), &[])
                .unwrap();
        let status =
            std::process::Command::new(&command.program).args(&command.args).status().unwrap();
        assert!(status.success(), "workspace write should succeed");
        assert!(inside.exists());

        let outside_marker = outside.path().join("outside-marker");
        let command = bwrap_command(
            program,
            &spec,
            &format!("printf bad > '{}'", outside_marker.display()),
            &[],
        )
        .unwrap();
        let status =
            std::process::Command::new(&command.program).args(&command.args).status().unwrap();
        assert!(!status.success(), "outside write must fail");
        assert!(!outside_marker.exists());

        let command = bwrap_command(program, &spec, "printf bad > .git/pwned", &[]).unwrap();
        let status =
            std::process::Command::new(&command.program).args(&command.args).status().unwrap();
        assert!(!status.success(), ".git write must fail");
        assert!(!workspace.path().join(".git/pwned").exists());

        let host_temp = std::env::temp_dir().join("ee-sandbox-temp-marker");
        let _ = std::fs::remove_file(&host_temp);
        let first =
            bwrap_command(program, &spec, "printf x > /tmp/ee-sandbox-temp-marker", &[]).unwrap();
        assert!(
            std::process::Command::new(&first.program)
                .args(&first.args)
                .status()
                .unwrap()
                .success()
        );
        let second =
            bwrap_command(program, &spec, "test ! -e /tmp/ee-sandbox-temp-marker", &[]).unwrap();
        assert!(
            std::process::Command::new(&second.program)
                .args(&second.args)
                .status()
                .unwrap()
                .success(),
            "each command must get a fresh /tmp"
        );
        assert!(!host_temp.exists());
    }
}
