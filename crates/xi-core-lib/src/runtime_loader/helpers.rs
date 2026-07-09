use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use super::types::{GrammarHandle, RuntimeGrammarSource, RuntimeOperationError};
use globset::Glob;
use regex::Regex;

pub fn normalize_lookup_key(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|ch| !matches!(ch, ' ' | '_' | '-'))
        .flat_map(char::to_lowercase)
        .collect()
}

pub fn runtime_query_dir_name(language_name: &str) -> String {
    match normalize_lookup_key(language_name).as_str() {
        "c#" => String::from("csharp"),
        "c++" => String::from("cpp"),
        normalized => normalized.to_string(),
    }
}

pub fn metadata_modified_time(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|metadata| metadata.modified()).ok()
}

pub fn current_source_mtimes(paths: &[PathBuf]) -> Vec<Option<SystemTime>> {
    paths.iter().map(|path| metadata_modified_time(path)).collect()
}

pub fn grammar_handle_is_fresh(handle: &GrammarHandle) -> bool {
    metadata_modified_time(handle.canonical_library_path()) == handle.modified_time()
}

pub fn canonicalize_or_original(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

pub fn shared_library_filename(stem: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{stem}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{stem}.dylib")
    } else {
        format!("lib{stem}.so")
    }
}

pub fn effective_host_triple() -> Result<String, Box<dyn Error + Send + Sync>> {
    env::var("HOST")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(Ok)
        .unwrap_or_else(detect_rustc_host_triple)
}

pub fn effective_target_triple(host_triple: &str) -> Result<String, Box<dyn Error + Send + Sync>> {
    Ok(env::var("TARGET")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| host_triple.to_string()))
}

pub fn detect_rustc_host_triple() -> Result<String, Box<dyn Error + Send + Sync>> {
    let rustc = env::var("RUSTC").unwrap_or_else(|_| String::from("rustc"));
    let output = Command::new(&rustc)
        .arg("-vV")
        .output()
        .map_err(|error| format!("failed starting `{rustc} -vV` to detect host target: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "`{rustc} -vV` exited with status {} while detecting host target:\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|error| format!("`{rustc} -vV` emitted non-utf8 output: {error}"))?;
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("`{rustc} -vV` did not report host target").into())
}

pub fn regex_matches(pattern: &str, text: &str) -> bool {
    Regex::new(pattern).ok().is_some_and(|regex| regex.is_match(text))
}

pub fn path_matches_glob(path: &Path, glob: &str) -> bool {
    let Ok(glob) = Glob::new(glob) else {
        return false;
    };
    let matcher = glob.compile_matcher();
    matcher.is_match(path)
        || path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matcher.is_match(name))
}

pub fn shebang_matches(marker: &str, first_line: &str) -> bool {
    if marker.starts_with("#!") {
        first_line.starts_with(marker)
    } else {
        first_line.contains(marker)
    }
}

pub fn default_symbol_name(grammar_id: &str) -> String {
    format!("tree_sitter_{}", grammar_id.replace('-', "_"))
}

pub fn validate_runtime_grammar_source(
    language_id: &str,
    source: &RuntimeGrammarSource,
) -> Result<(), String> {
    match source {
        RuntimeGrammarSource::Crate(source) => {
            if source.name.trim().is_empty() {
                return Err(format!(
                    "runtime language `{language_id}` has empty grammar.source.crate.name"
                ));
            }
            if source.version.trim().is_empty() {
                return Err(format!(
                    "runtime language `{language_id}` has empty grammar.source.crate.version"
                ));
            }
        }
        RuntimeGrammarSource::Git(source) => {
            if source.url.trim().is_empty() {
                return Err(format!(
                    "runtime language `{language_id}` has empty grammar.source.git.url"
                ));
            }
            let ref_count = usize::from(source.branch.is_some())
                + usize::from(source.tag.is_some())
                + usize::from(source.rev.is_some());
            if ref_count != 1 {
                return Err(format!(
                    "runtime language `{language_id}` must set exactly one of grammar.source.git.branch, tag, or rev"
                ));
            }
        }
    }
    Ok(())
}

pub fn workspace_root_from_current_dir() -> Option<PathBuf> {
    let mut current = env::current_dir().ok()?;
    loop {
        if current.join("Cargo.toml").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

pub fn cargo_registry_src_root() -> Result<PathBuf, RuntimeOperationError> {
    let cargo_home = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".cargo")))
        .ok_or_else(|| {
            RuntimeOperationError::grammar_source(
                "unable to determine cargo home for runtime grammar sources",
            )
        })?;
    Ok(cargo_home.join("registry").join("src"))
}

pub fn sanitize_path_component(value: &str) -> String {
    let mut sanitized = String::new();
    let mut last_was_dash = false;
    for ch in value.trim().chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            sanitized.push(ch);
            last_was_dash = false;
        } else if !last_was_dash {
            sanitized.push('-');
            last_was_dash = true;
        }
    }
    sanitized.trim_matches('-').to_string()
}

