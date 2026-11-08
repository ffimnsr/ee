//! External wire protocol.
//!
//! Native process and Wasm plugins use the same logical methods. There are no
//! v1 methods, no `HostNotification`/`PluginRequest` compatibility types, and no
//! legacy namespace alongside these variants.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::contribution::Contribution;
use crate::effect::Effect;
use crate::host::HostKind;
use crate::ids::{CommandId, ContributionId, PluginId, ServiceId};
use crate::manifest::Limits;

/// Messages sent by a host to a plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "method", content = "params")]
pub enum HostToPlugin {
    #[serde(rename = "plugin/initialize")]
    Initialize(InitializeParams),
    #[serde(rename = "plugin/event")]
    Event(EventParams),
    #[serde(rename = "plugin/command")]
    Command(CommandParams),
    #[serde(rename = "plugin/contribution/request")]
    ContributionRequest(ContributionRequestParams),
    #[serde(rename = "plugin/shutdown")]
    Shutdown(ShutdownParams),
    /// Response to a `plugin/request` service call. The host is the responder
    /// for service calls initiated by the plugin.
    #[serde(rename = "plugin/response")]
    Response(PluginResponse),
}

/// Messages sent by a plugin to a host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "method", content = "params")]
pub enum PluginToHost {
    #[serde(rename = "plugin/request")]
    Request(ServiceRequestParams),
    #[serde(rename = "plugin/effect")]
    Effect(EffectEnvelope),
    #[serde(rename = "plugin/contribution")]
    Contribution(ContributionEnvelope),
    #[serde(rename = "plugin/health")]
    Health(HealthReport),
    #[serde(rename = "plugin/response")]
    Response(PluginResponse),
}

/// Initial handshake payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InitializeParams {
    pub plugin_id: PluginId,
    pub host: HostKind,
    pub api: String,
    #[serde(default)]
    #[schemars(skip)]
    pub services: Vec<ServiceId>,
    #[serde(default)]
    #[schemars(skip)]
    pub config: Value,
    #[serde(default)]
    pub limits: Limits,
}

/// Bounded event delivery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EventParams {
    pub name: String,
    pub revision: u64,
    #[schemars(skip)]
    pub payload: Value,
}

/// Command invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CommandParams {
    pub id: CommandId,
    pub request_id: u64,
    #[schemars(skip)]
    pub args: Value,
    pub deadline_ms: u64,
}

/// Request for a fresh contribution snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContributionRequestParams {
    pub revision: u64,
}

/// Shutdown notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ShutdownParams {
    pub reason: String,
}

/// Service call from a plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ServiceRequestParams {
    pub service: ServiceId,
    pub method: String,
    pub request_id: u64,
    #[schemars(skip)]
    pub params: Value,
}

/// Effects produced by a command or event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EffectEnvelope {
    #[serde(default)]
    pub request_id: Option<u64>,
    #[serde(default)]
    pub effects: Vec<Effect>,
}

/// Contribution upsert/remove snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ContributionEnvelope {
    pub revision: u64,
    #[serde(default)]
    pub upsert: Vec<Contribution>,
    #[serde(default)]
    #[schemars(skip)]
    pub remove: Vec<ContributionId>,
}

/// Health state report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HealthReport {
    pub state: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Response to a host request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PluginResponse {
    pub request_id: u64,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<WireError>,
}

/// Typed error payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WireError {
    pub class: String,
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contribution::{CommandContribution, StatusAlignment, StatusItemContribution};
    use crate::effect::StyleRole;

    fn service() -> ServiceId {
        ServiceId::from_static("editor.document@1")
    }

    fn host_messages() -> Vec<HostToPlugin> {
        vec![
            HostToPlugin::Initialize(InitializeParams {
                plugin_id: PluginId::new("demo").unwrap(),
                host: HostKind::Editor,
                api: crate::manifest::API_VERSION.to_owned(),
                services: vec![service()],
                config: serde_json::json!({"enabled": true}),
                limits: Limits::default(),
            }),
            HostToPlugin::Event(EventParams {
                name: "editor.document.changed".to_owned(),
                revision: 3,
                payload: serde_json::json!({"rev": 3}),
            }),
            HostToPlugin::Command(CommandParams {
                id: CommandId::new("demo.run").unwrap(),
                request_id: 7,
                args: serde_json::json!({}),
                deadline_ms: 1000,
            }),
            HostToPlugin::ContributionRequest(ContributionRequestParams { revision: 4 }),
            HostToPlugin::Shutdown(ShutdownParams { reason: "host exit".to_owned() }),
            HostToPlugin::Response(PluginResponse {
                request_id: 9,
                result: None,
                error: Some(WireError {
                    class: "runtime".to_owned(),
                    code: "plugin.unsupported".to_owned(),
                    message: "not available".to_owned(),
                    retryable: false,
                }),
            }),
        ]
    }

    fn plugin_messages() -> Vec<PluginToHost> {
        vec![
            PluginToHost::Request(ServiceRequestParams {
                service: service(),
                method: "snapshot".to_owned(),
                request_id: 1,
                params: serde_json::json!({}),
            }),
            PluginToHost::Effect(EffectEnvelope {
                request_id: Some(1),
                effects: vec![Effect::SetDecorations(crate::effect::DecorationPatch {
                    view_id: crate::model::ViewId(1),
                    rev: 2,
                    decoration_kind: "demo".to_owned(),
                    spans: vec![crate::effect::DecorationSpan {
                        start: 0,
                        end: 1,
                        style: StyleRole::new("syntax.keyword"),
                        text: None,
                    }],
                })],
            }),
            PluginToHost::Contribution(ContributionEnvelope {
                revision: 5,
                upsert: vec![
                    Contribution::Command(CommandContribution {
                        id: CommandId::new("demo.run").unwrap(),
                        title: "Run".to_owned(),
                        description: String::new(),
                        category: None,
                        args_schema: None,
                    }),
                    Contribution::StatusItem(StatusItemContribution {
                        key: "demo.status".to_owned(),
                        text: "ok".to_owned(),
                        alignment: StatusAlignment::Left,
                        priority: 0,
                    }),
                ],
                remove: vec![ContributionId::new("old.item").unwrap()],
            }),
            PluginToHost::Health(HealthReport {
                state: "degraded".to_owned(),
                reason: Some("timeout".to_owned()),
            }),
            PluginToHost::Response(PluginResponse {
                request_id: 1,
                result: Some(serde_json::json!({"ok": true})),
                error: None,
            }),
        ]
    }

    #[test]
    fn host_messages_round_trip() {
        for message in host_messages() {
            let value = serde_json::to_value(&message).unwrap();
            assert!(value.get("method").is_some());
            let decoded: HostToPlugin = serde_json::from_value(value).unwrap();
            assert_eq!(decoded, message);
        }
    }

    #[test]
    fn plugin_messages_round_trip() {
        for message in plugin_messages() {
            let value = serde_json::to_value(&message).unwrap();
            assert!(value.get("method").is_some());
            let decoded: PluginToHost = serde_json::from_value(value).unwrap();
            assert_eq!(decoded, message);
        }
    }

    #[test]
    fn method_names_are_stable() {
        let initialize = serde_json::to_value(&host_messages()[0]).unwrap();
        assert_eq!(initialize["method"], "plugin/initialize");

        let effects = serde_json::to_value(&plugin_messages()[1]).unwrap();
        assert_eq!(effects["method"], "plugin/effect");
    }
}
