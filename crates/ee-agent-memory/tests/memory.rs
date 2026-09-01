use std::{
    fs,
    sync::{Arc, Barrier},
    thread,
};

use chrono::{Duration, Utc};
use ee_agent_memory::*;
use tempfile::TempDir;

struct Harness {
    _temp: TempDir,
    root_a: std::path::PathBuf,
    root_b: std::path::PathBuf,
    db: std::path::PathBuf,
}

impl Harness {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root_a = temp.path().join("a");
        let root_b = temp.path().join("b");
        fs::create_dir_all(&root_a).unwrap();
        fs::create_dir_all(&root_b).unwrap();
        let db = temp.path().join("state").join("memory.sqlite3");
        Self { _temp: temp, root_a, root_b, db }
    }

    fn config(&self) -> MemoryConfig {
        MemoryConfig { enabled: true, ..MemoryConfig::default() }
    }
    fn store(&self) -> WorkspaceMemory {
        WorkspaceMemory::at_path(&self.db, self.config()).unwrap()
    }
    fn a(&self) -> WorkspaceIdentity {
        WorkspaceIdentity::new(&self.root_a).unwrap()
    }
    fn b(&self) -> WorkspaceIdentity {
        WorkspaceIdentity::new(&self.root_b).unwrap()
    }
}

fn fact(namespace: &str, key: &str, value: &str, authority: FactAuthority) -> NewWorkspaceFact {
    NewWorkspaceFact {
        namespace: namespace.into(),
        key: key.into(),
        value: value.into(),
        kind: FactKind::Architecture,
        authority,
        freshness: FactFreshness::Current,
        provenance: FactProvenance {
            source_kind: "test".into(),
            source_id: format!("source-{key}"),
            source_revision: None,
            source_fingerprint: None,
            verified_at: None,
        },
        expires_at: None,
        relations: vec![],
    }
}

#[test]
fn disabled_and_unapproved_mutations_fail_closed() {
    let harness = Harness::new();
    let disabled = WorkspaceMemory::at_path(&harness.db, MemoryConfig::default()).unwrap();
    assert!(!harness.db.exists());
    assert!(matches!(
        disabled.remember(
            &harness.a(),
            fact("project", "one", "safe fact", FactAuthority::UserAsserted),
            MutationApproval::Approved
        ),
        Err(MemoryError::Disabled)
    ));

    let store = harness.store();
    assert!(matches!(
        store.remember(
            &harness.a(),
            fact("project", "one", "safe fact", FactAuthority::UserAsserted),
            MutationApproval::Denied
        ),
        Err(MemoryError::ApprovalRequired)
    ));
    assert!(store.read(&harness.a(), "project", "one").unwrap().is_none());
}

#[test]
fn candidates_require_verified_promotion_and_lifecycle_is_enforced() {
    let harness = Harness::new();
    let store = harness.store();
    let candidate = store
        .remember(
            &harness.a(),
            fact("project", "layers", "backend owns semantics", FactAuthority::AgentCandidate),
            MutationApproval::Approved,
        )
        .unwrap();
    assert_eq!(candidate.state, FactState::Candidate);
    assert!(store.read(&harness.a(), "project", "layers").unwrap().is_none());
    assert!(matches!(
        store.promote_candidate(
            &harness.a(),
            candidate.id,
            FactAuthority::AgentCandidate,
            MutationApproval::Approved
        ),
        Err(MemoryError::InvalidTransition)
    ));

    let promoted = store
        .promote_candidate(
            &harness.a(),
            candidate.id,
            FactAuthority::HostVerified,
            MutationApproval::Approved,
        )
        .unwrap();
    assert_eq!(promoted.state, FactState::Active);
    assert_eq!(promoted.authority, FactAuthority::HostVerified);
    assert!(promoted.provenance.verified_at.is_some());
    store.retract(&harness.a(), "project", "layers", MutationApproval::Approved).unwrap();
    assert!(store.read(&harness.a(), "project", "layers").unwrap().is_none());
}

