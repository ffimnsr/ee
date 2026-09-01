//! ACP Registry discovery and installation for external agent servers.

mod archive;

#[cfg(test)]
use archive::write_archive_file;
use archive::{classify_archive, extract_download, safe_relative_path};

use std::collections::{BTreeMap, BTreeSet};
use std::env;
#[cfg(windows)]
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use reqwest::blocking::{Client, Response};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use url::Url;

const REGISTRY_URL: &str = "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const VERSION_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_REGISTRY_BYTES: u64 = 4 * 1024 * 1024;
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 100_000;
const MAX_VERSION_OUTPUT_BYTES: u64 = 64 * 1024;
const MAX_MANIFEST_BYTES: u64 = 16 * 1024;
const INSTALL_MANIFEST: &str = "install-manifest.json";
const INSTALL_MANIFEST_SCHEMA: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedAgent {
    pub(crate) id: String,
    pub(crate) display_name: String,
    pub(crate) command: PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RegistryAgent {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) version: String,
    #[serde(default)]
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) repository: Option<String>,
    #[serde(default)]
    pub(crate) website: Option<String>,
    #[serde(default)]
    pub(crate) license: String,
    distribution: RegistryDistribution,
}

#[derive(Debug, Deserialize)]
struct Registry {
    version: String,
    agents: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RegistryDistribution {
    #[serde(default)]
    npx: Option<PackageDistribution>,
    #[serde(default)]
    uvx: Option<PackageDistribution>,
    #[serde(default)]
    binary: Option<BTreeMap<String, BinaryDistribution>>,
}

#[derive(Debug, Clone, Deserialize)]
struct PackageDistribution {
    package: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
struct BinaryDistribution {
    archive: String,
    cmd: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    sha256: Option<String>,
}

#[derive(Debug, Clone)]
enum LaunchDistribution {
    Binary(BinaryDistribution),
    Npx(PackageDistribution),
    Uvx(PackageDistribution),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BinaryCacheMetadata {
    platform: String,
    archive: String,
    sha256: Option<String>,
    cmd: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct InstallManifest {
    schema: u32,
    metadata: BinaryCacheMetadata,
    files: Vec<InstalledFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct InstalledFile {
    path: String,
    sha256: String,
    executable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveFormat {
    Zip,
    TarGz,
    TarBz2,
    Raw,
}

pub(crate) fn fetch_agents() -> Result<Vec<RegistryAgent>, String> {
    let client = http_client()?;
    let response = client
        .get(REGISTRY_URL)
        .send()
        .and_then(Response::error_for_status)
        .map_err(|error| format!("cannot fetch ACP registry: {error}"))?;
    let bytes = read_response_bounded(response, MAX_REGISTRY_BYTES, "ACP registry")?;
    let registry: Registry =
        serde_json::from_slice(&bytes).map_err(|error| format!("invalid ACP registry: {error}"))?;
    validate_registry(registry)
}

fn validate_registry(registry: Registry) -> Result<Vec<RegistryAgent>, String> {
    validate_registry_version(&registry.version)?;

    let mut ids = BTreeSet::new();
    let mut agents = Vec::new();
    for raw_agent in registry.agents {
        let agent = match serde_json::from_value::<RegistryAgent>(raw_agent) {
            Ok(agent) => agent,
            Err(error) => {
                eprintln!("warning: skipping malformed ACP registry agent: {error}");
                continue;
            }
        };
        match agent.validate() {
            Ok(true) => {
                if !ids.insert(agent.id.clone()) {
                    return Err(format!("ACP registry repeats valid agent id `{}`", agent.id));
                }
                agents.push(agent);
            }
            Ok(false) => {}
            Err(error) => eprintln!("warning: skipping malformed ACP registry agent: {error}"),
        }
    }
    agents.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(agents)
}

fn validate_registry_version(version: &str) -> Result<(), String> {
    let components = numeric_version_components(version).ok_or_else(|| {
        format!("unsupported ACP registry version {version}; expected well-formed 1.x version")
    })?;
    if components[0] == "1" {
        Ok(())
    } else {
        Err(format!("unsupported ACP registry version {version}; expected well-formed 1.x version"))
    }
}

impl RegistryAgent {
    pub(crate) fn source_url(&self) -> Option<&str> {
        self.repository.as_deref().or(self.website.as_deref())
    }

    pub(crate) fn is_download_verified(&self) -> bool {
        match self.launch_distribution() {
            Some(LaunchDistribution::Binary(binary)) => binary.sha256.is_some(),
            Some(LaunchDistribution::Npx(_) | LaunchDistribution::Uvx(_)) | None => true,
        }
    }

    pub(crate) fn uses_package_runner(&self) -> bool {
        matches!(
            self.launch_distribution(),
            Some(LaunchDistribution::Npx(_) | LaunchDistribution::Uvx(_))
        )
    }

    pub(crate) fn prepare(&self) -> Result<PreparedAgent, String> {
        let distribution = self
            .launch_distribution()
            .ok_or_else(|| format!("{} has no distribution for this platform", self.name))?;
        if let LaunchDistribution::Binary(binary) = &distribution
            && let Some(prepared) = self.installed_binary(binary)?
        {
            println!(
                "Using installed {} {} from {}.",
                self.name,
                self.version,
                prepared.command.display()
            );
            return Ok(prepared);
        }
        match distribution {
            LaunchDistribution::Binary(binary) => self.install_binary(&binary),
            LaunchDistribution::Npx(package) => self.package_agent("npx", &package),
            LaunchDistribution::Uvx(package) => self.package_agent("uvx", &package),
        }
    }

    fn validate(&self) -> Result<bool, String> {
        validate_agent_id(&self.id)?;
        validate_agent_version(&self.version)?;
        if self.name.trim().is_empty() {
            return Err(format!("ACP registry agent `{}` has empty name", self.id));
        }
        if self.description.trim().is_empty() {
            return Err(format!("ACP registry agent `{}` has empty description", self.id));
        }

        let Some(distribution) = self.launch_distribution() else {
            return Ok(false);
        };
        match distribution {
            LaunchDistribution::Binary(binary) => validate_binary(&binary)?,
            LaunchDistribution::Npx(package) => validate_package("npx", &package, &self.version)?,
            LaunchDistribution::Uvx(package) => validate_package("uvx", &package, &self.version)?,
        }
        Ok(true)
    }

    fn launch_distribution(&self) -> Option<LaunchDistribution> {
        current_platform()
            .and_then(|platform| {
                self.distribution.binary.as_ref().and_then(|binaries| binaries.get(platform))
            })
            .cloned()
            .map(LaunchDistribution::Binary)
            .or_else(|| self.distribution.npx.clone().map(LaunchDistribution::Npx))
            .or_else(|| self.distribution.uvx.clone().map(LaunchDistribution::Uvx))
    }

    fn installed_binary(
        &self,
        binary: &BinaryDistribution,
    ) -> Result<Option<PreparedAgent>, String> {
        let Some(name) = binary_executable_name(binary) else {
            return Ok(None);
        };
        let Some(command) = find_command_in_path(&name) else {
            return Ok(None);
        };
        if !command_reports_version(&command, &self.version)? {
            return Ok(None);
        }
        Ok(Some(self.prepared(command, binary.args.clone(), binary.env.clone())))
    }

    fn package_agent(
        &self,
        runner: &str,
        package: &PackageDistribution,
    ) -> Result<PreparedAgent, String> {
        let command = find_command_in_path(runner).ok_or_else(|| {
            format!(
                "{} requires `{runner}` in PATH; install {runner} or install {} {} directly",
                self.name, self.name, self.version
            )
        })?;
        let args = package_launch_args(runner, package);
        Ok(self.prepared(command, args, package.env.clone()))
    }

    fn install_binary(&self, binary: &BinaryDistribution) -> Result<PreparedAgent, String> {
        let platform = current_platform()
            .ok_or_else(|| String::from("cannot resolve current platform for ACP agent"))?;
        let metadata = binary_cache_metadata(platform, binary);
        let identity = binary_cache_identity(&metadata);
        let data_dir = dirs::data_local_dir()
            .ok_or_else(|| String::from("cannot resolve user data directory for ACP agents"))?;
        let agent_dir = data_dir.join("ee").join("agents").join(&self.id);
        let version_dir = agent_dir.join(&self.version);
        let install_dir = version_dir.join(identity);
        let relative_command = safe_relative_path(&binary.cmd)?;
        if validate_cached_install(&install_dir, &relative_command, &metadata)? {
            let command = install_dir.join(&relative_command);
            return Ok(self.prepared(command, binary.args.clone(), binary.env.clone()));
        }

        fs::create_dir_all(&version_dir)
            .map_err(|error| format!("cannot create {}: {error}", version_dir.display()))?;
        let staging = tempfile::Builder::new()
            .prefix(".install-")
            .tempdir_in(&version_dir)
            .map_err(|error| format!("cannot create agent staging directory: {error}"))?;
        let download = tempfile::Builder::new()
            .prefix(".download-")
            .tempfile_in(&version_dir)
            .map_err(|error| format!("cannot create agent download file: {error}"))?;

        println!("Downloading {} {} from ACP Registry.", self.name, self.version);
        download_archive(&binary.archive, download.as_file())?;
        verify_download(download.path(), binary.sha256.as_deref())?;
        extract_download(download.path(), &binary.archive, staging.path(), &relative_command)?;
        let staged_command = staging.path().join(&relative_command);
        make_executable(&staged_command)?;
        if !is_executable_file(&staged_command)? {
            return Err(format!(
                "ACP registry command missing after extraction: {}",
                relative_command.display()
            ));
        }
        let manifest = InstallManifest {
            schema: INSTALL_MANIFEST_SCHEMA,
            metadata: metadata.clone(),
            files: snapshot_install_files(staging.path())?,
        };
        write_install_manifest(staging.path(), &manifest)?;

        publish_staged_install(staging.path(), &install_dir, &relative_command, &metadata)?;
        let command = install_dir.join(relative_command);
        Ok(self.prepared(command, binary.args.clone(), binary.env.clone()))
    }

    fn prepared(
        &self,
        command: PathBuf,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    ) -> PreparedAgent {
        PreparedAgent { id: self.id.clone(), display_name: self.name.clone(), command, args, env }
    }
}

fn validate_binary(binary: &BinaryDistribution) -> Result<(), String> {
    validate_https_url(&binary.archive, "agent archive")?;
    classify_archive(&binary.archive)?;
    safe_relative_path(&binary.cmd)?;
    validate_process_metadata(&binary.args, &binary.env)?;
    if let Some(hash) = &binary.sha256 {
        validate_sha256(hash)?;
    }
    Ok(())
}

fn validate_package(
    runner: &str,
    package: &PackageDistribution,
    agent_version: &str,
) -> Result<(), String> {
    let (name, pin) = package_name_and_pin(runner, &package.package).ok_or_else(|| {
        format!(
            "ACP registry {runner} package `{}` is not an exactly pinned package name",
            package.package
        )
    })?;
    if !valid_package_name(runner, name) {
        return Err(format!("ACP registry has invalid {runner} package `{}`", package.package));
    }
    if pin != agent_version {
        return Err(format!(
            "ACP registry {runner} package `{}` does not match agent version {agent_version}",
            package.package
        ));
    }
    validate_process_metadata(&package.args, &package.env)
}

fn package_name_and_pin<'a>(runner: &str, package: &'a str) -> Option<(&'a str, &'a str)> {
    let separator = if runner == "uvx" && package.contains("==") { "==" } else { "@" };
    let index = package.rfind(separator)?;
    let name = &package[..index];
    let version = &package[index + separator.len()..];
    (!name.is_empty() && !version.is_empty()).then_some((name, version))
}

fn valid_package_name(runner: &str, name: &str) -> bool {
    let valid_component = |component: &str| {
        !component.is_empty()
            && component
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    };
    if runner == "npx" {
        if let Some(scoped) = name.strip_prefix('@') {
            let mut components = scoped.split('/');
            return matches!((components.next(), components.next(), components.next()),
                (Some(scope), Some(package), None) if valid_component(scope) && valid_component(package));
        }
        valid_component(name)
    } else {
        valid_component(name)
    }
}

fn validate_process_metadata(
    args: &[String],
    env: &BTreeMap<String, String>,
) -> Result<(), String> {
    if args.iter().any(|arg| arg.contains('\0')) {
        return Err(String::from("ACP registry argument contains NUL"));
    }
    validate_env(env)
}

fn validate_env(values: &BTreeMap<String, String>) -> Result<(), String> {
    for (name, value) in values {
        if name.is_empty() || name.contains('\0') || name.contains('=') {
            return Err(format!("ACP registry has invalid environment variable `{name}`"));
        }
        if value.contains('\0') {
            return Err(format!("ACP registry environment variable `{name}` contains NUL"));
        }
    }
    Ok(())
}

fn current_platform() -> Option<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Some("linux-x86_64"),
        ("linux", "aarch64") => Some("linux-aarch64"),
        ("macos", "x86_64") => Some("darwin-x86_64"),
        ("macos", "aarch64") => Some("darwin-aarch64"),
        ("windows", "x86_64") => Some("windows-x86_64"),
        ("windows", "aarch64") => Some("windows-aarch64"),
        _ => None,
    }
}

fn binary_executable_name(binary: &BinaryDistribution) -> Option<String> {
    let raw = safe_relative_path(&binary.cmd).ok()?.file_name()?.to_str()?.to_owned();
    Some(raw.trim_end_matches(".exe").trim_end_matches(".cmd").to_owned())
}

fn package_launch_args(runner: &str, package: &PackageDistribution) -> Vec<String> {
    let mut args = Vec::with_capacity(package.args.len() + 2);
    if runner == "npx" {
        args.push(String::from("--yes"));
    }
    args.push(package.package.clone());
    args.extend(package.args.clone());
    args
}

fn find_command_in_path(command: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    find_command_in_paths(command, env::split_paths(&path))
}

fn find_command_in_paths(
    command: &str,
    paths: impl IntoIterator<Item = PathBuf>,
) -> Option<PathBuf> {
    let extensions = command_extensions();
    paths.into_iter().find_map(|directory| {
        extensions.iter().find_map(|extension| {
            let candidate = directory.join(format!("{command}{extension}"));
            is_runnable_file(&candidate).ok().filter(|executable| *executable).map(|_| candidate)
        })
    })
}

#[cfg(windows)]
fn command_extensions() -> Vec<String> {
    let path_ext =
        env::var_os("PATHEXT").unwrap_or_else(|| OsStr::new(".COM;.EXE;.BAT;.CMD").into());
    let mut extensions = vec![String::new()];
    extensions.extend(path_ext.to_string_lossy().split(';').map(str::to_ascii_lowercase));
    extensions
}

#[cfg(not(windows))]
fn command_extensions() -> Vec<String> {
    vec![String::new()]
}

fn command_reports_version(command: &Path, expected: &str) -> Result<bool, String> {
    let mut stdout = tempfile::tempfile()
        .map_err(|error| format!("cannot capture {} version stdout: {error}", command.display()))?;
    let mut stderr = tempfile::tempfile()
        .map_err(|error| format!("cannot capture {} version stderr: {error}", command.display()))?;
    let mut probe = Command::new(command);
    probe
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout.try_clone().map_err(|error| {
            format!("cannot clone {} version stdout: {error}", command.display())
        })?))
        .stderr(Stdio::from(stderr.try_clone().map_err(|error| {
            format!("cannot clone {} version stderr: {error}", command.display())
        })?));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        probe.process_group(0);
    }
    let mut child = probe
        .spawn()
        .map_err(|error| format!("cannot check {} version: {error}", command.display()))?;
    let process_id = child.id();
    let deadline = Instant::now() + VERSION_TIMEOUT;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("cannot wait for {}: {error}", command.display()))?
        {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            terminate_probe_group(process_id);
            let _ = child.wait();
            break None;
        }
        thread::sleep(Duration::from_millis(10));
    };
    terminate_probe_group(process_id);
    if !status.is_some_and(|status| status.success()) {
        return Ok(false);
    }
    stdout
        .rewind()
        .map_err(|error| format!("cannot rewind {} version stdout: {error}", command.display()))?;
    stderr
        .rewind()
        .map_err(|error| format!("cannot rewind {} version stderr: {error}", command.display()))?;
    let (stdout, stdout_truncated) = read_output_bounded(stdout)?;
    let (stderr, stderr_truncated) = read_output_bounded(stderr)?;
    if stdout_truncated || stderr_truncated {
        return Ok(false);
    }
    Ok(version_token_matches(&stdout, expected) || version_token_matches(&stderr, expected))
}

