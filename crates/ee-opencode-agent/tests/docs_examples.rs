//! Documentation examples are routing contracts.
//!
//! `wiki/opencode-agent.md` tells a user which ids to put in `OPENCODE_MODEL`.
//! This test parses the endpoint/dialect table and the curl-free setup snippet,
//! then requires every documented example to resolve through the same catalog
//! the agent uses, with the documented dialect and endpoint.

use std::fs;
use std::path::PathBuf;

use ee_opencode_agent::routes::{self, OpenCodeDialect, OpenCodeSurface};

/// Documentation page that describes surfaces, dialects, and model ids.
const DOC: &str = "wiki/opencode-agent.md";

fn doc_text() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join(DOC);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

/// Table rows of the endpoint/dialect map, as `(surface, dialect, ids, endpoint)`.
fn dialect_map_rows(text: &str) -> Vec<(String, String, Vec<String>, String)> {
    text.lines()
        .filter(|line| line.starts_with("| `"))
        .map(|line| {
            let cells =
                line.split('|').map(str::trim).filter(|cell| !cell.is_empty()).collect::<Vec<_>>();
            assert_eq!(cells.len(), 4, "dialect map row has four columns: {line}");
            (
                cells[0].to_string(),
                cells[1].to_string(),
                backticked(cells[2]),
                strip_markup(cells[3]),
            )
        })
        .collect()
}

/// Extracts `` `id` `` spans from one table cell.
fn backticked(cell: &str) -> Vec<String> {
    cell.split('`').skip(1).step_by(2).map(str::to_string).collect()
}

fn strip_markup(cell: &str) -> String {
    cell.trim_matches(|character| character == '`').to_string()
}

#[test]
fn documented_dialect_map_matches_the_catalog_exactly() {
    let rows = dialect_map_rows(&doc_text());

    assert_eq!(rows.len(), 6, "both surfaces document all three dialects");
    for (surface, dialect, ids, endpoint) in rows {
        let surface = OpenCodeSurface::parse(strip_markup(&surface).as_str())
            .expect("documented surface parses");
        let dialect_cell = strip_markup(&dialect);
        assert!(!ids.is_empty(), "{surface:?} {dialect_cell} names representative ids");

        for id in &ids {
            let route = routes::resolve_route(surface, id)
                .unwrap_or_else(|error| panic!("documented example `{id}` must route: {error}"));

            assert_eq!(
                dialect_name(route.dialect),
                dialect_cell,
                "`{id}` is documented under {dialect_cell} but the catalog says {}",
                dialect_name(route.dialect)
            );
            assert_eq!(route.endpoint, endpoint, "`{id}` endpoint drifted from the docs");
        }
    }
}

#[test]
fn documented_setup_snippet_uses_an_exact_supported_route() {
    let text = doc_text();
    let model = documented_value(&text, "OPENCODE_MODEL").expect("setup snippet names a model");
    let surface =
        documented_value(&text, "OPENCODE_SURFACE").expect("setup snippet names a surface");

    let surface = OpenCodeSurface::parse(&surface).expect("documented surface parses");
    routes::resolve_route(surface, &model)
        .unwrap_or_else(|error| panic!("setup snippet `{model}` must route: {error}"));
}

#[test]
fn documented_roots_are_the_catalog_roots() {
    let text = doc_text();

    for root in [routes::ZEN_API_ROOT, routes::GO_API_ROOT] {
        assert!(text.contains(root), "documentation must show {root}");
    }
    for path in [
        OpenCodeDialect::OpenAiResponses.path(),
        OpenCodeDialect::AnthropicMessages.path(),
        OpenCodeDialect::OpenAiChatCompletions.path(),
    ] {
        assert!(
            text.contains(&format!("{}{path}", routes::ZEN_API_ROOT)),
            "documentation must show the zen {path} endpoint"
        );
    }
}

#[test]
fn documented_exclusions_and_secret_rules_stay_documented() {
    let text = doc_text();

    for required in [
        "Google Generative Language",
        "no request is sent",
        "opencode/gpt-5.5",
        "OPENCODE_API_KEY",
        "Authorization: Bearer",
        "--discover-models",
        "static-catalog-only",
        "never by model-name prefix",
        "OPENCODE_API_URL",
    ] {
        assert!(text.contains(required), "documentation must state `{required}`");
    }
}

fn dialect_name(dialect: OpenCodeDialect) -> String {
    match dialect {
        OpenCodeDialect::OpenAiResponses => String::from("OpenAI Responses"),
        OpenCodeDialect::AnthropicMessages => String::from("Anthropic Messages"),
        OpenCodeDialect::OpenAiChatCompletions => {
            String::from("OpenAI-compatible Chat Completions")
        }
    }
}

/// Reads `KEY = "value"` (or `KEY: value`) from the first snippet mentioning it.
fn documented_value(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find(|line| line.contains(key) && line.contains('='))
        .and_then(|line| line.split_once('='))
        .map(|(_, value)| value.trim().trim_matches(|c| c == '"' || c == '`').to_string())
}
