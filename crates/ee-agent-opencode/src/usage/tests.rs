//! Usage-probe parsing tests. Pure functions only: no network.

use serde_json::json;

use super::*;

#[test]
fn go_meter_parses_percent_status_and_reset() {
    let value = json!({
        "usage": {
            "rolling": { "status": "ok", "percent": 42.5, "resetsAt": "2026-11-18T00:00:00Z" },
            "weekly": { "status": "rate-limited", "percent": 100, "resetsAt": "2026-11-24T00:00:00Z" },
            "monthly": { "status": "ok", "percent": 0, "resetsAt": "2026-12-01T00:00:00Z" }
        }
    });
    let go = parse_go(&value).expect("parses");
    assert_eq!(go.rolling.percent, 42.5);
    assert!(!go.rolling.limited);
    assert!(go.weekly.limited);
    assert_eq!(go.weekly.percent, 100.0);
    assert_eq!(go.monthly.resets_at.as_deref(), Some("2026-12-01T00:00:00Z"));
}

#[test]
fn unknown_meter_status_is_rejected() {
    let value = json!({
        "usage": {
            "rolling": { "status": "weird", "percent": 1, "resetsAt": "x" },
            "weekly": { "status": "ok", "percent": 1, "resetsAt": "x" },
            "monthly": { "status": "ok", "percent": 1, "resetsAt": "x" }
        }
    });
    let error = parse_go(&value).expect_err("rejects");
    assert!(error.contains("rolling"), "{error}");
}

#[test]
fn budgets_parse_limits_spend_and_models() {
    let value = json!([{
        "ruleID": "rule-1",
        "limitDollars": 10.0,
        "spentDollars": 2.5,
        "scope": "workspace",
        "per": "month",
        "coverage": "all",
        "models": [
            { "providerID": "opencode", "modelID": "gpt-5.5", "name": "GPT-5.5", "spentDollars": 1.25 }
        ],
        "vendors": [{ "id": "openai", "name": "OpenAI" }],
        "picked": true,
        "shared": false,
        "resetsAt": "2026-12-01T00:00:00Z"
    }]);
    let budgets = parse_budgets(&value).expect("parses");
    assert_eq!(budgets.len(), 1);
    let budget = &budgets[0];
    assert_eq!(budget.id, "rule-1");
    assert_eq!(budget.limit_usd, 10.0);
    assert_eq!(budget.spent_usd, 2.5);
    assert!(budget.picked);
    assert_eq!(budget.vendors, vec![String::from("OpenAI")]);
    assert_eq!(budget.models[0].model_id, "gpt-5.5");
    assert_eq!(budget.models[0].spent_usd, Some(1.25));
}

#[test]
fn malformed_budget_payload_is_rejected() {
    let error = parse_budgets(&json!({ "budgets": [] })).expect_err("rejects");
    assert!(error.contains("not an array"), "{error}");
}

#[test]
fn report_json_never_carries_a_credential_and_marks_absent_go() {
    let report = UsageReport {
        workspace_id: Some(String::from("org-1")),
        budgets: Vec::new(),
        go: None,
        notices: vec![String::from("OpenCode Go usage is not available")],
    };
    let rendered = report.to_json().to_string();
    assert!(rendered.contains("\"go\":null"), "{rendered}");
    assert!(rendered.contains("org-1"), "{rendered}");
    assert!(!rendered.contains("Bearer"), "{rendered}");
}

#[test]
fn endpoints_stay_on_the_documented_origin() {
    assert!(budgets_endpoint().starts_with(OPENCODE_ORIGIN));
    assert!(go_usage_endpoint().starts_with(OPENCODE_ORIGIN));
    assert_eq!(USAGE_TIMEOUT.as_secs(), 15);
    assert_eq!(crate::routes::OpenCodeSurface::Go.quota_url(), Some(go_usage_endpoint().as_str()));
    assert!(crate::routes::OpenCodeSurface::Zen.quota_url().is_none());
}