#[cfg(unix)]
fn terminate_probe_group(process_id: u32) {
    if let Ok(process_group) = i32::try_from(process_id) {
        // Probe owns its process group, so descendants cannot retain captured pipes past deadline.
        unsafe {
            libc::kill(-process_group, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn terminate_probe_group(_process_id: u32) {}

fn read_output_bounded(mut reader: impl io::Read) -> Result<(Vec<u8>, bool), String> {
    let mut output = Vec::new();
    reader
        .by_ref()
        .take(MAX_VERSION_OUTPUT_BYTES + 1)
        .read_to_end(&mut output)
        .map_err(|error| format!("cannot read version output: {error}"))?;
    let truncated = output.len() as u64 > MAX_VERSION_OUTPUT_BYTES;
    output.truncate(MAX_VERSION_OUTPUT_BYTES as usize);
    Ok((output, truncated))
}

fn version_token_matches(output: &[u8], expected: &str) -> bool {
    let expected = expected.as_bytes();
    if expected.is_empty() {
        return false;
    }
    output.windows(expected.len()).enumerate().any(|(index, candidate)| {
        candidate == expected
            && version_start_boundary(output, index)
            && output.get(index + expected.len()).is_none_or(|byte| !is_version_character(*byte))
    })
}

fn version_start_boundary(output: &[u8], index: usize) -> bool {
    let Some(before) = index.checked_sub(1).and_then(|before| output.get(before)) else {
        return true;
    };
    if *before != b'v' {
        return !is_version_character(*before);
    }
    index
        .checked_sub(2)
        .and_then(|before_v| output.get(before_v))
        .is_none_or(|byte| !is_version_character(*byte))
}

fn is_version_character(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+')
}

fn http_client() -> Result<Client, String> {
    let redirect = reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 5 {
            attempt.error("too many redirects")
        } else if attempt.url().scheme() != "https" {
            attempt.error("redirect target must use HTTPS")
        } else {
            attempt.follow()
        }
    });
    Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(redirect)
        .build()
        .map_err(|error| format!("cannot build ACP registry HTTP client: {error}"))
}

fn read_response_bounded(
    mut response: Response,
    limit: u64,
    label: &str,
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read {label}: {error}"))?;
    if bytes.len() as u64 > limit {
        return Err(format!("{label} exceeds {limit} bytes"));
    }
    Ok(bytes)
}

fn download_archive(url: &str, mut destination: &File) -> Result<(), String> {
    validate_https_url(url, "agent archive")?;
    let response = http_client()?
        .get(url)
        .send()
        .and_then(Response::error_for_status)
        .map_err(|error| format!("cannot download agent archive: {error}"))?;
    let mut limited = response.take(MAX_DOWNLOAD_BYTES + 1);
    let written = io::copy(&mut limited, &mut destination)
        .map_err(|error| format!("cannot save agent archive: {error}"))?;
    if written > MAX_DOWNLOAD_BYTES {
        return Err(format!("agent archive exceeds {MAX_DOWNLOAD_BYTES} bytes"));
    }
    destination.sync_all().map_err(|error| format!("cannot sync agent archive: {error}"))
}

fn verify_download(path: &Path, expected: Option<&str>) -> Result<(), String> {
    let Some(expected) = expected else {
        eprintln!("warning: ACP registry provides no SHA-256 checksum for this agent");
        return Ok(());
    };
    validate_sha256(expected)?;
    let mut file = File::open(path).map_err(|error| format!("cannot verify download: {error}"))?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher).map_err(|error| format!("cannot hash download: {error}"))?;
    let actual = format!("{:x}", hasher.finalize());
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(format!("agent archive SHA-256 mismatch: expected {expected}, got {actual}"))
    }
}

