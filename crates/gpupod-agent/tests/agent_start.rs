//! Agent.start の検証エラー群（モックエンジン、Docker 不要）。

mod common;
use common::MockEngine;

use std::sync::Arc;

use gpupod_agent::{mock_gpus, Agent, StartRequest, StateStore};
use gpupod_common::{AgentError, Template};
use serde_json::json;

fn templates() -> std::collections::BTreeMap<String, Template> {
    let template = Template::from_value(json!({
        "id": "ubuntu-24.04",
        "image": "gpupod/ubuntu:24.04",
        "vendors": ["nvidia", "amd", "intel", "other"],
        "ports": ["22/tcp"]
    }))
    .expect("template");
    let mut map = std::collections::BTreeMap::new();
    map.insert(template.id.clone(), template);
    map
}

fn nvidia_only_templates() -> std::collections::BTreeMap<String, Template> {
    let template = Template::from_value(json!({
        "id": "nvidia-only",
        "image": "gpupod/ubuntu:24.04",
        "vendors": ["nvidia"],
        "ports": ["22/tcp"]
    }))
    .expect("template");
    let mut map = std::collections::BTreeMap::new();
    map.insert(template.id.clone(), template);
    map
}

fn agent_in(directory: &std::path::Path, templates: std::collections::BTreeMap<String, Template>) -> Agent {
    agent_with_gpus(directory, templates, mock_gpus())
}

fn agent_with_gpus(
    directory: &std::path::Path,
    templates: std::collections::BTreeMap<String, Template>,
    gpus: Vec<gpupod_common::GPU>,
) -> Agent {
    let store = Arc::new(StateStore::new(directory).expect("store"));
    let engine = Arc::new(MockEngine::default());
    Agent::new(store, engine, templates, gpus)
}

/// 同一ベンダー 2 枚を含む在庫（GPU グループの検証用）。
fn grouped_gpus() -> Vec<gpupod_common::GPU> {
    let mut gpus = mock_gpus();
    gpus.push(gpupod_common::GPU {
        id: "mock-nvidia-2".into(),
        vendor: "nvidia".into(),
        name: "MOCK NVIDIA GPU 2".into(),
        backend: "nvidia".into(),
        selector: "GPU-MOCK-2".into(),
        devices: Vec::new(),
        memory_mib: None,
    });
    gpus
}

fn error_of(result: Result<serde_json::Value, AgentError>) -> String {
    match result {
        Ok(_) => panic!("expected an error"),
        Err(exc) => exc.0,
    }
}

#[test]
fn rejects_unknown_template() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let agent = agent_in(temporary.path(), templates());
    let error = error_of(agent.start(StartRequest::new("missing-template", "mock-nvidia")));
    assert_eq!(error, "Only registered template IDs can be started");
}

#[test]
fn rejects_unknown_gpu() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let agent = agent_in(temporary.path(), templates());
    let error = error_of(agent.start(StartRequest::new("ubuntu-24.04", "mock-missing")));
    assert_eq!(error, "GPU is not registered on this node");
}

#[test]
fn rejects_vendor_mismatch() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let agent = agent_in(temporary.path(), nvidia_only_templates());
    agent.on().expect("on");
    let error = error_of(agent.start(StartRequest::new("nvidia-only", "mock-amd")));
    assert_eq!(error, "Template does not declare support for this GPU vendor");
}

#[test]
fn rejects_when_not_accepting() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let agent = agent_in(temporary.path(), templates());
    let error = error_of(agent.start(StartRequest::new("ubuntu-24.04", "mock-nvidia")));
    assert_eq!(error, "Node is not accepting leases");

    agent.on().expect("on");
    agent.set_auto_busy(true, "drain").expect("busy");
    let error = error_of(agent.start(StartRequest::new("ubuntu-24.04", "mock-nvidia")));
    assert_eq!(error, "Node is not accepting leases");
}

#[test]
fn rejects_double_allocation() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let agent = agent_in(temporary.path(), templates());
    agent.on().expect("on");
    let lease = agent
        .start(StartRequest::new("ubuntu-24.04", "mock-nvidia").lease_id("lease-one"))
        .expect("first start");
    assert_eq!(lease["status"], "running");
    assert_eq!(lease["container"], "gpulend-lease-one");

    let error = error_of(agent.start(StartRequest::new("ubuntu-24.04", "mock-nvidia").lease_id("lease-two")));
    assert_eq!(error, "GPU is already allocated or awaiting cleanup");

    // 返却してからなら再び借りられる。
    agent.stop("lease-one").expect("stop");
    let second = agent
        .start(StartRequest::new("ubuntu-24.04", "mock-nvidia").lease_id("lease-three"))
        .expect("second start");
    assert_eq!(second["status"], "running");
}

