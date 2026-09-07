//! `ee do runtime` report/fetch/build commands.
use super::language_cmd::query_kind_label;
use super::*;

pub(crate) fn read_runtime_probe(path: &Path) -> io::Result<(Option<String>, Option<String>)> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(RUNTIME_REPORT_READ_BYTES).read_to_end(&mut bytes)?;
    let sample = String::from_utf8_lossy(&bytes).into_owned();
    let first_line = sample.lines().next().map(str::to_string);
    Ok((first_line, (!sample.is_empty()).then_some(sample)))
}

pub(crate) fn detection_source_label(source: RuntimeLanguageDetectionSource) -> &'static str {
    match source {
        RuntimeLanguageDetectionSource::Explicit => "explicit",
        RuntimeLanguageDetectionSource::Shebang => "shebang",
        RuntimeLanguageDetectionSource::Glob => "glob",
        RuntimeLanguageDetectionSource::FileType => "file-type",
        RuntimeLanguageDetectionSource::FirstLineRegex => "first-line-regex",
        RuntimeLanguageDetectionSource::ContentRegex => "content-regex",
    }
}

pub(crate) fn grammar_health_label(status: &RuntimeGrammarHealth) -> String {
    match status {
        RuntimeGrammarHealth::Unresolved => String::from("unresolved"),
        RuntimeGrammarHealth::Loaded => String::from("loaded"),
        RuntimeGrammarHealth::Missing => String::from("missing"),
        RuntimeGrammarHealth::Error(error) => format!("error: {error}"),
    }
}

pub(crate) fn query_health_label(status: &RuntimeQueryHealth) -> String {
    match status {
        RuntimeQueryHealth::Unsupported => String::from("unsupported"),
        RuntimeQueryHealth::Missing => String::from("missing"),
        RuntimeQueryHealth::Loaded => String::from("loaded"),
        RuntimeQueryHealth::Error(error) => format!("error: {error}"),
    }
}

pub(crate) fn runtime_operation_exit_code(kind: RuntimeOperationErrorKind) -> i32 {
    match kind {
        RuntimeOperationErrorKind::ConfigMerge => EXIT_RUNTIME_CONFIG_MERGE,
        RuntimeOperationErrorKind::GrammarSource => EXIT_RUNTIME_GRAMMAR_SOURCE,
        RuntimeOperationErrorKind::RuntimeAsset => EXIT_RUNTIME_ASSET,
    }
}

pub(crate) fn runtime_report_exit_code(report: &RuntimeHealthReport) -> i32 {
    if report.language_id.is_none() {
        return EXIT_RUNTIME_CONFIG_MERGE;
    }
    if matches!(
        report.grammar_status,
        RuntimeGrammarHealth::Missing | RuntimeGrammarHealth::Error(_)
    ) {
        return EXIT_RUNTIME_ASSET;
    }
    if report.query_reports.iter().any(|query| {
        matches!(query.status, RuntimeQueryHealth::Missing | RuntimeQueryHealth::Error(_))
    }) {
        return EXIT_RUNTIME_ASSET;
    }
    0
}

pub(crate) fn exit_with_runtime_operation_error(context: &str, error: RuntimeOperationError) -> ! {
    eprintln!("{context}: {error}");
    std::process::exit(runtime_operation_exit_code(error.kind()));
}

