//! HTTP client injected into the orchestrator for configured instruction URLs.
//!
//! The orchestrator is deliberately network-free, so the provider binaries hand
//! it a [`RemoteInstructionFetcher`] built here. The contract is narrow: HTTPS
//! only, one request per URL, a short wall-clock limit, at most three redirects,
//! and a per-source byte cap. Errors are bounded strings that never include a
//! response body.
//!
//! "HTTPS only" is enforced on every redirect hop, not just on the configured
//! URL: `reqwest::redirect::Policy::limited` follows any scheme, so an HTTPS
//! source could redirect to `http://` and downgrade the fetch to cleartext.
//! [`redirect_policy`] re-checks the scheme after each hop and stops the chain
//! when a redirect is not HTTPS or the hop budget is exhausted.

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

/// Redirect policy for instruction fetches: HTTPS on every hop, bounded hops.
fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        match redirect_destination(attempt.url(), attempt.previous().len()) {
            Ok(()) => attempt.follow(),
            Err(reason) => attempt.error(reason),
        }
    })
}

/// Decides whether `next` may be followed after `followed` previous hops.
///
/// Split from the policy closure so the rule is testable without a server.
fn redirect_destination(next: &reqwest::Url, followed: usize) -> Result<(), &'static str> {
    if followed >= REMOTE_INSTRUCTION_MAX_REDIRECTS {
        return Err("too many redirects");
    }
    if next.scheme() != "https" {
        return Err("redirect target is not https");
    }
    Ok(())
}

fn fetch(url: String) -> FetchFuture {
    Box::pin(async move {
        let client = reqwest::Client::builder()
            .timeout(REMOTE_INSTRUCTION_TIMEOUT)
            .redirect(redirect_policy())
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

    #[test]
    fn redirect_to_http_is_refused_inside_the_hop_budget() {
        let target = reqwest::Url::parse("http://example.com/instructions.md").unwrap();
        assert_eq!(redirect_destination(&target, 1), Err("redirect target is not https"));
    }

    #[test]
    fn redirect_to_https_is_followed_until_the_hop_budget_is_exhausted() {
        let target = reqwest::Url::parse("https://example.com/instructions.md").unwrap();
        assert_eq!(redirect_destination(&target, 0), Ok(()));
        assert_eq!(
            redirect_destination(&target, REMOTE_INSTRUCTION_MAX_REDIRECTS),
            Err("too many redirects")
        );
    }
}