fn validate_sha256(hash: &str) -> Result<(), String> {
    if hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(String::from("ACP registry has invalid SHA-256 checksum"))
    }
}

fn binary_cache_metadata(platform: &str, binary: &BinaryDistribution) -> BinaryCacheMetadata {
    BinaryCacheMetadata {
        platform: platform.to_owned(),
        archive: binary.archive.clone(),
        sha256: binary.sha256.clone(),
        cmd: binary.cmd.clone(),
    }
}

fn binary_cache_identity(metadata: &BinaryCacheMetadata) -> String {
    let mut hasher = Sha256::new();
    hash_identity_field(&mut hasher, metadata.platform.as_bytes());
    hash_identity_field(&mut hasher, metadata.archive.as_bytes());
    match &metadata.sha256 {
        Some(hash) => {
            hasher.update([1]);
            hash_identity_field(&mut hasher, hash.as_bytes());
        }
        None => hasher.update([0]),
    }
    hash_identity_field(&mut hasher, metadata.cmd.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn hash_identity_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file =
        File::open(path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)
        .map_err(|error| format!("cannot hash {}: {error}", path.display()))?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn snapshot_install_files(root: &Path) -> Result<Vec<InstalledFile>, String> {
    let mut files = Vec::new();
    collect_install_files(root, root, &mut files)?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn collect_install_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<InstalledFile>,
) -> Result<(), String> {
    let entries = fs::read_dir(directory).map_err(|error| {
        format!("cannot inspect installed agent {}: {error}", directory.display())
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!("cannot inspect installed agent entry in {}: {error}", directory.display())
        })?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!("installed agent contains symlink {}", path.display()));
        }
        if metadata.is_dir() {
            collect_install_files(root, &path, files)?;
            continue;
        }
        if !metadata.is_file() {
            return Err(format!("installed agent contains special file {}", path.display()));
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| format!("installed agent path escapes cache: {}", path.display()))?;
        if relative == Path::new(INSTALL_MANIFEST) {
            continue;
        }
        files.push(InstalledFile {
            path: relative.to_string_lossy().replace('\\', "/"),
            sha256: sha256_file(&path)?,
            executable: is_file_executable(&metadata),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn is_file_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_file_executable(_metadata: &fs::Metadata) -> bool {
    false
}

fn write_install_manifest(directory: &Path, manifest: &InstallManifest) -> Result<(), String> {
    let path = directory.join(INSTALL_MANIFEST);
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|error| format!("cannot serialize agent install manifest: {error}"))?;
    let mut file = File::options()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
    file.write_all(&bytes).map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    file.sync_all().map_err(|error| format!("cannot sync {}: {error}", path.display()))
}

fn validate_cached_install(
    install_dir: &Path,
    relative_command: &Path,
    metadata: &BinaryCacheMetadata,
) -> Result<bool, String> {
    let install_metadata = match fs::symlink_metadata(install_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("cannot inspect {}: {error}", install_dir.display())),
    };
    if install_metadata.file_type().is_symlink() || !install_metadata.is_dir() {
        return Ok(false);
    }
    let command = install_dir.join(relative_command);
    if !is_executable_file(&command)? {
        return Ok(false);
    }
    let manifest_path = install_dir.join(INSTALL_MANIFEST);
    let manifest_metadata = match fs::symlink_metadata(&manifest_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("cannot inspect {}: {error}", manifest_path.display())),
    };
    if manifest_metadata.file_type().is_symlink() || !manifest_metadata.is_file() {
        return Ok(false);
    }
    let mut file = File::open(&manifest_path)
        .map_err(|error| format!("cannot open {}: {error}", manifest_path.display()))?;
    let mut bytes = Vec::new();
    io::Read::by_ref(&mut file)
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read {}: {error}", manifest_path.display()))?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Ok(false);
    }
    let Ok(manifest) = serde_json::from_slice::<InstallManifest>(&bytes) else {
        return Ok(false);
    };
    if manifest.schema != INSTALL_MANIFEST_SCHEMA || manifest.metadata != *metadata {
        return Ok(false);
    }
    Ok(snapshot_install_files(install_dir)? == manifest.files)
}

