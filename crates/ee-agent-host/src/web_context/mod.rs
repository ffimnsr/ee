//! Safe, bounded retrieval primitives for optional agent web context.
//!
//! This module deliberately has no default network transport. Callers provide a
//! transport which resolves DNS and reports its connected peer. That proof is
//! required to make DNS-rebinding checks testable and enforceable.

pub(crate) use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Mutex,
    time::{Duration, Instant, SystemTime},
};

pub(crate) use base64::Engine;
pub(crate) use futures::{StreamExt, future::BoxFuture};
pub(crate) use serde::{Deserialize, Serialize};
pub(crate) use tokio_util::sync::CancellationToken;
pub(crate) use url::{Host, Url};
pub(crate) use zeroize::Zeroizing;

mod cache;
mod config;
mod consts;
mod errors;
mod html;
mod normalize;
mod providers;
mod service;
mod transport;
mod types;
mod validation;
mod vendors;

mod defaults;

#[cfg(test)]
mod tests;

pub use config::*;
pub use consts::*;
pub use errors::*;
pub use providers::*;
pub use service::*;
pub use transport::*;
pub use types::*;
pub use validation::*;
pub use vendors::*;
