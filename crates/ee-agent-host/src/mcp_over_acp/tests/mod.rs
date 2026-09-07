//! MCP-over-ACP tests: harness.
use super::*;
use crate::reducer::SessionState;
use crate::turn_evidence::{
    EvidenceCheck, EvidenceRevision, HostValidationRecord, PromptTerminalOutcome,
    TurnEvidenceStore, TurnObservation, WriteEvidenceOutcome,
};
use rmcp::model::JsonRpcMessage as ModelJsonRpcMessage;
use serde_json::json;

mod evidence_tests;
mod memory_tests;
mod proxy_tests;
