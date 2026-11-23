//! Install prebuilt runtime grammar packs.
//!
//! Release CI publishes one pack per target. The pack carries the grammar
//! shared libraries for every catalog language plus a manifest, so installing a
//! grammar needs no C compiler, Rust toolchain, cargo registry, or git checkout.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use flate2::read::GzDecoder;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tar::Archive;
use xi_core_lib::runtime_loader::{
    RuntimeConfigSource, shared_library_filename, with_default_runtime_loader_mut,
};

const PACK_SCHEMA_VERSION: u32 = 1;
const PACK_MANIFEST_NAME: &str = "manifest.json";
const PACK_GRAMMAR_DIR: &str = "grammars/";
const PACK_ASSET_PREFIX: &str = "ee-language-pack";
const MAX_PACK_BYTES: u64 = 128 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_PACK_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CHECKSUM_BYTES: u64 = 8 * 1024;
const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");
const BUILD_TARGET: &str = env!("EE_BUILD_TARGET");

#[derive(Debug)]
pub(crate) struct RuntimePackError {
    message: String,
    hint: Option<String>,
}

impl RuntimePackError {
    fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), hint: None }
    }

    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub(crate) fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }
}

impl fmt::Display for RuntimePackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RuntimePackError {}

impl From<io::Error> for RuntimePackError {
    fn from(error: io::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl From<serde_json::Error> for RuntimePackError {
    fn from(error: serde_json::Error) -> Self {
        Self::new(format!("invalid language pack manifest: {error}"))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct LanguagePackManifest {
    schema_version: u32,
    package_version: String,
    target: String,
    files: Vec<LanguagePackFile>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct LanguagePackFile {
    path: String,
    sha256: String,
    size: u64,
}

#[derive(Debug, Clone)]
struct InstallTarget {
    entry_path: String,
    file_name: String,
    languages: BTreeSet<String>,
}

pub(crate) fn install(
    languages: &[String],
    all: bool,
    local_pack: Option<&Path>,
    expected_sha256: Option<&str>,
    force: bool,
    verbose: bool,
) -> Result<(), RuntimePackError> {
    crate::config::configure_runtime_loader_for_file(None, false)
        .map_err(|error| RuntimePackError::new(format!("runtime config failed: {error}")))?;

    let targets = resolve_install_targets(languages, all)?;
    let destination = grammar_destination()?;
    fs::create_dir_all(&destination)?;

    if !force && targets.iter().all(|target| destination.join(&target.file_name).is_file()) {
        println!(
            "runtime grammars already installed in {} (use --force to reinstall)",
            destination.display()
        );
        return Ok(());
    }

    let pack = match local_pack {
        Some(path) => {
            if !path.is_file() {
                return Err(RuntimePackError::new(format!(
                    "language pack not found: {}",
                    path.display()
                )));
            }
            match expected_sha256 {
                Some(expected) => verify_sha256(path, expected)?,
                None => eprintln!(
                    "warning: installing an unverified local language pack; pass --sha256 to verify it"
                ),
            }
            path.to_path_buf()
        }
        None => fetch_pack(expected_sha256, force)?,
    };

    install_from_pack(&pack, &targets, &destination)?;

    println!(
        "installed {} grammar librar{} into {}",
        targets.len(),
        if targets.len() == 1 { "y" } else { "ies" },
        destination.display()
    );
    if verbose {
        for target in &targets {
            println!(
                "  {} -> {}",
                target.languages.iter().cloned().collect::<Vec<_>>().join(", "),
                destination.join(&target.file_name).display()
            );
        }
    }
    Ok(())
}

fn resolve_install_targets(
    requested: &[String],
    all: bool,
) -> Result<Vec<InstallTarget>, RuntimePackError> {
    if !all && requested.is_empty() {
        return Err(RuntimePackError::new(
            "pass --all or at least one --language to `ee do runtime install`",
        ));
    }
    if all && !requested.is_empty() {
        return Err(RuntimePackError::new(
            "--all cannot be combined with --language for `ee do runtime install`",
        ));
    }

    with_default_runtime_loader_mut(|loader| {
        let mut selected = Vec::new();
        if all {
            for language in loader.languages() {
                if let Some(library) = language.grammar_library_name() {
                    selected.push((language.canonical_id().to_string(), library.to_string()));
                }
            }
        } else {
            for name in requested {
                let language = loader.language_for_name(name).ok_or_else(|| {
                    RuntimePackError::new(format!(
                        "unknown runtime language `{name}`; run `ee do runtime languages`"
                    ))
                })?;
                let library = language.grammar_library_name().ok_or_else(|| {
                    RuntimePackError::new(format!(
                        "runtime language `{name}` has no configured grammar library"
                    ))
                })?;
                selected.push((language.canonical_id().to_string(), library.to_string()));
            }
        }

        if selected.is_empty() {
            return Err(RuntimePackError::new(
                "no runtime languages with a configured grammar were selected",
            ));
        }

        let mut targets: BTreeMap<String, InstallTarget> = BTreeMap::new();
        for (language_id, library) in selected {
            let file_name = shared_library_filename(&library);
            let entry_path = format!("{PACK_GRAMMAR_DIR}{file_name}");
            targets
                .entry(entry_path.clone())
                .or_insert_with(|| InstallTarget {
                    entry_path,
                    file_name,
                    languages: BTreeSet::new(),
                })
                .languages
                .insert(language_id);
        }

        Ok(targets.into_values().collect())
    })
}

fn grammar_destination() -> Result<PathBuf, RuntimePackError> {
    with_default_runtime_loader_mut(|loader| {
        loader.runtime_roots().grammar_dir_for(RuntimeConfigSource::User)
    })
    .ok_or_else(|| RuntimePackError::new("user runtime root is unavailable"))
}

fn fetch_pack(expected_sha256: Option<&str>, force: bool) -> Result<PathBuf, RuntimePackError> {
    let url = pack_url();
    let cache_dir = pack_cache_dir();
    fs::create_dir_all(&cache_dir)?;
    let archive_name = format!("{PACK_ASSET_PREFIX}-{PACKAGE_VERSION}-{BUILD_TARGET}.tar.gz");
    let archive_path = cache_dir.join(&archive_name);
    let checksum_path = cache_dir.join(format!("{archive_name}.sha256"));

    let expected = match expected_sha256 {
        Some(expected) => normalize_sha256(expected)?,
        None => load_expected_checksum(&url, &checksum_path, force)?,
    };

    if !force && archive_path.is_file() && verify_sha256(&archive_path, &expected).is_ok() {
        return Ok(archive_path);
    }
    if archive_path.exists() {
        fs::remove_file(&archive_path)?;
    }

    let client = http_client()?;
    download_to(&client, &url, &archive_path).map_err(|error| {
        error.with_hint(format!(
            "no prebuilt language pack for target `{BUILD_TARGET}` in ee `{PACKAGE_VERSION}`. \
             Build from source with `ee do runtime fetch/build --language <language>`, or pass \
             `--pack <file>` for an offline pack."
        ))
    })?;
    verify_sha256(&archive_path, &expected).map_err(|error| {
        let _ = fs::remove_file(&archive_path);
        error.with_hint(
            "language pack checksum mismatch; retry with --force or verify the release asset",
        )
    })?;
    Ok(archive_path)
}

fn load_expected_checksum(
    pack_url: &str,
    checksum_path: &Path,
    force: bool,
) -> Result<String, RuntimePackError> {
    if !force && checksum_path.is_file() {
        if let Ok(text) = fs::read_to_string(checksum_path)
            && let Some(checksum) = parse_checksum_text(&text)
        {
            return Ok(checksum);
        }
        fs::remove_file(checksum_path)?;
    }

    let client = http_client()?;
    let text = download_text(&client, &format!("{pack_url}.sha256")).map_err(|error| {
        error.with_hint(format!(
            "the release does not expose a checksum for the `{BUILD_TARGET}` language pack. \
             Pass `--sha256 <hex>` to verify a mirrored pack, or build from source."
        ))
    })?;
    let checksum = parse_checksum_text(&text).ok_or_else(|| {
        RuntimePackError::new("language pack checksum file did not contain a SHA-256 digest")
    })?;
    fs::write(checksum_path, format!("{checksum}\n"))?;
    Ok(checksum)
}

fn pack_url() -> String {
    let base = env::var("EE_LANGUAGE_PACK_BASE_URL")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            format!("https://github.com/ffimnsr/ee/releases/download/v{PACKAGE_VERSION}")
        });
    format!("{base}/{PACK_ASSET_PREFIX}-{PACKAGE_VERSION}-{BUILD_TARGET}.tar.gz")
}

fn pack_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| env::temp_dir().join("ee-cache"))
        .join("ee")
        .join("language-packs")
}

fn http_client() -> Result<Client, RuntimePackError> {
    Client::builder()
        .user_agent(format!("ee/{PACKAGE_VERSION}"))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|error| RuntimePackError::new(format!("failed creating HTTP client: {error}")))
}

