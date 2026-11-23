//! Manifest-driven prompt collection for `ee do config setup agent`.
//!
//! Kept separate from `agent_setup.rs` so the wizard orchestration stays below
//! the repository's 1,000-line threshold. The manifest owns labels, defaults,
//! choices, sections, and the advanced-options flag; this module only renders
//! them and collects the resulting environment values.

use std::collections::BTreeMap;

use ee_agent_protocol::setup::{SetupChoice, SetupInput, SetupManifest};

use crate::secrets;
use crate::setup_prompt::{
    confirm, prompt_searchable_selection, prompt_selection, prompt_value, read_hidden_value,
};

/// Collects every environment value the manifest declares.
///
/// `fetch_remote` is called only for inputs that set [`SetupInput::remote_choices`]
/// and only when the host can offer a searchable picker; it receives the values
/// collected so far as environment for the agent process.
pub(crate) fn collect_setup_values(
    manifest: &SetupManifest,
    fetch_remote: impl Fn(&str, &BTreeMap<String, String>) -> Result<Vec<SetupChoice>, String>,
) -> Result<BTreeMap<String, String>, String> {
    let mut env_values = BTreeMap::new();
    let mut selected_inputs = BTreeMap::new();
    let mut secret_store = None;

    collect_env_values(manifest, &mut env_values, &mut secret_store)?;
    collect_manifest_inputs(manifest, &mut env_values, &mut selected_inputs, &fetch_remote)?;

    Ok(env_values)
}

fn collect_env_values(
    manifest: &SetupManifest,
    env_values: &mut BTreeMap<String, String>,
    secret_store: &mut Option<Result<secrets::SecretStore, secrets::SecretStoreError>>,
) -> Result<(), String> {
    if manifest.env_vars.is_empty() {
        return Ok(());
    }
    println!("\nCredentials:");

    for env in &manifest.env_vars {
        println!(
            "{}{}: {}",
            env.name,
            if env.required { " (required)" } else { " (optional)" },
            env.description
        );
        let value = if env.secret {
            read_hidden_value(env.required)?
                .map(|value| {
                    let secret_name = secrets::SecretName::new(&format!(
                        "agent.{}.{}",
                        manifest.agent.id, env.name
                    ))
                    .map_err(|error| {
                        format!("cannot store secret `{}`: invalid name: {error}", env.name)
                    })?;
                    let reference =
                        secrets::SecretReference::from_name(secret_name.clone()).to_string();
                    let store = secret_store.get_or_insert_with(secrets::SecretStore::default);
                    let store = store
                        .as_ref()
                        .map_err(|error| format!("cannot open encrypted secrets store: {error}"))?;
                    store
                        .set(&secret_name, &value)
                        .map_err(|error| format!("cannot store secret `{}`: {error}", env.name))?;
                    Ok::<String, String>(reference)
                })
                .transpose()?
        } else {
            prompt_value(&env.name, None, env.required)?
        };
        if let Some(value) = value {
            env_values.insert(env.name.clone(), value);
        }
    }
    Ok(())
}

fn collect_manifest_inputs(
    manifest: &SetupManifest,
    env_values: &mut BTreeMap<String, String>,
    selected_inputs: &mut BTreeMap<String, String>,
    fetch_remote: &impl Fn(&str, &BTreeMap<String, String>) -> Result<Vec<SetupChoice>, String>,
) -> Result<(), String> {
    let include_advanced = should_collect_advanced(manifest)?;
    let mut last_section: Option<&str> = None;

    for input in &manifest.inputs {
        if input.advanced && !include_advanced {
            continue;
        }
        if let Some(section) = input.section.as_deref()
            && last_section != Some(section)
        {
            println!("\n{section}:");
            last_section = Some(section);
        }

        let value = prompt_manifest_input(input, selected_inputs, env_values, fetch_remote)?;
        if let Some(value) = value {
            selected_inputs.insert(input.key.clone(), value.clone());
            env_values.insert(input.config.env.clone(), value);
        }
    }

    Ok(())
}

