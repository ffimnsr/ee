//! Interactive setup for locally installed ACP agent servers.
//!
//! Only agents installed in the per-user `~/.local/bin` directory are
//! considered. Each candidate must print a bounded versioned manifest on
//! `--ee-config`; the manifest determines every prompt and config mapping.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ee_agent_protocol::setup::{SETUP_MANIFEST_SCHEMA_VERSION, SetupManifest};

use crate::{config, secrets};

const AGENT_BIN_DIRECTORY: [&str; 2] = [".local", "bin"];
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_MANIFEST_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentCandidate {
    path: PathBuf,
    file_name: String,
}

pub(crate) fn run() -> Result<(), String> {
    let directory = dirs::home_dir()
        .map(|home| AGENT_BIN_DIRECTORY.iter().fold(home, |path, part| path.join(part)))
        .ok_or_else(|| String::from("cannot resolve home directory for agent setup"))?;
    let candidates = discover_agents(&directory)?;
    let candidate = select_agent(&candidates)?;
    let manifest = read_manifest(&candidate.path)?;

    println!("Setting up {}.", manifest.agent.display_name);
    let env = collect_setup_values(&manifest)?;
    let path = config::configure_global_agent_server(&manifest.agent.id, &candidate.path, &env)?;

    println!("Configured agent `{}` in {}.", manifest.agent.id, path.display());
    println!("Agents mode enabled. Default agent: {}.", manifest.agent.id);
    Ok(())
}

fn discover_agents(directory: &Path) -> Result<Vec<AgentCandidate>, String> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(format!("no agent servers found in {}", directory.display()));
        }
        Err(error) => return Err(format!("cannot read {}: {error}", directory.display())),
    };

    let mut candidates = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("cannot inspect {}: {error}", directory.display()))?;
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(OsStr::to_str).map(str::to_owned) else {
            continue;
        };
        if !is_agent_file_name(&file_name) || !is_executable_file(&path)? {
            continue;
        }
        candidates.push(AgentCandidate { path, file_name });
    }
    candidates.sort_by(|left, right| left.file_name.cmp(&right.file_name));

    if candidates.is_empty() {
        return Err(format!("no agent servers found in {}", directory.display()));
    }
    Ok(candidates)
}

fn is_agent_file_name(name: &str) -> bool {
    name.strip_prefix("ee-")
        .and_then(|name| name.strip_suffix("-agent"))
        .is_some_and(|name| !name.is_empty())
}

fn is_executable_file(path: &Path) -> Result<bool, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Ok(false);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        Ok(metadata.permissions().mode() & 0o111 != 0)
    }

    #[cfg(not(unix))]
    {
        Ok(true)
    }
}

fn select_agent(candidates: &[AgentCandidate]) -> Result<&AgentCandidate, String> {
    println!("Available agent servers:");
    for (index, candidate) in candidates.iter().enumerate() {
        println!("  {}) {}", index + 1, candidate.file_name);
    }

    loop {
        let selected = prompt_line(&format!("Select agent [1-{}]: ", candidates.len()))?;
        let index = selected
            .parse::<usize>()
            .ok()
            .and_then(|index| index.checked_sub(1))
            .filter(|index| *index < candidates.len());
        if let Some(index) = index {
            return Ok(&candidates[index]);
        }
        eprintln!("Enter a number from 1 through {}.", candidates.len());
    }
}

fn read_manifest(path: &Path) -> Result<SetupManifest, String> {
    let mut child = Command::new(path)
        .arg("--ee-config")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot run {} --ee-config: {error}", path.display()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("cannot capture setup manifest from {}", path.display()))?;
    let output_reader = thread::spawn(move || read_bounded(stdout));
    let deadline = Instant::now() + MANIFEST_TIMEOUT;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("cannot wait for {}: {error}", path.display()))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = output_reader.join();
            return Err(format!("{} --ee-config timed out", path.display()));
        }
        thread::sleep(Duration::from_millis(10));
    };
    let (bytes, truncated) = output_reader
        .join()
        .map_err(|_| format!("setup manifest reader panicked for {}", path.display()))?
        .map_err(|error| format!("cannot read setup manifest from {}: {error}", path.display()))?;
    if !status.success() {
        return Err(format!("{} --ee-config exited with {status}", path.display()));
    }
    if truncated {
        return Err(format!(
            "setup manifest from {} exceeds {MAX_MANIFEST_BYTES} bytes",
            path.display()
        ));
    }

    let text = String::from_utf8(bytes)
        .map_err(|_| format!("setup manifest from {} is not UTF-8", path.display()))?;
    let manifest = serde_json::from_str(&text)
        .map_err(|error| format!("invalid setup manifest from {}: {error}", path.display()))?;
    validate_manifest(manifest)
}