#[test]
fn conflicts_require_explicit_replace_and_preserve_versions() {
    let harness = Harness::new();
    let store = harness.store();
    let original = store
        .remember(
            &harness.a(),
            fact("project", "layout", "one layer", FactAuthority::UserAsserted),
            MutationApproval::Approved,
        )
        .unwrap();
    let duplicate = store
        .remember(
            &harness.a(),
            fact("project", "layout", "one layer", FactAuthority::UserAsserted),
            MutationApproval::Approved,
        )
        .unwrap();
    assert_eq!(original.id, duplicate.id);
    assert!(matches!(
        store.remember(
            &harness.a(),
            fact("project", "layout", "two layers", FactAuthority::UserAsserted),
            MutationApproval::Approved
        ),
        Err(MemoryError::Conflict)
    ));
    let replacement = store
        .replace(
            &harness.a(),
            fact("project", "layout", "two layers", FactAuthority::UserAsserted),
            MutationApproval::Approved,
        )
        .unwrap();
    assert_eq!(replacement.supersedes, Some(original.id));
    assert_eq!(store.read(&harness.a(), "project", "layout").unwrap().unwrap().value, "two layers");

    let connection = rusqlite::Connection::open(&harness.db).unwrap();
    let state: String = connection
        .query_row("SELECT state FROM facts WHERE id=?1", [original.id.0], |row| row.get(0))
        .unwrap();
    assert_eq!(state, "superseded");
}

#[test]
fn quotas_apply_to_values_counts_and_bytes() {
    let harness = Harness::new();
    let config = MemoryConfig {
        enabled: true,
        quotas: MemoryQuotas {
            max_value_bytes: 8,
            max_active_facts: 1,
            max_active_bytes: 8,
            max_total_facts: 2,
            max_total_bytes: 16,
            max_recall_results: 1,
        },
        ..MemoryConfig::default()
    };
    let store = WorkspaceMemory::at_path(&harness.db, config).unwrap();
    assert!(matches!(
        store.remember(
            &harness.a(),
            fact("p", "large", "123456789", FactAuthority::UserAsserted),
            MutationApproval::Approved
        ),
        Err(MemoryError::QuotaExceeded(_))
    ));
    store
        .remember(
            &harness.a(),
            fact("p", "one", "1234", FactAuthority::UserAsserted),
            MutationApproval::Approved,
        )
        .unwrap();
    assert!(matches!(
        store.remember(
            &harness.a(),
            fact("p", "two", "1234", FactAuthority::UserAsserted),
            MutationApproval::Approved
        ),
        Err(MemoryError::QuotaExceeded("active fact count"))
    ));
}

#[test]
fn retained_candidates_and_superseded_history_obey_total_quotas() {
    let harness = Harness::new();
    let config = MemoryConfig {
        enabled: true,
        quotas: MemoryQuotas {
            max_value_bytes: 16,
            max_active_facts: 8,
            max_active_bytes: 128,
            max_total_facts: 2,
            max_total_bytes: 16,
            max_recall_results: 8,
        },
        ..MemoryConfig::default()
    };
    let store = WorkspaceMemory::at_path(&harness.db, config).unwrap();
    for key in ["one", "two"] {
        store
            .remember(
                &harness.a(),
                fact("p", key, "1234", FactAuthority::AgentCandidate),
                MutationApproval::Approved,
            )
            .unwrap();
    }
    assert!(matches!(
        store.remember(
            &harness.a(),
            fact("p", "three", "1234", FactAuthority::AgentCandidate),
            MutationApproval::Approved,
        ),
        Err(MemoryError::QuotaExceeded("total fact count"))
    ));

    store.forget(&harness.a(), "p", "one", MutationApproval::Approved).unwrap();
    store
        .remember(
            &harness.a(),
            fact("p", "active", "1234", FactAuthority::UserAsserted),
            MutationApproval::Approved,
        )
        .unwrap();
    assert!(matches!(
        store.replace(
            &harness.a(),
            fact("p", "active", "5678", FactAuthority::UserAsserted),
            MutationApproval::Approved,
        ),
        Err(MemoryError::QuotaExceeded("total fact count"))
    ));
}

