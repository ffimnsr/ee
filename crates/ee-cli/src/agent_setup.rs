//! Interactive setup for local and ACP Registry agent servers.
//!
//! Local `ee-*-agent` candidates provide a bounded versioned `--ee-config`
//! manifest. Registry agents use registry launch metadata only; authentication
//! and provider configuration remain owned by each external agent.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ee_agent_protocol::setup::{SETUP_MANIFEST_SCHEMA_VERSION, SetupManifest};

use crate::agent_registry::{self, RegistryAgent};
use crate::setup_prompt::{
    confirm, prompt_line, prompt_value, read_hidden_value, validate_env_name, validate_server_name,
};
use crate::{config, secrets};

const AGENT_BIN_DIRECTORY: [&str; 2] = [".local", "bin"];
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_MANIFEST_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentCandidate {
    path: PathBuf,
    file_name: String,
}

#[derive(Debug, Clone)]
enum SetupCandidate {
    Local(AgentCandidate),
    Registry(Box<RegistryAgent>),
}

pub(crate) fn run(user: bool) -> Result<(), String> {
    let scope = if user { config::ConfigScope::Global } else { config::ConfigScope::Local };
    let candidates = collect_candidates()?;
    let candidate = select_agent(&candidates)?;
    match candidate {
        SetupCandidate::Local(candidate) => setup_local_agent(candidate, scope)?,
        SetupCandidate::Registry(agent) => setup_registry_agent(agent, scope)?,
    };
    setup_rubber_duck(&candidates, scope)
}

fn collect_candidates() -> Result<Vec<SetupCandidate>, String> {
    let directory = dirs::home_dir()
        .map(|home| AGENT_BIN_DIRECTORY.iter().fold(home, |path, part| path.join(part)))
        .ok_or_else(|| String::from("cannot resolve home directory for agent setup"))?;
    let local = discover_agents(&directory)?;
    let registry = match agent_registry::fetch_agents() {
        Ok(agents) => agents,
        Err(error) if !local.is_empty() => {
            eprintln!("warning: {error}; showing local agent servers only");
            Vec::new()
        }
        Err(error) => return Err(error),
    };
    let mut candidates = registry
        .into_iter()
        .map(|agent| SetupCandidate::Registry(Box::new(agent)))
        .chain(local.into_iter().map(SetupCandidate::Local))
        .collect::<Vec<_>>();
    candidates.sort_by_cached_key(candidate_sort_key);
    Ok(candidates)
}

fn setup_local_agent(
    candidate: &AgentCandidate,
    scope: config::ConfigScope,
) -> Result<String, String> {
    let manifest = read_manifest(&candidate.path)?;
    println!("Setting up {}.", manifest.agent.display_name);
    let server_id = prompt_server_name(&manifest.agent.display_name, &manifest.agent.id)?;
    let env = collect_setup_values(&manifest)?;
    let secret_names = manifest_secret_env_names(&manifest);
    configure_agent_setup(&server_id, &candidate.path, &[], &env, &secret_names, scope)?;
    Ok(server_id)
}

fn setup_registry_agent(
    agent: &RegistryAgent,
    scope: config::ConfigScope,
) -> Result<String, String> {
    println!("{} {}", agent.name, agent.version);
    if !agent.description.trim().is_empty() {
        println!("{}", agent.description.trim());
    }
    if !agent.license.trim().is_empty() {
        println!("License: {}", agent.license);
    }
    if let Some(source) = agent.source_url() {
        println!("Source: {source}");
    }
    if agent.uses_package_runner() {
        println!("Package runner may download pinned registry package on first launch.");
    } else if !agent.is_download_verified() {
        eprintln!("warning: registry binary has no SHA-256 checksum");
    }
    if !confirm("Configure this external agent? [y/N]: ")? {
        return Err(String::from("agent setup cancelled"));
    }

    let server_id = prompt_server_name(&agent.name, &agent.id)?;
    let prepared = agent.prepare()?;
    match agent_registry::bootstrap_first_run_config(
        agent,
        &dirs::home_dir()
            .ok_or_else(|| String::from("cannot resolve home directory for agent setup"))?,
    )? {
        agent_registry::FirstRunBootstrap::Created(path) => {
            println!("Created first-run configuration for {} ({}).", agent.name, path.display())
        }
        agent_registry::FirstRunBootstrap::Existing(path) => println!(
            "First-run configuration for {} already present: {}",
            agent.name,
            path.display()
        ),
        agent_registry::FirstRunBootstrap::NotApplicable => {}
    }
    // Registry launch metadata carries no secret marker; provider credentials
    // stay agent-owned, so no env values need the user config layer.
    configure_agent_setup(
        &server_id,
        &prepared.command,
        &prepared.args,
        &prepared.env,
        &BTreeSet::new(),
        scope,
    )?;
    println!(
        "Configured external agent {}. Authentication remains agent-owned.",
        prepared.display_name
    );
    Ok(server_id)
}