fn read_bounded(mut reader: impl io::Read) -> io::Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut chunk = [0_u8; 4096];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            return Ok((bytes, truncated));
        }
        let remaining = MAX_MANIFEST_BYTES.saturating_sub(bytes.len());
        let accepted = remaining.min(read);
        bytes.extend_from_slice(&chunk[..accepted]);
        truncated |= accepted != read;
    }
}

fn validate_manifest(manifest: SetupManifest) -> Result<SetupManifest, String> {
    if manifest.schema_version != SETUP_MANIFEST_SCHEMA_VERSION {
        return Err(format!(
            "unsupported agent setup manifest schema version {}; expected {SETUP_MANIFEST_SCHEMA_VERSION}",
            manifest.schema_version
        ));
    }
    if manifest.agent.display_name.trim().is_empty() {
        return Err(String::from("agent setup manifest has an empty display name"));
    }
    secrets::SecretName::new(&manifest.agent.id)
        .map_err(|error| format!("agent setup manifest has invalid agent id: {error}"))?;

    let mut env_names = BTreeSet::new();
    for env in &manifest.env_vars {
        validate_env_name(&env.name)?;
        if !env_names.insert(env.name.clone()) {
            return Err(format!(
                "agent setup manifest repeats environment variable `{}`",
                env.name
            ));
        }
        if env.description.trim().is_empty() {
            return Err(format!("agent setup manifest has no description for `{}`", env.name));
        }
    }
    let mut input_keys = BTreeSet::new();
    for input in &manifest.inputs {
        if input.key.trim().is_empty() || !input_keys.insert(input.key.clone()) {
            return Err(format!(
                "agent setup manifest has invalid or repeated input `{}`",
                input.key
            ));
        }
        if input.label.trim().is_empty() {
            return Err(format!("agent setup manifest has no label for `{}`", input.key));
        }
        validate_env_name(&input.config.env)?;
        if !env_names.insert(input.config.env.clone()) {
            return Err(format!(
                "agent setup manifest maps multiple values to `{}`",
                input.config.env
            ));
        }
    }
    Ok(manifest)
}

fn validate_env_name(name: &str) -> Result<(), String> {
    let mut characters = name.chars();
    match characters.next() {
        Some(character) if character.is_ascii_alphabetic() || character == '_' => {}
        _ => return Err(format!("agent setup manifest has invalid environment variable `{name}`")),
    }
    if characters.all(|character| character.is_ascii_alphanumeric() || character == '_') {
        Ok(())
    } else {
        Err(format!("agent setup manifest has invalid environment variable `{name}`"))
    }
}

fn collect_setup_values(manifest: &SetupManifest) -> Result<BTreeMap<String, String>, String> {
    let mut values = BTreeMap::new();
    let mut secret_store = None;

    for env in &manifest.env_vars {
        println!(
            "{}{}: {}",
            env.name,
            if env.required { " (required)" } else { " (optional)" },
            env.description
        );
        let value = if env.secret {
            let value = read_secret_value(env.required)?;
            value
                .map(|value| {
                    let secret_name = secrets::SecretName::new(&format!(
                        "agent.{}.{}",
                        manifest.agent.id, env.name
                    ))
                    .expect("validated setup manifest makes canonical secret names");
                    let reference =
                        secrets::SecretReference::from_name(secret_name.clone()).to_string();
                    let store = secret_store.get_or_insert_with(secrets::SecretStore::default);
                    let store = store
                        .as_ref()
                        .map_err(|error| format!("cannot open encrypted secrets store: {error}"))?;
                    store
                        .set(&secret_name, &value)
                        .map_err(|error| format!("cannot store secret `{}`: {error}", env.name))?;
                    Ok::<String, String>(reference)
                })
                .transpose()?
        } else {
            prompt_value(&env.name, None, env.required)?
        };
        if let Some(value) = value {
            values.insert(env.name.clone(), value);
        }
    }

    for input in &manifest.inputs {
        let value = prompt_value(&input.label, input.default.as_deref(), input.default.is_some())?;
        if let Some(value) = value {
            values.insert(input.config.env.clone(), value);
        }
    }
    Ok(values)
}

