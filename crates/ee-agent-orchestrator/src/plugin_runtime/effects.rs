//! Structured agent effects.
//!
//! Agent plugins never write editor UI. A plugin command or listener may return
//! only:
//!
//! - a structured [`ToolResultEffect`] (the editor host renders it through the
//!   existing ACP and `agent_tool_result` surfaces),
//! - a [`ContextUpdateEffect`] for bounded, trust-labeled turn context, or
//! - a [`SessionControlEffect`] for cancel, checkpoint, and close.
//!
//! Effects are applied in the order they were produced, after the borrow that
//! produced them ended. A panic in one effect is isolated.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;

use ee_plugin_api::effect::Effect;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::{ContributionId, PluginId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::plugin_runtime::services::ContextContribution;
use crate::trust::TrustLevel;

/// One tool result produced by an agent plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultEffect {
    pub call_id: String,
    pub is_error: bool,
    pub content: Value,
}

/// Bounded context contributed by one plugin for one session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextUpdateEffect {
    pub session_id: String,
    pub provider_id: String,
    pub trust: TrustLevel,
    pub revision: u64,
    pub items: Vec<ContextContribution>,
}

/// Session lifecycle control requested by a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SessionControlEffect {
    Cancel { session_id: String },
    Checkpoint { session_id: String, checkpoint_id: String },
    Close { session_id: String, reason: String },
}

impl SessionControlEffect {
    /// Session the control applies to.
    #[must_use]
    pub fn session_id(&self) -> &str {
        match self {
            Self::Cancel { session_id }
            | Self::Checkpoint { session_id, .. }
            | Self::Close { session_id, .. } => session_id,
        }
    }
}

/// Any state change an agent plugin may request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEffect {
    ToolResult(ToolResultEffect),
    ContextUpdate(ContextUpdateEffect),
    SessionControl(SessionControlEffect),
    Noop,
}

impl AgentEffect {
    /// Converts one wire effect into an agent effect.
    ///
    /// Editor-only effects fail closed: an agent plugin has no editor surface.
    pub fn from_plugin_effect(effect: Effect) -> Result<Self, PluginError> {
        match effect {
            Effect::EmitAgentToolResult(result) => Ok(Self::ToolResult(ToolResultEffect {
                call_id: result.call_id,
                is_error: result.is_error,
                content: result.content,
            })),
            Effect::Noop => Ok(Self::Noop),
            _ => Err(PluginError::Unsupported { feature: "editor effects in the agent host" }),
        }
    }
}

/// One queued effect with its originating plugin.
#[derive(Debug, Clone)]
pub struct QueuedAgentEffect {
    pub owner: Option<PluginId>,
    pub effect: AgentEffect,
}

/// Thread-safe queue drained by the agent host.
#[derive(Default)]
pub struct AgentEffectQueue {
    effects: Mutex<Vec<QueuedAgentEffect>>,
}