/// Asks once before prompting advanced knobs. Required advanced inputs cannot
/// be hidden, so they force the gate open instead of producing an incomplete
/// configuration.
fn should_collect_advanced(manifest: &SetupManifest) -> Result<bool, String> {
    let advanced = manifest.inputs.iter().filter(|input| input.advanced).collect::<Vec<_>>();
    if advanced.is_empty() {
        return Ok(false);
    }
    if advanced.iter().any(|input| input.required) {
        println!("\nAdvanced options are required by this agent.");
        return Ok(true);
    }
    let include =
        confirm("Configure advanced options (timeouts, retries, paths, prompts)? [y/N]: ")?;
    if !include {
        println!("Keeping agent defaults for advanced options.");
    }
    Ok(include)
}

fn prompt_manifest_input(
    input: &SetupInput,
    selected_inputs: &BTreeMap<String, String>,
    env_values: &BTreeMap<String, String>,
    fetch_remote: &impl Fn(&str, &BTreeMap<String, String>) -> Result<Vec<SetupChoice>, String>,
) -> Result<Option<String>, String> {
    let choices = applicable_choices(input, selected_inputs);
    if input.remote_choices && !choices.is_empty() {
        let collected = env_values.clone();
        return prompt_searchable_selection(
            &input.label,
            &choices,
            input.default.as_deref(),
            input.required,
            || {
                fetch_remote(&input.key, &collected).map(|all| {
                    all.into_iter()
                        .map(|choice| (choice.value.clone(), choice.display().to_owned()))
                        .collect()
                })
            },
        );
    }
    if choices.is_empty() {
        return prompt_value(&input.label, input.default.as_deref(), input.required);
    }
    prompt_selection(&input.label, &choices, input.default.as_deref(), input.required)
}

/// Keeps only choices whose sibling-input condition matches a value already
/// collected in this run.
fn applicable_choices(
    input: &SetupInput,
    selected_inputs: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    input
        .choices
        .iter()
        .filter(|choice| choice_applies(choice, selected_inputs))
        .map(|choice| (choice.value.clone(), choice.display().to_owned()))
        .collect()
}

fn choice_applies(choice: &SetupChoice, selected_inputs: &BTreeMap<String, String>) -> bool {
    match &choice.when {
        Some(when) => {
            selected_inputs.get(&when.input).is_some_and(|selected| selected == &when.value)
        }
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use ee_agent_protocol::setup::{SetupChoiceWhen, SetupInputConfig};

    use super::*;

    fn input_with_choices() -> SetupInput {
        SetupInput {
            key: String::from("model"),
            label: String::from("Model"),
            default: None,
            required: true,
            section: Some(String::from("Model selection")),
            choices: vec![
                SetupChoice {
                    value: String::from("gpt-5.5"),
                    label: Some(String::from("gpt-5.5 (zen)")),
                    when: Some(SetupChoiceWhen {
                        input: String::from("surface"),
                        value: String::from("zen"),
                    }),
                },
                SetupChoice {
                    value: String::from("kimi-k3"),
                    label: Some(String::from("kimi-k3 (go)")),
                    when: Some(SetupChoiceWhen {
                        input: String::from("surface"),
                        value: String::from("go"),
                    }),
                },
                SetupChoice {
                    value: String::from("gpt-5.5"),
                    label: Some(String::from("gpt-5.5 (go)")),
                    when: None,
                },
            ],
            remote_choices: false,
            advanced: false,
            config: SetupInputConfig { env: String::from("OPENCODE_MODEL") },
        }
    }

    #[test]
    fn choices_filter_by_an_already_selected_sibling_input() {
        let input = input_with_choices();
        let zen = BTreeMap::from([(String::from("surface"), String::from("zen"))]);
        let go = BTreeMap::from([(String::from("surface"), String::from("go"))]);

        assert_eq!(
            applicable_choices(&input, &zen),
            vec![
                (String::from("gpt-5.5"), String::from("gpt-5.5 (zen)")),
                (String::from("gpt-5.5"), String::from("gpt-5.5 (go)")),
            ]
        );
        assert_eq!(
            applicable_choices(&input, &go),
            vec![
                (String::from("kimi-k3"), String::from("kimi-k3 (go)")),
                (String::from("gpt-5.5"), String::from("gpt-5.5 (go)")),
            ]
        );
    }

    #[test]
    fn conditional_choices_stay_hidden_until_the_sibling_value_exists() {
        let input = input_with_choices();

        assert_eq!(
            applicable_choices(&input, &BTreeMap::new()),
            vec![(String::from("gpt-5.5"), String::from("gpt-5.5 (go)"))]
        );
    }
}
