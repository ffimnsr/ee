use super::*;

// ── Store: file behavior and atomicity ───────────────────────────────────────

#[test]
fn missing_store_loads_an_empty_document() {
    let (_base, _workspace_dir, store) = store_setup();
    let document = store.load().expect("missing store is an empty document");
    assert_eq!(document.workspace, *store.workspace());
    assert!(!document.workspace_enabled);
    assert!(document.rules.is_empty());
    assert!(store.effective().rules.is_empty());
    let op = command_op(*store.workspace(), "git", &["status"]);
    assert_eq!(decide_default(&op, &store.effective().rules).outcome, TrustOutcome::Confirm);
}

#[test]
fn store_filename_derives_from_digest_and_never_contains_raw_path() {
    let (base, workspace_dir, store) = store_setup();
    let filename = store.path().file_name().unwrap().to_string_lossy().to_string();
    assert_eq!(filename, format!("{}.toml", store.workspace().hex()));
    assert!(filename.ends_with(&format!("{}.toml", store.workspace().hex())));
    let raw_name = workspace_dir.path().file_name().unwrap().to_string_lossy().to_string();
    assert!(!filename.contains(&raw_name), "raw workspace path must never appear");
    assert!(!filename.contains("sha256:sha256:"));
    assert!(store.path().starts_with(base.path().join("trust")));
}

#[test]
fn store_round_trip_keeps_serialization_canonical() {
    let (_base, _workspace_dir, store) = store_setup();
    store.write(&sample_document(&store)).expect("write");
    let first = fs::read_to_string(store.path()).unwrap();
    store.write(&store.load().expect("reload")).expect("rewrite");
    let second = fs::read_to_string(store.path()).unwrap();
    assert_eq!(first, second, "load → write must be byte-stable");
}

#[test]
fn add_rule_appends_or_reuses_by_stable_rule_id() {
    let (_base, _workspace_dir, store) = store_setup();
    let ws = *store.workspace();
    let rule = command_rule(spec("cmd_1"), ws, "git", MatchMode::ArgvExact, &["status"]);

    let document = store.add_rule(rule.clone()).expect("first grant");
    assert_eq!(document.rules.len(), 1);
    let bytes_after_first = fs::read(store.path()).unwrap();

    let reused = store.add_rule(rule.clone()).expect("reuse");
    assert_eq!(reused.rules.len(), 1, "same id must not duplicate the grant");
    assert_eq!(fs::read(store.path()).unwrap(), bytes_after_first, "reuse must not rewrite");

    let second = command_rule(spec("cmd_2"), ws, "git", MatchMode::ArgvExact, &["diff"]);
    let appended = store.add_rule(second).expect("append");
    assert_eq!(appended.rules.len(), 2);
    assert_ne!(fs::read(store.path()).unwrap(), bytes_after_first, "new id appends");
}

#[test]
fn add_rule_rejects_rules_bound_to_another_workspace() {
    let (_base, _workspace_dir, store) = store_setup();
    let foreign = command_rule(
        spec("cmd_1"),
        identity(b"/work/other"),
        "git",
        MatchMode::ArgvExact,
        &["status"],
    );
    match store.add_rule(foreign) {
        Err(TrustStoreError::IdentityMismatch) => {}
        other => panic!("expected IdentityMismatch, got {other:?}"),
    }
    assert!(!store.path().exists(), "failed grant must not create the store");
}

#[cfg(unix)]
#[test]
fn symlinked_store_is_rejected() {
    let (_base, _workspace_dir, store) = store_setup();
    store.write(&sample_document(&store)).expect("write");
    let real = store.path().with_extension("real.toml");
    fs::rename(store.path(), &real).unwrap();
    std::os::unix::fs::symlink(&real, store.path()).unwrap();
    assert!(matches!(store.load(), Err(TrustStoreError::PermissionFailure(_))));
    assert!(store.effective().rules.is_empty());
}

#[cfg(unix)]
#[test]
fn insecure_store_permissions_are_rejected() {
    use std::os::unix::fs::PermissionsExt;
    let (_base, _workspace_dir, store) = store_setup();
    store.write(&sample_document(&store)).expect("write");

    fs::set_permissions(store.path(), fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        matches!(store.load(), Err(TrustStoreError::PermissionFailure(_))),
        "file 0644 must fail closed"
    );
    fs::set_permissions(store.path(), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(store.load().is_ok(), "file 0600 loads");

    let trust_dir = store.path().parent().unwrap();
    fs::set_permissions(trust_dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        matches!(store.load(), Err(TrustStoreError::PermissionFailure(_))),
        "dir 0755 must fail closed"
    );
    fs::set_permissions(trust_dir, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(store.load().is_ok(), "dir 0700 loads");
}

#[test]
fn directory_at_store_path_is_rejected() {
    let (_base, _workspace_dir, store) = store_setup();
    store.write(&sample_document(&store)).expect("write");
    fs::remove_file(store.path()).unwrap();
    fs::create_dir(store.path()).unwrap();
    assert!(matches!(store.load(), Err(TrustStoreError::PermissionFailure(_))));
    assert!(store.effective().rules.is_empty());
}

#[cfg(unix)]
#[test]
fn failed_write_preserves_prior_bytes_and_removes_temporary_file() {
    use std::os::unix::fs::PermissionsExt;
    let (_base, _workspace_dir, store) = store_setup();
    store.write(&sample_document(&store)).expect("write");
    let prior = fs::read(store.path()).unwrap();

    let trust_dir = store.path().parent().unwrap();
    fs::set_permissions(trust_dir, fs::Permissions::from_mode(0o500)).unwrap();
    let result = store.write(&sample_document(&store));
    fs::set_permissions(trust_dir, fs::Permissions::from_mode(0o700)).unwrap();

    assert!(result.is_err(), "write into read-only trust dir must fail");
    assert_eq!(fs::read(store.path()).unwrap(), prior, "failed write preserves prior store bytes");
    assert_eq!(
        dir_entries(trust_dir),
        vec![store.path().to_path_buf()],
        "no temp file left behind"
    );
}

#[test]
fn write_rejects_duplicate_or_empty_rule_ids() {
    let (_base, _workspace_dir, store) = store_setup();
    let ws = *store.workspace();
    let mut document = sample_document(&store);
    let duplicate = command_rule(spec("cmd_1"), ws, "git", MatchMode::ArgvExact, &["stash"]);
    document.rules.push(duplicate);
    assert!(matches!(store.write(&document), Err(TrustStoreError::ValidationFailure(_))));
    assert!(!store.path().exists());
}

#[test]
fn store_rules_feed_the_evaluator_end_to_end() {
    let (_base, _workspace_dir, store) = store_setup();
    let ws = *store.workspace();
    store
        .add_rule(command_rule(spec("cmd_1"), ws, "git", MatchMode::ArgvExact, &["status"]))
        .unwrap();
    let effective = store.effective();

    let matching = command_op(ws, "git", &["status"]);
    let decision = decide_default(&matching, &effective.rules);
    assert_eq!(decision.outcome, TrustOutcome::Allow);
    assert_eq!(decision.rule_id.as_deref(), Some("cmd_1"));

    // Session deny still wins over the persisted grant.
    let mut session = SessionPolicy::default();
    session.record("s1", "fp", SessionChoice::Deny);
    let denied = decide(
        &matching,
        &effective.rules,
        &session,
        "s1",
        "2026-08-07T12:00:00Z",
        &UsageSnapshot::default(),
        true,
    );
    assert_eq!(denied.outcome, TrustOutcome::Deny);
    assert_eq!(denied.reason, DecisionReason::SessionDeny);
}
