//! Deterministic usage and tool analytics (I23).
//!
//! Every number here is computed from the append-only session log: usage
//! records written per model response and tool records written per completed
//! execution. There is no second bookkeeping path, so a crash or a log replay
//! produces the same totals. The model-facing read-only `usage_stats` tool
//! wraps the same query.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::OrchestratorError;
use crate::session_log::{SessionLog, SessionLogRecord};
use crate::tools::{
    ServerTool, SideEffectClass, ToolCallContext, ToolDefinition, ToolErrorKind, ToolFuture,
    ToolResult,
};
use crate::usage::ModelUsage;

/// Maximum tool rows returned (the N most-called tools).
pub const MAX_TOOL_ROWS: usize = 16;
/// Maximum model rows returned (the N highest-record models).
pub const MAX_MODEL_ROWS: usize = 32;

/// Token and cost totals for one scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotals {
    /// Input tokens reported by the provider.
    pub input_tokens: u64,
    /// Output tokens reported by the provider.
    pub output_tokens: u64,
    /// Hidden reasoning tokens (subset of output when reported).
    pub reasoning_tokens: u64,
    /// Prompt-cache read tokens.
    pub cache_read_tokens: u64,
    /// Prompt-cache write tokens.
    pub cache_write_tokens: u64,
    /// Cost in integer micro-dollars.
    pub cost_micros: u64,
}

impl UsageTotals {
    fn add(&mut self, usage: &ModelUsage) {
        self.input_tokens += usage.input_tokens.unwrap_or(0) as u64;
        self.output_tokens += usage.output_tokens.unwrap_or(0) as u64;
        self.reasoning_tokens += usage.reasoning_tokens.unwrap_or(0) as u64;
        self.cache_read_tokens += usage.cached_input_tokens.unwrap_or(0) as u64;
        self.cache_write_tokens += usage.cache_creation_input_tokens.unwrap_or(0) as u64;
        self.cost_micros += usage.cost_micros.unwrap_or(0);
    }
}

/// Per-model usage totals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelUsageRow {
    /// Registry model id, or `unknown`.
    pub model_id: String,
    /// Usage records attributed to this model.
    pub records: u64,
    /// Token and cost totals.
    pub totals: UsageTotals,
}

/// Per-tool call statistics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolUsageRow {
    /// Tool name.
    pub name: String,
    /// Completed calls.
    pub calls: u64,
    /// Failed calls.
    pub failures: u64,
    /// Median duration in milliseconds.
    pub p50_ms: u64,
    /// 95th-percentile duration in milliseconds.
    pub p95_ms: u64,
    /// Longest observed duration in milliseconds.
    pub max_ms: u64,
}

/// Whole-session usage snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageStats {
    /// Session id.
    pub session_id: String,
    /// Recorded model-visible messages.
    pub messages: u64,
    /// Recorded model requests.
    pub model_requests: u64,
    /// Recorded usage observations.
    pub usage_records: u64,
    /// Recorded tool calls.
    pub tool_calls: u64,
    /// Started turns.
    pub turns: u64,
    /// Session-wide totals.
    pub totals: UsageTotals,
    /// Per-model totals, highest record count first.
    pub by_model: Vec<ModelUsageRow>,
    /// Tool statistics for the most-called tools.
    pub tools: Vec<ToolUsageRow>,
}

