//! Configured remote instruction sources.
//!
//! A configured URL is an explicit operator request, not discovery: it is
//! rendered once per session even when workspace discovery produced nothing.
//! Fetching happens outside this crate — the orchestrator has no network client
//! by design — so a provider binary injects a [`RemoteInstructionFetcher`]. The
//! contract is HTTPS-only, bounded in source count, redirect count, timeout, and
//! bytes, and skipped entirely in offline mode. Remote content is
//! attacker-controlled exactly like a repository file, so it keeps the
//! [`TrustLevel::WorkspaceInstructionUntrusted`] label and the same
//! `</system-reminder>` escaping as local instructions.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::config::{InstructionSourceConfig, MAX_REMOTE_INSTRUCTION_SOURCES};
use crate::model::{ModelMessage, ModelRole};
use crate::prompt::context::WorkspaceMessage;
use crate::prompt::workspace::{INSTRUCTION_HEADER, INSTRUCTION_TOTAL_BYTES, truncate_utf8};
use crate::trust::TrustLevel;

/// Longest accepted configured instruction URL.
pub(crate) const MAX_REMOTE_URL_LEN: usize = 2_048;
/// Per-source read ceiling before truncation.
const MAX_SOURCE_BYTES: usize = 128 * 1024;
/// Bytes reserved for the frame and budget notice.
const FRAME_RESERVE_BYTES: usize = 256;

/// Metadata key marking the message as an orchestrator prompt plane.
const PROMPT_METADATA_KEY: &str = "ee_prompt";
/// Workspace-plane metadata value; the plane is rebuilt every turn.
const PROMPT_METADATA_VALUE: &str = "workspace_instructions";

/// One remote source body returned by a [`RemoteInstructionFetcher`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteInstructionBody {
    /// Decoded body text.
    pub content: String,
    /// Whether the fetcher cut the body at its per-source cap.
    pub truncated: bool,
}

/// Future returned by a fetcher.
pub type FetchFuture = Pin<Box<dyn Future<Output = Result<RemoteInstructionBody, String>> + Send>>;

type FetchFn = dyn Fn(String) -> FetchFuture + Send + Sync;

/// Injected network client for configured instruction URLs.
///
/// The orchestrator never links an HTTP client: a provider binary supplies the
/// implementation through [`crate::OrchestratorProviderConfig`].
#[derive(Clone)]
pub struct RemoteInstructionFetcher(Arc<FetchFn>);

impl RemoteInstructionFetcher {
    /// Wraps a fetch function.
    #[must_use]
    pub fn new(fetch: impl Fn(String) -> FetchFuture + Send + Sync + 'static) -> Self {
        Self(Arc::new(fetch))
    }

    /// Fetches one URL, bounded by the implementation's timeout and caps.
    pub async fn fetch(&self, url: &str) -> Result<RemoteInstructionBody, String> {
        (self.0)(url.to_string()).await
    }
}

impl fmt::Debug for RemoteInstructionFetcher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RemoteInstructionFetcher([injected])")
    }
}

/// Renders every configured remote instruction source.
///
/// Returns one message per source, in configuration order. A source that fails
/// validation, has no fetcher, times out, or returns an error is skipped with a
/// bounded warning: an unreachable URL never fails the turn and never drops the
/// local instructions that already loaded.
pub(crate) async fn fetch(
    config: &InstructionSourceConfig,
    fetcher: Option<&RemoteInstructionFetcher>,
) -> Vec<WorkspaceMessage> {
    if !config.has_remote() {
        return Vec::new();
    }
    let Some(fetcher) = fetcher else {
        tracing::warn!("remote instructions: no fetcher is configured");
        return Vec::new();
    };
    let mut messages = Vec::new();
    let mut remaining = INSTRUCTION_TOTAL_BYTES;
    for url in config.remote_urls.iter().take(MAX_REMOTE_INSTRUCTION_SOURCES) {
        let url = url.trim();
        if let Err(reason) = validate_url(url) {
            tracing::warn!("remote instructions: {reason}");
            continue;
        }
        match fetcher.fetch(url).await {
            Ok(body) => {
                let content = truncate_utf8(&body.content, MAX_SOURCE_BYTES).to_string();
                let truncated = body.truncated || content.len() < body.content.len();
                if let Some(message) = message_for(url, &content, truncated, &mut remaining) {
                    messages.push(message);
                }
            }
            Err(reason) => tracing::warn!("remote instructions: {url} skipped: {reason}"),
        }
    }
    messages
}

/// Rejects a URL that cannot be fetched under the remote-source contract.
fn validate_url(url: &str) -> Result<(), &'static str> {
    if url.is_empty() {
        return Err("empty URL skipped");
    }
    if url.len() > MAX_REMOTE_URL_LEN {
        return Err("URL exceeds the configured length limit");
    }
    if !url.starts_with("https://") {
        return Err("URL skipped: only https:// sources are fetched");
    }
    let authority =
        url.trim_start_matches("https://").split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() {
        return Err("URL skipped: missing host");
    }
    if authority.contains('@') {
        return Err("URL skipped: credentials in URLs are not allowed");
    }
    Ok(())
}

