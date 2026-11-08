//! Typed event buses.
//!
//! Each event type declares a stable [`Event::NAME`]. Hosts use the sync bus on
//! the editor hot path and the async bus behind the `async` feature for agent
//! dispatch. Registration and disposal semantics are identical.

mod bus;
#[cfg(feature = "async")]
mod cancel;

#[cfg(feature = "async")]
mod async_bus;

#[cfg(feature = "async")]
pub use async_bus::AsyncEventBus;
pub use bus::{EventBus, Waterfall};
#[cfg(feature = "async")]
pub use cancel::{CancelFuture, CancelToken};

/// Maximum listeners registered for one event type and mode.
pub const MAX_LISTENERS: usize = 128;

/// Maximum nested dispatch depth before the bus fails closed.
pub const MAX_DISPATCH_DEPTH: usize = 16;

/// A typed event delivered through a bus.
///
/// `NAME` is part of the external plugin API: it appears in manifests,
/// diagnostics, and the wire protocol. Renaming it is a breaking change.
pub trait Event: 'static {
    const NAME: &'static str;
}

/// Errors produced by event registration and dispatch.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EventError {
    #[error("event `{name}` exceeded the listener limit of {max}")]
    ListenerLimit { name: &'static str, max: usize },
    #[error("event dispatch exceeded the depth limit of {max}")]
    DispatchDepth { max: usize },
    #[error("event `{name}` was cancelled")]
    Cancelled { name: &'static str },
    #[error("event `{name}` bail listener returned a mismatched result type")]
    BailTypeMismatch { name: &'static str },
}
