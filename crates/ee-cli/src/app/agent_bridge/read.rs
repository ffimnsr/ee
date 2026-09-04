//! Bounded text-read helpers and `impl App` read path.

use ee_agent_host::{AgentError, ClientRequestResponse, ClientRequestResult};
use ee_agent_protocol::{ReadTextFileRequest, ReadTextFileResponse, SessionId};
use tokio::sync::oneshot;

use super::super::*;

use super::app_search::paths_equivalent;
use super::write::{ActionLogEntry, split_lines};

/// Session key and fingerprint used for normalized read evaluations
/// (Phase 4): reads are prompt-free today and never record session
/// decisions, so the session state stays empty for these keys.
pub(super) const READ_SESSION: &str = "read";
pub(super) const READ_FINGERPRINT: &str = "read";

/// Hard cap on lines served by one unbounded `fs/read_text_file`.
pub(crate) const BRIDGE_READ_MAX_LINES: usize = 100_000;
/// Hard cap on bytes served by one unbounded `fs/read_text_file`.
pub(crate) const BRIDGE_READ_MAX_BYTES: usize = 1024 * 1024;

impl App {
    pub(super) fn session_thread(&self, session_id: &SessionId) -> Option<usize> {
        self.agents.thread_index(session_id.0.as_ref())
    }

    /// Validates and answers an `fs/read_text_file` request.
    ///
    /// Open buffers win over disk (unsaved in-memory text is returned);
    /// unopened files are read from disk only when inside the workspace.
    pub(super) fn bridge_read_file(
        &mut self,
        request: &ReadTextFileRequest,
        reply: oneshot::Sender<ClientRequestResult>,
    ) {
        if !request.path.is_absolute() {
            let _ = reply.send(Err(AgentError::invalid_params("path must be absolute")));
            return;
        }
        if let Err(error) = validate_read_window(request.line, request.limit, None) {
            let _ = reply.send(Err(error));
            return;
        }
        if !self.path_in_effective_workspace(&request.path) {
            let _ = reply.send(Err(AgentError::invalid_params(format!(
                "path outside allowed workspace: {}",
                request.path.display()
            ))));
            return;
        }

        // Open buffer first: the in-memory snapshot is authoritative.
        if let Some(buf) = self
            .backend
            .all_bufs()
            .iter()
            .find(|buf| buf.path.as_deref().is_some_and(|p| paths_equivalent(p, &request.path)))
        {
            let session_id = request.session_id.0.to_string();
            match self.read_from_buffer(buf, request) {
                Ok((content, bytes)) => {
                    self.agents.action_log.push(ActionLogEntry::Read {
                        path: request.path.clone(),
                        bytes,
                        session_id: session_id.clone(),
                    });
                    if let Some(thread) = self.session_thread(&request.session_id) {
                        self.agents.threads[thread]
                            .push_system(format!("agent read: {}", request.path.display()));
                    }
                    let _ = reply.send(Ok(ClientRequestResponse::ReadTextFile(
                        ReadTextFileResponse::new(content),
                    )));
                }
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            }
            return;
        }

        // Disk fallback after containment check above.
        match std::fs::read_to_string(&request.path) {
            Ok(content) => match read_text_window(&content, request.line, request.limit) {
                Ok(content) => {
                    let bytes = content.len();
                    self.agents.action_log.push(ActionLogEntry::Read {
                        path: request.path.clone(),
                        bytes,
                        session_id: request.session_id.0.to_string(),
                    });
                    let _ = reply.send(Ok(ClientRequestResponse::ReadTextFile(
                        ReadTextFileResponse::new(content),
                    )));
                }
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            },
            Err(error) => {
                let _ = reply.send(Err(AgentError::Io(format!(
                    "cannot read {}: {error}",
                    request.path.display()
                ))));
            }
        }
    }

