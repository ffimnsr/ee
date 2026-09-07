//! Per-tool argument validation and backend dispatch for `tools/call`.
use super::*;
use serde_json::json;

impl EeMcpProxy {
    /// Validates and dispatches one `tools/call` request.
    pub(crate) fn dispatch_tool(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<CallToolResponse, ErrorData> {
        if !self.is_supported(request.name.as_ref()) {
            return Ok(complete(CallToolResult::error(vec![ContentBlock::text(format!(
                "tool '{}' unavailable in this host mode",
                request.name
            ))])));
        }
        enforce_argument_cap(request)?;
        match request.name.as_ref() {
            "ee_tools_manifest" => {
                require_no_arguments(request)?;
                Ok(complete(CallToolResult::structured(json!(self.tools_manifest()))))
            }
            "ee_workspace_roots" => Ok(self
                .backend
                .workspace_roots()
                .map(|roots| complete(CallToolResult::structured(json!(roots))))
                .unwrap_or_else(backend_error_result)),
            "ee_list_directory" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                Ok(self
                    .backend
                    .list_directory(path.to_owned())
                    .map(|listing| complete(CallToolResult::structured(json!(listing))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_list_directory_all" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                Ok(self
                    .backend
                    .list_directory_all(path.to_owned())
                    .map(|listing| complete(CallToolResult::structured(json!(listing))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_search_files" => {
                let arguments = require_arguments(request)?;
                let pattern = require_nonempty_string(arguments, "pattern")?;
                Ok(self
                    .backend
                    .search_files(pattern.to_owned())
                    .map(|matches| complete(CallToolResult::structured(json!(matches))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_search_files_all" => {
                let arguments = require_arguments(request)?;
                let pattern = require_nonempty_string(arguments, "pattern")?;
                Ok(self
                    .backend
                    .search_files_all(pattern.to_owned())
                    .map(|matches| complete(CallToolResult::structured(json!(matches))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_search_text" => {
                let arguments = require_arguments(request)?;
                let query = require_nonempty_string(arguments, "query")?;
                Ok(self
                    .backend
                    .search_text(query.to_owned())
                    .map(|matches| complete(CallToolResult::structured(json!(matches))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_search_text_regex" => {
                let arguments = require_arguments(request)?;
                let pattern = require_nonempty_string(arguments, "pattern")?;
                Ok(self
                    .backend
                    .search_text_regex(pattern.to_owned())
                    .map(|matches| complete(CallToolResult::structured(json!(matches))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_search_text_in_files" => {
                let arguments = require_arguments(request)?;
                let query = require_nonempty_string(arguments, "query")?;
                let file_glob = require_nonempty_string(arguments, "file_glob")?;
                Ok(self
                    .backend
                    .search_text_in_files(query.to_owned(), file_glob.to_owned())
                    .map(|matches| complete(CallToolResult::structured(json!(matches))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_web_search" => {
                let arguments = require_arguments(request)?;
                require_exact_argument_keys(arguments, &["query"])?;
                let query = require_nonempty_string(arguments, "query")?;
                Ok(self
                    .backend
                    .web_search(WebSearchRequest { query: query.to_owned() })
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_fetch_url" => {
                let arguments = require_arguments(request)?;
                require_exact_argument_keys(arguments, &["url"])?;
                let url = require_nonempty_string(arguments, "url")?;
                Ok(self
                    .backend
                    .fetch_url(FetchUrlRequest { url: url.to_owned() })
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_browser_run_content" => {
                let arguments = require_arguments(request)?;
                require_exact_argument_keys(arguments, &["url"])?;
                let url = require_nonempty_string(arguments, "url")?;
                Ok(self
                    .backend
                    .browser_run(BrowserRunRequest {
                        action: BrowserRunAction::Content,
                        url: url.to_owned(),
                        selector: None,
                        prompt: None,
                    })
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_browser_run_screenshot" => {
                let arguments = require_arguments(request)?;
                require_exact_argument_keys(arguments, &["url"])?;
                let url = require_nonempty_string(arguments, "url")?;
                Ok(self
                    .backend
                    .browser_run(BrowserRunRequest {
                        action: BrowserRunAction::Screenshot,
                        url: url.to_owned(),
                        selector: None,
                        prompt: None,
                    })
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_browser_run_markdown" => {
                let arguments = require_arguments(request)?;
                require_exact_argument_keys(arguments, &["url"])?;
                let url = require_nonempty_string(arguments, "url")?;
                Ok(self
                    .backend
                    .browser_run(BrowserRunRequest {
                        action: BrowserRunAction::Markdown,
                        url: url.to_owned(),
                        selector: None,
                        prompt: None,
                    })
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_browser_run_scrape" => {
                let arguments = require_arguments(request)?;
                require_exact_argument_keys(arguments, &["url", "selector"])?;
                let url = require_nonempty_string(arguments, "url")?;
                let selector = require_nonempty_string(arguments, "selector")?;
                Ok(self
                    .backend
                    .browser_run(BrowserRunRequest {
                        action: BrowserRunAction::Scrape,
                        url: url.to_owned(),
                        selector: Some(selector.to_owned()),
                        prompt: None,
                    })
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_browser_run_json" => {
                let arguments = require_arguments(request)?;
                require_exact_argument_keys(arguments, &["url", "prompt"])?;
                let url = require_nonempty_string(arguments, "url")?;
                let prompt = require_nonempty_string(arguments, "prompt")?;
                Ok(self
                    .backend
                    .browser_run(BrowserRunRequest {
                        action: BrowserRunAction::Json,
                        url: url.to_owned(),
                        selector: None,
                        prompt: Some(prompt.to_owned()),
                    })
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_browser_run_links" => {
                let arguments = require_arguments(request)?;
                require_exact_argument_keys(arguments, &["url"])?;
                let url = require_nonempty_string(arguments, "url")?;
                Ok(self
                    .backend
                    .browser_run(BrowserRunRequest {
                        action: BrowserRunAction::Links,
                        url: url.to_owned(),
                        selector: None,
                        prompt: None,
                    })
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_replace_text" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let old_text = require_string(arguments, "old_text")?;
                let new_text = require_string(arguments, "new_text")?;
                Ok(self
                    .backend
                    .replace_text(path.to_owned(), old_text.to_owned(), new_text.to_owned())
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_apply_patch" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let edits = text_edits(arguments, "edits")?;
                Ok(self
                    .backend
                    .apply_patch(path.to_owned(), edits)
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_create_text_file" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let content = require_string(arguments, "content")?;
                Ok(self
                    .backend
                    .create_text_file(path.to_owned(), content.to_owned())
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_overwrite_text_file" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let content = require_string(arguments, "content")?;
                Ok(self
                    .backend
                    .overwrite_text_file(path.to_owned(), content.to_owned())
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_create_directory" | "ee_delete_path" => {
                let arguments = require_arguments(request)?;
                require_exact_argument_keys(arguments, &["path"])?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let result = if request.name == "ee_create_directory" {
                    self.backend.create_directory(path.to_owned())
                } else {
                    self.backend.delete_path(path.to_owned())
                };
                Ok(result
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_copy_path" | "ee_move_path" => {
                let arguments = require_arguments(request)?;
                require_exact_argument_keys(arguments, &["source_path", "destination_path"])?;
                let source_path = require_string(arguments, "source_path")?;
                let destination_path = require_string(arguments, "destination_path")?;
                require_absolute(source_path)?;
                require_absolute(destination_path)?;
                let result = if request.name == "ee_copy_path" {
                    self.backend.copy_path(source_path.to_owned(), destination_path.to_owned())
                } else {
                    self.backend.move_path(source_path.to_owned(), destination_path.to_owned())
                };
                Ok(result
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_read_buffer" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                Ok(self
                    .backend
                    .read_buffer(path.to_owned())
                    .map(|text| complete(CallToolResult::success(vec![ContentBlock::text(text)])))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_read_buffer_lines" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let line = require_positive_u32(arguments, "line")?;
                let limit = require_positive_u32(arguments, "limit")?;
                Ok(self
                    .backend
                    .read_buffer_lines(path.to_owned(), line, limit)
                    .map(|text| complete(CallToolResult::success(vec![ContentBlock::text(text)])))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_open_buffers" => Ok(self
                .backend
                .open_buffers()
                .map(|buffers| complete(CallToolResult::structured(json!(buffers))))
                .unwrap_or_else(backend_error_result)),
            "ee_get_diagnostics" => Ok(self
                .backend
                .get_diagnostics()
                .map(|diagnostics| complete(CallToolResult::structured(json!(diagnostics))))
                .unwrap_or_else(backend_error_result)),
            "ee_get_file_diagnostics" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                Ok(self
                    .backend
                    .get_file_diagnostics(path.to_owned())
                    .map(|diagnostics| complete(CallToolResult::structured(json!(diagnostics))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_document_symbols" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                Ok(self
                    .backend
                    .document_symbols(path.to_owned())
                    .map(|symbols| complete(CallToolResult::structured(json!(symbols))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_references" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let line = require_positive_u32(arguments, "line")?;
                let character = require_positive_u32(arguments, "character")?;
                Ok(self
                    .backend
                    .references(path.to_owned(), line, character)
                    .map(|references| complete(CallToolResult::structured(json!(references))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_list_code_actions" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let line = require_positive_u32(arguments, "line")?;
                let character = require_positive_u32(arguments, "character")?;
                Ok(self
                    .backend
                    .list_code_actions(path.to_owned(), line, character)
                    .map(|actions| complete(CallToolResult::structured(json!(actions))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_apply_code_action" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let action_id = require_nonempty_string(arguments, "action_id")?;
                Ok(self
                    .backend
                    .apply_code_action(path.to_owned(), action_id.to_owned())
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_format_file" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                Ok(self
                    .backend
                    .format_file(path.to_owned())
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_preview_rename_symbol" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let line = require_positive_u32(arguments, "line")?;
                let character = require_positive_u32(arguments, "character")?;
                let new_name = require_nonempty_string(arguments, "new_name")?;
                Ok(self
                    .backend
                    .preview_rename_symbol(path.to_owned(), line, character, new_name.to_owned())
                    .map(|preview| complete(CallToolResult::structured(json!(preview))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_rename_symbol" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let line = require_positive_u32(arguments, "line")?;
                let character = require_positive_u32(arguments, "character")?;
                let new_name = require_nonempty_string(arguments, "new_name")?;
                Ok(self
                    .backend
                    .rename_symbol(path.to_owned(), line, character, new_name.to_owned())
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_read_text_file" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let line = optional_u32(arguments, "line")?;
                let limit = optional_u32(arguments, "limit")?;
                Ok(self
                    .backend
                    .read_text_file(path.to_owned(), line, limit)
                    .map(|text| complete(CallToolResult::success(vec![ContentBlock::text(text)])))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_write_text_file" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let content = require_string(arguments, "content")?;
                Ok(self
                    .backend
                    .write_text_file(path.to_owned(), content.to_owned())
                    .map(|()| {
                        complete(CallToolResult::success(vec![ContentBlock::text(format!(
                            "wrote {} bytes to {path}",
                            content.len()
                        ))]))
                    })
                    .unwrap_or_else(backend_error_result))
            }
            "ee_terminal_create" => {
                let arguments = require_arguments(request)?;
                let command = require_string(arguments, "command")?;
                if command.is_empty() {
                    return Err(ErrorData::invalid_params(
                        "argument 'command' must not be empty",
                        None,
                    ));
                }
                let args = string_array(arguments, "args")?;
                let cwd = match optional_string(arguments, "cwd")? {
                    Some(cwd) => {
                        require_absolute(&cwd)?;
                        Some(cwd)
                    }
                    None => None,
                };
                let env = env_pairs(arguments)?;
                Ok(self
                    .backend
                    .terminal_create(command.to_owned(), args, cwd, env)
                    .map(|id| complete(CallToolResult::success(vec![ContentBlock::text(id)])))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_terminal_output" => {
                let arguments = require_arguments(request)?;
                let terminal_id = require_nonempty_string(arguments, "terminal_id")?;
                Ok(self
                    .backend
                    .terminal_output(terminal_id.to_owned())
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_terminal_output_since" => {
                let arguments = require_arguments(request)?;
                let terminal_id = require_nonempty_string(arguments, "terminal_id")?;
                let since_seq =
                    u64::from(optional_u32(arguments, "since_seq")?.ok_or_else(|| {
                        ErrorData::invalid_params("missing required argument 'since_seq'", None)
                    })?);
                Ok(self
                    .backend
                    .terminal_output_since(terminal_id.to_owned(), since_seq)
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_terminal_wait" => {
                let arguments = require_arguments(request)?;
                let terminal_id = require_nonempty_string(arguments, "terminal_id")?;
                Ok(self
                    .backend
                    .terminal_wait(terminal_id.to_owned())
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_terminal_wait_long" => {
                let arguments = require_arguments(request)?;
                let terminal_id = require_nonempty_string(arguments, "terminal_id")?;
                let timeout_ms = u64::from(require_positive_u32(arguments, "timeout_ms")?);
                Ok(self
                    .backend
                    .terminal_wait_long(terminal_id.to_owned(), timeout_ms)
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_terminal_kill" => {
                let arguments = require_arguments(request)?;
                let terminal_id = require_nonempty_string(arguments, "terminal_id")?;
                Ok(self
                    .backend
                    .terminal_kill(terminal_id.to_owned())
                    .map(|()| {
                        complete(CallToolResult::structured(json!({ "terminalId": terminal_id })))
                    })
                    .unwrap_or_else(backend_error_result))
            }
            "ee_terminal_release" => {
                let arguments = require_arguments(request)?;
                let terminal_id = require_nonempty_string(arguments, "terminal_id")?;
                Ok(self
                    .backend
                    .terminal_release(terminal_id.to_owned())
                    .map(|()| {
                        complete(CallToolResult::structured(json!({ "terminalId": terminal_id })))
                    })
                    .unwrap_or_else(backend_error_result))
            }

            "ee_git_status" => Ok(self
                .backend
                .git_status()
                .map(|result| complete(CallToolResult::structured(json!(result))))
                .unwrap_or_else(backend_error_result)),
            "ee_git_diff" => Ok(self
                .backend
                .git_diff()
                .map(|result| complete(CallToolResult::structured(json!(result))))
                .unwrap_or_else(backend_error_result)),
            "ee_git_diff_staged" => Ok(self
                .backend
                .git_diff_staged()
                .map(|result| complete(CallToolResult::structured(json!(result))))
                .unwrap_or_else(backend_error_result)),
            "ee_git_diff_file" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                Ok(self
                    .backend
                    .git_diff_file(path.to_owned())
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_changed_files" => Ok(self
                .backend
                .changed_files()
                .map(|result| complete(CallToolResult::structured(json!(result))))
                .unwrap_or_else(backend_error_result)),
            "ee_review_context" => Ok(self
                .backend
                .review_context()
                .map(|result| complete(CallToolResult::structured(json!(result))))
                .unwrap_or_else(backend_error_result)),
            "ee_turn_evidence_summary" => {
                let arguments = request.arguments.as_ref();
                if let Some(arguments) = arguments {
                    require_exact_argument_keys(arguments, &["session_id", "turn_id"])?;
                }
                let session_id = arguments
                    .map(|arguments| optional_nonempty_string(arguments, "session_id"))
                    .transpose()?
                    .flatten();
                let turn_id = arguments
                    .map(|arguments| optional_positive_u64(arguments, "turn_id"))
                    .transpose()?
                    .flatten();
                if turn_id.is_some() && session_id.is_none() {
                    return Err(ErrorData::invalid_params(
                        "argument 'session_id' is required when 'turn_id' is specified",
                        None,
                    ));
                }
                Ok(self
                    .backend
                    .turn_evidence_summary(session_id, turn_id)
                    .map(|summary| complete(CallToolResult::structured(summary)))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_project_instructions" => Ok(self
                .backend
                .project_instructions()
                .map(|result| complete(CallToolResult::structured(json!(result))))
                .unwrap_or_else(backend_error_result)),
            "ee_save_note" => {
                let arguments = require_arguments(request)?;
                let key = require_nonempty_string(arguments, "key")?;
                let content = require_nonempty_string(arguments, "content")?;
                Ok(self
                    .backend
                    .save_note(key.to_owned(), content.to_owned())
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_read_notes" => Ok(self
                .backend
                .read_notes()
                .map(|result| complete(CallToolResult::structured(json!(result))))
                .unwrap_or_else(backend_error_result)),
            "ee_read_note" => {
                let arguments = require_arguments(request)?;
                let key = require_nonempty_string(arguments, "key")?;
                Ok(self
                    .backend
                    .read_note(key.to_owned())
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_remember_workspace_fact" => {
                let arguments = require_arguments(request)?;
                require_exact_argument_keys(arguments, &["key", "value"])?;
                let key = require_bounded_nonempty_string(
                    arguments,
                    "key",
                    MAX_WORKSPACE_FACT_KEY_BYTES,
                )?;
                let value = require_bounded_nonempty_string(
                    arguments,
                    "value",
                    MAX_WORKSPACE_FACT_VALUE_BYTES,
                )?;
                Ok(self
                    .backend
                    .remember_workspace_fact(key.to_owned(), value.to_owned())
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_verify_workspace_fact" => {
                let arguments = require_arguments(request)?;
                require_exact_argument_keys(arguments, &["session_id", "turn_id", "key"])?;
                let session_id = require_bounded_nonempty_string(arguments, "session_id", 256)?;
                let turn_id = optional_positive_u64(arguments, "turn_id")?
                    .ok_or_else(|| ErrorData::invalid_params("missing argument 'turn_id'", None))?;
                let key = require_bounded_nonempty_string(
                    arguments,
                    "key",
                    MAX_WORKSPACE_FACT_KEY_BYTES,
                )?;
                Ok(self
                    .backend
                    .verify_workspace_fact(session_id.to_owned(), turn_id, key.to_owned())
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_recall_workspace_facts" => {
                let arguments = require_arguments(request)?;
                require_exact_argument_keys(arguments, &["query"])?;
                let query = require_bounded_nonempty_string(
                    arguments,
                    "query",
                    MAX_WORKSPACE_FACT_QUERY_BYTES,
                )?;
                Ok(self
                    .backend
                    .recall_workspace_facts(query.to_owned())
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_read_workspace_fact" | "ee_forget_workspace_fact" | "ee_retract_workspace_fact" => {
                let arguments = require_arguments(request)?;
                require_exact_argument_keys(arguments, &["key"])?;
                let key = require_bounded_nonempty_string(
                    arguments,
                    "key",
                    MAX_WORKSPACE_FACT_KEY_BYTES,
                )?;
                let result = match request.name.as_ref() {
                    "ee_read_workspace_fact" => {
                        self.backend.read_workspace_fact(key.to_owned()).map(|fact| json!(fact))
                    }
                    "ee_forget_workspace_fact" => self
                        .backend
                        .forget_workspace_fact(key.to_owned())
                        .map(|result| json!(result)),
                    _ => self
                        .backend
                        .retract_workspace_fact(key.to_owned())
                        .map(|result| json!(result)),
                };
                Ok(result
                    .map(|result| complete(CallToolResult::structured(result)))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_list_workspace_facts" => {
                let arguments = require_arguments(request)?;
                require_exact_argument_keys(arguments, &["limit"])?;
                let limit = require_positive_u32(arguments, "limit")?;
                if limit > MAX_WORKSPACE_FACT_LIST_RESULTS {
                    return Err(ErrorData::invalid_params(
                        format!(
                            "argument 'limit' must be at most {MAX_WORKSPACE_FACT_LIST_RESULTS}"
                        ),
                        None,
                    ));
                }
                Ok(self
                    .backend
                    .list_workspace_facts(limit)
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_export_workspace_memory" => {
                let arguments = require_arguments(request)?;
                require_exact_argument_keys(arguments, &["include_values"])?;
                let include_values = require_bool(arguments, "include_values")?;
                Ok(self
                    .backend
                    .export_workspace_memory(include_values)
                    .map(|result| complete(CallToolResult::structured(result)))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_import_workspace_memory" => {
                let arguments = require_arguments(request)?;
                require_exact_argument_keys(arguments, &["export_json"])?;
                let export_json = require_bounded_nonempty_string(
                    arguments,
                    "export_json",
                    MAX_WORKSPACE_MEMORY_IMPORT_BYTES,
                )?;
                if export_json.len() < 2 {
                    return Err(ErrorData::invalid_params(
                        "argument 'export_json' must contain at least 2 UTF-8 bytes",
                        None,
                    ));
                }
                Ok(self
                    .backend
                    .import_workspace_memory(export_json.to_owned())
                    .map(|result| complete(CallToolResult::structured(result)))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_clear_workspace_memory" => {
                require_no_arguments(request)?;
                Ok(self
                    .backend
                    .clear_workspace_memory()
                    .map(|result| complete(CallToolResult::structured(result)))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_file_dependency_map" => {
                let arguments = require_arguments(request)?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                Ok(self
                    .backend
                    .file_dependency_map(path.to_owned())
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_symbol_dependency_map" => {
                let arguments = require_arguments(request)?;
                require_exact_argument_keys(arguments, &["path", "line", "character"])?;
                let path = require_string(arguments, "path")?;
                require_absolute(path)?;
                let line = require_positive_u32(arguments, "line")?;
                let character = optional_u32(arguments, "character")?.ok_or_else(|| {
                    ErrorData::invalid_params("missing argument 'character'", None)
                })?;
                Ok(self
                    .backend
                    .symbol_dependency_map(path.to_owned(), line, character)
                    .map(|result| complete(CallToolResult::structured(json!(result))))
                    .unwrap_or_else(backend_error_result))
            }
            "ee_diagnostics" => {
                let lines = self.backend.diagnostics();
                Ok(complete(CallToolResult::success(vec![ContentBlock::text(lines.join("\n"))])))
            }
            _ => Err(ErrorData::method_not_found::<rmcp::model::CallToolRequestMethod>()),
        }
    }
}