pub(crate) fn render_runtime_report(report: &RuntimeHealthReport) -> String {
    let mut out = String::from("ee do runtime\n────────────\n");
    if let Some(requested_language) = &report.requested_language {
        out.push_str(&format!(
            "requested language: {}\n",
            terminal_runtime_language_name(requested_language)
        ));
    }
    if let Some(requested_injection_language) = &report.requested_injection_language {
        out.push_str(&format!("requested injection language: {requested_injection_language}\n"));
    }
    if let Some(file_path) = &report.file_path {
        out.push_str(&format!("file: {}\n", file_path.display()));
    }

    match &report.injection_match {
        Some(injection_match) => out.push_str(&format!(
            "injection language: {} [{}]\n",
            terminal_runtime_language_name(&injection_match.display_name),
            terminal_runtime_language_name(&injection_match.canonical_id)
        )),
        None if report.requested_injection_language.is_some() => {
            out.push_str("injection language: <none>\n")
        }
        None => {}
    }

    match (&report.language_id, &report.display_name, report.detection_source) {
        (Some(language_id), Some(display_name), Some(source)) => {
            out.push_str(&format!(
                "resolved language: {} [{}] via {}\n",
                terminal_runtime_language_name(display_name),
                terminal_runtime_language_name(language_id),
                detection_source_label(source)
            ));
        }
        _ => out.push_str("resolved language: <none>\n"),
    }

    out.push_str("runtime roots:\n");
    out.push_str(&format!("  bundled: {}\n", report.runtime_roots.bundled_root().display()));
    out.push_str(&format!("  user: {}\n", report.runtime_roots.user_root().display()));
    match report.runtime_roots.workspace_root() {
        Some(root) => out.push_str(&format!("  workspace: {}\n", root.display())),
        None => out.push_str("  workspace: <disabled>\n"),
    }

    if let Some(asset_source) = report.asset_source {
        out.push_str(&format!("asset source: {:?}\n", asset_source));
    }
    if let Some(root) = &report.effective_runtime_root {
        out.push_str(&format!("effective runtime root: {}\n", root.display()));
    }
    if let Some(grammar_path) = &report.grammar_path {
        out.push_str(&format!("grammar path: {}\n", grammar_path.display()));
    }
    out.push_str(&format!("grammar: {}\n", grammar_health_label(&report.grammar_status)));
    out.push_str("queries:\n");
    for query_report in &report.query_reports {
        out.push_str(&format!(
            "  {:<11} {}",
            query_kind_label(query_report.kind),
            query_health_label(&query_report.status)
        ));
        if !query_report.source_paths.is_empty() {
            let joined = query_report
                .source_paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!(" [{}]", joined));
        }
        out.push('\n');
    }

    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EffectiveRuntimeLanguageRow {
    pub(crate) canonical_id: String,
    pub(crate) display_name: String,
    pub(crate) asset_source: String,
    pub(crate) fetch_status: String,
    pub(crate) grammar_status: String,
    pub(crate) query_status: String,
    pub(crate) file_types: Vec<String>,
    pub(crate) globs: Vec<String>,
    pub(crate) shebangs: Vec<String>,
    pub(crate) query_language: String,
    pub(crate) scope: Option<String>,
    pub(crate) grammar_library: Option<String>,
    pub(crate) grammar_symbol: Option<String>,
    pub(crate) grammar_source: Option<String>,
    pub(crate) injection_regex: Option<String>,
    pub(crate) match_priority: i32,
}

pub(crate) fn summarize_query_status(report: &RuntimeHealthReport) -> String {
    let loaded = report
        .query_reports
        .iter()
        .filter(|query| matches!(query.status, RuntimeQueryHealth::Loaded))
        .count();
    let missing = report
        .query_reports
        .iter()
        .filter(|query| matches!(query.status, RuntimeQueryHealth::Missing))
        .count();
    let unsupported = report
        .query_reports
        .iter()
        .filter(|query| matches!(query.status, RuntimeQueryHealth::Unsupported))
        .count();
    let errored = report
        .query_reports
        .iter()
        .filter(|query| matches!(query.status, RuntimeQueryHealth::Error(_)))
        .count();
    format!("loaded {loaded}, missing {missing}, unsupported {unsupported}, errors {errored}")
}

pub(crate) fn language_fetch_status(
    asset_source: &str,
    staged_source_dir: Option<&Path>,
) -> String {
    if asset_source == "Bundled" {
        return String::from("bundled");
    }
    match staged_source_dir {
        Some(path) if path.exists() => format!("fetched ({})", path.display()),
        Some(path) => format!("missing ({})", path.display()),
        None => String::from("not fetchable"),
    }
}

pub(crate) fn grammar_source_summary(source: Option<&RuntimeGrammarSource>) -> Option<String> {
    match source {
        Some(RuntimeGrammarSource::Crate(source)) => {
            Some(format!("crate {}@{}", source.name, source.version))
        }
        Some(RuntimeGrammarSource::Git(source)) => {
            let pin = source
                .branch
                .as_deref()
                .map(|branch| format!("branch {branch}"))
                .or_else(|| source.tag.as_deref().map(|tag| format!("tag {tag}")))
                .or_else(|| source.rev.as_deref().map(|rev| format!("rev {rev}")))
                .unwrap_or_else(|| String::from("unresolved pin"));
            Some(format!("git {pin}"))
        }
        None => None,
    }
}

