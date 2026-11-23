//! Anonymized best-of-k selection.
//!
//! Candidate ids are a seeded shuffle of the pool, deterministic per task, k,
//! and mode, so a replay reproduces them. The base deliverable is a valid
//! candidate and the selector is told so. A parse failure never raises: it
//! falls back to a delivered base, else the first final.

use std::collections::BTreeMap;

use crate::channel::Deliverable;
use crate::error::{HarnessError, HarnessResult};

/// Maximum lines accepted in the selector's `<why>` block.
pub const MAX_WHY_LINES: usize = 6;

/// One candidate in the selection pool.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// Stable internal label (`ref-0`, `c-inv-0`, `composed-0`).
    pub label: String,
    /// Whether the run delivered a result.
    pub delivered: bool,
    /// Deliverable, when delivered.
    pub deliverable: Option<Deliverable>,
    /// Whether this is the base/reference deliverable.
    pub is_base: bool,
}

/// Anonymous id mapping.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnonPool {
    /// Anonymous id to internal label.
    pub ids: BTreeMap<String, String>,
    /// Anonymous ids in presentation order.
    pub order: Vec<String>,
}

impl AnonPool {
    /// Internal label for one anonymous id.
    #[must_use]
    pub fn label(&self, anon: &str) -> Option<&str> {
        self.ids.get(anon).map(String::as_str)
    }
}

/// One parsed selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Chosen internal label.
    pub chosen: Option<String>,
    /// Anonymous id the selector named.
    pub anon: Option<String>,
    /// Rationale lines (at most [`MAX_WHY_LINES`]).
    pub why: Vec<String>,
    /// Whether the neutral fallback produced this result.
    pub fallback: bool,
}

/// Drops candidates that delivered nothing, recording a cause for each.
#[must_use]
pub fn drop_non_delivering(candidates: Vec<Candidate>) -> (Vec<Candidate>, Vec<(String, String)>) {
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for candidate in candidates {
        if candidate.delivered && candidate.deliverable.is_some() {
            kept.push(candidate);
        } else {
            dropped.push((
                candidate.label.clone(),
                String::from("no deliverable observed for the rollout"),
            ));
        }
    }
    (kept, dropped)
}

/// Builds the deterministic anonymous pool for one task.
#[must_use]
pub fn assign_ids(task_id: &str, k: usize, mode: &str, candidates: &[Candidate]) -> AnonPool {
    let mut labels: Vec<String> =
        candidates.iter().map(|candidate| candidate.label.clone()).collect();
    labels.sort();
    let seed = seed_of(task_id, k, mode);
    seeded_shuffle(&mut labels, seed);
    let mut ids = BTreeMap::new();
    let mut order = Vec::new();
    for (index, label) in labels.into_iter().enumerate() {
        let anon = format!("a{}", index + 1);
        ids.insert(anon.clone(), label);
        order.push(anon);
    }
    AnonPool { ids, order }
}

/// Parses a ` ```choice ` fence plus a bounded `<why>` block.
pub fn parse_choice(reply: &str, pool: &AnonPool) -> HarnessResult<Selection> {
    let anon = choice_fence(reply)
        .ok_or_else(|| HarnessError::Parse("selector reply is missing a ```choice fence".into()))?;
    if !pool.ids.contains_key(&anon) {
        return Err(HarnessError::Parse(format!("selector chose unknown anonymous id {anon:?}")));
    }
    let why = why_block(reply)?;
    let chosen = pool.ids.get(&anon).cloned();
    Ok(Selection { chosen, anon: Some(anon), why, fallback: false })
}

/// Neutral fallback: the delivered base first, else the first delivered
/// candidate, else no deliverable.
#[must_use]
pub fn neutral_fallback(candidates: &[Candidate]) -> Selection {
    let chosen = candidates
        .iter()
        .find(|candidate| candidate.is_base && candidate.delivered)
        .or_else(|| candidates.iter().find(|candidate| candidate.delivered))
        .map(|candidate| candidate.label.clone());
    let why = vec![
        "selector reply was unusable; neutral fallback applied".to_string(),
        "delivered base preferred, otherwise the first final".to_string(),
    ];
    Selection { chosen, anon: None, why, fallback: true }
}

/// Whether k=1 bypasses the selector session.
#[must_use]
pub fn trivial_selection(k: usize) -> bool {
    k == 1
}