/// Writes the server definition to the chosen config layer. In workspace
/// scope, `secret://` env references are routed to the user config layer;
/// the encrypted secrets store already holds the values themselves.
fn configure_agent_setup(
    agent_id: &str,
    command: &Path,
    args: &[String],
    env_values: &BTreeMap<String, String>,
    secret_names: &BTreeSet<String>,
    scope: config::ConfigScope,
) -> Result<(), String> {
    match scope {
        config::ConfigScope::Global => {
            let path = config::configure_global_agent_server(agent_id, command, args, env_values)?;
            println!("Configured agent `{agent_id}` in {}.", path.display());
        }
        config::ConfigScope::Local => {
            let (local_env, secret_refs) = split_setup_env(env_values, secret_names);
            let path = config::configure_local_agent_server(agent_id, command, args, &local_env)?;
            println!("Configured agent `{agent_id}` in {}.", path.display());
            if !secret_refs.is_empty() {
                let user_path = config::configure_agent_server_user_env(agent_id, &secret_refs)?;
                println!(
                    "Stored env secrets for `{agent_id}` in {} (values stay in the secrets store).",
                    user_path.display()
                );
            }
        }
    }
    match scope {
        config::ConfigScope::Global => {
            println!("Agents mode enabled. Default agent: {agent_id} (user config).");
        }
        config::ConfigScope::Local => {
            println!("Agents mode enabled. Default agent: {agent_id} (workspace config).");
        }
    }
    Ok(())
}

/// Partitions collected env values into workspace-safe literals and
/// user-layer `secret://` references.
fn split_setup_env(
    env: &BTreeMap<String, String>,
    secret_names: &BTreeSet<String>,
) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    let mut literals = BTreeMap::new();
    let mut refs = BTreeMap::new();
    for (name, value) in env {
        if secret_names.contains(name) {
            refs.insert(name.clone(), value.clone());
        } else {
            literals.insert(name.clone(), value.clone());
        }
    }
    (literals, refs)
}

fn manifest_secret_env_names(manifest: &SetupManifest) -> BTreeSet<String> {
    manifest.env_vars.iter().filter(|env| env.secret).map(|env| env.name.clone()).collect()
}

enum RubberDuckBackendChoice {
    InternalModel,
    ExternalAgent,
}

/// Post-setup wizard: optionally enable the rubber duck critic, choose its
/// backend (internal model or a fully configured agent server), and write
/// `[agents.rubber_duck]` to the same config layer as the agent server.
fn setup_rubber_duck(
    candidates: &[SetupCandidate],
    scope: config::ConfigScope,
) -> Result<(), String> {
    if !confirm("Enable rubber duck critic? [y/N]: ")? {
        return Ok(());
    }
    println!("Rubber duck backend:");
    println!("  1) Internal model (EE host model)");
    println!("  2) External agent server");
    let backend = loop {
        let selected = prompt_line("Select backend [1-2]: ")?;
        match selected.trim() {
            "1" => break RubberDuckBackendChoice::InternalModel,
            "2" => break RubberDuckBackendChoice::ExternalAgent,
            _ => eprintln!("Enter 1 or 2."),
        }
    };
    let (internal_model_id, external_agent_id) = match backend {
        RubberDuckBackendChoice::InternalModel => {
            let model_id =
                prompt_value("Critic model id (e.g. openrouter/deepseek-v3)", None, true)?
                    .expect("required prompt always returns a value");
            (Some(model_id), None)
        }
        RubberDuckBackendChoice::ExternalAgent => {
            let candidate = select_agent(candidates)?;
            let agent_id = match candidate {
                SetupCandidate::Local(candidate) => setup_local_agent(candidate, scope)?,
                SetupCandidate::Registry(agent) => setup_registry_agent(agent, scope)?,
            };
            (None, Some(agent_id))
        }
    };
    let mode = loop {
        let value = prompt_line("Rubber duck mode (manual/automatic, default manual): ")?;
        match value.trim() {
            "" | "manual" => break config::RubberDuckModeSetting::Manual,
            "automatic" => break config::RubberDuckModeSetting::Automatic,
            _ => eprintln!("Mode must be manual or automatic."),
        }
    };
    let path = config::configure_rubber_duck(
        scope,
        mode,
        internal_model_id.as_deref(),
        external_agent_id.as_deref(),
    )?;
    let backend = match (&internal_model_id, &external_agent_id) {
        (Some(model_id), _) => format!("internal model `{model_id}`"),
        (_, Some(agent_id)) => format!("external agent `{agent_id}`"),
        (None, None) => String::from("none"),
    };
    let mode_name = match mode {
        config::RubberDuckModeSetting::Manual => "manual",
        config::RubberDuckModeSetting::Automatic => "automatic",
        config::RubberDuckModeSetting::Off => unreachable!("wizard never writes off"),
    };
    println!("Rubber duck enabled (mode: {mode_name}, backend: {backend}).");
    println!("Wrote [agents.rubber_duck] to {}.", path.display());
    Ok(())
}

