use std::path::PathBuf;

use jsonrpc_lite::Error as JsonRpcError;
use serde_json::Value;
use xi_core_lib::document_symbols::{MAX_DOCUMENT_SYMBOL_BYTES, document_symbols_for_text};
use xi_core_lib::plugin_rpc::SymbolItem;
use xi_plugin_lib::{ChunkCache, View};

use crate::conversion_utils::{
    symbol_items_from_document_symbols, symbol_items_from_workspace_symbols,
};
use crate::types::LanguageResponseError;

/// Current-buffer snapshot retained until an asynchronous LSP symbol request
/// completes. Tree-sitter fallback never reads stale content from disk.
pub(crate) struct DocumentSymbolContext {
    path: PathBuf,
    language_name: String,
    text: Option<String>,
}

impl DocumentSymbolContext {
    pub(crate) fn capture(view: &mut View<ChunkCache>) -> Option<Self> {
        let path = view.get_path()?.to_path_buf();
        let language_name = view.get_language_id().as_ref().to_owned();
        let text = (view.get_buf_size() <= MAX_DOCUMENT_SYMBOL_BYTES)
            .then(|| view.get_document().ok())
            .flatten();
        Some(Self { path, language_name, text })
    }

    pub(crate) fn path_string(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }

    pub(crate) fn resolve_lsp_response(
        &self,
        response: Result<Value, JsonRpcError>,
    ) -> Vec<SymbolItem> {
        let parsed = response
            .map_err(|error| LanguageResponseError::LanguageServerError(format!("{error:?}")))
            .and_then(|value| self.parse_lsp_value(value));
        prefer_lsp_symbols(parsed, || self.fallback_symbols())
    }

    pub(crate) fn fallback_symbols(&self) -> Vec<SymbolItem> {
        let Some(text) = self.text.as_deref() else {
            return Vec::new();
        };
        document_symbols_for_text(Some(&self.language_name), Some(self.path.as_path()), text)
    }

    fn parse_lsp_value(&self, value: Value) -> Result<Vec<SymbolItem>, LanguageResponseError> {
        if let Ok(Some(symbols)) =
            serde_json::from_value::<Option<Vec<lsp_types::DocumentSymbol>>>(value.clone())
        {
            return symbol_items_from_document_symbols(
                symbols,
                &self.path_string(),
                self.text.as_deref(),
            );
        }

        serde_json::from_value::<Option<Vec<lsp_types::SymbolInformation>>>(value)
            .map_err(|error| LanguageResponseError::Transport(error.to_string()))
            .map(|symbols| symbol_items_from_workspace_symbols(symbols.unwrap_or_default()))
    }
}

fn prefer_lsp_symbols(
    lsp_symbols: Result<Vec<SymbolItem>, LanguageResponseError>,
    fallback: impl FnOnce() -> Vec<SymbolItem>,
) -> Vec<SymbolItem> {
    match lsp_symbols {
        Ok(symbols) if !symbols.is_empty() => symbols,
        Ok(_) | Err(_) => fallback(),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::Path;

    use serde_json::json;

    use super::*;

    fn symbol(name: &str) -> SymbolItem {
        SymbolItem {
            name: name.to_owned(),
            kind: String::from("function"),
            path: String::from("/tmp/example.rs"),
            line: 0,
            column: 0,
            children: Vec::new(),
        }
    }

    #[test]
    fn nonempty_lsp_symbols_win_without_running_fallback() {
        let fallback_called = Cell::new(false);

        let symbols = prefer_lsp_symbols(Ok(vec![symbol("lsp")]), || {
            fallback_called.set(true);
            vec![symbol("fallback")]
        });

        assert_eq!(symbols, vec![symbol("lsp")]);
        assert!(!fallback_called.get());
    }

    #[test]
    fn empty_or_failed_lsp_symbols_use_fallback_once() {
        for lsp_symbols in
            [Ok(Vec::new()), Err(LanguageResponseError::Transport(String::from("malformed")))]
        {
            let fallback_calls = Cell::new(0);
            let symbols = prefer_lsp_symbols(lsp_symbols, || {
                fallback_calls.set(fallback_calls.get() + 1);
                vec![symbol("fallback")]
            });

            assert_eq!(symbols, vec![symbol("fallback")]);
            assert_eq!(fallback_calls.get(), 1);
        }
    }

    #[test]
    fn valid_nonempty_document_symbol_response_wins() {
        let context = DocumentSymbolContext {
            path: Path::new("/tmp/example.rs").to_path_buf(),
            language_name: String::from("unknown"),
            text: Some(String::from("fn server() {}\n")),
        };

        let symbols = context.resolve_lsp_response(Ok(json!([{
            "name": "server",
            "kind": 12,
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 14 }
            },
            "selectionRange": {
                "start": { "line": 0, "character": 3 },
                "end": { "line": 0, "character": 9 }
            }
        }])));

        assert_eq!(
            symbols,
            vec![SymbolItem {
                name: String::from("server"),
                kind: String::from("function"),
                path: String::from("/tmp/example.rs"),
                line: 0,
                column: 3,
                children: Vec::new(),
            }]
        );
    }

    #[test]
    fn null_and_malformed_responses_degrade_to_empty_fallback() {
        let context = DocumentSymbolContext {
            path: Path::new("/tmp/example.unknown").to_path_buf(),
            language_name: String::from("unknown"),
            text: Some(String::from("content\n")),
        };

        assert!(context.resolve_lsp_response(Ok(Value::Null)).is_empty());
        assert!(context.resolve_lsp_response(Ok(json!({ "invalid": true }))).is_empty());
    }

    #[test]
    fn missing_snapshot_has_empty_fallback_instead_of_disk_read() {
        let context = DocumentSymbolContext {
            path: Path::new("/tmp/example.rs").to_path_buf(),
            language_name: String::from("rust"),
            text: None,
        };

        assert!(context.fallback_symbols().is_empty());
    }
}