fn choice_fence(reply: &str) -> Option<String> {
    let mut rest = reply;
    while let Some(start) = rest.find("```choice") {
        let after = &rest[start + "```choice".len()..];
        let after = after.trim_start_matches(['\r', '\n', ' ']);
        let first = after.lines().next()?.trim();
        if !first.is_empty() && !first.starts_with("```") {
            return Some(first.to_string());
        }
        rest = after;
    }
    None
}

fn why_block(reply: &str) -> HarnessResult<Vec<String>> {
    let Some(start) = reply.find("<why>") else {
        return Err(HarnessError::Parse("selector reply is missing a <why> block".into()));
    };
    let after = &reply[start + "<why>".len()..];
    let Some(end) = after.find("</why>") else {
        return Err(HarnessError::Parse("selector <why> block is unterminated".into()));
    };
    let lines: Vec<String> = after[..end]
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    if lines.is_empty() {
        return Err(HarnessError::Parse("selector <why> block is empty".into()));
    }
    if lines.len() > MAX_WHY_LINES {
        return Err(HarnessError::Parse(format!(
            "selector <why> block has {} lines; at most {MAX_WHY_LINES} are allowed",
            lines.len()
        )));
    }
    Ok(lines)
}

fn seed_of(task_id: &str, k: usize, mode: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in task_id.bytes().chain(mode.bytes()).chain((k as u64).to_le_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub(crate) fn seeded_shuffle(values: &mut [String], mut state: u64) {
    if values.len() < 2 {
        return;
    }
    for index in (1..values.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let swap = (state % (index as u64 + 1)) as usize;
        values.swap(index, swap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(label: &str, delivered: bool, is_base: bool) -> Candidate {
        Candidate {
            label: label.into(),
            delivered,
            deliverable: delivered
                .then(|| Deliverable { text: format!("{label} deliverable"), assets: Vec::new() }),
            is_base,
        }
    }

    #[test]
    fn ids_are_deterministic_per_task_k_and_mode() {
        let candidates = vec![
            candidate("ref-0", true, true),
            candidate("c-inv-0", true, false),
            candidate("composed-0", true, false),
        ];
        let first = assign_ids("task-1", 1, "parallel", &candidates);
        let second = assign_ids("task-1", 1, "parallel", &candidates);
        assert_eq!(first, second);
        let other = assign_ids("task-2", 1, "parallel", &candidates);
        assert_ne!(first.ids, other.ids, "task id changes the seeded mapping");

        let mut labels: Vec<&str> = first.ids.values().map(String::as_str).collect();
        labels.sort_unstable();
        assert_eq!(labels, vec!["c-inv-0", "composed-0", "ref-0"]);
    }

    #[test]
    fn choice_and_why_parse_with_base_winning() {
        let candidates = vec![candidate("ref-0", true, true), candidate("final-1", true, false)];
        let pool = assign_ids("task", 2, "parallel", &candidates);
        let base_anon = pool
            .ids
            .iter()
            .find(|(_, label)| label.as_str() == "ref-0")
            .map(|(anon, _)| anon.clone())
            .expect("base");
        let reply = format!(
            "```choice\n{base_anon}\n```\n<why>\ndelivery criterion\nrunner-up costs a rewrite\n</why>"
        );
        let selection = parse_choice(&reply, &pool).expect("parses");
        assert_eq!(selection.chosen.as_deref(), Some("ref-0"));
        assert!(!selection.fallback);
        assert_eq!(selection.why.len(), 2);
    }

    #[test]
    fn parse_failures_are_typed() {
        let candidates = vec![candidate("ref-0", true, true)];
        let pool = assign_ids("task", 1, "parallel", &candidates);
        assert!(parse_choice("no fence", &pool).is_err());
        assert!(parse_choice("```choice\na9\n```\n<why>x</why>", &pool).is_err());
        assert!(parse_choice("```choice\na1\n```\nno why", &pool).is_err());
    }

    #[test]
    fn neutral_fallback_prefers_delivered_base() {
        let candidates = vec![candidate("c1", true, false), candidate("ref-0", true, true)];
        let selection = neutral_fallback(&candidates);
        assert_eq!(selection.chosen.as_deref(), Some("ref-0"));
        assert!(selection.fallback);

        let none = neutral_fallback(&[candidate("c1", false, false)]);
        assert_eq!(none.chosen, None);
    }

    #[test]
    fn non_delivering_candidates_drop_with_causes() {
        let (kept, dropped) = drop_non_delivering(vec![
            candidate("ref-0", true, true),
            candidate("c1", false, false),
        ]);
        assert_eq!(kept.len(), 1);
        assert_eq!(dropped[0].0, "c1");
        assert!(dropped[0].1.contains("no deliverable"));
    }
}
