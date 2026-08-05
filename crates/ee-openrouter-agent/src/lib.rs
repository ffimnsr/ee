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
//! `OPENROUTER_ORCHESTRATED=1` switches the binary into orchestrated mode:
//! [`orchestrated::OpenRouterModelAdapter`] feeds OpenRouter into
//! `ee-agent-orchestrator`'s bounded model–tool loop instead of the simple
//! provider mode.

pub mod config;
pub mod dotenv;
pub mod openrouter;
pub mod orchestrated;
pub mod provider;
pub mod tools;
