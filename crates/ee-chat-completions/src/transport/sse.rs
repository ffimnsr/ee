//! Server-sent event framing over a chunk source.
//!
//! Framing is dialect independent: both OpenAI Responses and Anthropic Messages
//! put the event name in the JSON payload, so consumers dispatch on `type`.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use ee_agent_acp_server::ProviderError;

use crate::stream_meta::chunk_timeout;
use crate::wire::SseDecoder;

/// Boxed future returned by a chunk source.
pub type ChunkFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>, String>> + Send + 'a>>;

/// Bytes of a streaming response, in arrival order.
///
/// Implemented for [`reqwest::Response`]; tests implement it directly so SSE
/// handling stays hermetic.
pub trait ChunkSource: Send {
    /// Returns the next chunk, or `None` at the end of the body.
    ///
    /// The error text is a provider-neutral detail; the caller frames it with
    /// its own label.
    fn next_chunk(&mut self) -> ChunkFuture<'_>;
}

impl ChunkSource for reqwest::Response {
    fn next_chunk(&mut self) -> ChunkFuture<'_> {
        Box::pin(async move {
            self.chunk()
                .await
                .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
                .map_err(|error| error.to_string())
        })
    }
}

/// Whether an SSE consumer wants more events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseControl {
    /// Keep reading.
    Continue,
    /// Stop reading; the caller already has what it needs.
    Stop,
}

/// Drives SSE framing over a chunk source, invoking `on_event` per data payload.
///
/// `[DONE]` and [`SseControl::Stop`] end the stream without reading further bytes;
/// trailing bytes are framed only when the body ended on its own.
///
/// # Errors
///
/// Returns a typed error when the body cannot be read, an event is not UTF-8, or
/// the consumer rejects an event.
pub async fn drive_sse<F>(
    label: &str,
    source: &mut dyn ChunkSource,
    on_event: &mut F,
) -> Result<(), ProviderError>
where
    F: FnMut(&str) -> Result<SseControl, ProviderError>,
{
    drive_sse_with_chunk_timeout(label, source, None, on_event).await
}

/// Drives SSE framing with an optional idle budget between chunks.
///
/// `chunk` is the maximum silence tolerated between body reads. A stream that
/// produces no bytes for longer than the budget fails with
/// `Timeout { phase: "chunk" }` (`code = "TimeoutChunk"`), which is distinct from
/// a whole-request timeout so retry policy can treat them differently.
///
/// # Errors
///
/// Returns a typed error when the body cannot be read, an event is not UTF-8,
/// the consumer rejects an event, or the idle budget expires.
pub async fn drive_sse_with_chunk_timeout<F>(
    label: &str,
    source: &mut dyn ChunkSource,
    chunk: Option<Duration>,
    on_event: &mut F,
) -> Result<(), ProviderError>
where
    F: FnMut(&str) -> Result<SseControl, ProviderError>,
{
    let mut decoder = SseDecoder::new(label);
    loop {
        let Some(chunk) = next_chunk(label, source, chunk).await? else {
            break;
        };
        for event in decoder.push(&chunk)? {
            if event == "[DONE]" || on_event(&event)? == SseControl::Stop {
                return Ok(());
            }
        }
    }
    for event in decoder.finish()? {
        if event == "[DONE]" || on_event(&event)? == SseControl::Stop {
            return Ok(());
        }
    }
    Ok(())
}

/// Reads the next body chunk, enforcing the idle budget when one is set.
pub(crate) async fn next_chunk(
    label: &str,
    source: &mut dyn ChunkSource,
    chunk: Option<Duration>,
) -> Result<Option<Vec<u8>>, ProviderError> {
    let read = match chunk {
        Some(limit) => match tokio::time::timeout(limit, source.next_chunk()).await {
            Ok(result) => result,
            Err(_) => return Err(chunk_timeout(label)),
        },
        None => source.next_chunk().await,
    };
    read.map_err(|detail| {
        ProviderError::BackendFailure(format!("{label} streaming response failed: {detail}"))
    })
}

/// Appends `chunk` to `buffer`, failing closed beyond `limit` bytes.
pub(crate) fn extend_bounded(
    buffer: &mut Vec<u8>,
    chunk: &[u8],
    limit: usize,
) -> Result<(), String> {
    if buffer.len().saturating_add(chunk.len()) > limit {
        return Err(format!("body exceeded the {limit}-byte limit"));
    }
    buffer.extend_from_slice(chunk);
    Ok(())
}