    /// Serves a read from an open buffer, applying ACP line/limit semantics
    /// (1-based `line`, optional `limit`, both enforced against caps).
    pub(super) fn read_from_buffer(
        &self,
        buf: &crate::buffer::BufState,
        request: &ReadTextFileRequest,
    ) -> Result<(String, usize), AgentError> {
        let line_count = buf.line_count();
        let start = validate_read_window(request.line, request.limit, Some(line_count))?;
        if buf.is_vlf {
            if request.line.is_none() && request.limit.is_none() {
                return Err(AgentError::invalid_params(
                    "unbounded reads are not supported for very large files",
                ));
            }
            let count = request.limit.map(|limit| limit as usize).unwrap_or(BRIDGE_READ_MAX_LINES);
            let end = start.saturating_add(count);
            let cache_start = buf.vlf_cache_start_line;
            let cache_end = cache_start.saturating_add(buf.line_cache.len());
            if start < cache_start || end > cache_end {
                return Err(AgentError::invalid_params(
                    "requested range is not loaded in the very-large-file viewport",
                ));
            }
            let lines: Vec<String> = buf
                .line_cache
                .iter()
                .skip(start - cache_start)
                .take(count)
                .map(|slot| match slot {
                    crate::backend::LineSlot::Known(cached) => cached.text.clone(),
                    crate::backend::LineSlot::Invalid => String::new(),
                })
                .collect();
            let content = lines.join("\n");
            let bytes = content.len();
            return Ok((content, bytes));
        }
        let content =
            read_text_window(&buf.whole_text().unwrap_or_default(), request.line, request.limit)?;
        let bytes = content.len();
        Ok((content, bytes))
    }
}

fn validate_read_window(
    line: Option<u32>,
    limit: Option<u32>,
    line_count: Option<usize>,
) -> Result<usize, AgentError> {
    if matches!(line, Some(0)) {
        return Err(AgentError::invalid_params("line must be 1-based"));
    }
    if let Some(limit) = limit {
        let count = limit as usize;
        if count > BRIDGE_READ_MAX_LINES {
            return Err(AgentError::invalid_params(format!(
                "line limit {count} exceeds the {BRIDGE_READ_MAX_LINES} cap"
            )));
        }
    }
    let start = line.map(|line| (line - 1) as usize).unwrap_or(0);
    if let Some(line_count) = line_count
        && start > line_count
    {
        return Err(AgentError::invalid_params(format!(
            "start line {} is beyond the end of the file ({line_count} lines)",
            line.unwrap_or(1)
        )));
    }
    Ok(start)
}

/// Applies ACP read-window semantics and unbounded-read caps to `content`.
pub(super) fn read_text_window(
    content: &str,
    line: Option<u32>,
    limit: Option<u32>,
) -> Result<String, AgentError> {
    let lines = split_lines(content);
    let start = validate_read_window(line, limit, Some(lines.len()))?;
    let selected = if let Some(limit) = limit {
        let count = limit as usize;
        lines.into_iter().skip(start).take(count).collect::<Vec<_>>()
    } else {
        let mut tail = lines.into_iter().skip(start).collect::<Vec<_>>();
        if line.is_some() || start == 0 {
            tail.truncate(BRIDGE_READ_MAX_LINES);
        }
        tail
    };
    let mut text = selected.join("\n");
    if limit.is_none() && text.len() > BRIDGE_READ_MAX_BYTES {
        let mut cut = BRIDGE_READ_MAX_BYTES;
        while cut < text.len() && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    Ok(text)
}

#[cfg(test)]
#[cfg(test)]
#[cfg(test)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_caps_truncate_unbounded_reads() {
        let content = "x\n".repeat(200_000);
        let capped = read_text_window(&content, None, None).expect("unbounded read caps");
        assert!(capped.len() <= BRIDGE_READ_MAX_BYTES);
        let bounded = read_text_window(&content, None, Some(3)).expect("bounded read caps");
        assert_eq!(bounded, "x\nx\nx");
    }
}
