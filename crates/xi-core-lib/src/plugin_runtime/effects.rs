//! Deterministic effect application.
//!
//! Effects are applied in the order the plugin returned them, after the core
//! borrow that produced them has ended. A panic inside one effect is isolated
//! so unrelated effects and the rest of the host keep running.

use std::panic::{AssertUnwindSafe, catch_unwind};

use ee_plugin_api::effect::Effect;
use ee_plugin_api::error::PluginError;
use ee_plugin_api::ids::PluginId;

use crate::plugin_runtime::core_api::EffectSink;
use crate::plugin_runtime::services::tasks::TaskService;
use crate::plugin_runtime::services::ui::UiService;

/// Result of applying one effect batch.
#[derive(Debug, Default)]
pub struct EffectReport {
    pub applied: usize,
    pub errors: Vec<(usize, PluginError)>,
}

impl EffectReport {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Apply a batch of effects through the host sink.
pub fn apply_effects(
    sink: &mut dyn EffectSink,
    owner: Option<&PluginId>,
    effects: Vec<Effect>,
    ui: Option<&UiService>,
    tasks: Option<&TaskService>,
) -> EffectReport {
    let mut report = EffectReport::default();
    for (index, effect) in effects.into_iter().enumerate() {
        let result = catch_unwind(AssertUnwindSafe(|| match effect {
            Effect::ApplyEdit(request) => sink.effect_apply_edit(&request).map(|_| ()),
            Effect::SetSelection(request) => sink.effect_set_selection(&request),
            Effect::ScrollTo(request) => sink.effect_scroll_to(&request),
            Effect::SetStatus(item) => sink.effect_set_status(owner, &item),
            Effect::RemoveStatus { key } => sink.effect_remove_status(owner, key.as_str()),
            Effect::SetDecorations(patch) => sink.effect_set_decorations(owner, &patch),
            Effect::ShowPanel(update) => match ui {
                Some(ui) => ui.apply_panel(update),
                None => Err(unsupported("panel updates")),
            },
            Effect::ShowOverlay(update) => match ui {
                Some(ui) => ui.apply_overlay(update),
                None => Err(unsupported("overlay updates")),
            },
            Effect::ShowPicker(update) => match ui {
                Some(ui) => ui.apply_picker(update),
                None => Err(unsupported("picker updates")),
            },
            Effect::RunTask(request) => match tasks {
                Some(tasks) => tasks.run_request(&request),
                None => Err(unsupported("background tasks")),
            },
            Effect::EmitAgentToolResult(result) => sink.effect_emit_agent_tool_result(
                &result.call_id,
                result.is_error,
                &result.content,
            ),
            Effect::Noop => Ok(()),
        }));

        match result {
            Ok(Ok(())) => report.applied += 1,
            Ok(Err(error)) => report.errors.push((index, error)),
            Err(_) => report.errors.push((
                index,
                PluginError::Runtime { message: "effect handler panicked".to_owned() },
            )),
        }
    }
    report
}

/// Stable class for host capability gaps.
pub fn unsupported(feature: &'static str) -> PluginError {
    PluginError::Unsupported { feature }
}
