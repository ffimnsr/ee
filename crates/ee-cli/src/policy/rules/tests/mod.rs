//! Policy-rules tests: harness.
use super::*;
use crate::policy::{TransportKind, TrustOperation};

fn workspace() -> WorkspaceIdentity {
    WorkspaceIdentity::from_canonical_root_bytes(b"/phase9")
}

fn operation(category: TrustCategory, identity: OperationIdentity) -> TrustOperation {
    TrustOperation {
        workspace: workspace(),
        agent: None,
        transport: TransportKind::McpStdio,
        category,
        identity,
    }
}

mod rules_eval_tests;