/// Collects deterministic usage and tool statistics from one session log.
///
/// # Errors
///
/// Fails when the log cannot be read.
pub fn collect_usage_stats(log: &SessionLog) -> Result<UsageStats, OrchestratorError> {
    let mut stats = UsageStats {
        session_id: log.session_id().to_string(),
        messages: 0,
        model_requests: 0,
        usage_records: 0,
        tool_calls: 0,
        turns: 0,
        totals: UsageTotals::default(),
        by_model: Vec::new(),
        tools: Vec::new(),
    };
    let mut by_model: BTreeMap<String, (u64, UsageTotals)> = BTreeMap::new();
    let mut durations: BTreeMap<String, (u64, u64, Vec<u64>)> = BTreeMap::new();
    for record in log.records()? {
        match record {
            SessionLogRecord::Message(_) => stats.messages += 1,
            SessionLogRecord::ModelRequest(_) => stats.model_requests += 1,
            SessionLogRecord::TurnStarted { .. } => stats.turns += 1,
            SessionLogRecord::Usage(record) => {
                stats.usage_records += 1;
                stats.totals.add(&record.usage);
                let key = record.model_id.unwrap_or_else(|| String::from("unknown"));
                let entry = by_model.entry(key).or_default();
                entry.0 += 1;
                entry.1.add(&record.usage);
            }
            SessionLogRecord::Tool(record) => {
                stats.tool_calls += 1;
                let entry = durations.entry(record.tool_name).or_default();
                entry.0 += 1;
                if !record.success {
                    entry.1 += 1;
                }
                entry.2.push(record.duration_ms);
            }
            _ => {}
        }
    }

    let mut models = by_model
        .into_iter()
        .map(|(model_id, (records, totals))| ModelUsageRow { model_id, records, totals })
        .collect::<Vec<_>>();
    models.sort_by(|left, right| {
        right.records.cmp(&left.records).then_with(|| left.model_id.cmp(&right.model_id))
    });
    models.truncate(MAX_MODEL_ROWS);
    stats.by_model = models;

    let mut tools = durations
        .into_iter()
        .map(|(name, (calls, failures, mut samples))| {
            samples.sort_unstable();
            ToolUsageRow {
                name,
                calls,
                failures,
                p50_ms: percentile(&samples, 50),
                p95_ms: percentile(&samples, 95),
                max_ms: samples.last().copied().unwrap_or(0),
            }
        })
        .collect::<Vec<_>>();
    tools.sort_by(|left, right| {
        right.calls.cmp(&left.calls).then_with(|| left.name.cmp(&right.name))
    });
    tools.truncate(MAX_TOOL_ROWS);
    stats.tools = tools;
    Ok(stats)
}