fn publish_staged_install(
    staging: &Path,
    install_dir: &Path,
    relative_command: &Path,
    metadata: &BinaryCacheMetadata,
) -> Result<(), String> {
    if install_dir.exists() {
        if validate_cached_install(install_dir, relative_command, metadata)? {
            return Ok(());
        }
        quarantine_invalid_install(install_dir)?;
    }
    match fs::rename(staging, install_dir) {
        Ok(()) => Ok(()),
        Err(error) if install_dir.exists() => {
            if validate_cached_install(install_dir, relative_command, metadata)? {
                Ok(())
            } else {
                Err(format!("cannot publish {}: {error}", install_dir.display()))
            }
        }
        Err(error) => Err(format!("cannot publish {}: {error}", install_dir.display())),
    }
}

fn quarantine_invalid_install(install_dir: &Path) -> Result<(), String> {
    let parent = install_dir
        .parent()
        .ok_or_else(|| format!("invalid install path {}", install_dir.display()))?;
    let name = install_dir.file_name().and_then(|name| name.to_str()).unwrap_or("cache");
    for sequence in 0..1000_u16 {
        let quarantine = parent.join(format!(".{name}.invalid-{sequence}"));
        if !quarantine.exists() {
            return fs::rename(install_dir, &quarantine).map_err(|error| {
                format!("cannot quarantine invalid cache {}: {error}", install_dir.display())
            });
        }
    }
    Err(format!("cannot allocate quarantine path for {}", install_dir.display()))
}