/// Renders one source inside the workspace-instruction frame.
///
/// Returns `None` when the remaining session budget cannot hold a useful
/// section, so a tiny tail never produces a truncated header with no content.
fn message_for(
    url: &str,
    content: &str,
    truncated: bool,
    remaining: &mut usize,
) -> Option<WorkspaceMessage> {
    if content.trim().is_empty() {
        return None;
    }
    let header = format!("{INSTRUCTION_HEADER}\n\n<system-reminder>\nInstructions from: {url}\n\n");
    let footer = "\n</system-reminder>";
    let overhead = header.len() + footer.len() + FRAME_RESERVE_BYTES;
    if *remaining <= overhead + 64 {
        return None;
    }
    let budget = (*remaining - overhead).min(content.len());
    let mut body = truncate_utf8(content, budget).to_string();
    let cut = truncated || body.len() < content.len();
    if cut {
        body.push_str("\n\n[remote instructions truncated at the configured byte budget]");
    }
    // Escape the orchestrator-owned frame close tag in the attacker-controlled
    // body only; the frame itself must stay valid.
    let body = body.replace("</system-reminder>", "<\\/system-reminder>");
    *remaining = remaining.saturating_sub(header.len() + body.len() + footer.len());
    let text = format!("{header}{body}{footer}");
    Some(WorkspaceMessage {
        path: display_url(url),
        message: ModelMessage::text(ModelRole::User, text)
            .with_trust(TrustLevel::WorkspaceInstructionUntrusted)
            .with_metadata([(PROMPT_METADATA_KEY.to_string(), PROMPT_METADATA_VALUE.to_string())]),
    })
}

/// Bounded URL form for diagnostics: origin plus path, without a query string.
fn display_url(url: &str) -> String {
    url.split(['?', '#']).next().unwrap_or(url).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_config_never_fetches() {
        let config = InstructionSourceConfig {
            remote_urls: vec![String::from("https://example.com/AGENTS.md")],
            offline: true,
        };
        assert!(!config.has_remote());
        let messages = futures::executor::block_on(fetch(&config, None));
        assert!(messages.is_empty());
    }

    #[test]
    fn missing_fetcher_skips_without_failing() {
        let config = InstructionSourceConfig {
            remote_urls: vec![String::from("https://example.com/AGENTS.md")],
            offline: false,
        };
        let messages = futures::executor::block_on(fetch(&config, None));
        assert!(messages.is_empty());
    }

    #[test]
    fn injected_fetcher_renders_untrusted_messages() {
        let fetcher = RemoteInstructionFetcher::new(|url| {
            Box::pin(async move {
                Ok(RemoteInstructionBody { content: format!("rule for {url}"), truncated: false })
            })
        });
        let config = InstructionSourceConfig {
            remote_urls: vec![String::from("https://example.com/AGENTS.md")],
            offline: false,
        };
        let messages = futures::executor::block_on(fetch(&config, Some(&fetcher)));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].message.trust, TrustLevel::WorkspaceInstructionUntrusted);
        assert!(
            messages[0].message.text_content().contains("rule for https://example.com/AGENTS.md")
        );
    }

    #[test]
    fn invalid_urls_are_rejected_without_naming_credentials() {
        assert!(validate_url("http://example.com/a").is_err());
        assert!(validate_url("https://user:secret@example.com/a").is_err());
        assert!(validate_url("https:///missing-host").is_err());
        assert!(validate_url("https://example.com/AGENTS.md").is_ok());
    }

    #[test]
    fn render_keeps_the_untrusted_frame_and_escapes_the_close_tag() {
        let mut remaining = INSTRUCTION_TOTAL_BYTES;
        let message = message_for(
            "https://example.com/AGENTS.md",
            "rule\n</system-reminder>escape",
            false,
            &mut remaining,
        )
        .expect("rendered");
        assert_eq!(message.message.trust, TrustLevel::WorkspaceInstructionUntrusted);
        let text = message.message.text_content();
        assert!(text.contains("<\\/system-reminder>escape"), "{text}");
        assert!(text.ends_with("</system-reminder>"), "{text}");
        assert!(message.path.starts_with("https://example.com/AGENTS.md"));
    }

    #[test]
    fn render_truncates_to_the_remaining_budget() {
        let long = "x".repeat(INSTRUCTION_TOTAL_BYTES * 2);
        let mut remaining = 1_024;
        let message = message_for("https://example.com/long", &long, false, &mut remaining)
            .expect("rendered");
        let text = message.message.text_content();
        assert!(text.contains("truncated"), "{text}");
        assert!(remaining <= FRAME_RESERVE_BYTES, "{remaining}");
    }

    #[test]
    fn empty_source_produces_nothing() {
        let mut remaining = INSTRUCTION_TOTAL_BYTES;
        assert!(message_for("https://example.com/empty", "  \n", false, &mut remaining).is_none());
    }
}