fn download_to(client: &Client, url: &str, destination: &Path) -> Result<(), RuntimePackError> {
    ensure_https(url)?;
    let response = client
        .get(url)
        .send()
        .map_err(|error| RuntimePackError::new(format!("failed downloading {url}: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(RuntimePackError::new(format!("failed downloading {url}: HTTP {status}")));
    }
    if let Some(length) = response.content_length()
        && length > MAX_PACK_BYTES
    {
        return Err(RuntimePackError::new(format!(
            "language pack at {url} is too large ({length} bytes; limit {MAX_PACK_BYTES})"
        )));
    }

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_path(destination);
    let result = (|| -> Result<(), RuntimePackError> {
        let mut file = File::create(&temporary)?;
        let copied = io::copy(&mut response.take(MAX_PACK_BYTES + 1), &mut file)?;
        if copied > MAX_PACK_BYTES {
            return Err(RuntimePackError::new(format!(
                "language pack at {url} exceeds the {MAX_PACK_BYTES}-byte limit"
            )));
        }
        file.flush()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
        return result;
    }
    replace_file(&temporary, destination)?;
    Ok(())
}

fn download_text(client: &Client, url: &str) -> Result<String, RuntimePackError> {
    ensure_https(url)?;
    let response = client
        .get(url)
        .send()
        .map_err(|error| RuntimePackError::new(format!("failed downloading {url}: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(RuntimePackError::new(format!("failed downloading {url}: HTTP {status}")));
    }
    let mut text = String::new();
    response.take(MAX_CHECKSUM_BYTES + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_CHECKSUM_BYTES {
        return Err(RuntimePackError::new(format!("checksum file at {url} is too large")));
    }
    Ok(text)
}

fn ensure_https(url: &str) -> Result<(), RuntimePackError> {
    if url.starts_with("https://") {
        Ok(())
    } else {
        Err(RuntimePackError::new(format!("refusing non-HTTPS language pack URL: {url}")))
    }
}

fn install_from_pack(
    pack: &Path,
    targets: &[InstallTarget],
    destination: &Path,
) -> Result<(), RuntimePackError> {
    let manifest = read_manifest(pack)?;
    validate_manifest(&manifest, targets)?;
    let targets_by_path = targets
        .iter()
        .map(|target| (target.entry_path.as_str(), target))
        .collect::<BTreeMap<_, _>>();
    let files_by_path =
        manifest.files.iter().map(|file| (file.path.as_str(), file)).collect::<BTreeMap<_, _>>();

    let mut staged = Vec::new();
    let mut seen = BTreeSet::new();
    let archive_file = File::open(pack)?;
    let mut archive = Archive::new(GzDecoder::new(archive_file));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_path = normalize_tar_path(&entry.path()?)?;
        if entry_path == PACK_MANIFEST_NAME {
            continue;
        }
        let Some(target) = targets_by_path.get(entry_path.as_str()) else {
            continue;
        };
        if !seen.insert(entry_path.clone()) {
            return Err(RuntimePackError::new(format!(
                "language pack contains duplicate `{entry_path}`"
            )));
        }
        if !entry.header().entry_type().is_file() {
            return Err(RuntimePackError::new(format!(
                "language pack entry `{entry_path}` is not a regular file"
            )));
        }
        let file = files_by_path.get(entry_path.as_str()).ok_or_else(|| {
            RuntimePackError::new(format!("language pack manifest is missing `{entry_path}`"))
        })?;
        if entry.size() != file.size {
            return Err(RuntimePackError::new(format!(
                "language pack entry `{entry_path}` has size {}, expected {}",
                entry.size(),
                file.size
            )));
        }

        let temporary = temporary_path(&destination.join(&target.file_name));
        let result = (|| -> Result<(), RuntimePackError> {
            let mut output = File::create(&temporary)?;
            let copied = io::copy(&mut entry.by_ref().take(MAX_PACK_FILE_BYTES + 1), &mut output)?;
            if copied != file.size || copied > MAX_PACK_FILE_BYTES {
                return Err(RuntimePackError::new(format!(
                    "language pack entry `{entry_path}` has an unexpected size"
                )));
            }
            output.flush()?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_file(&temporary);
            cleanup_staged(&staged);
            return Err(error);
        }
        if let Err(error) = verify_sha256(&temporary, &file.sha256) {
            let _ = fs::remove_file(&temporary);
            cleanup_staged(&staged);
            return Err(error.with_hint(format!(
                "language pack entry `{entry_path}` failed checksum validation"
            )));
        }
        staged.push((temporary, destination.join(&target.file_name)));
    }

    let missing = targets
        .iter()
        .filter(|target| !seen.contains(&target.entry_path))
        .map(|target| target.entry_path.as_str())
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        cleanup_staged(&staged);
        return Err(RuntimePackError::new(format!(
            "language pack is missing requested grammar entries: {}",
            missing.join(", ")
        )));
    }

    while let Some((temporary, final_path)) = staged.pop() {
        if let Err(error) = replace_file(&temporary, &final_path) {
            let _ = fs::remove_file(&temporary);
            cleanup_staged(&staged);
            return Err(error);
        }
    }
    Ok(())
}

fn read_manifest(pack: &Path) -> Result<LanguagePackManifest, RuntimePackError> {
    let archive_file = File::open(pack)?;
    let mut archive = Archive::new(GzDecoder::new(archive_file));
    let mut manifest = None;
    for entry in archive.entries()? {
        let entry = entry?;
        let entry_path = normalize_tar_path(&entry.path()?)?;
        if entry_path != PACK_MANIFEST_NAME {
            continue;
        }
        if manifest.is_some() {
            return Err(RuntimePackError::new(
                "language pack contains more than one manifest.json",
            ));
        }
        if entry.size() > MAX_MANIFEST_BYTES {
            return Err(RuntimePackError::new("language pack manifest is too large"));
        }
        let mut data = Vec::new();
        entry.take(MAX_MANIFEST_BYTES + 1).read_to_end(&mut data)?;
        if data.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(RuntimePackError::new("language pack manifest is too large"));
        }
        manifest = Some(serde_json::from_slice(&data)?);
    }
    manifest.ok_or_else(|| RuntimePackError::new("language pack is missing manifest.json"))
}

fn validate_manifest(
    manifest: &LanguagePackManifest,
    targets: &[InstallTarget],
) -> Result<(), RuntimePackError> {
    if manifest.schema_version != PACK_SCHEMA_VERSION {
        return Err(RuntimePackError::new(format!(
            "unsupported language pack schema version {}",
            manifest.schema_version
        )));
    }
    if manifest.package_version != PACKAGE_VERSION {
        return Err(RuntimePackError::new(format!(
            "language pack is for ee {}, but this binary is ee {PACKAGE_VERSION}",
            manifest.package_version
        ))
        .with_hint("install the language pack from the matching ee release"));
    }
    if manifest.target != BUILD_TARGET {
        return Err(RuntimePackError::new(format!(
            "language pack is for target {}, but this binary targets {BUILD_TARGET}",
            manifest.target
        ))
        .with_hint("install the language pack from the matching ee release"));
    }

    let mut paths = BTreeSet::new();
    for file in &manifest.files {
        let path = normalize_manifest_path(&file.path)?;
        if !path.starts_with(PACK_GRAMMAR_DIR)
            || path[PACK_GRAMMAR_DIR.len()..].is_empty()
            || path[PACK_GRAMMAR_DIR.len()..].contains('/')
        {
            return Err(RuntimePackError::new(format!(
                "language pack manifest contains an invalid grammar path `{}`",
                file.path
            )));
        }
        if !paths.insert(path) {
            return Err(RuntimePackError::new(format!(
                "language pack manifest contains duplicate path `{}`",
                file.path
            )));
        }
        if file.size == 0 || file.size > MAX_PACK_FILE_BYTES {
            return Err(RuntimePackError::new(format!(
                "language pack manifest has an invalid size for `{}`",
                file.path
            )));
        }
        normalize_sha256(&file.sha256)?;
    }

    for target in targets {
        if !paths.contains(&target.entry_path) {
            return Err(RuntimePackError::new(format!(
                "language pack does not contain `{}`; the release may not include this language",
                target.entry_path
            )));
        }
    }
    Ok(())
}

fn normalize_manifest_path(path: &str) -> Result<String, RuntimePackError> {
    if path.starts_with('/') || path.contains('\\') {
        return Err(RuntimePackError::new(format!(
            "language pack manifest contains an unsafe path `{path}`"
        )));
    }
    let mut parts = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str().ok_or_else(|| {
                RuntimePackError::new(format!("language pack path is not UTF-8: `{path}`"))
            })?),
            _ => {
                return Err(RuntimePackError::new(format!(
                    "language pack manifest contains an unsafe path `{path}`"
                )));
            }
        }
    }
    if parts.is_empty() {
        return Err(RuntimePackError::new("language pack manifest contains an empty path"));
    }
    Ok(parts.join("/"))
}

fn normalize_tar_path(path: &Path) -> Result<String, RuntimePackError> {
    let raw = path
        .to_str()
        .ok_or_else(|| RuntimePackError::new("language pack entry path is not UTF-8"))?;
    if raw.contains('\\') {
        return Err(RuntimePackError::new(format!(
            "language pack contains an unsafe path `{raw}`"
        )));
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str().ok_or_else(|| {
                RuntimePackError::new(format!("language pack path is not UTF-8: `{raw}`"))
            })?),
            _ => {
                return Err(RuntimePackError::new(format!(
                    "language pack contains an unsafe path `{raw}`"
                )));
            }
        }
    }
    if parts.is_empty() {
        return Err(RuntimePackError::new("language pack contains an empty path"));
    }
    Ok(parts.join("/"))
}

