use super::*;

use super::inbound_dispatch::dispatch_client_request;
use super::inbound_dispatch::handle_elicitation_complete;
use super::inbound_dispatch::handle_permission_request;
use super::inbound_dispatch::handle_permission_request_v2;
use super::inbound_dispatch::handle_session_notification;
use super::inbound_dispatch::handle_v2_session_notification;

/// Builds the SDK client with typed handlers for every agent-to-client
/// request ACP v1 defines.
/// Builds the SDK client with typed handlers for every agent-to-client
/// request ACP v1 defines.
pub(super) fn build_client_builder(
    inner: Arc<AgentConnectionInner>,
) -> ee_agent_protocol::Builder<
    ClientRole,
    impl ee_agent_protocol::HandleDispatchFrom<AgentRole>,
    impl ee_agent_protocol::RunWithConnectionTo<AgentRole>,
    impl ee_agent_protocol::HandleConnectionClose<AgentRole>,
> {
    ClientRole
        .builder()
        .name(format!("ee-agent-host:{}", inner.agent_id))
        .on_receive_notification(
            {
                let inner = inner.clone();
                async move |notification: SessionNotification, _cx| {
                    handle_session_notification(notification, &inner);
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        .on_receive_notification(
            {
                let inner = inner.clone();
                async move |notification: CompleteElicitationNotification, _cx| {
                    handle_elicitation_complete(notification, &inner);
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        .on_receive_notification(
            {
                let inner = inner.clone();
                async move |notification: CancelRequestNotification, _cx| {
                    let cancelled = inner.cancel_client_request(&notification.request_id);
                    tracing::debug!(
                        agent_id = %inner.agent_id,
                        request_id = ?notification.request_id,
                        cancelled,
                        "received $/cancel_request for client request"
                    );
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: RequestPermissionRequest, responder, cx| {
                    handle_permission_request(request, responder, &cx, &inner)
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: ReadTextFileRequest, responder, cx| {
                    dispatch_client_request(
                        &inner,
                        ClientRequest::ReadTextFile(request),
                        responder.erase_to_json(),
                        &cx,
                    )
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: WriteTextFileRequest, responder, cx| {
                    dispatch_client_request(
                        &inner,
                        ClientRequest::WriteTextFile(request),
                        responder.erase_to_json(),
                        &cx,
                    )
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: CreateTerminalRequest, responder, cx| {
                    dispatch_client_request(
                        &inner,
                        ClientRequest::CreateTerminal(request),
                        responder.erase_to_json(),
                        &cx,
                    )
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: TerminalOutputRequest, responder, cx| {
                    dispatch_client_request(
                        &inner,
                        ClientRequest::TerminalOutput(request),
                        responder.erase_to_json(),
                        &cx,
                    )
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: WaitForTerminalExitRequest, responder, cx| {
                    dispatch_client_request(
                        &inner,
                        ClientRequest::WaitForTerminalExit(request),
                        responder.erase_to_json(),
                        &cx,
                    )
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: KillTerminalRequest, responder, cx| {
                    dispatch_client_request(
                        &inner,
                        ClientRequest::KillTerminal(request),
                        responder.erase_to_json(),
                        &cx,
                    )
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: ReleaseTerminalRequest, responder, cx| {
                    dispatch_client_request(
                        &inner,
                        ClientRequest::ReleaseTerminal(request),
                        responder.erase_to_json(),
                        &cx,
                    )
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: CreateElicitationRequest, responder, cx| {
                    dispatch_client_request(
                        &inner,
                        ClientRequest::CreateElicitation(request),
                        responder.erase_to_json(),
                        &cx,
                    )
                }
            },
            on_receive_request!(),
        )
        // ACP-native MCP-over-ACP for the ee proxy.  These use the
        // official SDK request types (method metadata from
        // `CLIENT_METHOD_NAMES`); strict ordering/identity rules live in
        // `crate::mcp_over_acp`.
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: ConnectMcpRequest, responder, cx| {
                    let _ = cx;
                    inner.mcp.handle_connect(request, responder, inner.agent_advertises_acp())
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: MessageMcpRequest, responder, cx| {
                    inner.mcp.handle_message(request, responder, &cx, inner.agent_advertises_acp())
                }
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            {
                let inner = inner.clone();
                async move |notification: MessageMcpNotification, _cx| {
                    inner.mcp.handle_notification(notification);
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: DisconnectMcpRequest, responder, cx| {
                    let _ = cx;
                    inner.mcp.handle_disconnect(request, responder)
                }
            },
            on_receive_request!(),
        )
}

/// Builds the SDK client for a v2 connection.
///
/// v2 removes the client fs/terminal/elicitation request surface, so none of
/// those handlers are registered (a conforming v2 agent never sends them; a
/// non-conforming one gets a method-not-found response).  Only the v2-typed
/// session surface is handled: updates (including the turn-ending idle
/// `state_update`), permission requests, and `$/cancel_request`.
pub(super) fn build_client_builder_v2(
    inner: Arc<AgentConnectionInner>,
) -> ee_agent_protocol::Builder<
    ClientRole,
    impl ee_agent_protocol::HandleDispatchFrom<AgentRole>,
    impl ee_agent_protocol::RunWithConnectionTo<AgentRole>,
    impl ee_agent_protocol::HandleConnectionClose<AgentRole>,
> {
    // SDK >= 2.1: `Role::v2()` yields a `V2Builder` whose callbacks receive
    // version-typed `V2ConnectionTo` contexts.  The host driver is written
    // against the version-neutral [`ConnectionTo`]; `with_v2_protocol_guard`
    // applies the same wire validation while retaining raw callback contexts.
    ClientRole
        .builder()
        .with_v2_protocol_guard()
        .name(format!("ee-agent-host:{}:v2", inner.agent_id))
        .on_receive_notification(
            {
                let inner = inner.clone();
                async move |notification: v2::UpdateSessionNotification, _cx| {
                    handle_v2_session_notification(notification, &inner);
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        .on_receive_notification(
            {
                let inner = inner.clone();
                async move |notification: v2::CancelRequestNotification, _cx| {
                    let cancelled = inner.cancel_client_request(&notification.request_id);
                    tracing::debug!(
                        agent_id = %inner.agent_id,
                        request_id = ?notification.request_id,
                        cancelled,
                        "received $/cancel_request for client request (v2)"
                    );
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            {
                let inner = inner.clone();
                async move |request: v2::RequestPermissionRequest, responder, cx| {
                    handle_permission_request_v2(request, responder, &cx, &inner)
                }
            },
            on_receive_request!(),
        )
}

/// The client-side capabilities advertised during `initialize`, derived from
/// the registered handler so nothing unsupported is ever advertised.
pub(super) fn client_capabilities(
    handler_capabilities: &HandlerCapabilities,
) -> ClientCapabilities {
    let mut capabilities = ClientCapabilities::new();
    if handler_capabilities.fs_read || handler_capabilities.fs_write {
        capabilities = capabilities.fs(FileSystemCapabilities::new()
            .read_text_file(handler_capabilities.fs_read)
            .write_text_file(handler_capabilities.fs_write));
    }
    if handler_capabilities.terminal {
        capabilities = capabilities.terminal(true);
    }
    if handler_capabilities.session_config_boolean {
        capabilities = capabilities.session(ClientSessionCapabilities::new().config_options(
            SessionConfigOptionsCapabilities::new().boolean(BooleanConfigOptionCapabilities::new()),
        ));
    }
    if handler_capabilities.elicitation_form || handler_capabilities.elicitation_url {
        let mut elicitation = ElicitationCapabilities::new();
        if handler_capabilities.elicitation_form {
            elicitation = elicitation.form(ElicitationFormCapabilities::new());
        }
        if handler_capabilities.elicitation_url {
            elicitation = elicitation.url(ElicitationUrlCapabilities::new());
        }
        capabilities = capabilities.elicitation(elicitation);
    }
    capabilities
}
