use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use semver::Version;
use serde::Deserialize;
use tree_sitter_loader::Loader;

use super::grammar_layout::candidate_dir_matches_language;
use super::helpers::{
    bundled_repo_runtime_root, cargo_registry_src_root, copy_runtime_query_file,
    default_symbol_name, effective_host_triple, effective_target_triple,
    looks_like_runtime_grammar_source, normalize_lookup_key, redact_git_command_args,
    redact_git_url_credentials, runtime_output_query_dir, runtime_query_dir_name,
    sanitize_path_component, stable_hash_hex, workspace_root_from_current_dir,
};
use super::types::{
    GrammarCrateSpec, GrammarFetchPlan, GrammarGitSpec, QUERIES_DIR_NAME, RuntimeGrammarSource,
    RuntimeLanguage, RuntimeOperationError, RuntimeQueryKind, RuntimeStandardQueryPaths,
};

// ---------------------------------------------------------------------------
// GrammarFetchPlan impl
// ---------------------------------------------------------------------------
impl GrammarFetchPlan {
    pub fn crate_name(&self, language: &RuntimeLanguage) -> String {
        match self {
            Self::Crate(spec) => spec.crate_name.clone(),
            Self::Git(_) => language
                .grammar_library_name()
                .map(str::to_string)
                .unwrap_or_else(|| language.canonical_id().to_string()),
        }
    }

    pub fn source_pin(&self) -> String {
        match self {
            Self::Crate(spec) => {
                format!("crate:{}@{}", spec.crate_name, spec.version.as_deref().unwrap_or("*"))
            }
            Self::Git(spec) => match (&spec.branch, &spec.tag, &spec.rev) {
                (Some(branch), None, None) => {
                    format!("git:{}#branch:{}", redact_git_url_credentials(&spec.url), branch)
                }
                (None, Some(tag), None) => {
                    format!("git:{}#tag:{}", redact_git_url_credentials(&spec.url), tag)
                }
                (None, None, Some(rev)) => {
                    format!("git:{}#rev:{}", redact_git_url_credentials(&spec.url), rev)
                }
                _ => String::from("git:invalid"),
            },
        }
    }

    pub fn source_type(&self) -> &'static str {
        match self {
            Self::Crate(_) => "crate",
            Self::Git(_) => "git",
        }
    }

    pub fn reference_summary(&self) -> String {
        match self {
            Self::Crate(spec) => format!(
                "crate `{}` version `{}`",
                spec.crate_name,
                spec.version.as_deref().unwrap_or("*")
            ),
            Self::Git(spec) => {
                let url = redact_git_url_credentials(&spec.url);
                match (&spec.branch, &spec.tag, &spec.rev) {
                    (Some(branch), None, None) => format!("url `{url}` ref branch `{branch}`"),
                    (None, Some(tag), None) => format!("url `{url}` ref tag `{tag}`"),
                    (None, None, Some(rev)) => format!("url `{url}` ref rev `{rev}`"),
                    _ => format!("url `{url}` ref invalid"),
                }
            }
        }
    }

    pub fn diagnostic_summary(&self, language_id: &str) -> String {
        format!(
            "language `{language_id}` {} source {}",
            self.source_type(),
            self.reference_summary()
        )
    }

    pub fn stage_dir_name(&self, language: &RuntimeLanguage) -> String {
        let language_id = sanitize_path_component(language.canonical_id());
        match self {
            Self::Crate(spec) => format!(
                "{language_id}-crate-{}-{}",
                sanitize_path_component(&spec.crate_name),
                sanitize_path_component(spec.version.as_deref().unwrap_or("unlocked"))
            ),
            Self::Git(spec) => {
                let (ref_kind, ref_value) = match (&spec.branch, &spec.tag, &spec.rev) {
                    (Some(branch), None, None) => ("branch", branch.as_str()),
                    (None, Some(tag), None) => ("tag", tag.as_str()),
                    (None, None, Some(rev)) => ("rev", rev.as_str()),
                    _ => ("invalid", "invalid"),
                };
                format!(
                    "{language_id}-git-{ref_kind}-{}-{}",
                    sanitize_path_component(ref_value),
                    stable_hash_hex(&spec.url)
                )
            }
        }
    }
}

