//! Stream mode: per-domain ordered task lists with cross-domain concurrency.
//!
//! Each domain's order is a seeded shuffle of its sorted task ids, so one
//! domain's stream is independent of the others. The frozen order is written
//! once and reused; a stale order (missing domain or changed task set) is a
//! loud error rather than an arbitrary placement. Concurrency runs across
//! domains only; tasks inside one domain run in frozen order.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use futures::{StreamExt, stream};
use serde::{Deserialize, Serialize};

use crate::channel::Channel;
use crate::error::{HarnessError, HarnessResult};
use crate::select::seeded_shuffle;
use crate::task::HarnessTask;

use super::{HarnessConfig, HarnessRun, ReflectionWriter, run_task};

const STREAM_ORDER_FILE: &str = "stream-order.json";
const STREAM_ORDER_SCHEMA_VERSION: u32 = 1;

/// One finished task inside a stream run.
#[derive(Debug)]
pub struct StreamTaskRun {
    /// Task domain.
    pub domain: String,
    /// Task id.
    pub task_id: String,
    /// Driver result for the task.
    pub result: HarnessResult<HarnessRun>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StreamOrder {
    schema_version: u32,
    domains: Vec<FrozenDomain>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FrozenDomain {
    domain: String,
    task_ids: Vec<String>,
}

/// Runs a task list under per-domain frozen order.
pub async fn run_stream(
    channel: &dyn Channel,
    tasks: &[HarnessTask],
    config: &HarnessConfig,
    run_root: &Path,
    writer: Option<&dyn ReflectionWriter>,
) -> HarnessResult<Vec<StreamTaskRun>> {
    config.validate()?;
    let mut by_domain: BTreeMap<String, Vec<HarnessTask>> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for task in tasks {
        task.validate()?;
        if !seen.insert(task.id.clone()) {
            return Err(HarnessError::State(format!(
                "duplicate task id {:?} in one stream run",
                task.id
            )));
        }
        by_domain.entry(task.domain.clone()).or_default().push(task.clone());
    }
    if by_domain.is_empty() {
        return Ok(Vec::new());
    }
    let frozen = load_or_freeze(run_root, &by_domain, config)?;
    let lookup: BTreeMap<String, HarnessTask> =
        tasks.iter().cloned().map(|task| (task.id.clone(), task)).collect();
    let planned: Vec<(usize, String, Vec<HarnessTask>)> = frozen
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let ordered = entry.task_ids.iter().filter_map(|id| lookup.get(id).cloned()).collect();
            (index, entry.domain.clone(), ordered)
        })
        .collect();
    let workers = config.max_workers.max(1);
    let run_root = run_root.to_path_buf();
    let config = config.clone();
    let batches = stream::iter(planned.into_iter().map(move |(order, domain, tasks)| {
        let run_root = run_root.clone();
        let config = config.clone();
        async move {
            let mut results = Vec::new();
            for (position, task) in tasks.into_iter().enumerate() {
                let task_id = task.id.clone();
                let task_root = run_root.join("tasks").join(&domain).join(safe_name(&task_id));
                let result = run_task(channel, &task, &config, &task_root, writer).await;
                if let Err(error) = &result {
                    tracing::warn!(
                        domain = %domain,
                        task = %task_id,
                        %error,
                        "harness stream task failed; continuing with the next task"
                    );
                }
                results.push((
                    order,
                    position,
                    StreamTaskRun { domain: domain.clone(), task_id, result },
                ));
            }
            results
        }
    }))
    .buffer_unordered(workers)
    .collect::<Vec<Vec<_>>>()
    .await;
    let mut flattened: Vec<(usize, usize, StreamTaskRun)> = batches.into_iter().flatten().collect();
    flattened.sort_by_key(|(order, position, _)| (*order, *position));
    Ok(flattened.into_iter().map(|(_, _, result)| result).collect())
}

fn load_or_freeze(
    run_root: &Path,
    by_domain: &BTreeMap<String, Vec<HarnessTask>>,
    config: &HarnessConfig,
) -> HarnessResult<Vec<FrozenDomain>> {
    let path = run_root.join(STREAM_ORDER_FILE);
    if path.is_file() {
        let text = std::fs::read_to_string(&path)?;
        let frozen: StreamOrder = serde_json::from_str(&text).map_err(|error| {
            HarnessError::State(format!("stream order {} is unreadable: {error}", path.display()))
        })?;
        validate_frozen(&frozen, by_domain)?;
        return Ok(frozen.domains);
    }
    let mut domains = Vec::new();
    for (domain, tasks) in by_domain {
        let mut ids: Vec<String> = tasks.iter().map(|task| task.id.clone()).collect();
        ids.sort();
        seeded_shuffle(&mut ids, stream_seed(domain, config));
        domains.push(FrozenDomain { domain: domain.clone(), task_ids: ids });
    }
    let order = StreamOrder { schema_version: STREAM_ORDER_SCHEMA_VERSION, domains };
    std::fs::create_dir_all(run_root)?;
    let text = serde_json::to_string_pretty(&order)
        .map_err(|error| HarnessError::State(error.to_string()))?;
    std::fs::write(&path, text)?;
    Ok(order.domains)
}

fn validate_frozen(
    frozen: &StreamOrder,
    by_domain: &BTreeMap<String, Vec<HarnessTask>>,
) -> HarnessResult<()> {
    if frozen.schema_version != STREAM_ORDER_SCHEMA_VERSION {
        return Err(HarnessError::State(format!(
            "stream order schema {} is not {STREAM_ORDER_SCHEMA_VERSION}",
            frozen.schema_version
        )));
    }
    let current: BTreeSet<&str> = by_domain.keys().map(String::as_str).collect();
    let expected: BTreeSet<&str> =
        frozen.domains.iter().map(|entry| entry.domain.as_str()).collect();
    if current != expected {
        let missing: Vec<&str> = current.difference(&expected).copied().collect();
        let extra: Vec<&str> = expected.difference(&current).copied().collect();
        return Err(HarnessError::State(format!(
            "stale frozen stream order: domains differ (missing {missing:?}, extra {extra:?}); \
             remove {STREAM_ORDER_FILE} to rebuild the order"
        )));
    }
    for entry in &frozen.domains {
        let tasks = by_domain.get(&entry.domain).ok_or_else(|| {
            HarnessError::State(format!(
                "stale frozen stream order: domain {:?} is not in this run",
                entry.domain
            ))
        })?;
        let current_ids: BTreeSet<&str> = tasks.iter().map(|task| task.id.as_str()).collect();
        let frozen_ids: BTreeSet<&str> = entry.task_ids.iter().map(String::as_str).collect();
        if current_ids != frozen_ids || frozen_ids.len() != entry.task_ids.len() {
            return Err(HarnessError::State(format!(
                "stale frozen stream order for domain {:?}: the task set changed; \
                 remove {STREAM_ORDER_FILE} to rebuild the order",
                entry.domain
            )));
        }
    }
    Ok(())
}

fn stream_seed(domain: &str, config: &HarnessConfig) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in
        domain.bytes().chain(config.mode.as_str().bytes()).chain((config.k as u64).to_le_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.trim_matches('_').is_empty() { String::from("task") } else { cleaned }
}

/// Path of the frozen order file inside one stream run root.
#[must_use]
pub fn stream_order_path(run_root: &Path) -> PathBuf {
    run_root.join(STREAM_ORDER_FILE)
}
