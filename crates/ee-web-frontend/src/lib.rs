//! Rust/WASM web chat frontend for the ee headless agent host.
//!
//! The crate has two layers:
//!
//! - [`model`] + [`protocol`]: plain Rust, host-testable, DOM-free, and the
//!   only place UI state transitions happen;
//! - `client` + `ui` (wasm32 only): WebSocket transport and DOM rendering.
//!   They contain no policy: every mutation is a registry method call, every
//!   transcript line comes from backend notifications, and the DOM is built
//!   from text nodes only (no `innerHTML`).
//!
//! Build with `scripts/build-web-frontend.sh`; the only non-Rust files in this
//! crate are the static HTML shell and stylesheet, plus toolchain-generated
//! loader glue under `dist/`.

pub mod model;
pub mod protocol;

#[cfg(target_arch = "wasm32")]
mod client;
#[cfg(target_arch = "wasm32")]
mod ui;

pub use model::{
    AppModel, ApprovalCard, ApprovalOption, ConnectionState, TranscriptItem, ViewState,
};
pub use protocol::SessionEntry;

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

/// WASM entry point. wasm-bindgen's generated loader calls this on load.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(start)]
pub fn start() {
    ui::start();
}
