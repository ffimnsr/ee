//! `impl App`: runtime model picker for the agents pane.

use std::path::Path;

use ee_agent_host::AgentProcessConfig;
use ee_agent_protocol::{
    SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOptions,
};

use super::super::*;
use super::thread_ui::ThreadUiState;

impl App {
    /// `/model` or `/agent` with no argument: open the model picker.
    ///
    /// Advertised ACP model choices win. Agents that only expose the setup
    /// choice probe (OpenCode/OpenRouter) are asked for their current catalog.
    pub(super) fn open_agent_model_picker(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        if matches!(
            self.agents.threads[active].state,
            ThreadUiState::Starting
                | ThreadUiState::Queued
                | ThreadUiState::Running
                | ThreadUiState::AwaitingPermission
                | ThreadUiState::AwaitingElicitation
                | ThreadUiState::Cancelling
                | ThreadUiState::PausedRecoverable
        ) {
            self.backend.status_message = Some(String::from(
                "cannot switch models while a turn is active or paused; wait for it to settle",
            ));
            return;
        }

        let agent_id = self.agents.threads[active].agent_id.clone();
        let option = self.agents.threads[active]
            .host
            .config_options()
            .into_iter()
            .find(is_model_config_option);
        if let Some(option) = option {
            let (items, selected) = model_items_from_option(&option);
            if !items.is_empty() {
                self.open_model_picker(items, selected);
                return;
            }
        }

        let Some(config) =
            self.agents.host.as_ref().and_then(|host| host.manager.agent_config(&agent_id))
        else {
            self.backend.status_message = Some(String::from("agent host not ready"));
            return;
        };
        match remote_model_items(&config) {
            Ok((items, _)) if items.is_empty() => {
                self.backend.status_message =
                    Some(String::from("agent advertised no models; use /model <model-id>"));
            }
            Ok((items, selected)) => self.open_model_picker(items, selected),
            Err(error) => {
                self.backend.status_message =
                    Some(format!("model list unavailable: {error}; use /model <model-id>"));
            }
        }
    }

    fn open_model_picker(&mut self, items: Vec<crate::picker::PickerItem>, selected: usize) {
        self.open_picker(crate::picker::PickerState::new_agent_models(items, selected));
        self.backend.status_message = Some(String::from("select model for this session"));
    }
}

pub(super) fn is_model_config_option(option: &SessionConfigOption) -> bool {
    option.id.0.as_ref() == "model"
        || matches!(option.category, Some(SessionConfigOptionCategory::Model))
}

fn model_items_from_option(
    option: &SessionConfigOption,
) -> (Vec<crate::picker::PickerItem>, usize) {
    let SessionConfigKind::Select(select) = &option.kind else {
        return (Vec::new(), 0);
    };
    let current = select.current_value.0.as_ref();
    let mut items = Vec::new();
    let mut selected = 0;
    match &select.options {
        SessionConfigSelectOptions::Ungrouped(options) => {
            for choice in options {
                push_model_item(
                    &mut items,
                    &mut selected,
                    current,
                    choice.value.0.as_ref(),
                    &choice.name,
                    None,
                );
            }
        }
        SessionConfigSelectOptions::Grouped(groups) => {
            for group in groups {
                for choice in &group.options {
                    push_model_item(
                        &mut items,
                        &mut selected,
                        current,
                        choice.value.0.as_ref(),
                        &choice.name,
                        Some(&group.name),
                    );
                }
            }
        }
        _ => {}
    }
    (items, selected)
}

fn push_model_item(
    items: &mut Vec<crate::picker::PickerItem>,
    selected: &mut usize,
    current: &str,
    value: &str,
    name: &str,
    group: Option<&str>,
) {
    let label = match group {
        Some(group) => format!("{group} · {name} ({value})"),
        None if name == value => value.to_string(),
        None => format!("{name} ({value})"),
    };
    if value == current {
        *selected = items.len();
    }
    items.push(crate::picker::PickerItem {
        label,
        detail: Some(value.to_string()),
        path: None,
        buf_id: None,
        line: None,
        col: None,
        choice_index: Some(items.len()),
    });
}

fn remote_model_items(
    config: &AgentProcessConfig,
) -> Result<(Vec<crate::picker::PickerItem>, usize), String> {
    let choices =
        crate::agent_setup::read_choices(Path::new(&config.command), "model", &config.env)?;
    let items: Vec<crate::picker::PickerItem> = choices
        .into_iter()
        .map(|choice| crate::picker::PickerItem {
            label: choice.display().to_owned(),
            detail: Some(choice.value),
            path: None,
            buf_id: None,
            line: None,
            col: None,
            choice_index: None,
        })
        .collect();
    let selected = configured_model_index(config, &items).unwrap_or(0);
    Ok((items, selected))
}

fn configured_model_index(
    config: &AgentProcessConfig,
    items: &[crate::picker::PickerItem],
) -> Option<usize> {
    let matches_value =
        |value: &str| items.iter().position(|item| item.detail.as_deref() == Some(value));
    // Prefer the main model env slot. Side slots such as critic, classifier,
    // or small-model routes must not decide which row starts selected.
    for (key, value) in &config.env {
        let key = key.to_ascii_uppercase();
        if key.contains("CRITIC")
            || key.contains("RUBBER")
            || key.contains("CLASSIFIER")
            || key.contains("SMALL")
        {
            continue;
        }
        if (key == "MODEL" || key.ends_with("_MODEL"))
            && let Some(index) = matches_value(value)
        {
            return Some(index);
        }
    }
    config.env.values().find_map(|value| matches_value(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config_option() -> SessionConfigOption {
        serde_json::from_value(json!({
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": "model-b",
            "options": [
                { "value": "model-a", "name": "Model A" },
                { "value": "model-b", "name": "Model B" }
            ]
        }))
        .expect("config option")
    }

    #[test]
    fn advertised_model_choices_preselect_the_current_value() {
        let (items, selected) = model_items_from_option(&config_option());
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].detail.as_deref(), Some("model-a"));
        assert_eq!(items[1].label, "Model B (model-b)");
        assert_eq!(selected, 1, "current model preselected");
    }

    #[cfg(unix)]
    #[test]
    fn remote_choices_use_the_setup_probe_and_select_the_configured_model() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("tempdir");
        let agent = root.path().join("fake-agent");
        std::fs::write(
            &agent,
            b"#!/bin/sh\nprintf '%s' '[{\"value\":\"model-a\",\"label\":\"Model A\"},{\"value\":\"model-b\"}]'\n",
        )
        .expect("write fake agent");
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755))
            .expect("mark executable");

        let mut config = AgentProcessConfig::new(agent.display().to_string());
        config.env.insert(String::from("CRITIC_MODEL"), String::from("model-a"));
        config.env.insert(String::from("MODEL"), String::from("model-b"));
        let (items, selected) = remote_model_items(&config).expect("model choices");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "Model A");
        assert_eq!(items[0].detail.as_deref(), Some("model-a"));
        assert_eq!(items[1].label, "model-b");
        assert_eq!(selected, 1, "main model preselected, not the critic route");
    }
}