fn normalize_sha256(value: &str) -> Result<String, RuntimePackError> {
    let value = value.trim().to_ascii_lowercase();
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(value)
    } else {
        Err(RuntimePackError::new("expected a 64-character SHA-256 digest"))
    }
}

fn parse_checksum_text(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        line.split_whitespace()
            .next()
            .filter(|token| token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .map(str::to_ascii_lowercase)
    })
}

fn verify_sha256(path: &Path, expected: &str) -> Result<(), RuntimePackError> {
    let expected = normalize_sha256(expected)?;
    let actual = sha256_file(path)?;
    if actual == expected {
        Ok(())
    } else {
        Err(RuntimePackError::new(format!(
            "SHA-256 mismatch for {}: expected {expected}, got {actual}",
            path.display()
        )))
    }
}

fn sha256_file(path: &Path) -> Result<String, RuntimePackError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)?;
    Ok(hex_encode(hasher.finalize().as_slice()))
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn temporary_path(destination: &Path) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let name = destination.file_name().and_then(|name| name.to_str()).unwrap_or("language-pack");
    destination.with_file_name(format!(".{name}.{}.{nonce}.tmp", std::process::id()))
}

fn replace_file(temporary: &Path, destination: &Path) -> Result<(), RuntimePackError> {
    match fs::rename(temporary, destination) {
        Ok(()) => Ok(()),
        Err(error) if destination.exists() => {
            fs::remove_file(destination)?;
            fs::rename(temporary, destination).map_err(|retry| {
                RuntimePackError::new(format!(
                    "failed replacing {}: {error}; retry failed: {retry}",
                    destination.display()
                ))
            })
        }
        Err(error) => Err(RuntimePackError::new(format!(
            "failed moving {} to {}: {error}",
            temporary.display(),
            destination.display()
        ))),
    }
}

