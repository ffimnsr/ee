//! Prompt task execution and post-settle inbox promotion.
//!
//! One [`PromptRunner`] is shared by a dispatcher and every prompt task it
//! spawns.  The task streams through the provider, reports its completion on
//! the FIFO outbound channel, then settles the session inbox: steer waiters
//! receive the completed turn's result and the next queued prompt starts
//! without a client round-trip.

use std::sync::{Arc, OnceLock};

use ee_agent_protocol::{PromptResponse, ProtocolVersion, RequestId, SessionId};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::client::ClientBridgeFactory;
use crate::error::ProviderError;
use crate::inbox::{PromptInbox, QueuedPrompt, SteerInbox};
use crate::provider::{AgentProvider, PromptContext};
use crate::server::{ActivePrompts, OutboundEvent};
use crate::session::SessionStore;
use crate::updates::UpdateSink;

/// Shared prompt execution state.
pub(crate) struct PromptRunner<P> {
    provider: Arc<P>,
    sessions: Arc<SessionStore>,
    active_prompts: Arc<ActivePrompts>,
    outbound_tx: tokio::sync::mpsc::UnboundedSender<OutboundEvent>,
    client_factory: ClientBridgeFactory,
    inbox: Arc<PromptInbox>,
    negotiated: Arc<OnceLock<ProtocolVersion>>,
}

impl<P: AgentProvider> PromptRunner<P> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        provider: Arc<P>,
        sessions: Arc<SessionStore>,
        active_prompts: Arc<ActivePrompts>,
        outbound_tx: tokio::sync::mpsc::UnboundedSender<OutboundEvent>,
        client_factory: ClientBridgeFactory,
        inbox: Arc<PromptInbox>,
        negotiated: Arc<OnceLock<ProtocolVersion>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            provider,
            sessions,
            active_prompts,
            outbound_tx,
            client_factory,
            inbox,
            negotiated,
        })
    }

    /// Spawns one prompt task.  The registry reservation is owned by the
    /// caller so a promotion race cannot reserve twice.
    pub(crate) fn start(
        self: &Arc<Self>,
        request_id: RequestId,
        ctx: PromptContext,
        cancel: watch::Receiver<bool>,
    ) -> JoinHandle<()> {
        let runner = self.clone();
        let session_id = ctx.session_id.clone();
        tokio::spawn(async move {
            let sink = UpdateSink::new(session_id.clone(), runner.outbound_tx.clone());
            let client = runner.client_factory.bridge_for(&session_id);
            let result = runner.provider.prompt(ctx, sink, client, cancel).await;
            let _ = runner.outbound_tx.send(OutboundEvent::PromptCompleted {
                session_id: session_id.clone(),
                request_id,
                result: result.clone(),
            });
            runner.active_prompts.remove(&session_id);
            runner.settle(session_id, result);
        })
    }

    /// Reports a prompt that cannot be started (session closed or a registry
    /// race) as cancelled so the client never waits forever.
    pub(crate) fn cancel_prompt(&self, session_id: SessionId, request_id: RequestId) {
        let _ = self.outbound_tx.send(OutboundEvent::PromptCompleted {
            session_id,
            request_id,
            result: Err(ProviderError::Cancellation),
        });
    }

    /// Promotes queued work and answers steer requests once a turn settles.
    fn settle(
        self: &Arc<Self>,
        session_id: SessionId,
        result: Result<PromptResponse, ProviderError>,
    ) {
        let finished = self.inbox.finish(&session_id);
        for waiter in finished.waiters {
            let _ = self.outbound_tx.send(OutboundEvent::PromptCompleted {
                session_id: session_id.clone(),
                request_id: waiter,
                result: result.clone(),
            });
        }
        let Some((queued, steer_inbox)) = finished.next else {
            return;
        };
        if !self.sessions.contains(&session_id) {
            let _ = self.inbox.close(&session_id);
            self.cancel_prompt(session_id, queued.request_id);
            return;
        }
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let Ok(generation) = self.active_prompts.start(&session_id, cancel_tx) else {
            // A fresh prompt reserved the session between settle and promote;
            // the queued item cannot run, so report cancellation rather than
            // dropping it silently.
            self.cancel_prompt(session_id, queued.request_id);
            return;
        };
        let request_id = queued.request_id.clone();
        // The queued prompt's `user_message`/running ack was sent when it was
        // accepted; the v2 client needs a fresh running state now that the
        // previous turn has settled.
        if self.negotiated.get() == Some(&ProtocolVersion::V2) {
            let _ = self.outbound_tx.send(OutboundEvent::UpdateV2 {
                session_id: session_id.clone(),
                update: Box::new(crate::v2::state_running()),
            });
        }
        let ctx = self.promoted_context(session_id.clone(), queued, steer_inbox);
        let join = self.start(request_id, ctx, cancel_rx);
        self.active_prompts.attach_join(&session_id, generation, join);
    }

    fn promoted_context(
        &self,
        session_id: SessionId,
        queued: QueuedPrompt,
        steer_inbox: SteerInbox,
    ) -> PromptContext {
        let mut ctx = PromptContext::new(session_id, queued.prompt).with_steer(steer_inbox);
        ctx.metadata = queued.metadata;
        ctx
    }
}
