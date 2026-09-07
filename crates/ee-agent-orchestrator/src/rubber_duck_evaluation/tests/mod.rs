//! Rubber-duck evaluation tests: harness.
use super::*;

async fn baseline_report() -> RubberDuckGateReport {
    let runs = run_required_rubber_duck_suite().await.unwrap();
    let baseline = required_rubber_duck_baseline().unwrap();
    evaluate_rubber_duck_gate(&baseline, aggregate_rubber_duck_runs(&runs).unwrap())
}

fn fixture_json(mutator: impl FnOnce(&mut serde_json::Value)) -> String {
    let mut value: serde_json::Value =
        serde_json::from_str(REQUIRED_RUBBER_DUCK_FIXTURE_SUITE).unwrap();
    mutator(&mut value);
    serde_json::to_string(&value).unwrap()
}

mod evaluation_tests;