pub fn grammar_fetch_plan_for_language(
    language: &RuntimeLanguage,
) -> Result<GrammarFetchPlan, RuntimeOperationError> {
    match language.grammar_source() {
        Some(RuntimeGrammarSource::Crate(source)) => {
            Ok(GrammarFetchPlan::Crate(GrammarCrateSpec {
                crate_name: source.name.clone(),
                version: Some(source.version.clone()),
            }))
        }
        Some(RuntimeGrammarSource::Git(source)) => Ok(GrammarFetchPlan::Git(GrammarGitSpec {
            url: source.url.clone(),
            branch: source.branch.clone(),
            tag: source.tag.clone(),
            rev: source.rev.clone(),
        })),
        None => {
            let crate_name = language.grammar_library_name().ok_or_else(|| {
                RuntimeOperationError::config_merge(format!(
                    "language `{}` has no configured grammar package",
                    language.canonical_id()
                ))
            })?;
            Ok(GrammarFetchPlan::Crate(GrammarCrateSpec {
                crate_name: crate_name.to_string(),
                version: language.grammar_crate_version().map(str::to_string),
            }))
        }
    }
}

// ---------------------------------------------------------------------------
// Compilation helpers
// ---------------------------------------------------------------------------
pub fn compile_runtime_grammar(
    builder: &Loader,
    build_source_dir: &Path,
    grammar_path: &Path,
    skip_load: bool,
    canonical_id: &str,
) -> Result<(), RuntimeOperationError> {
    if skip_load {
        compile_parser_shared_library(build_source_dir, grammar_path).map_err(|error| {
            RuntimeOperationError::grammar_source(format!(
                "failed building grammar `{canonical_id}` from {}: {error}",
                build_source_dir.display()
            ))
        })
    } else {
        builder.compile_parser_at_path(build_source_dir, grammar_path.to_path_buf(), &[]).map_err(
            |error| {
                RuntimeOperationError::grammar_source(format!(
                    "failed building grammar `{canonical_id}` from {}: {error}",
                    build_source_dir.display()
                ))
            },
        )
    }
}

pub fn validate_built_grammar_symbol(
    grammar_path: &Path,
    language: &RuntimeLanguage,
) -> Result<(), RuntimeOperationError> {
    let symbol_name = language
        .grammar_symbol_name()
        .map(str::to_owned)
        .unwrap_or_else(|| default_symbol_name(language.grammar_id()));
    Loader::load_language(grammar_path, &symbol_name).map_err(|error| {
        RuntimeOperationError::runtime_asset(format!(
            "failed validating built grammar `{}` at {} with symbol `{symbol_name}`: {error}",
            language.canonical_id(),
            grammar_path.display()
        ))
    })?;
    Ok(())
}

pub fn compile_parser_shared_library(
    grammar_path: &Path,
    output_path: &Path,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let src_path = grammar_path.join("src");
    let parser_path = src_path.join("parser.c");
    if !parser_path.exists() {
        return Err(format!("missing parser source {}", parser_path.display()).into());
    }

    let mut cc_config = cc::Build::new();
    let host_triple = effective_host_triple()?;
    let target_triple = effective_target_triple(&host_triple)?;
    cc_config
        .cargo_metadata(false)
        .cargo_warnings(false)
        .debug(false)
        .opt_level(2)
        .extra_warnings(false)
        .host(&host_triple)
        .target(&target_triple)
        .file(&parser_path)
        .include(&src_path)
        .std("c11");

    let scanner_path = src_path.join("scanner.c");
    if scanner_path.exists() {
        cc_config.file(&scanner_path);
    }

    let compiler = cc_config.get_compiler();
    let mut command = Command::new(compiler.path());
    command.args(compiler.args());
    for (key, value) in compiler.env() {
        command.env(key, value);
    }

    if compiler.is_like_msvc() {
        command.arg(if cfg!(debug_assertions) { "-LDd" } else { "-LD" });
        command.arg("-utf-8");
    } else {
        command.arg("-Werror=implicit-function-declaration");
        if cfg!(any(target_os = "macos", target_os = "ios")) {
            command.arg("-dynamiclib");
            command.arg("-UTREE_SITTER_REUSE_ALLOCATOR");
        } else {
            command.arg("-shared");
            command.arg("-Wl,--no-undefined");
            #[cfg(target_os = "openbsd")]
            command.arg("-lc");
        }
    }

    command.args(cc_config.get_files());
    command.arg("-o").arg(output_path);

    let output = command.output().map_err(|error| {
        format!(
            "failed starting compiler `{}` for {}: {error}",
            compiler.path().display(),
            output_path.display()
        )
    })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "compiler exited with status {} while building {}:\nstdout:\n{}\nstderr:\n{}",
            output.status,
            output_path.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into())
    }
}