impl AgentEffectQueue {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, owner: Option<PluginId>, effect: AgentEffect) {
        self.effects
            .lock()
            .expect("agent effect lock poisoned")
            .push(QueuedAgentEffect { owner, effect });
    }

    pub fn push_batch(&self, owner: Option<PluginId>, effects: Vec<AgentEffect>) {
        let mut queue = self.effects.lock().expect("agent effect lock poisoned");
        queue.extend(
            effects.into_iter().map(|effect| QueuedAgentEffect { owner: owner.clone(), effect }),
        );
    }

    pub fn drain(&self) -> Vec<QueuedAgentEffect> {
        std::mem::take(&mut *self.effects.lock().expect("agent effect lock poisoned"))
    }

    pub fn len(&self) -> usize {
        self.effects.lock().expect("agent effect lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Boundary that applies agent effects.
pub trait AgentEffectSink: Send + Sync {
    fn effect_tool_result(
        &self,
        owner: Option<&PluginId>,
        result: &ToolResultEffect,
    ) -> Result<(), PluginError>;

    fn effect_context_update(
        &self,
        owner: Option<&PluginId>,
        update: &ContextUpdateEffect,
    ) -> Result<(), PluginError>;

    fn effect_session_control(
        &self,
        owner: Option<&PluginId>,
        control: &SessionControlEffect,
    ) -> Result<(), PluginError>;
}

/// Outcome of applying one batch.
#[derive(Debug, Default)]
pub struct EffectReport {
    pub applied: usize,
    pub errors: Vec<(usize, PluginError)>,
}

impl EffectReport {
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Applies a batch in deterministic order with per-effect panic isolation.
pub fn apply_effects(sink: &dyn AgentEffectSink, effects: Vec<QueuedAgentEffect>) -> EffectReport {
    let mut report = EffectReport::default();
    for (index, queued) in effects.into_iter().enumerate() {
        let owner = queued.owner.as_ref();
        let result = catch_unwind(AssertUnwindSafe(|| match &queued.effect {
            AgentEffect::ToolResult(result) => sink.effect_tool_result(owner, result),
            AgentEffect::ContextUpdate(update) => sink.effect_context_update(owner, update),
            AgentEffect::SessionControl(control) => sink.effect_session_control(owner, control),
            AgentEffect::Noop => Ok(()),
        }));
        match result {
            Ok(Ok(())) => report.applied += 1,
            Ok(Err(error)) => report.errors.push((index, error)),
            Err(_) => report.errors.push((
                index,
                PluginError::Runtime { message: "agent effect handler panicked".to_owned() },
            )),
        }
    }
    report
}

/// Declarative context contribution re-exported for effect payloads.
pub type ContextItem = ContextContribution;

/// Identifier used by builtin context updates without a contribution id.
pub fn default_provider_id() -> ContributionId {
    ContributionId::new("agent.context.default").expect("static contribution id is valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct RecordingSink {
        tool_results: AtomicUsize,
        context_updates: AtomicUsize,
        session_controls: AtomicUsize,
    }

    impl AgentEffectSink for RecordingSink {
        fn effect_tool_result(
            &self,
            _owner: Option<&PluginId>,
            _result: &ToolResultEffect,
        ) -> Result<(), PluginError> {
            self.tool_results.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn effect_context_update(
            &self,
            _owner: Option<&PluginId>,
            _update: &ContextUpdateEffect,
        ) -> Result<(), PluginError> {
            self.context_updates.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn effect_session_control(
            &self,
            _owner: Option<&PluginId>,
            _control: &SessionControlEffect,
        ) -> Result<(), PluginError> {
            self.session_controls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn owner() -> PluginId {
        PluginId::new("demo").expect("valid plugin id")
    }

    #[test]
    fn editor_effects_fail_closed() {
        let error = AgentEffect::from_plugin_effect(Effect::ScrollTo(
            ee_plugin_api::effect::ScrollRequest {
                view_id: ee_plugin_api::model::ViewId(1),
                line: 1,
                column: 0,
            },
        ))
        .expect_err("editor effect rejected");
        assert!(matches!(error, PluginError::Unsupported { .. }));
    }

    #[test]
    fn tool_result_effect_converts_from_wire() {
        let effect = AgentEffect::from_plugin_effect(Effect::EmitAgentToolResult(
            ee_plugin_api::effect::AgentToolResult {
                call_id: "call-1".to_owned(),
                is_error: false,
                content: serde_json::json!({"ok": true}),
            },
        ))
        .expect("converts");
        assert_eq!(
            effect,
            AgentEffect::ToolResult(ToolResultEffect {
                call_id: "call-1".to_owned(),
                is_error: false,
                content: serde_json::json!({"ok": true}),
            })
        );
    }

    #[test]
    fn queue_applies_in_order() {
        let sink = RecordingSink::default();
        let queue = AgentEffectQueue::new();
        queue.push(
            Some(owner()),
            AgentEffect::ToolResult(ToolResultEffect {
                call_id: "call-1".to_owned(),
                is_error: false,
                content: serde_json::json!({}),
            }),
        );
        queue.push(
            Some(owner()),
            AgentEffect::SessionControl(SessionControlEffect::Checkpoint {
                session_id: "session-1".to_owned(),
                checkpoint_id: "cp-1".to_owned(),
            }),
        );
        let report = apply_effects(&sink, queue.drain());
        assert!(report.is_ok());
        assert_eq!(report.applied, 2);
        assert_eq!(sink.tool_results.load(Ordering::SeqCst), 1);
        assert_eq!(sink.session_controls.load(Ordering::SeqCst), 1);
        assert_eq!(sink.context_updates.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn panic_is_isolated_per_effect() {
        struct PanickingSink;
        impl AgentEffectSink for PanickingSink {
            fn effect_tool_result(
                &self,
                _owner: Option<&PluginId>,
                _result: &ToolResultEffect,
            ) -> Result<(), PluginError> {
                panic!("boom");
            }
            fn effect_context_update(
                &self,
                _owner: Option<&PluginId>,
                _update: &ContextUpdateEffect,
            ) -> Result<(), PluginError> {
                Ok(())
            }
            fn effect_session_control(
                &self,
                _owner: Option<&PluginId>,
                _control: &SessionControlEffect,
            ) -> Result<(), PluginError> {
                Ok(())
            }
        }

        let queue = AgentEffectQueue::new();
        queue.push(
            None,
            AgentEffect::ToolResult(ToolResultEffect {
                call_id: "call-1".to_owned(),
                is_error: false,
                content: serde_json::json!({}),
            }),
        );
        queue.push(None, AgentEffect::Noop);
        let report = apply_effects(&PanickingSink, queue.drain());
        assert_eq!(report.applied, 1);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].0, 0);
    }

    #[test]
    fn effect_queue_is_shared_through_arc() {
        let queue = Arc::new(AgentEffectQueue::default());
        let clone = Arc::clone(&queue);
        clone.push(None, AgentEffect::Noop);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.drain().len(), 1);
        assert!(queue.is_empty());
    }
}