#[test]
fn persistence_restart_and_workspace_isolation_hold() {
    let harness = Harness::new();
    harness
        .store()
        .remember(
            &harness.a(),
            fact("project", "owner", "team alpha", FactAuthority::UserAsserted),
            MutationApproval::Approved,
        )
        .unwrap();
    let restarted = harness.store();
    assert_eq!(
        restarted.read(&harness.a(), "project", "owner").unwrap().unwrap().value,
        "team alpha"
    );
    assert!(restarted.read(&harness.b(), "project", "owner").unwrap().is_none());
    assert_eq!(restarted.clear(&harness.b(), MutationApproval::Approved).unwrap(), 0);
    assert!(restarted.read(&harness.a(), "project", "owner").unwrap().is_some());
}

#[test]
fn recall_orders_exact_prefix_then_fts_and_reports_bounds() {
    let harness = Harness::new();
    let store = harness.store();
    for (key, value) in [
        ("build", "compile workspace"),
        ("builder", "compile helper"),
        ("other", "build command details"),
    ] {
        store
            .remember(
                &harness.a(),
                fact("commands", key, value, FactAuthority::HostVerified),
                MutationApproval::Approved,
            )
            .unwrap();
    }
    let roots = WorkspaceRootSet::new([&harness.root_a]).unwrap();
    let result = store
        .recall(&roots, &FactQuery { text: "build".into(), limit: Some(2), ..FactQuery::default() })
        .unwrap();
    assert_eq!(
        result.hits.iter().map(|hit| hit.reason).collect::<Vec<_>>(),
        [SelectionReason::ExactKey, SelectionReason::KeyPrefix]
    );
    assert!(result.truncated);
    assert_eq!(result.omitted_count, 1);
}

#[test]
fn stale_and_expired_facts_are_filtered() {
    let harness = Harness::new();
    let store = harness.store();
    let mut stale = fact("project", "stale", "old architecture", FactAuthority::HostVerified);
    stale.provenance.source_kind = "file".into();
    stale.provenance.source_id = "src/lib.rs".into();
    store.remember(&harness.a(), stale, MutationApproval::Approved).unwrap();
    let mut expired =
        fact("project", "expired", "expired architecture", FactAuthority::HostVerified);
    expired.expires_at = Some(Utc::now() - Duration::seconds(1));
    store.remember(&harness.a(), expired, MutationApproval::Approved).unwrap();
    assert_eq!(
        store
            .mark_stale_by_source(&harness.a(), "file", "src/lib.rs", MutationApproval::Approved)
            .unwrap(),
        1
    );
    let roots = WorkspaceRootSet::new([&harness.root_a]).unwrap();
    let result = store
        .recall(&roots, &FactQuery { text: "architecture".into(), ..FactQuery::default() })
        .unwrap();
    assert!(result.hits.is_empty());

    let with_stale = store
        .recall(
            &roots,
            &FactQuery { text: "architecture".into(), include_stale: true, ..FactQuery::default() },
        )
        .unwrap();
    assert_eq!(with_stale.hits.len(), 1);
    assert_eq!(with_stale.hits[0].fact.state, FactState::Stale);
    assert_eq!(with_stale.hits[0].fact.freshness, FactFreshness::Stale);
}