// ---------------------------------------------------------------------------
// Cargo operations
// ---------------------------------------------------------------------------
pub fn cargo_fetch_locked() -> Result<(), RuntimeOperationError> {
    let cargo = env::var("CARGO").unwrap_or_else(|_| String::from("cargo"));
    let mut command = Command::new(cargo);
    command.arg("fetch").arg("--locked");
    if let Some(root) = workspace_root_from_current_dir() {
        command.current_dir(root);
    }
    let status = command.status().map_err(|error| {
        RuntimeOperationError::grammar_source(format!(
            "failed starting `cargo fetch --locked`: {error}"
        ))
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(RuntimeOperationError::grammar_source(format!(
            "`cargo fetch --locked` exited with status {status}"
        )))
    }
}

pub fn dedupe_grammar_crate_specs<'a>(
    specs: impl IntoIterator<Item = &'a GrammarCrateSpec>,
) -> Result<Vec<GrammarCrateSpec>, RuntimeOperationError> {
    let mut deduped = BTreeMap::<String, Option<String>>::new();
    for spec in specs {
        match deduped.entry(spec.crate_name.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(spec.version.clone());
            }
            std::collections::btree_map::Entry::Occupied(entry) if entry.get() == &spec.version => {
            }
            std::collections::btree_map::Entry::Occupied(entry) => {
                return Err(RuntimeOperationError::config_merge(format!(
                    "grammar crate `{}` requested with conflicting versions `{}` and `{}`",
                    spec.crate_name,
                    entry.get().as_deref().unwrap_or("<unspecified>"),
                    spec.version.as_deref().unwrap_or("<unspecified>")
                )));
            }
        }
    }
    Ok(deduped
        .into_iter()
        .map(|(crate_name, version)| GrammarCrateSpec { crate_name, version })
        .collect())
}

pub fn locate_grammar_crate_source(
    crate_name: &str,
    version: Option<&str>,
) -> Result<PathBuf, RuntimeOperationError> {
    let registry_root = cargo_registry_src_root()?;
    let prefix = format!("{crate_name}-");
    let mut best_match: Option<(Version, PathBuf)> = None;
    for registry in fs::read_dir(&registry_root).map_err(|error| {
        RuntimeOperationError::grammar_source(format!(
            "failed reading cargo registry source root {}: {error}",
            registry_root.display()
        ))
    })? {
        let registry = registry.map_err(|error| {
            RuntimeOperationError::grammar_source(format!(
                "failed reading cargo registry entry under {}: {error}",
                registry_root.display()
            ))
        })?;
        if !registry.path().is_dir() {
            continue;
        }
        for entry in fs::read_dir(registry.path()).map_err(|error| {
            RuntimeOperationError::grammar_source(format!(
                "failed listing cargo registry directory {}: {error}",
                registry.path().display()
            ))
        })? {
            let entry = entry.map_err(|error| {
                RuntimeOperationError::grammar_source(format!(
                    "failed reading cargo registry crate entry under {}: {error}",
                    registry.path().display()
                ))
            })?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(version_str) = name.strip_prefix(&prefix) else {
                continue;
            };
            if version.is_some_and(|requested| requested != version_str) {
                continue;
            }
            let Ok(version) = Version::parse(version_str) else {
                continue;
            };
            let candidate = path.clone();
            if !looks_like_runtime_grammar_source(&candidate) {
                continue;
            }
            match &best_match {
                Some((best_version, _)) if best_version >= &version => {}
                _ => best_match = Some((version, candidate)),
            }
        }
    }

    best_match.map(|(_, path)| path).ok_or_else(|| match version {
        Some(version) => RuntimeOperationError::grammar_source(format!(
            "cargo registry source for grammar crate `{crate_name}` version `{version}` not found"
        )),
        None => RuntimeOperationError::grammar_source(format!(
            "cargo registry source for grammar crate `{crate_name}` not found"
        )),
    })
}

