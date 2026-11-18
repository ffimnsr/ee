//! Phase-aware timeout tests.
//!
//! Every test uses paused Tokio time, so a 900-second chunk budget elapses
//! instantly and the suite stays deterministic and fast.

use std::time::Duration;

use ee_agent_acp_server::ProviderError;

use super::*;
use crate::error_classification::{is_timeout, is_unreachable, timeout_phase};

/// A chunk source that stalls once for `stall`, then replays `chunks`.
struct StallingChunks {
    chunks: Vec<Vec<u8>>,
    stall: Option<Duration>,
}

impl ChunkSource for StallingChunks {
    fn next_chunk(&mut self) -> ChunkFuture<'_> {
        Box::pin(async move {
            if let Some(stall) = self.stall.take() {
                tokio::time::sleep(stall).await;
            }
            if self.chunks.is_empty() {
                return Ok(None);
            }
            Ok(Some(self.chunks.remove(0)))
        })
    }
}

fn event(text: &str) -> Vec<u8> {
    format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n").into_bytes()
}

#[tokio::test(start_paused = true)]
async fn a_stall_inside_the_chunk_budget_completes() {
    let mut source = StallingChunks {
        chunks: vec![event("late"), b"data: [DONE]\n\n".to_vec()],
        stall: Some(Duration::from_secs(200)),
    };
    let mut seen = 0usize;

    let result = drive_sse_with_chunk_timeout(
        "Test",
        &mut source,
        Some(Duration::from_secs(900)),
        &mut |_event| {
            seen += 1;
            Ok(SseControl::Continue)
        },
    )
    .await;

    result.expect("a 200 s stall is inside the 900 s chunk budget");
    assert_eq!(seen, 1, "the late event still reached the consumer");
}

#[tokio::test(start_paused = true)]
async fn exceeding_the_chunk_budget_fails_with_the_chunk_phase() {
    let mut source =
        StallingChunks { chunks: vec![event("late")], stall: Some(Duration::from_secs(901)) };

    let error = drive_sse_with_chunk_timeout(
        "Test",
        &mut source,
        Some(Duration::from_secs(900)),
        &mut |_event| Ok(SseControl::Continue),
    )
    .await
    .expect_err("the idle budget expired");

    assert!(matches!(error, ProviderError::Transport { .. }), "{error:?}");
    assert!(is_timeout(&error), "a chunk timeout is a timeout");
    assert_eq!(timeout_phase(&error), Some("chunk"));
    assert!(!is_unreachable(&error));
}

#[tokio::test(start_paused = true)]
async fn no_budget_means_no_watchdog() {
    let mut source =
        StallingChunks { chunks: vec![event("slow")], stall: Some(Duration::from_secs(7_200)) };
    let mut seen = 0usize;

    drive_sse_with_chunk_timeout("Test", &mut source, None, &mut |_event| {
        seen += 1;
        Ok(SseControl::Continue)
    })
    .await
    .expect("an unset chunk budget never expires");

    assert_eq!(seen, 1);
}