#[test]
fn default_expiry_applies_only_when_fact_has_no_explicit_expiry() {
    let harness = Harness::new();
    let config = MemoryConfig {
        enabled: true,
        retention: MemoryRetention {
            default_expiry: Some(std::time::Duration::from_secs(60)),
            ..MemoryRetention::default()
        },
        ..MemoryConfig::default()
    };
    let store = WorkspaceMemory::at_path(&harness.db, config).unwrap();
    let before = Utc::now() + Duration::seconds(59);
    let implicit = store
        .remember(
            &harness.a(),
            fact("project", "implicit", "safe", FactAuthority::UserAsserted),
            MutationApproval::Approved,
        )
        .unwrap();
    assert!(implicit.expires_at.is_some_and(|expiry| expiry > before));

    let explicit_expiry = Utc::now() + Duration::seconds(10);
    let mut explicit = fact("project", "explicit", "safe", FactAuthority::UserAsserted);
    explicit.expires_at = Some(explicit_expiry);
    let explicit = store.remember(&harness.a(), explicit, MutationApproval::Approved).unwrap();
    assert_eq!(explicit.expires_at, Some(explicit_expiry));
}

#[test]
fn retained_pruning_uses_per_state_inclusive_cutoffs_and_keeps_newer_rows() {
    let harness = Harness::new();
    let config = MemoryConfig {
        enabled: true,
        retention: MemoryRetention {
            default_expiry: None,
            candidate_retention: std::time::Duration::from_secs(10),
            stale_retention: std::time::Duration::from_secs(20),
            superseded_retention: std::time::Duration::from_secs(30),
        },
        ..MemoryConfig::default()
    };
    let store = WorkspaceMemory::at_path(&harness.db, config).unwrap();
    let workspace = harness.a();
    let candidate = store
        .remember(
            &workspace,
            fact("project", "candidate", "safe", FactAuthority::AgentCandidate),
            MutationApproval::Approved,
        )
        .unwrap();
    let stale_old = store
        .remember(
            &workspace,
            fact("project", "stale-old", "safe", FactAuthority::HostVerified),
            MutationApproval::Approved,
        )
        .unwrap();
    let stale_new = store
        .remember(
            &workspace,
            fact("project", "stale-new", "safe", FactAuthority::HostVerified),
            MutationApproval::Approved,
        )
        .unwrap();
    let superseded = store
        .remember(
            &workspace,
            fact("project", "history", "old", FactAuthority::UserAsserted),
            MutationApproval::Approved,
        )
        .unwrap();
    store
        .replace(
            &workspace,
            fact("project", "history", "new", FactAuthority::UserAsserted),
            MutationApproval::Approved,
        )
        .unwrap();
    let mut expired = fact("project", "expired-active", "safe", FactAuthority::UserAsserted);
    let now = Utc::now() + Duration::minutes(1);
    expired.expires_at = Some(now);
    let expired = store.remember(&workspace, expired, MutationApproval::Approved).unwrap();

    let connection = rusqlite::Connection::open(&harness.db).unwrap();
    for (id, state, updated_at) in [
        (candidate.id.0, "candidate", now - Duration::seconds(10)),
        (stale_old.id.0, "stale", now - Duration::seconds(20)),
        (stale_new.id.0, "stale", now - Duration::seconds(19)),
        (superseded.id.0, "superseded", now - Duration::seconds(30)),
    ] {
        connection
            .execute(
                "UPDATE facts SET state=?1, freshness=CASE WHEN ?1='stale' THEN 'stale' ELSE freshness END, updated_at=?2 WHERE id=?3",
                rusqlite::params![state, updated_at.to_rfc3339(), id],
            )
            .unwrap();
    }
    drop(connection);

    assert_eq!(store.prune_retained(&workspace, now, MutationApproval::Approved).unwrap(), 4);
    let connection = rusqlite::Connection::open(&harness.db).unwrap();
    for deleted in [candidate.id, stale_old.id, superseded.id, expired.id] {
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM facts WHERE id=?1", [deleted.0], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "fact {} should be pruned", deleted.0);
    }
    let kept: i64 = connection
        .query_row("SELECT COUNT(*) FROM facts WHERE id=?1", [stale_new.id.0], |row| row.get(0))
        .unwrap();
    assert_eq!(kept, 1);
}

