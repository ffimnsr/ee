// Copyright 2018 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::{Arc, Mutex, MutexGuard, Weak};

use serde_json::Value;

use xi_rpc::{Handler, RemoteError, RpcCtx};

use crate::rpc::*;
use crate::tabs::CoreState;
use crate::tracing_support;

/// A reference to the main core state.
///
/// # Note
///
/// Various items of initial setup are dependent on how the client
/// is configured, so we defer instantiating state until we have that
/// information.
pub enum XiCore {
    // TODO: profile startup, and determine what things
    // we should be doing before client_init.
    Waiting,
    Running(Arc<Mutex<CoreState>>),
}

/// A weak reference to the main state. This is passed to plugin threads.
#[derive(Clone)]
pub struct WeakXiCore(Weak<Mutex<CoreState>>);

#[allow(dead_code)]
impl XiCore {
    pub fn new() -> Self {
        tracing_support::install();
        XiCore::Waiting
    }

    /// Run one plugin host cycle outside the core lock.
    ///
    /// Native transports may issue service calls while pumping; those services
    /// lock the core mutex, so the pump must not happen with the guard held.
    fn plugin_cycle(&self) {
        if let Some(weak) = self.weak_self() {
            weak.plugin_cycle();
        }
    }

    /// Returns `true` if the `client_started` has not been received.
    fn is_waiting(&self) -> bool {
        matches!(*self, XiCore::Waiting)
    }

    /// Returns a guard to the core state. A convenience around `Mutex::lock`.
    ///
    /// # Panics
    ///
    /// Panics if core has not yet received the `client_started` message.
    pub fn inner(&self) -> MutexGuard<'_, CoreState> {
        match self {
            XiCore::Running(inner) => inner.lock().unwrap(),
            XiCore::Waiting => panic!(
                "core does not start until client_started \
                 RPC is received"
            ),
        }
    }

    /// Returns a new reference to the core state, if core is running.
    fn weak_self(&self) -> Option<WeakXiCore> {
        match self {
            XiCore::Running(inner) => Some(WeakXiCore(Arc::downgrade(inner))),
            XiCore::Waiting => None,
        }
    }
}

/// Handler for messages originating with the frontend.
impl Handler for XiCore {
    type Notification = CoreNotification;
    type Request = CoreRequest;

    fn handle_notification(&mut self, ctx: &RpcCtx, rpc: Self::Notification) {
        use self::CoreNotification::*;

        // wait for client_started before setting up inner
        if let ClientStarted { ref config_dir, ref client_extras_dir } = rpc {
            assert!(self.is_waiting(), "client_started can only be sent once");
            let state =
                CoreState::new(ctx.get_peer(), config_dir.clone(), client_extras_dir.clone());
            let state = Arc::new(Mutex::new(state));
            *self = XiCore::Running(state);
            let weak_self = self.weak_self().unwrap();
            self.inner().finish_setup(weak_self);
        }

        self.inner().client_notification(rpc);
        self.plugin_cycle();
    }

    fn handle_request(
        &mut self,
        _ctx: &RpcCtx,
        rpc: Self::Request,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Value, RemoteError> {
        let result = self.inner().client_request(rpc);
        self.plugin_cycle();
        result
    }

    fn idle(&mut self, _ctx: &RpcCtx, token: usize) {
        self.inner().handle_idle(token);
        self.plugin_cycle();
    }
}

impl WeakXiCore {
    /// Run a closure with core state locked without blocking.
    ///
    /// Plugin services must fail closed rather than deadlock when the core
    /// mutex is already held by the current event.
    pub(crate) fn with_state<R>(&self, operation: impl FnOnce(&mut CoreState) -> R) -> Option<R> {
        let core = self.0.upgrade()?;
        let mut guard = core.try_lock().ok()?;
        Some(operation(&mut guard))
    }

    /// Attempts to upgrade the weak reference. Essentially a wrapper
    /// for `Arc::upgrade`.
    /// Pump native plugin transports and apply queued effects.
    pub(crate) fn plugin_cycle(&self) {
        let (host, queued_commands, languages) = self
            .with_state(|core| {
                (
                    core.plugin_host.clone(),
                    std::mem::take(&mut core.queued_plugin_commands),
                    std::mem::take(&mut core.pending_plugin_language_activations),
                )
            })
            .unwrap_or((None, Vec::new(), Vec::new()));
        let Some(host) = host else {
            return;
        };
        if !host.is_started()
            && !host.is_shutdown()
            && let Err(error) = host.start()
        {
            tracing::warn!("failed to start plugin host: {error}");
        }
        for language in languages {
            for (plugin, error) in host.activate_for_language(&language) {
                tracing::warn!("failed to activate plugin `{plugin}` for `{language}`: {error}");
            }
        }
        if host.take_reload_request()
            && let Err(error) = host.reload_paths(&[])
        {
            tracing::warn!("failed to reload plugins: {error}");
        }
        for (id, view, args) in queued_commands {
            if let Err(error) = host.dispatch_command(&id, view, args) {
                tracing::warn!("plugin command `{id}` failed: {error}");
            }
        }
        let _ = host.pump();
        let effects = host.take_effects();
        let generation = host.generation();
        self.with_state(|core| {
            if !effects.is_empty() {
                core.apply_plugin_effects(effects);
            }
            if core.plugin_generation != generation {
                core.plugin_generation = generation;
                core.plugin_refresh_languages();
            }
            core.plugin_send_notifications();
        });
    }
}

#[cfg(test)]
/// Returns a non-functional `WeakXiRef`, needed to mock other types.
pub fn dummy_weak_core() -> WeakXiCore {
    use xi_rpc::Peer;
    use xi_rpc::test_utils::DummyPeer;
    let peer = Box::new(DummyPeer);
    let state = CoreState::new(&peer.box_clone(), None, None);
    let core = Arc::new(Mutex::new(state));
    WeakXiCore(Arc::downgrade(&core))
}
