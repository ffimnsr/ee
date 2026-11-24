//! Interactive selection over discovered agent server candidates.
//!
//! Kept out of `agent_setup.rs` so the wizard orchestration stays below the
//! repository's 1,000-line threshold. The fuzzy picker is preferred when
//! installed; otherwise selection falls back to the numbered list.

use crate::setup_prompt::{prompt_fuzzy_pick, prompt_line};

use super::SetupCandidate;

pub(super) fn select_agent(candidates: &[SetupCandidate]) -> Result<&SetupCandidate, String> {
    if candidates.is_empty() {
        return Err(String::from("no compatible agent servers found"));
    }
    let choices = candidate_choices(candidates);
    if let Some(index) = prompt_fuzzy_pick("Available agent servers", &choices)?
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|index| *index < candidates.len())
    {
        return Ok(&candidates[index]);
    }

    println!("Available agent servers:");
    for (index, candidate) in candidates.iter().enumerate() {
        println!("  {}) {}", index + 1, candidate_label(candidate));
    }

    loop {
        let selected = prompt_line(&format!("Select agent [1-{}]: ", candidates.len()))?;
        let index = selected
            .parse::<usize>()
            .ok()
            .and_then(|index| index.checked_sub(1))
            .filter(|index| *index < candidates.len());
        if let Some(index) = index {
            return Ok(&candidates[index]);
        }
        eprintln!("Enter a number from 1 through {}.", candidates.len());
    }
}

/// Picker records use the candidate index as the stable value and a readable
/// label, so a fuzzy-filtered selection maps back without parsing display text.
fn candidate_choices(candidates: &[SetupCandidate]) -> Vec<(String, String)> {
    candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| (index.to_string(), candidate_label(candidate)))
        .collect()
}

fn candidate_label(candidate: &SetupCandidate) -> String {
    match candidate {
        SetupCandidate::Local(candidate) => format!("{} (local)", candidate.file_name),
        SetupCandidate::Registry(agent) => {
            format!("{} {} (ACP Registry)", agent.name, agent.version)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::agent_registry::RegistryAgent;

    use super::super::AgentCandidate;
    use super::*;

    #[test]
    fn candidate_choices_pair_stable_indices_with_readable_labels() {
        let registry: RegistryAgent = serde_json::from_str(
            r#"{"id":"alpha","name":"Alpha","version":"1.0.0","distribution":{"npx":{"package":"alpha@1.0.0"}}}"#,
        )
        .unwrap();
        let candidates = [
            SetupCandidate::Local(AgentCandidate {
                path: PathBuf::from("/tmp/ee-agent-zulu"),
                file_name: String::from("ee-agent-zulu"),
            }),
            SetupCandidate::Registry(Box::new(registry)),
        ];

        assert_eq!(
            candidate_choices(&candidates),
            vec![
                (String::from("0"), String::from("ee-agent-zulu (local)")),
                (String::from("1"), String::from("Alpha 1.0.0 (ACP Registry)")),
            ]
        );
    }
}
