//! Anthropic Messages dialect codec for OpenCode (`/messages`).
//!
//! Routes whose `OpenCodeDialect::AnthropicMessages` models are served by this
//! codec (Zen Claude/Qwen and Go MiniMax/Qwen families) get their own request,
//! buffered-response, and streaming shapes:
//!
//! - system content is hoisted to the top-level `system` field, because Messages
//!   has no system role inside `messages`;
//! - the remaining transcript becomes user/assistant content blocks, and tool
//!   observations become correlated `tool_result` blocks grouped into one user
//!   turn, keeping tool-call identity;
//! - tool definitions become `{ name, description, input_schema }`;
//! - buffered `text` / `thinking` / `tool_use` blocks plus `stop_reason` and
//!   reported usage decode into the normalized
//!   [`ModelResponse`](ee_agent_orchestrator::ModelResponse), and the SSE
//!   lifecycle (`message_start`, `content_block_*`, `message_delta`,
//!   `message_stop`, `error`) decodes in arrival order.
//!
//! The normalized transcript carries assistant text only, so assistant tool-call
//! arguments cannot be reconstructed; the `tool_result` block still carries the
//! original call id, and the model sees the observation in order. A request
//! encoded here is never replayed through another dialect.
//!
//! Layout: `body` encodes requests, `decode` decodes buffered and streaming
//! responses, and `codec` owns the round trips and retry boundaries.

mod body;
mod codec;
mod decode;

#[cfg(test)]
mod tests;

pub use body::{MessagesBody, messages_body, request_body};
pub use codec::MessagesCodec;
pub use decode::{StreamDecoder, response_from_buffered};

/// Dialect every route served by this module speaks.
pub const DIALECT: crate::routes::OpenCodeDialect =
    crate::routes::OpenCodeDialect::AnthropicMessages;

/// Output bound sent when the route carries no catalog limit.
///
/// Messages requires `max_tokens`; EE sends the shared fallback rather than
/// letting the provider pick an unbounded default. A catalog limit or an
/// explicit override replaces it.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = ee_agent_orchestrator::OUTPUT_LIMIT_FALLBACK;

/// Key holding the replayable Anthropic thinking blocks.
const THINKING_BLOCKS: &str = "thinking_blocks";
