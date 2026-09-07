//! `impl LspPlugin` methods: lsp_requests.
use super::*;

impl LspPlugin {
    pub(super) fn record_view_failure(&mut self, view: &mut View<ChunkCache>, message: String) {
        if let Ok(client) = self.client_for_view(view)
            && let Ok(mut client) = client.lock()
        {
            client.record_server_failure(message);
        }
    }

    pub(super) fn current_position(
        &mut self,
        view: &mut View<ChunkCache>,
    ) -> Result<Position, LanguageResponseError> {
        let selection = view
            .get_selections()
            .map_err(LanguageResponseError::from)?
            .into_iter()
            .next()
            .unwrap_or(xi_plugin_lib::SelectionRange { start: 0, end: 0 });
        get_position_of_offset(view, selection.end).map_err(LanguageResponseError::from)
    }

    pub(super) fn current_range(
        &mut self,
        view: &mut View<ChunkCache>,
    ) -> Result<Range, LanguageResponseError> {
        let selection = view
            .get_selections()
            .map_err(LanguageResponseError::from)?
            .into_iter()
            .next()
            .unwrap_or(xi_plugin_lib::SelectionRange { start: 0, end: 0 });
        let start = selection.start.min(selection.end);
        let end = selection.start.max(selection.end);
        Ok(Range {
            start: get_position_of_offset(view, start).map_err(LanguageResponseError::from)?,
            end: get_position_of_offset(view, end).map_err(LanguageResponseError::from)?,
        })
    }

    pub(super) fn request_completion(&mut self, view: &mut View<ChunkCache>, index: Option<usize>) {
        let view_id = view.get_id();
        if let Some(index) = index
            && let Some(item) = self
                .pending_completions
                .get(&view_id)
                .and_then(|items| index.checked_sub(1).and_then(|idx| items.get(idx)).cloned())
        {
            self.apply_completion(view, &item);
            return;
        }

        let position = match self.current_position(view) {
            Ok(position) => position,
            Err(err) => {
                self.record_view_failure(view, format!("completion failed: {err:?}"));
                return;
            }
        };
        let Ok(ls_client_arc) = self.client_for_view(view) else {
            return;
        };
        let request = ls_client_arc
            .lock()
            .map_err(|_| String::from("language server client lock poisoned"))
            .and_then(|mut ls_client| {
                ls_client
                    .request_completion(view_id, position, move |ls_client, result| {
                        let response = result
                            .map_err(|err| {
                                LanguageResponseError::LanguageServerError(format!("{err:?}"))
                            })
                            .and_then(|value| {
                                serde_json::from_value::<Option<CompletionResponse>>(value)
                                    .map_err(|err| {
                                        LanguageResponseError::Transport(err.to_string())
                                    })
                                    .map(|response| {
                                        response
                                            .map(pending_completions_from_response)
                                            .unwrap_or_default()
                                    })
                            });
                        ls_client
                            .result_queue
                            .push_result(view_id.into(), LspResponse::Completions(response));
                        ls_client.core.schedule_idle(view_id);
                    })
                    .map_err(|err| err.to_string())
            });
        if let Err(err) = request {
            self.record_view_failure(view, format!("completion failed: {err}"));
        }
    }

    pub(super) fn request_definition(&mut self, view: &mut View<ChunkCache>) {
        let view_id = view.get_id();
        let position = match self.current_position(view) {
            Ok(position) => position,
            Err(err) => {
                self.record_view_failure(view, format!("definition failed: {err:?}"));
                return;
            }
        };
        let current_document_uri = match view.get_path().map(file_path_to_uri) {
            Some(Ok(uri)) => uri,
            Some(Err(err)) => {
                self.record_view_failure(view, format!("definition failed: {err}"));
                return;
            }
            None => {
                self.record_view_failure(
                    view,
                    String::from("definition failed: missing file path"),
                );
                return;
            }
        };
        let current_document_text = match view.get_document() {
            Ok(text) => text,
            Err(err) => {
                self.record_view_failure(view, format!("definition failed: {err:?}"));
                return;
            }
        };
        let Ok(ls_client_arc) = self.client_for_view(view) else {
            return;
        };
        let request = ls_client_arc
            .lock()
            .map_err(|_| String::from("language server client lock poisoned"))
            .and_then(|mut ls_client| {
                ls_client
                    .request_definition(view_id, position, move |ls_client, result| {
                        let current_document_uri = current_document_uri.clone();
                        let current_document_text = current_document_text.clone();
                        let response = result
                            .map_err(|err| {
                                LanguageResponseError::LanguageServerError(format!("{err:?}"))
                            })
                            .and_then(|value| {
                                serde_json::from_value::<Option<GotoDefinitionResponse>>(value)
                                    .map_err(|err| {
                                        LanguageResponseError::Transport(err.to_string())
                                    })
                            })
                            .and_then(|response| match response {
                                Some(response) => navigation_targets_from_definition_response(
                                    &current_document_uri,
                                    &current_document_text,
                                    response,
                                ),
                                None => Ok(Vec::new()),
                            });
                        ls_client.result_queue.push_result(
                            view_id.into(),
                            LspResponse::Locations {
                                title: String::from("definition"),
                                result: response,
                            },
                        );
                        ls_client.core.schedule_idle(view_id);
                    })
                    .map_err(|err| err.to_string())
            });
        if let Err(err) = request {
            self.record_view_failure(view, format!("definition failed: {err}"));
        }
    }

