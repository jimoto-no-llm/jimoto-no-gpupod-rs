//! StateStore の往復と操作ロック。

mod common;
use common::MockEngine;

use std::sync::Arc;

use gpupod_agent::{Agent, StateStore};

fn store_in(directory: &std::path::Path) -> StateStore {
    StateStore::new(directory).expect("state store")
}

#[test]
fn default_state_round_trips_and_locks() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let store = store_in(temporary.path());

    let node_id = {
        let guard = store.locked().expect("first lock");
        assert_eq!(guard.version, 1);
        assert!(!guard.manual_enabled);
        assert!(!guard.auto_busy);
        assert!(guard.leases.is_empty());
        assert_eq!(guard.node_id.len(), 32);
        assert!(guard.node_id.chars().all(|c| c.is_ascii_hexdigit()));
        guard.node_id.clone()
    };

    // 保存して読み直すと値が残る。
    {
        let mut guard = store.locked().expect("second lock");
        guard.manual_enabled = true;
        guard.save().expect("save");
    }
    {
        let guard = store.locked().expect("third lock");
        assert!(guard.manual_enabled);
        assert_eq!(guard.node_id, node_id);
    }
}

#[test]
fn operation_lock_is_not_reentrant() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let store = store_in(temporary.path());

    let first = store.operation("lease-one").expect("operation");
    assert!(first.acquired());

    // 同じ貸出の操作は同時に走らせない。
    let second = store.operation("lease-one").expect("operation");
    assert!(!second.acquired());

    // 別の貸出は同時にできる。
    let other = store.operation("lease-two").expect("operation");
    assert!(other.acquired());

    drop(first);
    let third = store.operation("lease-one").expect("operation");
    assert!(third.acquired());
}

#[test]
fn operation_rejects_invalid_identifier() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let store = store_in(temporary.path());
    let error = store.operation("-bad").expect_err("invalid id");
    assert_eq!(error.0, "Invalid identifier");
}

#[test]
fn legacy_state_json_is_refused() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let store = store_in(temporary.path());
    std::fs::write(store.directory.join("state.json"), "{}").expect("legacy file");
    let error = store.locked().expect_err("legacy state");
    assert_eq!(
        error.0,
        "Legacy state.json exists; migrate it before starting the SQLite agent"
    );
}

#[test]
fn unsupported_payload_is_refused() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let store = store_in(temporary.path());
    {
        let guard = store.locked().expect("lock");
        let _ = &guard.node_id;
    }
    let connection = rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE;
    let raw = rusqlite::Connection::open_with_flags(&store.path, connection).expect("open");
    raw.execute(
        "UPDATE agent_state SET payload = ?1 WHERE id = 1",
        rusqlite::params![r#"{"version": 2, "node_id": "abc", "manual_enabled": false, "auto_busy": false, "leases": {}}"#],
    )
    .expect("update");
    drop(raw);

    let error = store.locked().expect_err("invalid payload");
    assert_eq!(
        error.0,
        "Unsupported or invalid state; existing reservations retained"
    );
}

#[test]
fn malformed_json_is_refused() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let store = store_in(temporary.path());
    {
        let _ = store.locked().expect("lock");
    }
    let raw = rusqlite::Connection::open(&store.path).expect("open");
    raw.execute(
        "INSERT INTO agent_state(id, payload) VALUES(1, ?1) \
         ON CONFLICT(id) DO UPDATE SET payload = excluded.payload",
        rusqlite::params!["not json"],
    )
    .expect("update");
    drop(raw);
    let error = store.locked().expect_err("malformed payload");
    assert_eq!(
        error.0,
        "Unsupported or invalid state; existing reservations retained"
    );
}

#[test]
fn agent_status_reports_pool_summary() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(store_in(temporary.path()));
    let engine = Arc::new(MockEngine::default());
    let agent = Agent::new(store, engine, Default::default(), Vec::new());
    let status = agent.status().expect("status");
    assert_eq!(status["accepting"], serde_json::json!(false));
    assert_eq!(status["resources"], serde_json::json!(null));
    assert!(status["gpus"].as_array().unwrap().is_empty());
}