fn candidate_sort_key(candidate: &SetupCandidate) -> (u8, String) {
    match candidate {
        SetupCandidate::Local(candidate) => (0, candidate.file_name.to_ascii_lowercase()),
        SetupCandidate::Registry(agent) => (1, agent.name.to_ascii_lowercase()),
    }
}

fn discover_agents(directory: &Path) -> Result<Vec<AgentCandidate>, String> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
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

fn select_agent(candidates: &[SetupCandidate]) -> Result<&SetupCandidate, String> {
    if candidates.is_empty() {
        return Err(String::from("no compatible agent servers found"));
    }
    println!("Available agent servers:");
    for (index, candidate) in candidates.iter().enumerate() {
        match candidate {
            SetupCandidate::Local(candidate) => {
                println!("  {}) {} (local)", index + 1, candidate.file_name);
            }
            SetupCandidate::Registry(agent) => {
                println!("  {}) {} {} (ACP Registry)", index + 1, agent.name, agent.version);
            }
        }
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
        validate_env_name_manifest(&env.name)?;
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
        validate_env_name_manifest(&input.config.env)?;
        if !env_names.insert(input.config.env.clone()) {
            return Err(format!(
                "agent setup manifest maps multiple values to `{}`",
                input.config.env
            ));
        }
    }
    Ok(manifest)
}