    pub(super) fn request_declaration(&mut self, view: &mut View<ChunkCache>) {
        let view_id = view.get_id();
        let position = match self.current_position(view) {
            Ok(position) => position,
            Err(err) => {
                self.record_view_failure(view, format!("declaration failed: {err:?}"));
                return;
            }
        };
        let current_document_uri = match view.get_path().map(file_path_to_uri) {
            Some(Ok(uri)) => uri,
            Some(Err(err)) => {
                self.record_view_failure(view, format!("declaration failed: {err}"));
                return;
            }
            None => {
                self.record_view_failure(
                    view,
                    String::from("declaration failed: missing file path"),
                );
                return;
            }
        };
        let current_document_text = match view.get_document() {
            Ok(text) => text,
            Err(err) => {
                self.record_view_failure(view, format!("declaration failed: {err:?}"));
                return;
            }
        };
        let Ok(ls_client_arc) = self.client_for_view(view) else {
            return;
        };
        let request = ls_client_arc
            .lock()
            .map_err(|_| String::from("language server client lock poisoned"))
            .and_then(|mut ls_client| {
                ls_client
                    .request_declaration(view_id, position, move |ls_client, result| {
                        let current_document_uri = current_document_uri.clone();
                        let current_document_text = current_document_text.clone();
                        let response = result
                            .map_err(|err| {
                                LanguageResponseError::LanguageServerError(format!("{err:?}"))
                            })
                            .and_then(|value| {
                                serde_json::from_value::<Option<GotoDefinitionResponse>>(value)
                                    .map_err(|err| {
                                        LanguageResponseError::Transport(err.to_string())
                                    })
                            })
                            .and_then(|response| match response {
                                Some(response) => navigation_targets_from_definition_response(
                                    &current_document_uri,
                                    &current_document_text,
                                    response,
                                ),
                                None => Ok(Vec::new()),
                            });
                        ls_client.result_queue.push_result(
                            view_id.into(),
                            LspResponse::Locations {
                                title: String::from("declaration"),
                                result: response,
                            },
                        );
                        ls_client.core.schedule_idle(view_id);
                    })
                    .map_err(|err| err.to_string())
            });
        if let Err(err) = request {
            self.record_view_failure(view, format!("declaration failed: {err}"));
        }
    }

    pub(super) fn request_references(&mut self, view: &mut View<ChunkCache>) {
        let view_id = view.get_id();
        let position = match self.current_position(view) {
            Ok(position) => position,
            Err(err) => {
                self.record_view_failure(view, format!("references failed: {err:?}"));
                return;
            }
        };
        let current_document_uri = match view.get_path().map(file_path_to_uri) {
            Some(Ok(uri)) => uri,
            Some(Err(err)) => {
                self.record_view_failure(view, format!("references failed: {err}"));
                return;
            }
            None => {
                self.record_view_failure(
                    view,
                    String::from("references failed: missing file path"),
                );
                return;
            }
        };
        let current_document_text = match view.get_document() {
            Ok(text) => text,
            Err(err) => {
                self.record_view_failure(view, format!("references failed: {err:?}"));
                return;
            }
        };
        let Ok(ls_client_arc) = self.client_for_view(view) else {
            return;
        };
        let request = ls_client_arc
            .lock()
            .map_err(|_| String::from("language server client lock poisoned"))
            .and_then(|mut ls_client| {
                ls_client
                    .request_references(view_id, position, move |ls_client, result| {
                        let current_document_uri = current_document_uri.clone();
                        let current_document_text = current_document_text.clone();
                        let response = result
                            .map_err(|err| {
                                LanguageResponseError::LanguageServerError(format!("{err:?}"))
                            })
                            .and_then(|value| {
                                serde_json::from_value::<Option<Vec<Location>>>(value).map_err(
                                    |err| LanguageResponseError::Transport(err.to_string()),
                                )
                            })
                            .and_then(|response| match response {
                                Some(response) => navigation_targets_from_references(
                                    &current_document_uri,
                                    &current_document_text,
                                    response,
                                ),
                                None => Ok(Vec::new()),
                            });
                        ls_client.result_queue.push_result(
                            view_id.into(),
                            LspResponse::Locations {
                                title: String::from("references"),
                                result: response,
                            },
                        );
                        ls_client.core.schedule_idle(view_id);
                    })
                    .map_err(|err| err.to_string())
            });
        if let Err(err) = request {
            self.record_view_failure(view, format!("references failed: {err}"));
        }
    }