/// Nearest-rank percentile over an already-sorted sample set.
fn percentile(sorted: &[u64], percent: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (percent * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

/// Model-facing read-only `usage_stats` tool.
#[derive(Debug)]
pub struct UsageStatsTool {
    log: Arc<SessionLog>,
}

impl UsageStatsTool {
    /// Creates the tool over one session's log.
    #[must_use]
    pub fn new(log: Arc<SessionLog>) -> Self {
        Self { log }
    }

    /// One bounded text summary; the structured payload carries the full rows.
    fn render(stats: &UsageStats) -> String {
        let totals = &stats.totals;
        let mut out = format!(
            "session {}: messages={} model-requests={} usage-records={} tool-calls={} turns={} input={} output={} reasoning={} cache-read={} cache-write={} cost-micros={}",
            stats.session_id,
            stats.messages,
            stats.model_requests,
            stats.usage_records,
            stats.tool_calls,
            stats.turns,
            totals.input_tokens,
            totals.output_tokens,
            totals.reasoning_tokens,
            totals.cache_read_tokens,
            totals.cache_write_tokens,
            totals.cost_micros,
        );
        for model in &stats.by_model {
            out.push_str(&format!(
                "\nmodel {}: records={} input={} output={} reasoning={} cache-read={} cache-write={} cost-micros={}",
                model.model_id,
                model.records,
                model.totals.input_tokens,
                model.totals.output_tokens,
                model.totals.reasoning_tokens,
                model.totals.cache_read_tokens,
                model.totals.cache_write_tokens,
                model.totals.cost_micros,
            ));
        }
        for tool in &stats.tools {
            out.push_str(&format!(
                "\ntool {}: calls={} failures={} p50={}ms p95={}ms max={}ms",
                tool.name, tool.calls, tool.failures, tool.p50_ms, tool.p95_ms, tool.max_ms,
            ));
        }
        out
    }
}

impl ServerTool for UsageStatsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "usage_stats",
            "Report deterministic session usage: token/cost totals per model and per-tool call counts with success and duration percentiles. Computed from the session log; no secrets or client handles.",
        )
        .input_schema(json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        }))
        .side_effect_class(SideEffectClass::Read)
    }

    fn execute(
        &self,
        _arguments: serde_json::Value,
        _client: ee_agent_acp_server::ClientBridge,
        _cancel: tokio::sync::watch::Receiver<bool>,
        _context: ToolCallContext,
    ) -> ToolFuture<ToolResult> {
        let log = self.log.clone();
        Box::pin(async move {
            match collect_usage_stats(&log) {
                Ok(stats) => match serde_json::to_value(&stats) {
                    Ok(value) => ToolResult::success_structured(Self::render(&stats), value),
                    Err(error) => {
                        ToolResult::failure(ToolErrorKind::Backend, format!("usage_stats: {error}"))
                    }
                },
                Err(error) => {
                    ToolResult::failure(ToolErrorKind::Backend, format!("usage_stats: {error}"))
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_with_usage_and_tools() -> Arc<SessionLog> {
        let log = Arc::new(SessionLog::in_memory("s-1"));
        log.record_turn_started("task-1").expect("turn");
        log.record_usage(
            &ModelUsage::new().with_input_tokens(100).with_output_tokens(20).with_cost_micros(5),
            Some("model-a"),
        )
        .expect("usage a");
        log.record_usage(
            &ModelUsage::new().with_input_tokens(40).with_output_tokens(10).with_cost_micros(2),
            Some("model-b"),
        )
        .expect("usage b");
        log.record_usage(
            &ModelUsage::new().with_input_tokens(5).with_output_tokens(1),
            Some("model-a"),
        )
        .expect("usage a2");
        for (name, success, ms) in [
            ("read_file", true, 10u64),
            ("read_file", true, 20),
            ("read_file", false, 30),
            ("grep", true, 5),
        ] {
            log.record_tool(name, success, (!success).then_some("backend"), ms).expect("tool");
        }
        log
    }

    #[test]
    fn stats_report_per_model_totals_and_tool_percentiles() {
        let stats = collect_usage_stats(&log_with_usage_and_tools()).expect("stats");
        assert_eq!(stats.turns, 1);
        assert_eq!(stats.usage_records, 3);
        assert_eq!(stats.totals.input_tokens, 145);
        assert_eq!(stats.totals.output_tokens, 31);
        assert_eq!(stats.totals.cost_micros, 7);
        assert_eq!(stats.by_model[0].model_id, "model-a");
        assert_eq!(stats.by_model[0].records, 2);
        assert_eq!(stats.by_model[0].totals.input_tokens, 105);
        assert_eq!(stats.by_model[1].model_id, "model-b");

        let read = stats.tools.iter().find(|tool| tool.name == "read_file").expect("read stats");
        assert_eq!(read.calls, 3);
        assert_eq!(read.failures, 1);
        assert_eq!(read.p50_ms, 20);
        assert_eq!(read.p95_ms, 30);
        assert_eq!(read.max_ms, 30);
        assert_eq!(stats.tools[0].name, "read_file", "most-called tool first");
    }

    #[test]
    fn stats_are_computed_from_the_log_only() {
        let log = log_with_usage_and_tools();
        let first = collect_usage_stats(&log).expect("first");
        let second = collect_usage_stats(&log).expect("second");
        assert_eq!(first, second, "deterministic query");
    }

    #[tokio::test]
    async fn usage_stats_tool_returns_structured_and_bounded_text() {
        let tool = UsageStatsTool::new(log_with_usage_and_tools());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (cancel, cancel_rx) = tokio::sync::watch::channel(false);
        let _ = cancel;
        let task = crate::tasks::TaskNode::new(crate::tasks::TaskId::new("t"), "t", "d");
        let result = tool
            .execute(
                json!({}),
                ee_agent_acp_server::ClientBridge::new_for_test(
                    std::time::Duration::from_secs(1),
                    tx,
                ),
                cancel_rx,
                ToolCallContext {
                    task,
                    transcript: Vec::new(),
                    events: crate::events::EventRecorder::new(),
                    scope: None,
                    model_id: None,
                    terminal_wait: crate::tools::TerminalWaitLimits::default(),
                },
            )
            .await;
        assert!(result.success);
        assert!(result.text_output.contains("read_file"));
        let structured = result.structured_output.expect("structured");
        assert_eq!(structured["totals"]["input_tokens"], 145);
    }
}