pub(crate) fn effective_runtime_language_rows() -> Vec<EffectiveRuntimeLanguageRow> {
    with_default_runtime_loader_mut(|loader| {
        let language_ids = loader
            .languages()
            .map(|language| language.canonical_id().to_string())
            .collect::<Vec<_>>();

        language_ids
            .into_iter()
            .filter_map(|language_id| {
                let report =
                    loader.runtime_health_report(Some(&language_id), None, None, None, None);
                let language = loader.language_for_name(&language_id)?;
                let asset_source = format!("{:?}", language.asset_source());
                Some(EffectiveRuntimeLanguageRow {
                    canonical_id: language.canonical_id().to_string(),
                    display_name: language.display_name().to_string(),
                    asset_source: asset_source.clone(),
                    fetch_status: language_fetch_status(
                        &asset_source,
                        language.staged_source_dir(loader.runtime_roots()).as_deref(),
                    ),
                    grammar_status: grammar_health_label(&report.grammar_status),
                    query_status: summarize_query_status(&report),
                    file_types: language.file_types().to_vec(),
                    globs: language.globs().to_vec(),
                    shebangs: language.shebangs().to_vec(),
                    query_language: language.query_language().to_string(),
                    scope: language.scope().map(str::to_string),
                    grammar_library: language.grammar_library_name().map(str::to_string),
                    grammar_symbol: language.grammar_symbol_name().map(str::to_string),
                    grammar_source: grammar_source_summary(language.grammar_source()),
                    injection_regex: language.injection_regex().map(str::to_string),
                    match_priority: language.match_priority(),
                })
            })
            .collect()
    })
}

pub(crate) fn render_runtime_languages_report(
    title: &str,
    rows: &[EffectiveRuntimeLanguageRow],
) -> String {
    let mut out = format!("{title}\n{}\n", "─".repeat(title.chars().count()));
    if rows.is_empty() {
        out.push_str("<no runtime languages loaded>\n");
        return out;
    }

    for row in rows {
        out.push_str(&format!(
            "{} [{}]\n",
            terminal_runtime_language_name(&row.display_name),
            terminal_runtime_language_name(&row.canonical_id)
        ));
        out.push_str(&format!("  source: {}\n", row.asset_source));
        out.push_str(&format!("  fetch status: {}\n", row.fetch_status));
        out.push_str(&format!("  grammar status: {}\n", row.grammar_status));
        out.push_str(&format!("  queries: {}\n", row.query_status));
        out.push_str(&format!("  file types: {}\n", row.file_types.join(", ")));
        if !row.globs.is_empty() {
            out.push_str(&format!("  globs: {}\n", row.globs.join(", ")));
        }
        if !row.shebangs.is_empty() {
            out.push_str(&format!("  shebangs: {}\n", row.shebangs.join(", ")));
        }
        out.push_str(&format!(
            "  query language: {}\n",
            terminal_runtime_language_name(&row.query_language)
        ));
        if let Some(scope) = &row.scope {
            out.push_str(&format!("  scope: {scope}\n"));
        }
        if let Some(grammar_library) = &row.grammar_library {
            out.push_str(&format!("  grammar library: {grammar_library}\n"));
        }
        if let Some(grammar_symbol) = &row.grammar_symbol {
            out.push_str(&format!("  grammar symbol: {grammar_symbol}\n"));
        }
        if let Some(grammar_source) = &row.grammar_source {
            out.push_str(&format!("  grammar source: {grammar_source}\n"));
        }
        if let Some(injection_regex) = &row.injection_regex {
            out.push_str(&format!("  injection regex: {injection_regex}\n"));
        }
        out.push_str(&format!("  match priority: {}\n", row.match_priority));
    }

    out
}

