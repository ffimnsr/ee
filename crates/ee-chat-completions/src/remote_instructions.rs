//! HTTP client injected into the orchestrator for configured instruction URLs.
//!
//! The orchestrator is deliberately network-free, so the provider binaries hand
//! it a [`RemoteInstructionFetcher`] built here. The contract is narrow: HTTPS
//! only, one request per URL, a short wall-clock limit, at most three redirects,
//! and a per-source byte cap. Errors are bounded strings that never include a
//! response body.

use std::time::Duration;

use ee_agent_orchestrator::{FetchFuture, RemoteInstructionBody, RemoteInstructionFetcher};

/// Wall-clock limit for one instruction fetch.
pub const REMOTE_INSTRUCTION_TIMEOUT: Duration = Duration::from_secs(5);
/// Redirect hops a fetch may follow.
pub const REMOTE_INSTRUCTION_MAX_REDIRECTS: usize = 3;
/// Per-source read ceiling before truncation.
pub const REMOTE_INSTRUCTION_MAX_BYTES: usize = 128 * 1024;

/// Builds the default fetcher used by both provider binaries.
#[must_use]
pub fn http_instruction_fetcher() -> RemoteInstructionFetcher {
    RemoteInstructionFetcher::new(|url| fetch(url))
}

fn fetch(url: String) -> FetchFuture {
    Box::pin(async move {
        let client = reqwest::Client::builder()
            .timeout(REMOTE_INSTRUCTION_TIMEOUT)
            .redirect(reqwest::redirect::Policy::limited(REMOTE_INSTRUCTION_MAX_REDIRECTS))
            .build()
            .map_err(|error| format!("client unavailable ({})", error.without_url()))?;
        let mut response = client
            .get(&url)
            .send()
            .await
            .map_err(|error| format!("request failed ({})", error.without_url()))?;
        if !response.status().is_success() {
            return Err(format!("status {}", response.status().as_u16()));
        }
        let mut bytes = Vec::new();
        let mut truncated = false;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| format!("body failed ({})", error.without_url()))?
        {
            let remaining = REMOTE_INSTRUCTION_MAX_BYTES.saturating_sub(bytes.len());
            if chunk.len() >= remaining {
                bytes.extend_from_slice(&chunk[..remaining]);
                truncated = true;
                break;
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(RemoteInstructionBody {
            content: String::from_utf8_lossy(&bytes).into_owned(),
            truncated,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetcher_builds_without_performing_io() {
        let fetcher = http_instruction_fetcher();
        assert!(format!("{fetcher:?}").contains("injected"));
    }
}
