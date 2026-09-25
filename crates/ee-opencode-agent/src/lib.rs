//! OpenCode Zen and OpenCode Go agent with exact surface routing.
//!
//! OpenCode publishes two independent service surfaces whose model ids overlap
//! while their endpoints and protocol dialects differ, so a model id alone never
//! identifies a request shape. This crate therefore routes by exact
//! `(surface, model)` catalog data instead of assuming every model speaks one
//! OpenAI-shaped API.
//!
//! - [`routes`] is the auditable routing truth: the three dialect identities,
//!   the per-surface catalog, and [`routes::resolve_route`], which fails closed
//!   before any HTTP client, request body, or `Authorization` header exists.
//! - [`config`] defines [`config::ConfigError`], the explicit
//!   `OPENCODE_SURFACE` / `OPENCODE_MODEL` selection, and
//!   [`config::route_profile`], which turns one resolved route into the endpoint
//!   profile its codec sends with.
//! - [`responses`], [`messages`], and [`chat_completions`] are the three
//!   protocol codecs. Each one encodes a normalized transcript, decodes buffered
//!   and streamed output into the same normalized `ModelResponse`, and fails
//!   closed on malformed protocol data before any tool can run. The Responses and
//!   Messages codecs are dialect-specific; the Chat Completions codec is the
//!   shared `ee-chat-completions` client, so nothing is duplicated from
//!   OpenRouter.
//! - [`adapter`] is the production path: [`adapter::OpenCodeModelAdapter`]
//!   builds only the routed dialect's codec, preserves orchestrator cancellation
//!   and streamed-update ordering, and reports failures without a credential or
//!   a raw provider body. [`critic::opencode_multi_model_provider`] wires one or
//!   two of those adapters into `ee-agent-orchestrator` with the shared ee agent
//!   tool policy, and the binary runs that provider over
//!   `ee-acp-agent-server`'s stdio loop, so there is no second ACP loop, tool
//!   executor, or simple provider mode.
//! - [`critic`] adds the optional second opinion: `OPENCODE_CRITIC_MODEL` names one
//!   more catalog id on the same surface, resolved through the same exact routing
//!   and registered as the rubber-duck critic when its declared vendor family
//!   differs ([`family`]). An unusable critic degrades to root-only operation with
//!   one bounded warning instead of failing startup.
//! - [`family`] holds the declared vendor families contrast needs, and
//!   [`reasoning`] maps `OPENCODE_REASONING_EFFORT` onto the field each dialect
//!   documents.
//! - [`discovery`] is the bounded, display-only `/models` probe behind
//!   `--discover-models`: one explicit local action against the trusted surface
//!   root whose output can never select an endpoint, dialect, or route.
//!
//! `OPENCODE_API_KEY` is never read here: route resolution performs no I/O, and
//! no endpoint can be supplied through the environment, so a rejected model
//! cannot disclose a credential to an arbitrary origin.

pub mod adapter;
pub mod chat_completions;
pub mod config;
pub mod critic;
pub mod dialect;
pub mod discovery;
pub mod family;
pub mod messages;
pub mod reasoning;
pub mod responses;
pub mod routes;