fn validate_env_name_manifest(name: &str) -> Result<(), String> {
    if validate_env_name(name) {
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
            let value = read_hidden_value(env.required)?;
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

/// Asks for the `[agents.servers.<name>]` key. Defaults to the agent's own
/// id; a custom name lets one binary run as several servers (for example with
/// different model env). Restricts to TOML bare-key and secret-store-safe
/// characters so the name survives config serialization unchanged.
fn prompt_server_name(display_name: &str, default: &str) -> Result<String, String> {
    loop {
        let value =
            prompt_value(&format!("Agent server name for {display_name}"), Some(default), true)?
                .expect("required prompt always returns a value");
        if validate_server_name(&value) {
            return Ok(value);
        }
        eprintln!("Server name must use only letters, digits, `-`, or `_` (max 64).");
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ee_agent_protocol::setup::{SetupAgent, SetupEnvVar, SetupInput, SetupInputConfig};

    use super::*;

    #[test]
    fn candidates_sort_local_first_then_by_name() {
        let registry: RegistryAgent = serde_json::from_str(
            r#"{"id":"alpha","name":"Alpha","version":"1.0.0","distribution":{"npx":{"package":"alpha@1.0.0"}}}"#,
        )
        .unwrap();
        let mut candidates = [
            SetupCandidate::Registry(Box::new(registry)),
            SetupCandidate::Local(AgentCandidate {
                path: PathBuf::from("/tmp/ee-zulu-agent"),
                file_name: String::from("ee-zulu-agent"),
            }),
        ];

        candidates.sort_by_cached_key(candidate_sort_key);

        assert!(matches!(candidates[0], SetupCandidate::Local(_)));
        assert!(matches!(candidates[1], SetupCandidate::Registry(_)));
    }

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

    // ── OpenCode provider setup (Phase 5 integration) ────────────────────

    /// The OpenCode agent ships as one more local `ee-*-agent` binary, so the
    /// generic discovery pattern has to accept it unchanged.
    #[test]
    fn opencode_agent_discovers_as_one_local_provider() {
        assert!(is_agent_file_name("ee-opencode-agent"));

        let manifest = ee_opencode_agent::config::setup_manifest();
        assert_eq!(manifest.agent.id, "opencode");
        assert_eq!(manifest.agent.display_name, "OpenCode");
        assert!(!manifest.agent.id.contains("openrouter"), "not an OpenRouter alias");
        assert!(!manifest.agent.id.contains("openai"), "not a generic OpenAI provider");
    }

    /// The manifest the OpenCode binary emits must satisfy every rule this host
    /// setup path enforces, including unique environment destinations.
    #[test]
    fn opencode_manifest_passes_host_setup_validation_with_explicit_surface_selection() {
        let manifest = validate_manifest(ee_opencode_agent::config::setup_manifest())
            .expect("host setup accepts the OpenCode manifest");

        let key = manifest
            .env_vars
            .iter()
            .find(|env| env.name == ee_opencode_agent::config::API_KEY_ENV)
            .expect("api key variable");
        assert!(key.required && key.secret, "the OpenCode key is required and secret");

        for env in [ee_opencode_agent::config::SURFACE_ENV, ee_opencode_agent::config::MODEL_ENV] {
            let input = manifest
                .inputs
                .iter()
                .find(|input| input.config.env == env)
                .expect("surface and model stay explicit inputs");
            assert!(input.default.is_none(), "{env} is never defaulted for the user");
        }
    }

    /// Setup must describe Go and Zen as OpenCode's own service surfaces: the
    /// host never claims billing, balance, usage, or account capability.
    #[test]
    fn opencode_manifest_keeps_provider_claims_with_provider() {
        let manifest = ee_opencode_agent::config::setup_manifest();
        let mut text = vec![manifest.agent.display_name.clone()];
        text.extend(manifest.env_vars.iter().map(|env| env.description.clone()));
        text.extend(manifest.inputs.iter().map(|input| input.label.clone()));

        let described = text.join("\n").to_ascii_lowercase();
        for claim in ["billing", "balances", "usage limits", "account management"] {
            assert!(described.contains(claim), "setup explains `{claim}` ownership");
        }
        assert!(
            described.contains("stay with opencode"),
            "the descriptor attributes those to OpenCode: {described}"
        );
        assert!(
            described.contains("zen") && described.contains("go"),
            "both surfaces are named for the user: {described}"
        );
        for root in ["https://opencode.ai/zen/v1", "https://opencode.ai/zen/go/v1"] {
            assert!(described.contains(root), "setup shows the `{root}` root");
        }
    }

    #[test]
    fn split_setup_env_partitions_secret_references_from_literals() {
        let env = BTreeMap::from([
            (
                String::from("OPENROUTER_API_KEY"),
                String::from("secret://agent.openrouter.OPENROUTER_API_KEY"),
            ),
            (String::from("OPENROUTER_MODEL"), String::from("example/model")),
        ]);
        let secret_names = BTreeSet::from([String::from("OPENROUTER_API_KEY")]);

        let (literals, refs) = split_setup_env(&env, &secret_names);

        assert_eq!(
            literals,
            BTreeMap::from([(String::from("OPENROUTER_MODEL"), String::from("example/model"))])
        );
        assert_eq!(
            refs,
            BTreeMap::from([(
                String::from("OPENROUTER_API_KEY"),
                String::from("secret://agent.openrouter.OPENROUTER_API_KEY")
            )])
        );
    }

    #[test]
    fn split_setup_env_without_secret_names_keeps_full_env_local() {
        let env = BTreeMap::from([(String::from("MODEL"), String::from("example/model"))]);

        let (literals, refs) = split_setup_env(&env, &BTreeSet::new());

        assert_eq!(literals, env);
        assert!(refs.is_empty());
    }

    #[test]
    fn manifest_secret_env_names_returns_only_secret_flagged_vars() {
        let manifest = SetupManifest {
            schema_version: SETUP_MANIFEST_SCHEMA_VERSION,
            agent: SetupAgent {
                id: String::from("example"),
                display_name: String::from("Example"),
            },
            env_vars: vec![
                SetupEnvVar {
                    name: String::from("SECRET_KEY"),
                    required: true,
                    secret: true,
                    description: String::from("Secret."),
                },
                SetupEnvVar {
                    name: String::from("MODEL"),
                    required: false,
                    secret: false,
                    description: String::from("Model."),
                },
            ],
            inputs: Vec::new(),
        };

        assert_eq!(
            manifest_secret_env_names(&manifest),
            BTreeSet::from([String::from("SECRET_KEY")])
        );
    }
}