fn read_secret_value(required: bool) -> Result<Option<zeroize::Zeroizing<String>>, String> {
    let mut stdin = io::empty();
    let mut terminal = secrets::cli::HiddenTerminalSecretSource;
    match secrets::cli::read_secret_value(false, &mut stdin, &mut terminal) {
        Ok(value) => Ok(Some(value)),
        Err(secrets::cli::SecretsCliError::EmptySecret) if !required => Ok(None),
        Err(error) => Err(format!("cannot read secret value: {error}")),
    }
}

fn prompt_value(
    label: &str,
    default: Option<&str>,
    required: bool,
) -> Result<Option<String>, String> {
    let prompt = match default {
        Some(default) => format!("{label} [{default}]: "),
        None if required => format!("{label}: "),
        None => format!("{label} (press Enter to skip): "),
    };
    loop {
        let value = prompt_line(&prompt)?;
        if !value.is_empty() {
            return Ok(Some(value));
        }
        if let Some(default) = default {
            return Ok(Some(default.to_owned()));
        }
        if !required {
            return Ok(None);
        }
        eprintln!("{label} is required.");
    }
}

fn prompt_line(prompt: &str) -> Result<String, String> {
    print!("{prompt}");
    io::stdout().flush().map_err(|error| format!("cannot write setup prompt: {error}"))?;
    let mut line = String::new();
    let read = io::stdin()
        .read_line(&mut line)
        .map_err(|error| format!("cannot read setup input: {error}"))?;
    if read == 0 {
        return Err(String::from("setup input closed"));
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ee_agent_protocol::setup::{SetupAgent, SetupEnvVar, SetupInput, SetupInputConfig};

    use super::*;

    #[test]
    fn agent_file_name_requires_nonempty_ee_name_agent_pattern() {
        assert!(is_agent_file_name("ee-openrouter-agent"));
        assert!(is_agent_file_name("ee-a-agent"));
        assert!(!is_agent_file_name("ee--agent"));
        assert!(!is_agent_file_name("openrouter-agent"));
        assert!(!is_agent_file_name("ee-openrouter"));
    }

    #[test]
    fn discovery_only_returns_executable_agent_servers() {
        let temp = tempfile::tempdir().expect("temp directory");
        let executable = temp.path().join("ee-openrouter-agent");
        let not_executable = temp.path().join("ee-other-agent");
        let unrelated = temp.path().join("ee-not-agent.txt");
        fs::write(&executable, "#!/bin/sh\n").expect("write executable");
        fs::write(&not_executable, "#!/bin/sh\n").expect("write non-executable");
        fs::write(&unrelated, "ignored").expect("write unrelated");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
                .expect("make executable");
        }

        let candidates = discover_agents(temp.path()).expect("discover agents");

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].file_name, "ee-openrouter-agent");
    }

    #[cfg(unix)]
    #[test]
    fn manifest_is_read_from_agent_ee_config_command() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().expect("temp directory");
        let agent = temp.path().join("ee-example-agent");
        fs::write(
            &agent,
            "#!/bin/sh\nprintf '%s' '{\"schema_version\":1,\"agent\":{\"id\":\"example\",\"display_name\":\"Example\"},\"env_vars\":[],\"inputs\":[]}'\n",
        )
        .expect("write agent");
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).expect("make executable");

        let manifest = read_manifest(&agent).expect("read manifest");

        assert_eq!(manifest.agent.id, "example");
        assert_eq!(manifest.agent.display_name, "Example");
    }

    #[test]
    fn manifest_rejects_duplicate_environment_destinations() {
        let manifest = SetupManifest {
            schema_version: SETUP_MANIFEST_SCHEMA_VERSION,
            agent: SetupAgent {
                id: String::from("example"),
                display_name: String::from("Example"),
            },
            env_vars: vec![SetupEnvVar {
                name: String::from("EXAMPLE_MODEL"),
                required: true,
                secret: false,
                description: String::from("Model."),
            }],
            inputs: vec![SetupInput {
                key: String::from("model"),
                label: String::from("Model"),
                default: Some(String::from("default")),
                config: SetupInputConfig { env: String::from("EXAMPLE_MODEL") },
            }],
        };

        assert!(validate_manifest(manifest).is_err());
    }

    #[test]
    fn bounded_reader_drains_and_marks_oversized_manifest() {
        let input = vec![b'x'; MAX_MANIFEST_BYTES + 1];
        let (bytes, truncated) = read_bounded(&input[..]).expect("read manifest");

        assert_eq!(bytes.len(), MAX_MANIFEST_BYTES);
        assert!(truncated);
    }
}