pub fn redact_git_url_credentials(url: &str) -> String {
    let Some(scheme_index) = url.find("://") else {
        return url.to_string();
    };
    let authority_start = scheme_index + 3;
    let authority_end = url[authority_start..]
        .find(['/', '?', '#'])
        .map(|index| authority_start + index)
        .unwrap_or(url.len());
    let authority = &url[authority_start..authority_end];
    let Some(user_info_end) = authority.rfind('@') else {
        return url.to_string();
    };

    format!(
        "{}{}{}",
        &url[..authority_start],
        &authority[user_info_end + 1..],
        &url[authority_end..]
    )
}

pub fn redact_git_command_args(args: &[String]) -> String {
    args.iter().map(|arg| redact_git_url_credentials(arg)).collect::<Vec<_>>().join(" ")
}

pub fn stable_hash_hex(value: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

pub fn copy_dir_recursive(source: &Path, destination: &Path) -> io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_recursive(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &destination_path)?;
        }
    }
    Ok(())
}

pub fn copy_runtime_query_file(
    source_path: &Path,
    destination_path: &Path,
) -> Result<(), RuntimeOperationError> {
    fs::copy(source_path, destination_path).map_err(|error| {
        RuntimeOperationError::runtime_asset(format!(
            "failed copying query {} to {}: {error}",
            source_path.display(),
            destination_path.display()
        ))
    })?;
    Ok(())
}

pub fn runtime_output_query_dir(
    output_root: &Path,
    language: &super::types::RuntimeLanguage,
) -> PathBuf {
    output_root
        .join(super::types::QUERIES_DIR_NAME)
        .join(runtime_query_dir_name(language.query_language()))
}

pub fn bundled_repo_runtime_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("runtime")
}

pub fn looks_like_runtime_grammar_source(path: &Path) -> bool {
    if path.join("tree-sitter.json").exists() || path.join("src").join("parser.c").exists() {
        return true;
    }

    fs::read_dir(path)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .map(|entry| entry.path())
        .filter(|entry_path| entry_path.is_dir())
        .any(|entry_path| entry_path.join("src").join("parser.c").exists())
}

pub fn runtime_loading_disabled_reason() -> Option<&'static str> {
    runtime_loading_disabled_reason_for(cfg!(any(
        target_os = "linux",
        target_os = "macos",
        windows
    )))
}

pub fn runtime_loading_disabled_reason_for(runtime_supported: bool) -> Option<&'static str> {
    (!runtime_supported).then_some(
        "shared-library runtime grammars are only supported on Linux, macOS, and Windows",
    )
}

pub fn resolve_bundled_runtime_root(
    env_override: Option<&Path>,
    exe_path: Option<&Path>,
    fallback_dir: &Path,
    windows_layout: bool,
) -> PathBuf {
    if let Some(path) = env_override {
        return path.to_path_buf();
    }
    if let Some(exe) = exe_path {
        if windows_layout {
            if let Some(parent) = exe.parent() {
                return parent.join("runtime");
            }
        } else if let Some(bin_dir) = exe.parent() {
            if let Some(prefix) = bin_dir.parent() {
                return prefix.join("share").join(super::types::RUNTIME_DIR_NAME);
            }
        }
    }
    fallback_dir.join("runtime")
}

fn source_tree_runtime_root_from_manifest() -> Option<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .map(|dir| dir.join(super::types::RUNTIME_DIR_NAME))
        .find(|path| path.exists())
}

pub(crate) fn resolve_existing_bundled_runtime_root(
    env_override: Option<&Path>,
    exe_path: Option<&Path>,
    fallback_dir: &Path,
    source_tree_root: Option<&Path>,
    windows_layout: bool,
) -> PathBuf {
    if let Some(path) = env_override {
        return path.to_path_buf();
    }

    let release_layout = resolve_bundled_runtime_root(None, exe_path, fallback_dir, windows_layout);
    if exe_path.is_some() && release_layout.exists() {
        return release_layout;
    }

    if let Some(path) = source_tree_root.filter(|path| path.exists()) {
        return path.to_path_buf();
    }

    let fallback_runtime = fallback_dir.join(super::types::RUNTIME_DIR_NAME);
    if fallback_runtime.exists() {
        return fallback_runtime;
    }

    release_layout
}

pub fn bundled_runtime_root_from_env() -> PathBuf {
    let env_override = env::var_os("EE_RUNTIME_DIR").map(PathBuf::from);
    let exe_path = env::current_exe().ok();
    let fallback_dir = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let source_tree_root = source_tree_runtime_root_from_manifest();
    resolve_existing_bundled_runtime_root(
        env_override.as_deref(),
        exe_path.as_deref(),
        &fallback_dir,
        source_tree_root.as_deref(),
        cfg!(windows),
    )
}