    pub(super) fn request_type_definition(&mut self, view: &mut View<ChunkCache>) {
        let view_id = view.get_id();
        let position = match self.current_position(view) {
            Ok(position) => position,
            Err(err) => {
                self.record_view_failure(view, format!("type definition failed: {err:?}"));
                return;
            }
        };
        let current_document_uri = match view.get_path().map(file_path_to_uri) {
            Some(Ok(uri)) => uri,
            Some(Err(err)) => {
                self.record_view_failure(view, format!("type definition failed: {err}"));
                return;
            }
            None => {
                self.record_view_failure(
                    view,
                    String::from("type definition failed: missing file path"),
                );
                return;
            }
        };
        let current_document_text = match view.get_document() {
            Ok(text) => text,
            Err(err) => {
                self.record_view_failure(view, format!("type definition failed: {err:?}"));
                return;
            }
        };
        let Ok(ls_client_arc) = self.client_for_view(view) else {
            return;
        };
        let request = ls_client_arc
            .lock()
            .map_err(|_| String::from("language server client lock poisoned"))
            .and_then(|mut ls_client| {
                ls_client
                    .request_type_definition(view_id, position, move |ls_client, result| {
                        let current_document_uri = current_document_uri.clone();
                        let current_document_text = current_document_text.clone();
                        let response = result
                            .map_err(|err| {
                                LanguageResponseError::LanguageServerError(format!("{err:?}"))
                            })
                            .and_then(|value| {
                                serde_json::from_value::<Option<GotoDefinitionResponse>>(value)
                                    .map_err(|err| {
                                        LanguageResponseError::Transport(err.to_string())
                                    })
                            })
                            .and_then(|response| match response {
                                Some(response) => navigation_targets_from_definition_response(
                                    &current_document_uri,
                                    &current_document_text,
                                    response,
                                ),
                                None => Ok(Vec::new()),
                            });
                        ls_client.result_queue.push_result(
                            view_id.into(),
                            LspResponse::Locations {
                                title: String::from("type definition"),
                                result: response,
                            },
                        );
                        ls_client.core.schedule_idle(view_id);
                    })
                    .map_err(|err| err.to_string())
            });
        if let Err(err) = request {
            self.record_view_failure(view, format!("type definition failed: {err}"));
        }
    }

    pub(super) fn request_implementation(&mut self, view: &mut View<ChunkCache>) {
        let view_id = view.get_id();
        let position = match self.current_position(view) {
            Ok(position) => position,
            Err(err) => {
                self.record_view_failure(view, format!("implementation failed: {err:?}"));
                return;
            }
        };
        let current_document_uri = match view.get_path().map(file_path_to_uri) {
            Some(Ok(uri)) => uri,
            Some(Err(err)) => {
                self.record_view_failure(view, format!("implementation failed: {err}"));
                return;
            }
            None => {
                self.record_view_failure(
                    view,
                    String::from("implementation failed: missing file path"),
                );
                return;
            }
        };
        let current_document_text = match view.get_document() {
            Ok(text) => text,
            Err(err) => {
                self.record_view_failure(view, format!("implementation failed: {err:?}"));
                return;
            }
        };
        let Ok(ls_client_arc) = self.client_for_view(view) else {
            return;
        };
        let request = ls_client_arc
            .lock()
            .map_err(|_| String::from("language server client lock poisoned"))
            .and_then(|mut ls_client| {
                ls_client
                    .request_implementation(view_id, position, move |ls_client, result| {
                        let current_document_uri = current_document_uri.clone();
                        let current_document_text = current_document_text.clone();
                        let response = result
                            .map_err(|err| {
                                LanguageResponseError::LanguageServerError(format!("{err:?}"))
                            })
                            .and_then(|value| {
                                serde_json::from_value::<Option<GotoDefinitionResponse>>(value)
                                    .map_err(|err| {
                                        LanguageResponseError::Transport(err.to_string())
                                    })
                            })
                            .and_then(|response| match response {
                                Some(response) => navigation_targets_from_definition_response(
                                    &current_document_uri,
                                    &current_document_text,
                                    response,
                                ),
                                None => Ok(Vec::new()),
                            });
                        ls_client.result_queue.push_result(
                            view_id.into(),
                            LspResponse::Locations {
                                title: String::from("implementation"),
                                result: response,
                            },
                        );
                        ls_client.core.schedule_idle(view_id);
                    })
                    .map_err(|err| err.to_string())
            });
        if let Err(err) = request {
            self.record_view_failure(view, format!("implementation failed: {err}"));
        }
    }
}