#[test]
fn export_import_is_bounded_atomic_and_rejects_poisoning() {
    let source = Harness::new();
    let source_store = source.store();
    source_store
        .remember(
            &source.a(),
            fact("project", "safe", "layered design", FactAuthority::UserAsserted),
            MutationApproval::Approved,
        )
        .unwrap();
    let export = source_store.export(&source.a(), false, MutationApproval::Approved).unwrap();
    let target = Harness::new();
    let target_store = target.store();
    let mut remapped = export.clone();
    remapped.workspace_digest = target.a().digest().into();
    assert_eq!(target_store.import(&target.a(), remapped, MutationApproval::Approved).unwrap(), 1);

    let mut poisoned = WorkspaceExport {
        schema_version: SCHEMA_VERSION,
        workspace_digest: target.a().digest().into(),
        redacted: false,
        facts: vec![export.facts[0].clone()],
    };
    poisoned.facts.push(ExportedFact {
        key: "secret".into(),
        value: Some("token=stolen".into()),
        ..poisoned.facts[0].clone()
    });
    assert!(matches!(
        target_store.import(&target.a(), poisoned, MutationApproval::Approved),
        Err(MemoryError::SensitiveMaterial)
    ));
    assert!(target_store.read(&target.a(), "project", "new").unwrap().is_none());

    let redacted = source_store.export(&source.a(), true, MutationApproval::Approved).unwrap();
    assert!(matches!(
        source_store.import(&source.a(), redacted, MutationApproval::Approved),
        Err(MemoryError::RedactedImport)
    ));
}

#[test]
fn concurrent_writers_serialize_without_lost_facts() {
    let harness = Harness::new();
    let store = Arc::new(harness.store());
    let workspace = Arc::new(harness.a());
    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    for index in 0..2 {
        let store = Arc::clone(&store);
        let workspace = Arc::clone(&workspace);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            store
                .remember(
                    &workspace,
                    fact("parallel", &format!("key-{index}"), "safe", FactAuthority::HostVerified),
                    MutationApproval::Approved,
                )
                .unwrap();
        }));
    }
    barrier.wait();
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(store.status(&workspace).unwrap().active_facts, 2);
}

#[test]
fn migration_reapplies_missing_version_two_index() {
    let harness = Harness::new();
    drop(harness.store());
    let connection = rusqlite::Connection::open(&harness.db).unwrap();
    connection.execute_batch("DROP INDEX facts_active_expiry; PRAGMA user_version=1;").unwrap();
    drop(connection);
    drop(harness.store());
    let connection = rusqlite::Connection::open(&harness.db).unwrap();
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='facts_active_expiry'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[cfg(unix)]
#[test]
fn database_and_parent_permissions_are_restrictive() {
    use std::os::unix::fs::PermissionsExt;
    let harness = Harness::new();
    drop(harness.store());
    assert_eq!(
        fs::metadata(harness.db.parent().unwrap()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(fs::metadata(&harness.db).unwrap().permissions().mode() & 0o777, 0o600);
}

#[test]
fn corrupt_database_is_quarantined_and_values_never_enter_error() {
    let harness = Harness::new();
    fs::create_dir_all(harness.db.parent().unwrap()).unwrap();
    fs::write(&harness.db, b"not a sqlite database containing super-secret-value").unwrap();
    fs::write(format!("{}-wal", harness.db.display()), b"wal").unwrap();
    fs::write(format!("{}-shm", harness.db.display()), b"shm").unwrap();
    let error = WorkspaceMemory::at_path(&harness.db, harness.config()).unwrap_err();
    let rendered = error.to_string();
    assert!(matches!(error, MemoryError::DatabaseQuarantined { .. }));
    assert!(!rendered.contains("super-secret-value"));
    assert!(!harness.db.exists());
    let quarantined: Vec<_> = fs::read_dir(harness.db.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("corrupt-"))
        .collect();
    assert_eq!(quarantined.len(), 3);
}
