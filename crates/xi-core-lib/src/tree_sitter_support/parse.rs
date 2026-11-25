use super::*;

pub(super) fn parse_tree_with_timeout_and_queries(
    language_name: &str,
    text: &str,
    timeout: Duration,
    warmup: QueryWarmup,
) -> Option<Tree> {
    if timeout.is_zero() {
        return None;
    }

    let language = runtime_language(Some(language_name), None, warmup)?;
    let started = Instant::now();
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;

    let mut progress = |_: &tree_sitter::ParseState| {
        if started.elapsed() > timeout { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
    };
    let bytes = text.as_bytes();
    let mut read = |offset: usize, _: Point| bytes.get(offset..).unwrap_or_default();
    let options = ParseOptions { progress_callback: Some(&mut progress) };
    let tree = parser.parse_with_options(&mut read, None, Some(options))?;
    (started.elapsed() <= timeout).then_some(tree)
}

#[derive(Debug, Clone, Copy)]
pub(super) enum QueryWarmup {
    None,
    Kind(RuntimeQueryKind),
}

pub(super) fn runtime_language(
    language_name: Option<&str>,
    file_path: Option<&Path>,
    warmup: QueryWarmup,
) -> Option<Language> {
    #[cfg(any(test, feature = "test-grammars"))]
    crate::runtime_loader::ensure_default_runtime_loader_has_test_grammars();

    with_default_runtime_loader_mut(|loader| {
        let resolved_language_name =
            resolve_runtime_language_name(loader, language_name, file_path)?;
        match warmup {
            QueryWarmup::None => {}
            QueryWarmup::Kind(kind) => {
                if loader.supports_query_kind(&resolved_language_name, kind)
                    && loader.compile_query_kind(&resolved_language_name, kind).is_err()
                {
                    return None;
                }
            }
        }

        loader.load_language_for_name(&resolved_language_name).ok().map(|handle| handle.language())
    })
}

pub(super) fn resolve_runtime_language_name(
    loader: &RuntimeLoader,
    language_name: Option<&str>,
    file_path: Option<&Path>,
) -> Option<String> {
    language_name.and_then(|language_name| loader.canonical_language_name(language_name)).or_else(
        || {
            file_path.and_then(|path| {
                loader.detect_language(Some(path), None, None).map(|language| language.canonical_id)
            })
        },
    )
}
