//! Shared `_meta` marker for approval-classifier auto-approval.
//!
//! The orchestrator adds this marker only after its risk classifier approved a
//! host-gated tool call. The marker is advisory: a frontend must still verify
//! that the requesting agent is one of its own orchestrator-backed binaries
//! before skipping an approval prompt, and host safeguards always win.

use serde_json::Value;

use crate::Meta;

/// `_meta` key carrying the classifier verdict.
pub const APPROVAL_CLASSIFIER_META_KEY: &str = "ee/approval_classifier";
/// Version of the marker payload.
pub const APPROVAL_CLASSIFIER_META_VERSION: u64 = 1;

/// Adds the classifier auto-approval marker to existing request metadata.
#[must_use]
pub fn mark_classifier_auto_approved(meta: Option<Meta>) -> Meta {
    let mut meta = meta.unwrap_or_default();
    meta.insert(
        APPROVAL_CLASSIFIER_META_KEY.to_string(),
        serde_json::json!({
            "auto_approved": true,
            "version": APPROVAL_CLASSIFIER_META_VERSION,
        }),
    );
    meta
}

/// Whether request metadata carries a well-formed classifier auto-approval.
#[must_use]
pub fn classifier_auto_approved(meta: Option<&Meta>) -> bool {
    let Some(value) = meta.and_then(|meta| meta.get(APPROVAL_CLASSIFIER_META_KEY)) else {
        return false;
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    object.get("version").and_then(Value::as_u64) == Some(APPROVAL_CLASSIFIER_META_VERSION)
        && object.get("auto_approved").and_then(Value::as_bool) == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_round_trips_and_rejects_other_payloads() {
        let meta = mark_classifier_auto_approved(None);
        assert!(classifier_auto_approved(Some(&meta)));

        let mut wrong_version = meta.clone();
        wrong_version.insert(
            APPROVAL_CLASSIFIER_META_KEY.to_string(),
            serde_json::json!({ "auto_approved": true, "version": 99 }),
        );
        assert!(!classifier_auto_approved(Some(&wrong_version)));

        let mut denied = meta;
        denied.insert(
            APPROVAL_CLASSIFIER_META_KEY.to_string(),
            serde_json::json!({ "auto_approved": false, "version": 1 }),
        );
        assert!(!classifier_auto_approved(Some(&denied)));
    }

    #[test]
    fn missing_or_foreign_metadata_is_not_trusted() {
        assert!(!classifier_auto_approved(None));
        let foreign = serde_json::json!({ "other": true }).as_object().expect("object").clone();
        assert!(!classifier_auto_approved(Some(&foreign)));
    }
}
