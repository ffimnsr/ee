//! OpenAI Responses dialect codec for OpenCode (`/responses`).
//!
//! The codec itself lives in `ee-chat-completions`, shared with every other
//! provider that speaks the OpenAI Responses protocol, so no protocol code is
//! duplicated across provider crates. This module keeps the OpenCode dialect
//! identity and re-exports the shared surface at its historical path.

pub use ee_chat_completions::responses::*;

/// Dialect every route served by this module speaks.
pub const DIALECT: crate::routes::OpenCodeDialect = crate::routes::OpenCodeDialect::OpenAiResponses;
