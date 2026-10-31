//! 提供PC設定（daemon.load_config）の検証と秘密の取り扱い。

use std::collections::BTreeMap;
use std::path::Path;

use gpupod_agent::daemon::{load_config, Forwarder, ProviderConfig};
use gpupod_common::protocol::RelayRoute;
use serde_json::{json, Value};

fn relay_public_key() -> String {
    let key =
        russh::keys::PrivateKey::random(&mut rand::rngs::OsRng, russh::keys::Algorithm::Ed25519)
            .expect("key");
    key.public_key().to_openssh().expect("openssh")
}

fn valid_config() -> Value {
    json!({
        "url": "https://central.example.org",
        "node_id": "node-abc",
        "token": "super-secret-token-value",
        "relay_host": "relay.example.org",
        "relay_host_key": relay_public_key(),
        "relay_port": 19000,
    })
}

fn write_config(directory: &Path, value: &Value) -> std::path::PathBuf {
    let path = directory.join("node.secret.json");
    std::fs::write(&path, serde_json::to_vec_pretty(value).expect("json")).expect("write");
    path
}

#[test]
fn accepts_a_minimal_https_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), &valid_config());
    let config = load_config(&path).expect("valid");
    assert_eq!(config.url, "https://central.example.org");
    assert_eq!(config.relay_port, 19000);
}

#[test]
fn rejects_insecure_urls_and_bad_values() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cases = [
        ("url", json!("http://central.example.org")),
        ("url", json!("https://central.example.org/?x=1")),
        ("url", json!("https://user:pass@central.example.org")),
        ("url", json!("ftp://central.example.org")),
        ("relay_port", json!(0)),
        ("relay_port", json!(70000)),
        ("relay_host_key", json!("not-a-key")),
        ("relay_host", json!("bad host")),
        ("node_id", json!("")),
        ("token", json!("")),
        ("watchdog_timeout", json!(10)),
        ("direct_host", json!("direct.example.org")),
        ("limits", json!({"unknown": 1})),
    ];
    for (key, value) in cases {
        let mut config = valid_config();
        config[key] = value;
        let path = write_config(dir.path(), &config);
        assert!(
            load_config(&path).is_err(),
            "accepted invalid value for {key}: {config:?}"
        );
    }
}

#[test]
fn accepts_loopback_http_for_demos() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = valid_config();
    config["url"] = json!("http://127.0.0.1:8000");
    let path = write_config(dir.path(), &config);
    load_config(&path).expect("loopback http is allowed");
}

#[cfg(unix)]
#[test]
fn tightens_permissions_of_a_world_readable_config() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), &valid_config());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    load_config(&path).expect("valid");
    let mode = std::fs::metadata(&path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o600,
        "provider config must not be readable by others"
    );
}

#[test]
fn debug_never_prints_the_token() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), &valid_config());
    let config = load_config(&path).expect("valid");
    let text = format!("{config:?}");
    assert!(
        !text.contains("super-secret-token-value"),
        "token leaked: {text}"
    );
    assert!(text.contains("<redacted>"));
}

fn forwarder_config() -> ProviderConfig {
    ProviderConfig {
        url: "http://127.0.0.1:1".to_string(),
        node_id: "node-abc".to_string(),
        token: "token".to_string(),
        relay_host: "127.0.0.1".to_string(),
        relay_host_key: relay_public_key(),
        relay_port: 1,
        direct_host: None,
        direct_bind: None,
        limits: BTreeMap::new(),
        min_disk_free_bytes: 0,
        runtime: None,
        watchdog_timeout: 180.0,
        disk_path: None,
        extra: BTreeMap::new(),
    }
}

fn running_state() -> Value {
    json!({
        "leases": {
            "lease1": {
                "status": "running",
                "connection_mode": "relay",
                "ports": {"22/tcp": [{"HostIp": "127.0.0.1", "HostPort": "5555"}]}
            }
        }
    })
}

#[tokio::test]
async fn privileged_route_ports_are_ignored() {
    let token = std::sync::Arc::new(std::sync::RwLock::new("token".to_string()));
    let mut forwarder = Forwarder::new(forwarder_config(), token).expect("forwarder");
    let routes = vec![RelayRoute {
        lease_id: "lease1".to_string(),
        container_port: "22/tcp".to_string(),
        relay_port: 22,
    }];
    // 特権ポートは要求しないので、中継へ接続せず正常終了する。
    forwarder
        .update(&routes, &running_state(), "0.0.0.0")
        .await
        .expect("privileged route is filtered out");
}

#[tokio::test]
async fn allowed_route_ports_are_attempted() {
    let token = std::sync::Arc::new(std::sync::RwLock::new("token".to_string()));
    let mut forwarder = Forwarder::new(forwarder_config(), token).expect("forwarder");
    let routes = vec![RelayRoute {
        lease_id: "lease1".to_string(),
        container_port: "22/tcp".to_string(),
        relay_port: 19001,
    }];
    // 中継（127.0.0.1:1）には繋がらないので、経路は接続を試みて失敗する。
    assert!(forwarder
        .update(&routes, &running_state(), "0.0.0.0")
        .await
        .is_err());
}