fn validate_agent_id(value: &str) -> Result<(), String> {
    let mut bytes = value.bytes();
    if !matches!(bytes.next(), Some(byte) if byte.is_ascii_lowercase())
        || !bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        Err(format!("ACP registry has invalid agent id `{value}`"))
    } else {
        Ok(())
    }
}

fn numeric_version_components(value: &str) -> Option<[&str; 3]> {
    let mut components = value.split('.');
    let result = [components.next()?, components.next()?, components.next()?];
    if components.next().is_none()
        && result.iter().all(|component| {
            !component.is_empty() && component.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        Some(result)
    } else {
        None
    }
}

fn validate_agent_version(value: &str) -> Result<(), String> {
    if numeric_version_components(value).is_some() {
        Ok(())
    } else {
        Err(format!("ACP registry has invalid agent version `{value}`"))
    }
}

fn validate_https_url(raw: &str, label: &str) -> Result<(), String> {
    let url = Url::parse(raw).map_err(|error| format!("invalid {label} URL: {error}"))?;
    if url.scheme() == "https" && url.host_str().is_some() {
        Ok(())
    } else {
        Err(format!("{label} URL must use HTTPS"))
    }
}

fn is_runnable_file(path: &Path) -> Result<bool, String> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
    };
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

fn is_executable_file(path: &Path) -> Result<bool, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
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

fn make_executable(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("agent command is not regular file: {}", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = metadata.permissions();
        permissions.set_mode((permissions.mode() & 0o777) | 0o700);
        fs::set_permissions(path, permissions)
            .map_err(|error| format!("cannot make {} executable: {error}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
