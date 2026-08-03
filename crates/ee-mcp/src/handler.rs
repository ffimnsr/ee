//! The ee client handler: rmcp `ClientHandler` implementation with policy.
//!
//! Wire handling is entirely rmcp's.  ee-owned policy lives here:
//! - sampling (`create_message`) and roots (`list_roots`) requests are
//!   answered with `method_not_found` — deprecated features are never
//!   implemented as client features.
//! - protocol `logging` notifications become diagnostics-only events.
//! - list-changed notifications invalidate the registry via events.
//! - `elicitation/create` requests are forwarded to the host with a reply
//!   channel; form fields requesting secret-like names are rejected without
//!   ever reaching the host.
//!
//! Sampling/roots/logging types are deprecated by SEP-2577; ee references
//! them only to reject them, so deprecation warnings are intentionally
//! allowed here.
#![allow(deprecated)]

use std::future::Future;
use std::time::Duration;

use rmcp::ClientHandler;
use rmcp::model::{
    CancelledNotificationParam, ClientCapabilities, CreateMessageResult, CustomNotification,
    CustomRequest, CustomResult, ElicitRequestParams, ElicitResult, ElicitationAction,
    ElicitationCapability, ErrorData as McpErrorData, FormElicitationCapability, Implementation,
    InitializeRequestParams, ListRootsResult, LoggingMessageNotificationParam,
    ProgressNotificationParam, ProtocolVersion, ResourceUpdatedNotificationParam,
    UrlElicitationCapability,
};
use rmcp::service::{MaybeSendFuture, NotificationContext, RequestContext, RoleClient};
use tokio::sync::{mpsc, oneshot};

use crate::events::{ElicitationHandle, McpEvent};
use crate::{CLIENT_NAME, CLIENT_VERSION, McpError};

/// Secret-like form field name markers (case-insensitive substring).
const SECRET_FIELD_MARKERS: [&str; 6] =
    ["TOKEN", "KEY", "SECRET", "PASSWORD", "AUTH", "CREDENTIAL"];

/// Whether an elicitation form field name looks secret-like.
#[must_use]
pub fn is_secret_field_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_FIELD_MARKERS.iter().any(|marker| upper.contains(marker))
}

/// The client-side handler served inside every rmcp connection.
#[derive(Debug, Clone)]
pub struct EeClientHandler {
    events: mpsc::UnboundedSender<McpEvent>,
    server_id: String,
    /// How long the handler waits for the host to answer an elicitation.
    elicitation_timeout: Duration,
}

impl EeClientHandler {
    /// Creates a handler for `server_id` that emits events on `events`.
    #[must_use]
    pub fn new(server_id: impl Into<String>, events: mpsc::UnboundedSender<McpEvent>) -> Self {
        Self { events, server_id: server_id.into(), elicitation_timeout: Duration::from_secs(60) }
    }

    /// Emits a host event, swallowing send errors (host gone = no-op).
    fn emit(&self, event: McpEvent) {
        let _ = self.events.send(event);
    }

    /// Forwards an elicitation to the host and awaits the answer.
    async fn bridge_elicitation(
        &self,
        request: ElicitRequestParams,
    ) -> Result<ElicitResult, McpErrorData> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.emit(McpEvent::Elicitation(ElicitationHandle {
            server_id: self.server_id.clone(),
            request,
            reply: reply_tx,
        }));
        match tokio::time::timeout(self.elicitation_timeout, reply_rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(error))) => Err(McpErrorData::internal_error(
                "elicitation failed",
                Some(serde_json::json!({ "reason": error.to_string() })),
            )),
            Ok(Err(_)) => Ok(ElicitResult::new(ElicitationAction::Decline)),
            Err(_) => Ok(ElicitResult::new(ElicitationAction::Decline)),
        }
    }
}

impl ClientHandler for EeClientHandler {
    // Sampling/roots/logging are deprecated by SEP-2577; ee implements their
    // rejection, so referencing the deprecated types here is intentional.
    #[allow(deprecated)]
    fn get_info(&self) -> InitializeRequestParams {
        let mut capabilities = ClientCapabilities::builder().enable_elicitation().build();
        // Field mutation is allowed for non-exhaustive structs; advertise both
        // form and URL elicitation support.
        capabilities.elicitation = Some(
            ElicitationCapability::new()
                .with_form(FormElicitationCapability::new())
                .with_url(UrlElicitationCapability::new()),
        );
        InitializeRequestParams::new(
            capabilities,
            Implementation::new(CLIENT_NAME, CLIENT_VERSION)
                .with_title("ee agent editor")
                .with_description("ee MCP client (2026-07-28 only)"),
        )
        .with_protocol_version(ProtocolVersion::V_2026_07_28)
    }

    fn ping(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> impl Future<Output = Result<(), McpErrorData>> + MaybeSendFuture + '_ {
        std::future::ready(Ok(()))
    }

    /// Sampling is deprecated; ee never fulfils `sampling/createMessage`.
    #[allow(deprecated)]
    fn create_message(
        &self,
        _params: rmcp::model::CreateMessageRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> impl Future<Output = Result<CreateMessageResult, McpErrorData>> + MaybeSendFuture + '_
    {
        std::future::ready(Err(McpErrorData::method_not_found::<
            rmcp::model::CreateMessageRequestMethod,
        >()))
    }

    /// Roots are deprecated; ee never answers `roots/list`.
    #[allow(deprecated)]
    fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> impl Future<Output = Result<ListRootsResult, McpErrorData>> + MaybeSendFuture + '_ {
        std::future::ready(Err(
            McpErrorData::method_not_found::<rmcp::model::ListRootsRequestMethod>(),
        ))
    }

    fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> impl Future<Output = Result<ElicitResult, McpErrorData>> + MaybeSendFuture + '_ {
        // Policy: reject secret-like form fields before they reach the host.
        let this = self.clone();
        Box::pin(async move {
            if let ElicitRequestParams::FormElicitationParams { requested_schema, .. } = &request {
                let secret_fields: Vec<String> = requested_schema
                    .properties
                    .keys()
                    .filter(|name| is_secret_field_name(name))
                    .cloned()
                    .collect();
                if !secret_fields.is_empty() {
                    this.emit(McpEvent::Diagnostics {
                        server_id: this.server_id.clone(),
                        message: format!(
                            "elicitation form rejected: secret-like fields {secret_fields:?}"
                        ),
                    });
                    return Err(McpErrorData::invalid_params(
                        "elicitation form requests secret-like fields; declined by ee policy",
                        None,
                    ));
                }
            }
            this.bridge_elicitation(request).await
        })
    }

    fn on_custom_request(
        &self,
        request: CustomRequest,
        _context: RequestContext<RoleClient>,
    ) -> impl Future<Output = Result<CustomResult, McpErrorData>> + MaybeSendFuture + '_ {
        let method = request.method;
        std::future::ready(Err(McpErrorData::new(
            rmcp::model::ErrorCode::METHOD_NOT_FOUND,
            method,
            None,
        )))
    }

    fn on_cancelled(
        &self,
        _params: CancelledNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + MaybeSendFuture + '_ {
        std::future::ready(())
    }

    fn on_progress(
        &self,
        _params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + MaybeSendFuture + '_ {
        std::future::ready(())
    }

    /// Deprecated protocol `logging`: diagnostics-only, never a feature.
    #[allow(deprecated)]
    fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + MaybeSendFuture + '_ {
        let server_id = self.server_id.clone();
        let events = self.events.clone();
        async move {
            let _ = events.send(McpEvent::Diagnostics {
                server_id,
                message: format!("mcp logging ({:?}): {}", params.level, params.data),
            });
        }
    }

    fn on_resource_updated(
        &self,
        _params: ResourceUpdatedNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + MaybeSendFuture + '_ {
        std::future::ready(())
    }

    fn on_resource_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + MaybeSendFuture + '_ {
        let server_id = self.server_id.clone();
        let events = self.events.clone();
        async move {
            let _ = events.send(McpEvent::ResourceListChanged { server_id });
        }
    }

    fn on_tool_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + MaybeSendFuture + '_ {
        let server_id = self.server_id.clone();
        let events = self.events.clone();
        async move {
            let _ = events.send(McpEvent::ToolListChanged { server_id });
        }
    }

    fn on_prompt_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + MaybeSendFuture + '_ {
        let server_id = self.server_id.clone();
        let events = self.events.clone();
        async move {
            let _ = events.send(McpEvent::PromptListChanged { server_id });
        }
    }

    fn on_subscriptions_acknowledged(
        &self,
        _notification: rmcp::model::SubscriptionsAcknowledgedNotificationParams,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + MaybeSendFuture + '_ {
        std::future::ready(())
    }

    fn on_task_status(
        &self,
        _notification: rmcp::model::TaskStatusNotificationParams,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + MaybeSendFuture + '_ {
        std::future::ready(())
    }

    fn on_custom_notification(
        &self,
        _notification: CustomNotification,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + MaybeSendFuture + '_ {
        std::future::ready(())
    }
}

/// The host-side bridge that answers elicitations (owned by the manager;
/// responses flow back through the oneshot in the event).
#[derive(Debug, Clone, Default)]
pub struct ElicitationBroker {
    events: Option<mpsc::UnboundedSender<McpEvent>>,
}

impl ElicitationBroker {
    /// Creates a broker bound to `events`.
    #[must_use]
    pub fn new(events: mpsc::UnboundedSender<McpEvent>) -> Self {
        Self { events: Some(events) }
    }

    /// Forwards an elicitation request to the host and waits for the reply.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::Cancelled`] when no host is attached or the host
    /// dropped the reply.
    pub async fn request(
        &self,
        server_id: &str,
        request: ElicitRequestParams,
        timeout: Duration,
    ) -> Result<ElicitResult, McpError> {
        let Some(events) = &self.events else {
            return Err(McpError::Cancelled);
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        events
            .send(McpEvent::Elicitation(ElicitationHandle {
                server_id: server_id.to_string(),
                request,
                reply: reply_tx,
            }))
            .map_err(|_| McpError::Cancelled)?;
        tokio::time::timeout(timeout, reply_rx)
            .await
            .map_err(|_| McpError::Timeout { timeout_ms: timeout.as_millis() as u64 })?
            .map_err(|_| McpError::Cancelled)?
    }
}

/// List-changed and logging notifications flow through the handler callbacks
/// above; no raw `ServerNotification` handling exists in ee-owned code.
#[allow(dead_code)]
fn _notification_type_note() {
    let _ = "notifications handled via ClientHandler callbacks";
}
