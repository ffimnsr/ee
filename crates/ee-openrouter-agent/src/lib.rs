//! OpenRouter ACP v1 agent provider built on [`ee-acp-agent-server`].
//!
//! [`provider::OpenRouterProvider`] carries the business logic (session
//! history, prompt turns, the bounded tool loop) while the framework owns
//! JSON-RPC dispatch, version negotiation, the session store, typed updates,
//! and agent → client requests.  The binary in `main.rs` only parses
//! arguments and runs the framework over stdio.
//!
//! All `OPENROUTER_*` configuration variables are documented on
//! [`config::Args`]; `.env` parsing lives in [`dotenv`] and never mutates
//! the process environment.  `OPENROUTER_API_KEY` is used only for the
//! Authorization header and is never logged.
//!
//! Orchestrated mode is the default: [`orchestrated::OpenRouterModelAdapter`]
//! feeds OpenRouter into `ee-agent-orchestrator`'s bounded model–tool loop
//! instead of the simple provider mode. `OPENROUTER_ORCHESTRATED=0` is a
//! temporary fallback for diagnostics; ee MCP proxy tool availability depends
//! on the Phase 12 MCP bridge work.

pub mod config;
pub mod dotenv;
pub mod openrouter;
pub mod orchestrated;
pub mod provider;
pub mod tools;
