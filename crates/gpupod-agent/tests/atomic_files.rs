//! 原子書き込み（files.rs）の権限の扱い。

use std::os::unix::fs::PermissionsExt;

use gpupod_agent::{atomic_bytes, atomic_json};
use serde_json::json;

#[test]
fn existing_parent_directory_keeps_its_permissions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shared = dir.path().join("shared");
    std::fs::create_dir(&shared).expect("mkdir");
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    atomic_json(shared.join("health.json"), &json!({"healthy": true})).expect("write");

    let parent_mode = std::fs::metadata(&shared)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(parent_mode, 0o755, "既存ディレクトリの権限を変えない");
    let file_mode = std::fs::metadata(shared.join("health.json"))
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(file_mode, 0o600, "生成するファイルは 0600");
}

#[test]
fn newly_created_parent_directory_is_private() {
    let dir = tempfile::tempdir().expect("tempdir");
    let nested = dir.path().join("private").join("state");
    atomic_bytes(nested.join("state.json"), b"{}", 0o600).expect("write");
    let mode = std::fs::metadata(&nested)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700, "新規ディレクトリは 0700");
}

#[test]
fn writes_are_atomic_and_replace_contents() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("desired-release.json");
    atomic_json(&path, &json!({"sequence": 1})).expect("first");
    atomic_json(&path, &json!({"sequence": 2})).expect("second");
    let text = std::fs::read_to_string(&path).expect("read");
    let value: serde_json::Value = serde_json::from_str(&text).expect("parse");
    assert_eq!(value["sequence"], json!(2));
    // 一時ファイルが残らない。
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .expect("readdir")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with('.'))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporary files left behind: {leftovers:?}"
    );
}
