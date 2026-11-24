//! `session/set_config_option` dispatch.
//!
//! Kept out of `dispatch.rs` so the JSON-RPC router stays below the
//! repository's 1,000-line threshold. The framework validates the advertised
//! option and session state; the provider owns the semantic value check and
//! returns the updated option list.

use ee_agent_protocol::{
    ProtocolVersion, SessionConfigId, SessionConfigOptionValue, SessionId,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, v2,
};
use serde_json::Value;

use super::{DispatchOutcome, RequestDispatcher, parse_params};
use crate::error::AcpServerError;
use crate::provider::{AgentProvider, SetConfigOptionContext};

/// Handles one `session/set_config_option` request for the negotiated protocol.
///
/// A running turn keeps the configuration it started with; switching
/// mid-turn is rejected before the provider sees it, matching model switches.
pub(crate) async fn dispatch<P: AgentProvider>(
    dispatcher: &RequestDispatcher<P>,
    params: Value,
) -> DispatchOutcome {
    DispatchOutcome::Immediate(session_set_config_option(dispatcher, params).await)
}

async fn session_set_config_option<P: AgentProvider>(
    dispatcher: &RequestDispatcher<P>,
    params: Value,
) -> Result<Value, ee_agent_protocol::Error> {
    let is_v2 = dispatcher.negotiated.get() == Some(&ProtocolVersion::V2);
    let (session_id, config_id, value, metadata) = if is_v2 {
        let request: v2::SetSessionConfigOptionRequest = parse_params(params)?;
        let value = translate_value(&request.value)?;
        (
            SessionId::new(request.session_id.0.to_string()),
            SessionConfigId::new(request.config_id.0.to_string()),
            value,
            request.meta,
        )
    } else {
        let request: SetSessionConfigOptionRequest = parse_params(params)?;
        (request.session_id, request.config_id, request.value, request.meta)
    };

    validate_advertised_option(dispatcher, &session_id, &config_id)?;
    if dispatcher.active_prompts.contains(&session_id) {
        return Err(ee_agent_protocol::Error::invalid_params().data(serde_json::json!({
            "reason": "cannot change session configuration while a prompt is active; wait for the turn to settle or cancel it",
        })));
    }
    let config_options = dispatcher
        .with_provider_timeout(dispatcher.provider.set_config_option(SetConfigOptionContext {
            session_id: session_id.clone(),
            config_id,
            value,
            metadata,
        }))
        .await?;
    if !dispatcher.sessions.set_config_options(&session_id, config_options.clone()) {
        return Err(ee_agent_protocol::Error::internal_error().data(serde_json::json!({
            "reason": "session disappeared before the configuration change completed",
        })));
    }
    let response = if is_v2 {
        serde_json::to_value(v2::SetSessionConfigOptionResponse::new(
            crate::v2::translate_config_options(&config_options),
        ))
    } else {
        serde_json::to_value(SetSessionConfigOptionResponse::new(config_options))
    };
    response.map_err(|error| {
        ee_agent_protocol::Error::internal_error()
            .data(serde_json::json!({ "reason": error.to_string() }))
    })
}

/// Rejects unknown sessions and options before any provider call.
fn validate_advertised_option<P: AgentProvider>(
    dispatcher: &RequestDispatcher<P>,
    session_id: &SessionId,
    config_id: &SessionConfigId,
) -> Result<(), ee_agent_protocol::Error> {
    let Some(session) = dispatcher.sessions.get(session_id) else {
        return Err(AcpServerError::UnknownSession(session_id.clone()).into_rpc_error());
    };
    if !session.config_options.iter().any(|option| option.id == *config_id) {
        return Err(ee_agent_protocol::Error::invalid_params().data(serde_json::json!({
            "reason": format!(
                "config option {config_id:?} was not advertised for this session"
            ),
        })));
    }
    Ok(())
}

/// Converts the v2 typed value into the v1 provider contract value.
fn translate_value(
    value: &v2::SessionConfigOptionValue,
) -> Result<SessionConfigOptionValue, ee_agent_protocol::Error> {
    match value {
        v2::SessionConfigOptionValue::Id { value } => {
            Ok(SessionConfigOptionValue::value_id(value.0.to_string()))
        }
        v2::SessionConfigOptionValue::Boolean { value } => {
            Ok(SessionConfigOptionValue::boolean(*value))
        }
        _ => Err(ee_agent_protocol::Error::invalid_params().data(serde_json::json!({
            "reason": "unsupported session config option value type",
        }))),
    }
}
