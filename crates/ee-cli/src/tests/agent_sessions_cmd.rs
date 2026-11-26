//! `ee do agent sessions prune` command tests.

use std::fs;

use crate::agent_session_store::{SessionArchiveRecord, WorkspaceSessionArchive};
use crate::agent_sessions_cmd::prune_at;
use crate::app::agent_pane::{MessageRenderKind, TranscriptItem};
use crate::args::AgentSessionsPruneArgs;
use crate::policy::WorkspaceIdentity;

fn message(text: &str) -> TranscriptItem {
    TranscriptItem::Message {
        nick: String::from("agent"),
        text: text.to_string(),
        kind: MessageRenderKind::Assistant,
        message_id: None,
        response_group: None,
        at: std::time::SystemTime::UNIX_EPOCH,
    }
}

fn archive(root: &std::path::Path, workspace: &std::path::Path) -> WorkspaceSessionArchive {
    let identity =
        WorkspaceIdentity::from_canonical_root_bytes(workspace.as_os_str().as_encoded_bytes());
    WorkspaceSessionArchive::open(root.to_path_buf(), identity).expect("archive")
}

fn record(session_id: &str) -> SessionArchiveRecord {
    SessionArchiveRecord::new("agent", session_id, None)
}

fn make_old(archive: &WorkspaceSessionArchive, session_id: &str) {
    let dir = archive.session_dir(session_id).unwrap();
    let path = dir.join("meta.json");
    let mut meta: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    meta["updated_at"] = serde_json::json!(0u64);
    fs::write(&path, serde_json::to_vec(&meta).unwrap()).unwrap();
}

fn registry(state: &std::path::Path, live: &[&str]) {
    let sessions =
        live.iter().map(|id| serde_json::json!({ "session_id": id })).collect::<Vec<_>>();
    let document = serde_json::json!({
        "sha256:test": { "sessions": sessions }
    });
    fs::write(state.join("agent-sessions.json"), serde_json::to_vec(&document).unwrap()).unwrap();
}

#[test]
fn prune_removes_stale_entries_protects_live_and_honors_dry_run() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let state = temp.path().join("state");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&state).unwrap();
    let archive = archive(&data, &workspace);
    archive.save(&record("session-old"), &[message("old")]).unwrap();
    archive.save(&record("session-live"), &[message("live")]).unwrap();
    archive.save(&record("session-new"), &[message("new")]).unwrap();
    make_old(&archive, "session-old");
    make_old(&archive, "session-live");
    registry(&state, &["session-live"]);

    let args = AgentSessionsPruneArgs { older_than_days: 30, all: false, dry_run: true };
    prune_at(&data, &state, &args).unwrap();
    assert!(archive.load_transcript("session-old").unwrap().is_some(), "dry run keeps files");

    let args = AgentSessionsPruneArgs { older_than_days: 30, all: false, dry_run: false };
    prune_at(&data, &state, &args).unwrap();
    assert!(archive.load_transcript("session-old").unwrap().is_none());
    assert!(archive.load_transcript("session-live").unwrap().is_some(), "live is protected");
    assert!(archive.load_transcript("session-new").unwrap().is_some(), "recent is kept");
}

#[test]
fn malformed_registry_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let state = temp.path().join("state");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&state).unwrap();
    let archive = archive(&data, &workspace);
    archive.save(&record("session-old"), &[message("old")]).unwrap();
    make_old(&archive, "session-old");
    fs::write(state.join("agent-sessions.json"), b"{not json").unwrap();
    let args = AgentSessionsPruneArgs { older_than_days: 30, all: true, dry_run: false };
    assert!(prune_at(&data, &state, &args).is_err());
    assert!(archive.load_transcript("session-old").unwrap().is_some());
}

#[cfg(unix)]
#[test]
fn prune_never_follows_symlinked_session_dirs() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let state = temp.path().join("state");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&state).unwrap();
    let archive = archive(&data, &workspace);
    archive.save(&record("session-old"), &[message("old")]).unwrap();
    make_old(&archive, "session-old");
    let session_dir = archive.session_dir("session-old").unwrap();
    let target = temp.path().join("outside-target");
    fs::rename(&session_dir, &target).unwrap();
    std::os::unix::fs::symlink(&target, &session_dir).unwrap();

    let args = AgentSessionsPruneArgs { older_than_days: 30, all: true, dry_run: false };
    prune_at(&data, &state, &args).unwrap();
    assert!(target.join("meta.json").is_file(), "prune must not follow symlinks");
}

#[test]
fn prune_removes_stale_provider_session_logs_but_keeps_live_ones() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let state = temp.path().join("state");
    fs::create_dir_all(data.join("session-log")).unwrap();
    fs::create_dir_all(&state).unwrap();
    for id in ["stale", "live"] {
        let dir = data.join("session-log").join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("records.jsonl"), b"{}\n").unwrap();
    }
    registry(&state, &["live"]);
    let args = AgentSessionsPruneArgs { older_than_days: 30, all: true, dry_run: false };
    prune_at(&data, &state, &args).unwrap();
    assert!(!data.join("session-log").join("stale").exists());
    assert!(data.join("session-log").join("live").exists(), "live log is protected");
}