pub(crate) fn cmd_runtime(
    file_path: Option<&Path>,
    explicit_language: Option<&str>,
    injection_language: Option<&str>,
) {
    if let Err(error) = config::configure_runtime_loader_for_file(file_path, true) {
        eprintln!("runtime config failed: {error}");
        std::process::exit(EXIT_RUNTIME_CONFIG_MERGE);
    }
    let (first_line, content) = match file_path {
        Some(path) => match read_runtime_probe(path) {
            Ok(probe) => probe,
            Err(error) => {
                eprintln!("Cannot inspect runtime inputs for {}: {error}", path.display());
                std::process::exit(1);
            }
        },
        None => (None, None),
    };

    let report = with_default_runtime_loader_mut(|loader| {
        loader.runtime_health_report(
            explicit_language,
            file_path,
            first_line.as_deref(),
            content.as_deref(),
            injection_language,
        )
    });
    print!("{}", render_runtime_report(&report));
    let exit_code = runtime_report_exit_code(&report);
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

pub(crate) fn cmd_runtime_languages(file_path: Option<&Path>) {
    if let Err(error) = config::configure_runtime_loader_for_file(file_path, true) {
        eprintln!("runtime config failed: {error}");
        std::process::exit(EXIT_RUNTIME_CONFIG_MERGE);
    }
    print!(
        "{}",
        render_runtime_languages_report(
            "ee do runtime languages",
            &effective_runtime_language_rows(),
        )
    );
}

pub(crate) fn default_runtime_source_root() -> PathBuf {
    with_default_runtime_loader_mut(|loader| loader.default_user_source_root())
}

pub(crate) fn default_runtime_output_root() -> PathBuf {
    with_default_runtime_loader_mut(|loader| loader.runtime_roots().user_root().to_path_buf())
}

pub(crate) fn cmd_runtime_fetch(
    languages: &[String],
    include_all: bool,
    source_root: Option<&Path>,
    force: bool,
    trust_workspace: bool,
) {
    if let Err(error) = config::configure_runtime_loader_for_file(None, trust_workspace) {
        eprintln!("runtime config failed: {error}");
        std::process::exit(EXIT_RUNTIME_CONFIG_MERGE);
    }
    let source_root =
        source_root.map(Path::to_path_buf).unwrap_or_else(default_runtime_source_root);
    let fetched = with_default_runtime_loader_mut(|loader| {
        loader.fetch_grammar_sources(languages, include_all, &source_root, force)
    })
    .unwrap_or_else(|error| exit_with_runtime_operation_error("runtime fetch failed", error));

    println!("fetched {} grammar source trees into {}", fetched.len(), source_root.display());
    for grammar in fetched {
        let revision_suffix =
            grammar.resolved_rev.as_deref().map(|rev| format!(" @ {rev}")).unwrap_or_default();
        println!(
            "  {} -> {} ({}){}",
            terminal_runtime_language_name(&grammar.language_id),
            grammar.source_dir.display(),
            grammar.source_pin,
            revision_suffix
        );
    }
}

pub(crate) fn cmd_runtime_build(
    languages: &[String],
    include_all: bool,
    source_root: Option<&Path>,
    output_root: Option<&Path>,
    force: bool,
    skip_load: bool,
    trust_workspace: bool,
) {
    if let Err(error) = config::configure_runtime_loader_for_file(None, trust_workspace) {
        eprintln!("runtime config failed: {error}");
        std::process::exit(EXIT_RUNTIME_CONFIG_MERGE);
    }
    let source_root =
        source_root.map(Path::to_path_buf).unwrap_or_else(default_runtime_source_root);
    let output_root =
        output_root.map(Path::to_path_buf).unwrap_or_else(default_runtime_output_root);
    let built = with_default_runtime_loader_mut(|loader| {
        loader.build_runtime_assets(
            languages,
            include_all,
            &source_root,
            &output_root,
            force,
            skip_load,
        )
    })
    .unwrap_or_else(|error| exit_with_runtime_operation_error("runtime build failed", error));

    println!("built {} runtime grammars into {}", built.len(), output_root.display());
    for grammar in built {
        let query_summary = if grammar.query_paths.is_empty() {
            String::from("no standard queries copied")
        } else {
            format!("{} query files", grammar.query_paths.len())
        };
        let revision_suffix =
            grammar.resolved_rev.as_deref().map(|rev| format!(", rev {rev}")).unwrap_or_default();
        println!(
            "  {} -> {} ({query_summary}, {}{})",
            terminal_runtime_language_name(&grammar.language_id),
            grammar.grammar_path.display(),
            grammar.source_pin,
            revision_suffix
        );
    }
}
