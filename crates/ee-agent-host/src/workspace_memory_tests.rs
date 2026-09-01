use super::*;
use chrono::{Duration as ChronoDuration, Utc};
use tempfile::tempdir;

fn enabled_config(root: PathBuf, database_path: PathBuf) -> WorkspaceMemoryHostConfig {
    WorkspaceMemoryHostConfig {
        enabled: true,
        trusted_roots: vec![root],
        database_path: Some(database_path),
        ..WorkspaceMemoryHostConfig::default()
    }
}

#[test]
fn disabled_and_invalid_enabled_configuration_fail_closed() {
    let disabled = WorkspaceMemoryHost::disabled();
    let error = disabled.read("key".to_string()).expect_err("disabled read fails");
    assert_eq!(error.message, "workspace_memory_unavailable: workspace memory is disabled");

    let temp = tempdir().expect("tempdir");
    let invalid = WorkspaceMemoryHost::new(&WorkspaceMemoryHostConfig {
        enabled: true,
        trusted_roots: vec![temp.path().join("missing")],
        database_path: Some(temp.path().join("memory.sqlite3")),
        ..WorkspaceMemoryHostConfig::default()
    });
    let error = invalid.recall("key".to_string()).expect_err("invalid root fails closed");
    assert_eq!(error.message, "workspace_memory_unavailable: workspace memory is unavailable");
}

#[test]
fn enabled_host_prunes_expired_and_retained_history_during_startup() {
    let temp = tempdir().expect("tempdir");
    let root = temp.path().join("workspace");
    std::fs::create_dir_all(&root).expect("workspace root");
    let database = temp.path().join("memory.sqlite3");
    let config = enabled_config(root, database.clone());
    let first = WorkspaceMemoryHost::new(&config);
    for key in ["candidate", "stale", "superseded", "expired", "recent-stale"] {
        first
            .remember(key.to_string(), "safe value".to_string(), format!("source-{key}"))
            .expect("remember seed");
    }
    drop(first);

    let now = Utc::now();
    let connection = rusqlite::Connection::open(&database).expect("open memory database");
    for (key, state, age_days) in [
        ("candidate", "candidate", 8),
        ("stale", "stale", 31),
        ("superseded", "superseded", 91),
        ("recent-stale", "stale", 29),
    ] {
        connection
            .execute(
                "UPDATE facts SET state=?1, freshness=CASE WHEN ?1='stale' THEN 'stale' ELSE freshness END, updated_at=?2 WHERE normalized_key=?3",
                rusqlite::params![state, (now - ChronoDuration::days(age_days)).to_rfc3339(), key],
            )
            .expect("age retained row");
    }
    connection
        .execute(
            "UPDATE facts SET expires_at=?1 WHERE normalized_key='expired'",
            [(now - ChronoDuration::seconds(1)).to_rfc3339()],
        )
        .expect("expire active row");
    drop(connection);

    let restarted = WorkspaceMemoryHost::new(&config);
    assert_eq!(restarted.status().availability, WorkspaceMemoryAvailability::Available);
    let connection = rusqlite::Connection::open(&database).expect("reopen memory database");
    for key in ["candidate", "stale", "superseded", "expired"] {
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM facts WHERE normalized_key=?1", [key], |row| {
                row.get(0)
            })
            .expect("count pruned row");
        assert_eq!(count, 0, "{key} should be pruned");
    }
    let recent: i64 = connection
        .query_row("SELECT COUNT(*) FROM facts WHERE normalized_key='recent-stale'", [], |row| {
            row.get(0)
        })
        .expect("count retained row");
    assert_eq!(recent, 1);
}

#[test]
fn shared_database_persists_across_hosts_and_isolates_roots() {
    let temp = tempdir().expect("tempdir");
    let root_a = temp.path().join("a");
    let root_b = temp.path().join("b");
    std::fs::create_dir_all(&root_a).expect("root a");
    std::fs::create_dir_all(&root_b).expect("root b");
    let database = temp.path().join("memory.sqlite3");

    let first = WorkspaceMemoryHost::new(&enabled_config(root_a.clone(), database.clone()));
    first
        .remember(
            "architecture.parser".to_string(),
            "Tree-sitter runs in backend".to_string(),
            "mcp:source-a".to_string(),
        )
        .expect("remember");

    let second = WorkspaceMemoryHost::new(&enabled_config(root_a, database.clone()));
    assert_eq!(
        second.read("architecture.parser".to_string()).expect("shared read").value,
        "Tree-sitter runs in backend"
    );
    assert_eq!(second.recall("parser".to_string()).expect("shared recall").facts.len(), 1);

    let isolated = WorkspaceMemoryHost::new(&enabled_config(root_b, database));
    let error =
        isolated.read("architecture.parser".to_string()).expect_err("other root cannot read");
    assert_eq!(error.message, "workspace_fact_not_found: workspace fact not found");
}