pub fn cargo_fetch_runtime_crates(specs: &[GrammarCrateSpec]) -> Result<(), RuntimeOperationError> {
    let cargo = env::var("CARGO").unwrap_or_else(|_| String::from("cargo"));
    let manifest_dir = cargo_registry_src_root()?.join("..").join("cache").join("ee-runtime-fetch");
    fs::create_dir_all(&manifest_dir).map_err(|error| {
        RuntimeOperationError::grammar_source(format!(
            "failed creating runtime fetch manifest directory {}: {error}",
            manifest_dir.display()
        ))
    })?;
    fs::create_dir_all(manifest_dir.join("src")).map_err(|error| {
        RuntimeOperationError::grammar_source(format!(
            "failed creating runtime fetch source directory {}: {error}",
            manifest_dir.join("src").display()
        ))
    })?;
    let manifest_path = manifest_dir.join("Cargo.toml");
    let manifest = render_runtime_fetch_manifest(specs);
    fs::write(&manifest_path, manifest).map_err(|error| {
        RuntimeOperationError::grammar_source(format!(
            "failed writing runtime fetch manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    fs::write(manifest_dir.join("src").join("lib.rs"), "pub fn _ee_runtime_fetch() {}\n").map_err(
        |error| {
            RuntimeOperationError::grammar_source(format!(
                "failed writing runtime fetch stub source under {}: {error}",
                manifest_dir.display()
            ))
        },
    )?;

    let mut command = Command::new(cargo);
    command.arg("fetch").arg("--manifest-path").arg(&manifest_path);
    if let Some(root) = workspace_root_from_current_dir() {
        command.current_dir(root);
    }
    let status = command.status().map_err(|error| {
        RuntimeOperationError::grammar_source(format!(
            "failed starting `cargo fetch --manifest-path {}`: {error}",
            manifest_path.display()
        ))
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(RuntimeOperationError::grammar_source(format!(
            "`cargo fetch --manifest-path {}` exited with status {status}",
            manifest_path.display()
        )))
    }
}

pub fn render_runtime_fetch_manifest(specs: &[GrammarCrateSpec]) -> String {
    let mut manifest = String::from(
        "[package]\nname = \"ee-runtime-fetch\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\n",
    );
    for spec in specs {
        if let Some(version) = &spec.version {
            manifest.push_str(&format!("{} = \"={}\"\n", spec.crate_name, version));
        } else {
            manifest.push_str(&format!("{} = \"*\"\n", spec.crate_name));
        }
    }
    manifest
}

// ---------------------------------------------------------------------------
// Git operations
// ---------------------------------------------------------------------------
pub fn fetch_git_grammar_source(
    language_id: &str,
    spec: &GrammarGitSpec,
    source_dir: &Path,
) -> Result<String, RuntimeOperationError> {
    let source_label = GrammarFetchPlan::Git(spec.clone()).diagnostic_summary(language_id);
    if source_dir.exists() {
        if !source_dir.join(".git").exists() {
            return Err(RuntimeOperationError::grammar_source(format!(
                "{source_label}: staged checkout {} exists but is not a git repository",
                source_dir.display()
            )));
        }
    } else {
        run_git(None, ["clone", "--no-checkout", &spec.url, &source_dir.display().to_string()])
            .map_err(|error| {
                RuntimeOperationError::grammar_source(format!("{source_label}: {error}"))
            })?;
    }

    run_git(Some(source_dir), ["remote", "set-url", "origin", &spec.url]).map_err(|error| {
        RuntimeOperationError::grammar_source(format!("{source_label}: {error}"))
    })?;
    run_git(
        Some(source_dir),
        ["fetch", "--tags", "--force", "origin", "+refs/heads/*:refs/remotes/origin/*"],
    )
    .map_err(|error| RuntimeOperationError::grammar_source(format!("{source_label}: {error}")))?;

    let resolved_rev = match (&spec.branch, &spec.tag, &spec.rev) {
        (Some(branch), None, None) => git_rev_parse(
            source_dir,
            &format!("refs/remotes/origin/{branch}^{{commit}}"),
            &format!("branch `{branch}`"),
            &source_label,
        )?,
        (None, Some(tag), None) => git_rev_parse(
            source_dir,
            &format!("refs/tags/{tag}^{{commit}}"),
            &format!("tag `{tag}`"),
            &source_label,
        )?,
        (None, None, Some(rev)) => git_rev_parse(
            source_dir,
            &format!("{rev}^{{commit}}"),
            &format!("rev `{rev}`"),
            &source_label,
        )?,
        _ => {
            return Err(RuntimeOperationError::grammar_source(format!(
                "{source_label}: must set exactly one git ref"
            )));
        }
    };

    run_git(Some(source_dir), ["checkout", "--force", &resolved_rev]).map_err(|error| {
        RuntimeOperationError::grammar_source(format!("{source_label}: {error}"))
    })?;
    Ok(resolved_rev)
}

pub fn git_rev_parse(
    source_dir: &Path,
    revision: &str,
    display_ref: &str,
    source_label: &str,
) -> Result<String, RuntimeOperationError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(source_dir)
        .arg("rev-parse")
        .arg("--verify")
        .arg(revision)
        .output()
        .map_err(|error| {
            RuntimeOperationError::grammar_source(format!(
                "{source_label}: failed starting `git rev-parse` in {}: {error}",
                source_dir.display(),
            ))
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(RuntimeOperationError::grammar_source(format!(
            "{source_label}: missing {display_ref} in {}{}",
            source_dir.display(),
            if stderr.is_empty() { String::new() } else { format!(": {stderr}") }
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn run_git<I, S>(current_dir: Option<&Path>, args: I) -> Result<(), RuntimeOperationError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args = args.into_iter().map(|value| value.as_ref().to_string()).collect::<Vec<_>>();
    let display_args = redact_git_command_args(&args);
    let mut command = Command::new("git");
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
    command.args(&args);
    let output = command.output().map_err(|error| {
        RuntimeOperationError::grammar_source(format!(
            "failed starting `git {}`: {error}",
            display_args
        ))
    })?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(RuntimeOperationError::grammar_source(format!(
        "`git {}` failed{}",
        display_args,
        if stderr.is_empty() { String::new() } else { format!(": {stderr}") }
    )))
}

// ---------------------------------------------------------------------------
// Query file copy operations
// ---------------------------------------------------------------------------
pub fn copy_standard_queries_to_runtime(
    source_dir: &Path,
    output_root: &Path,
    language: &RuntimeLanguage,
) -> Result<Vec<PathBuf>, RuntimeOperationError> {
    let manifest_query_paths = resolve_manifest_standard_query_paths(source_dir, language)?;
    let source_query_dir = source_dir.join("queries");
    let destination_query_dir = runtime_output_query_dir(output_root, language);
    fs::create_dir_all(&destination_query_dir).map_err(|error| {
        RuntimeOperationError::runtime_asset(format!(
            "failed creating query output dir {}: {error}",
            destination_query_dir.display()
        ))
    })?;

    let mut copied = Vec::new();
    for kind in RuntimeQueryKind::STANDARD {
        let source_path = manifest_query_paths
            .for_kind(kind)
            .and_then(|paths| paths.first().cloned())
            .unwrap_or_else(|| source_query_dir.join(kind.file_name()));
        if !source_path.exists() {
            continue;
        }
        let destination_path = destination_query_dir.join(kind.file_name());
        copy_runtime_query_file(&source_path, &destination_path)?;
        copied.push(destination_path);
    }
    Ok(copied)
}

pub fn copy_bundled_standard_queries_to_runtime(
    output_root: &Path,
    language: &RuntimeLanguage,
) -> Result<Vec<PathBuf>, RuntimeOperationError> {
    let source_query_dir = bundled_repo_runtime_root()
        .join(QUERIES_DIR_NAME)
        .join(runtime_query_dir_name(language.query_language()));
    if !source_query_dir.exists() {
        return Ok(Vec::new());
    }

    let destination_query_dir = runtime_output_query_dir(output_root, language);
    fs::create_dir_all(&destination_query_dir).map_err(|error| {
        RuntimeOperationError::runtime_asset(format!(
            "failed creating query output dir {}: {error}",
            destination_query_dir.display()
        ))
    })?;

    let mut copied = Vec::new();
    for kind in RuntimeQueryKind::STANDARD {
        if !language.supported_query_kinds().contains(&kind) {
            continue;
        }

        let source_path = source_query_dir.join(kind.file_name());
        if !source_path.exists() {
            continue;
        }

        let destination_path = destination_query_dir.join(kind.file_name());
        copy_runtime_query_file(&source_path, &destination_path)?;
        copied.push(destination_path);
    }
    Ok(copied)
}

pub fn copy_all_bundled_runtime_queries_to_runtime(
    output_root: &Path,
) -> Result<Vec<PathBuf>, RuntimeOperationError> {
    let bundled_queries_root = bundled_repo_runtime_root().join(QUERIES_DIR_NAME);
    if !bundled_queries_root.exists() {
        return Ok(Vec::new());
    }

    let mut copied = Vec::new();
    for entry in fs::read_dir(&bundled_queries_root).map_err(|error| {
        RuntimeOperationError::runtime_asset(format!(
            "failed reading bundled query root {}: {error}",
            bundled_queries_root.display()
        ))
    })? {
        let entry = entry.map_err(|error| {
            RuntimeOperationError::runtime_asset(format!(
                "failed reading bundled query entry under {}: {error}",
                bundled_queries_root.display()
            ))
        })?;
        let source_query_dir = entry.path();
        if !source_query_dir.is_dir() {
            continue;
        }

        let Some(language_dir_name) = source_query_dir.file_name() else {
            continue;
        };
        let destination_query_dir = output_root.join(QUERIES_DIR_NAME).join(language_dir_name);
        fs::create_dir_all(&destination_query_dir).map_err(|error| {
            RuntimeOperationError::runtime_asset(format!(
                "failed creating query output dir {}: {error}",
                destination_query_dir.display()
            ))
        })?;

        for query_entry in fs::read_dir(&source_query_dir).map_err(|error| {
            RuntimeOperationError::runtime_asset(format!(
                "failed reading bundled query dir {}: {error}",
                source_query_dir.display()
            ))
        })? {
            let query_entry = query_entry.map_err(|error| {
                RuntimeOperationError::runtime_asset(format!(
                    "failed reading bundled query entry under {}: {error}",
                    source_query_dir.display()
                ))
            })?;
            let source_path = query_entry.path();
            if !source_path.is_file() {
                continue;
            }
            let Some(file_name) = source_path.file_name() else {
                continue;
            };
            let destination_path = destination_query_dir.join(file_name);
            copy_runtime_query_file(&source_path, &destination_path)?;
            copied.push(destination_path);
        }
    }

    Ok(copied)
}

pub fn copy_bundled_ee_owned_queries_to_runtime(
    output_root: &Path,
    language: &RuntimeLanguage,
) -> Result<Vec<PathBuf>, RuntimeOperationError> {
    let source_query_dir = bundled_repo_runtime_root()
        .join(QUERIES_DIR_NAME)
        .join(runtime_query_dir_name(language.query_language()));
    if !source_query_dir.exists() {
        return Ok(Vec::new());
    }

    let destination_query_dir = runtime_output_query_dir(output_root, language);
    fs::create_dir_all(&destination_query_dir).map_err(|error| {
        RuntimeOperationError::runtime_asset(format!(
            "failed creating query output dir {}: {error}",
            destination_query_dir.display()
        ))
    })?;

    let mut copied = Vec::new();
    for kind in RuntimeQueryKind::EE_OWNED {
        if !language.supported_query_kinds().contains(&kind) {
            continue;
        }

        let source_path = source_query_dir.join(kind.file_name());
        if !source_path.exists() {
            continue;
        }

        let destination_path = destination_query_dir.join(kind.file_name());
        copy_runtime_query_file(&source_path, &destination_path)?;
        copied.push(destination_path);
    }
    Ok(copied)
}

// ---------------------------------------------------------------------------
// Tree-sitter manifest parsing
// ---------------------------------------------------------------------------
#[derive(Debug, Deserialize)]
pub(crate) struct TreeSitterPackageManifest {
    #[serde(default)]
    pub grammars: Vec<TreeSitterPackageGrammar>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum TreeSitterQueryPathSpec {
    Single(String),
    Multiple(Vec<String>),
}

impl TreeSitterQueryPathSpec {
    fn into_paths(self, root: &Path) -> Vec<PathBuf> {
        match self {
            Self::Single(path) => vec![root.join(path)],
            Self::Multiple(paths) => paths.into_iter().map(|path| root.join(path)).collect(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TreeSitterPackageGrammar {
    pub name: String,
    pub path: Option<String>,
    pub highlights: Option<TreeSitterQueryPathSpec>,
    pub injections: Option<TreeSitterQueryPathSpec>,
    pub locals: Option<TreeSitterQueryPathSpec>,
    pub tags: Option<TreeSitterQueryPathSpec>,
}

pub(crate) fn parse_tree_sitter_manifest(
    source_dir: &Path,
) -> Result<Option<TreeSitterPackageManifest>, RuntimeOperationError> {
    let manifest_path = source_dir.join("tree-sitter.json");
    if !manifest_path.exists() {
        return Ok(None);
    }

    let manifest_text = fs::read_to_string(&manifest_path).map_err(|error| {
        RuntimeOperationError::grammar_source(format!(
            "failed reading tree-sitter manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    let manifest = serde_json::from_str(&manifest_text).map_err(|error| {
        RuntimeOperationError::grammar_source(format!(
            "failed parsing tree-sitter manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    Ok(Some(manifest))
}

pub(crate) fn select_manifest_grammar<'a>(
    manifest: &'a TreeSitterPackageManifest,
    language: &RuntimeLanguage,
) -> Option<&'a TreeSitterPackageGrammar> {
    if manifest.grammars.is_empty() {
        return None;
    }

    let target_names = [language.grammar_id(), language.canonical_id(), language.query_language()]
        .into_iter()
        .map(normalize_lookup_key)
        .collect::<Vec<_>>();
    manifest
        .grammars
        .iter()
        .find(|grammar| {
            target_names.iter().any(|target| *target == normalize_lookup_key(&grammar.name))
        })
        .or_else(|| manifest.grammars.first())
}

pub(crate) fn resolve_manifest_standard_query_paths(
    source_dir: &Path,
    language: &RuntimeLanguage,
) -> Result<RuntimeStandardQueryPaths, RuntimeOperationError> {
    let Some(manifest) = parse_tree_sitter_manifest(source_dir)? else {
        return Ok(RuntimeStandardQueryPaths::default());
    };
    let Some(grammar) = select_manifest_grammar(&manifest, language) else {
        return Ok(RuntimeStandardQueryPaths::default());
    };

    let resolve = |path: &Option<TreeSitterQueryPathSpec>| {
        path.clone()
            .map(|path| {
                path.into_paths(source_dir)
                    .into_iter()
                    .filter(|path| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| !name.trim().is_empty())
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|paths| !paths.is_empty())
    };

    Ok(RuntimeStandardQueryPaths {
        highlights: resolve(&grammar.highlights),
        injections: resolve(&grammar.injections),
        locals: resolve(&grammar.locals),
        tags: resolve(&grammar.tags),
    })
}

pub fn resolve_staged_grammar_build_dir(
    source_dir: &Path,
    language: &RuntimeLanguage,
) -> Result<PathBuf, RuntimeOperationError> {
    if source_dir.join("src").join("parser.c").exists() {
        return Ok(source_dir.to_path_buf());
    }

    if let Some(path) = resolve_manifest_grammar_subdir(source_dir, language)? {
        return Ok(path);
    }

    if let Some(path) = resolve_nested_grammar_subdir(source_dir, language)? {
        return Ok(path);
    }

    Err(RuntimeOperationError::grammar_source(format!(
        "failed resolving grammar source directory for `{}` under {}",
        language.canonical_id(),
        source_dir.display()
    )))
}

pub fn resolve_manifest_grammar_subdir(
    source_dir: &Path,
    language: &RuntimeLanguage,
) -> Result<Option<PathBuf>, RuntimeOperationError> {
    let Some(manifest) = parse_tree_sitter_manifest(source_dir)? else {
        return Ok(None);
    };
    if manifest.grammars.is_empty() {
        return Ok(Some(source_dir.to_path_buf()));
    }

    let grammar = select_manifest_grammar(&manifest, language).expect("checked non-empty grammars");

    Ok(Some(
        grammar
            .path
            .as_deref()
            .filter(|path| !path.is_empty() && *path != ".")
            .map(|path| source_dir.join(path))
            .unwrap_or_else(|| source_dir.to_path_buf()),
    ))
}

pub fn resolve_nested_grammar_subdir(
    source_dir: &Path,
    language: &RuntimeLanguage,
) -> Result<Option<PathBuf>, RuntimeOperationError> {
    let candidates = fs::read_dir(source_dir)
        .map_err(|error| {
            RuntimeOperationError::grammar_source(format!(
                "failed listing grammar source root {}: {error}",
                source_dir.display()
            ))
        })?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && path.join("src").join("parser.c").exists())
        .collect::<Vec<_>>();

    if let Some(path) =
        candidates.iter().find(|path| candidate_dir_matches_language(path, language))
    {
        return Ok(Some(path.clone()));
    }

    if candidates.len() == 1 {
        return Ok(candidates.into_iter().next());
    }

    Ok(None)
}