#[test]
fn rejects_diagnostic_group_rules() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let agent = agent_with_gpus(temporary.path(), templates(), grouped_gpus());

    // 受付中のノードでは診断できない。
    agent.on().expect("on");
    let error = error_of(
        agent.start(
            StartRequest::new("ubuntu-24.04", "mock-nvidia")
                .gpu_ids(Some(vec!["mock-nvidia".into(), "mock-nvidia-2".into()]))
                .diagnostic(true),
        ),
    );
    assert_eq!(error, "Turn lending OFF before running diagnostics");

    // 診断以外の GPU グループは予約できない。
    let error = error_of(
        agent.start(
            StartRequest::new("ubuntu-24.04", "mock-nvidia")
                .gpu_ids(Some(vec!["mock-nvidia".into(), "mock-nvidia-2".into()])),
        ),
    );
    assert_eq!(error, "GPU groups are reserved for the local diagnostic helper");

    // 主 GPU 以外から始められない。
    let error = error_of(
        agent.start(
            StartRequest::new("ubuntu-24.04", "mock-amd")
                .gpu_ids(Some(vec!["mock-nvidia".into()])),
        ),
    );
    assert_eq!(error, "Invalid diagnostic GPU group");
}

#[test]
fn rejects_unknown_connection_mode_and_lease_mismatch() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let agent = agent_in(temporary.path(), templates());
    let error = error_of(
        agent.start(StartRequest::new("ubuntu-24.04", "mock-nvidia").connection_mode("wan")),
    );
    assert_eq!(error, "Invalid connection mode");

    agent.on().expect("on");
    agent
        .start(StartRequest::new("ubuntu-24.04", "mock-nvidia").lease_id("lease-a"))
        .expect("start");
    // 同じ貸出 ID で別 GPU を要求しても返答は同じ貸出。
    let repeat = agent
        .start(StartRequest::new("ubuntu-24.04", "mock-nvidia").lease_id("lease-a"))
        .expect("repeat");
    assert_eq!(repeat["id"], "lease-a");

    let error = error_of(
        agent.start(StartRequest::new("ubuntu-24.04", "mock-amd").lease_id("lease-a")),
    );
    assert_eq!(error, "Lease ID belongs to a different request");
}

#[test]
fn failed_engine_run_wraps_the_message() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(StateStore::new(temporary.path()).expect("store"));
    let engine = Arc::new(MockEngine::failing("boom", false));
    let agent = Agent::new(store, engine, templates(), mock_gpus());
    agent.on().expect("on");
    let error = error_of(
        agent.start(StartRequest::new("ubuntu-24.04", "mock-nvidia").lease_id("lease-fail")),
    );
    assert_eq!(error, "Start failed for lease lease-fail: boom");

    // 確実な失敗では予約もコンテナも片付く。
    let status = agent.status().expect("status");
    let lease = &status["leases"]["lease-fail"];
    assert_eq!(lease["status"], "stopped");
    assert_eq!(lease["error"], "boom");
}

#[test]
fn uncertain_engine_failure_retains_the_reservation() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(StateStore::new(temporary.path()).expect("store"));
    let engine = Arc::new(MockEngine::failing(
        "Docker timed out; reservation retained until reconciliation",
        true,
    ));
    let agent = Agent::new(store, engine, templates(), mock_gpus());
    agent.on().expect("on");
    let error = error_of(
        agent.start(StartRequest::new("ubuntu-24.04", "mock-nvidia").lease_id("lease-uncertain")),
    );
    assert_eq!(
        error,
        "Start failed for lease lease-uncertain: Docker timed out; reservation retained until reconciliation"
    );
    let status = agent.status().expect("status");
    assert_eq!(status["leases"]["lease-uncertain"]["status"], "cleanup_pending");
}

#[test]
fn validate_ssh_key_requirements() {
    let error = match gpupod_agent::validate_ssh_key("ssh-ed25519 AAAA extra\nmore") {
        Ok(_) => panic!("multiline is rejected"),
        Err(exc) => exc,
    };
    assert_eq!(error.0, "A single SSH public key is required");

    let long = "x".repeat(4097);
    let error = match gpupod_agent::validate_ssh_key(&long) {
        Ok(_) => panic!("oversized key is rejected"),
        Err(exc) => exc,
    };
    assert_eq!(error.0, "A single SSH public key is required");

    let error = match gpupod_agent::validate_ssh_key("not-a-key") {
        Ok(_) => panic!("garbage is rejected"),
        Err(exc) => exc,
    };
    assert_eq!(error.0, "Invalid SSH public key");
}