fn cleanup_staged(staged: &[(PathBuf, PathBuf)]) {
    for (temporary, _) in staged {
        let _ = fs::remove_file(temporary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tar::{Builder, Header};

    fn target(file_name: &str) -> InstallTarget {
        InstallTarget {
            entry_path: format!("{PACK_GRAMMAR_DIR}{file_name}"),
            file_name: file_name.to_string(),
            languages: BTreeSet::from([String::from("test")]),
        }
    }

    fn manifest(files: &[(&str, &[u8])]) -> LanguagePackManifest {
        LanguagePackManifest {
            schema_version: PACK_SCHEMA_VERSION,
            package_version: PACKAGE_VERSION.to_string(),
            target: BUILD_TARGET.to_string(),
            files: files
                .iter()
                .map(|(path, contents)| LanguagePackFile {
                    path: (*path).to_string(),
                    sha256: hex_encode(Sha256::digest(*contents).as_slice()),
                    size: contents.len() as u64,
                })
                .collect(),
        }
    }

    fn build_pack(
        directory: &Path,
        manifest: &LanguagePackManifest,
        files: &[(&str, &[u8])],
    ) -> PathBuf {
        let path = directory.join("pack.tar.gz");
        let file = File::create(&path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = Builder::new(encoder);
        let data = serde_json::to_vec(manifest).unwrap();
        let mut header = Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, PACK_MANIFEST_NAME, data.as_slice()).unwrap();
        for (name, contents) in files {
            let mut header = Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, *name, *contents).unwrap();
        }
        builder.finish().unwrap();
        path
    }

    #[test]
    fn installs_selected_grammar_and_skips_others() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("grammars");
        fs::create_dir_all(&destination).unwrap();
        let wanted: &[u8] = b"wanted grammar";
        let unwanted: &[u8] = b"unwanted grammar";
        let manifest = manifest(&[
            ("grammars/libtree-sitter-rust.so", wanted),
            ("grammars/libtree-sitter-python.so", unwanted),
        ]);
        let pack = build_pack(
            directory.path(),
            &manifest,
            &[
                ("grammars/libtree-sitter-rust.so", wanted),
                ("grammars/libtree-sitter-python.so", unwanted),
            ],
        );

        install_from_pack(&pack, &[target("libtree-sitter-rust.so")], &destination).unwrap();

        assert_eq!(fs::read(destination.join("libtree-sitter-rust.so")).unwrap(), wanted);
        assert!(!destination.join("libtree-sitter-python.so").exists());
    }

    #[test]
    fn rejects_manifest_version_mismatch() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("grammars");
        fs::create_dir_all(&destination).unwrap();
        let mut manifest = manifest(&[("grammars/libtest.so", b"grammar")]);
        manifest.package_version = String::from("0.0.1");
        let pack = build_pack(directory.path(), &manifest, &[("grammars/libtest.so", b"grammar")]);

        let error = install_from_pack(&pack, &[target("libtest.so")], &destination).unwrap_err();
        assert!(error.to_string().contains("language pack is for ee"));
        assert!(!destination.join("libtest.so").exists());
    }

    #[test]
    fn rejects_checksum_mismatch_without_writing_file() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("grammars");
        fs::create_dir_all(&destination).unwrap();
        let mut manifest = manifest(&[("grammars/libtest.so", b"grammar")]);
        manifest.files[0].sha256 = "0".repeat(64);
        let pack = build_pack(directory.path(), &manifest, &[("grammars/libtest.so", b"grammar")]);

        let error = install_from_pack(&pack, &[target("libtest.so")], &destination).unwrap_err();
        assert!(error.to_string().contains("SHA-256 mismatch"));
        assert!(!destination.join("libtest.so").exists());
    }

    #[test]
    fn rejects_path_traversal_in_manifest() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("grammars");
        fs::create_dir_all(&destination).unwrap();
        let contents = b"grammar";
        let mut manifest = manifest(&[("grammars/libtest.so", contents)]);
        manifest.files[0].path = String::from("grammars/../escape.so");
        let pack = build_pack(directory.path(), &manifest, &[("grammars/libtest.so", contents)]);

        let error = install_from_pack(&pack, &[target("libtest.so")], &destination).unwrap_err();
        assert!(error.to_string().contains("unsafe path"));
        assert!(!destination.join("libtest.so").exists());
    }

    #[test]
    fn parses_checksum_file() {
        let checksum = "a".repeat(64);
        let text = format!("{checksum}  ee-language-pack.tar.gz\n");
        assert_eq!(parse_checksum_text(&text), Some(checksum));
    }

    #[test]
    fn rejects_non_https_urls() {
        let error = ensure_https("http://example.com/pack.tar.gz").unwrap_err();
        assert!(error.to_string().contains("non-HTTPS"));
        assert!(ensure_https("https://example.com/pack.tar.gz").is_ok());
    }
}
