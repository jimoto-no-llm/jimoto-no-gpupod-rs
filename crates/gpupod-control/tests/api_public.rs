//! GET /api/public/inventory の公開範囲・有効/無効・レート制限の検証。
//!
//! 本番と同じ web::router（host_guard を含む）を組み立て、
//! tower::ServiceExt::oneshot でリクエストを流す。ネットワークは使わない。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use rusqlite::params;
use serde_json::{json, Value};
use tower::ServiceExt;

use gpupod_control::{db, models, oauth, services, web, App, Config};

/// 公開在庫 API は Discord API を呼ばないので、常に失敗するモックにする。
struct UnusedDiscord;

impl oauth::DiscordApi for UnusedDiscord {
    fn exchange_code(
        &self,
        _client_id: &str,
        _client_secret: &str,
        _code: &str,
        _redirect_uri: &str,
        _code_verifier: &str,
    ) -> Result<String, String> {
        Err("unused".to_owned())
    }

    fn fetch_user(&self, _access_token: &str) -> Result<oauth::DiscordUser, String> {
        Err("unused".to_owned())
    }

    fn list_guilds(&self, _access_token: &str) -> Result<Vec<String>, String> {
        Err("unused".to_owned())
    }

    fn guild_member_roles(
        &self,
        _access_token: &str,
        _guild_id: &str,
    ) -> Result<Option<Vec<String>>, String> {
        Err("unused".to_owned())
    }
}

/// 一時ディレクトリ上のアプリを作る。
fn temp_app() -> (tempfile::TempDir, Arc<App>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::from_env();
    config.state_dir = dir.path().to_path_buf();
    config.insecure_http = true;
    let app = App::with_discord(config, Arc::new(UnusedDiscord)).expect("app");
    (dir, app)
}

/// 提供PCを 1 台登録する。enabled が false のものは在庫に出てはいけない。
fn register_node(
    app: &App,
    name: &str,
    direct_host: &str,
    gpu_names: &[&str],
    enabled: bool,
) -> models::NodeRow {
    let node = {
        let conn = app.db.lock();
        let (node, _token) = services::enroll_node(&conn, name).expect("enroll");
        let gpus: Vec<Value> = gpu_names
            .iter()
            .enumerate()
            .map(|(index, gpu_name)| {
                json!({
                    "id": format!("gpu-{index}"),
                    "vendor": "nvidia",
                    "name": gpu_name,
                    "memory_mib": 24576,
                })
            })
            .collect();
        // スナップショットには公開してはいけない値を入れておく。
        let snapshot = json!({
            "node_id": node.uuid_str(),
            "accepting": true,
            "manual_enabled": true,
            "gpus": gpus,
            "leases": {},
            "templates": {},
            "direct_host": direct_host,
        });
        conn.execute(
            "UPDATE portal_node SET last_seen = ?2, enabled = ?4, desired_enabled = ?4, snapshot = ?3 WHERE id = ?1",
            params![node.id, db::fmt_dt(db::now()), snapshot.to_string(), enabled as i64],
        )
        .expect("update node");
        node
    };
    // ノード ID がレスポンスに出てはいけない。呼び出し側で確認するために返す。
    node
}

/// GET /api/public/inventory を流し、(status, headers, JSON) を返す。
async fn fetch_inventory(
    app: &Arc<App>,
    forwarded_for: Option<&str>,
) -> (StatusCode, HeaderMap, Value) {
    let mut builder = Request::builder()
        .method("GET")
        .uri("/api/public/inventory")
        .header(header::HOST, "127.0.0.1");
    if let Some(forwarded_for) = forwarded_for {
        builder = builder.header("x-forwarded-for", forwarded_for);
    }
    let request = builder.body(Body::empty()).expect("request");
    let response = web::router(app.clone())
        .oneshot(request)
        .await
        .expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    (
        status,
        headers,
        serde_json::from_slice(&bytes).expect("json body"),
    )
}

/// レスポンスの JSON（値とキーの両方）を文字列へ展開する。
fn as_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().map(as_text).collect::<Vec<_>>().join(" "),
        Value::Object(map) => map
            .iter()
            .map(|(key, item)| format!("{key} {}", as_text(item)))
            .collect::<Vec<_>>()
            .join(" "),
        other => other.to_string(),
    }
}

#[tokio::test]
async fn returns_only_public_fields() {
    let (_dir, app) = temp_app();
    let node = register_node(&app, "gpu-01", "10.0.0.5", &["RTX 4090", "RTX 4080"], true);
    let (status, headers, body) = fetch_inventory(&app, Some("203.0.113.10")).await;
    assert_eq!(status, StatusCode::OK);
    // 5 秒キャッシュ・共有可能（CDN 前提）。
    assert_eq!(
        headers
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("public, max-age=5")
    );
    // 集計。
    assert_eq!(body["providers"], json!(1));
    assert_eq!(body["offered_gpus"], json!(2));
    assert_eq!(body["available_gpus"], json!(2));
    let gpus = body["gpus"].as_array().expect("gpus array");
    assert_eq!(gpus.len(), 2);
    for gpu in gpus {
        let keys: Vec<&str> = gpu
            .as_object()
            .expect("gpu object")
            .keys()
            .map(|key| key.as_str())
            .collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            vec!["available", "name", "provider", "status", "vendor", "vram"]
        );
        assert_eq!(gpu["vendor"], json!("NVIDIA"));
        assert_eq!(gpu["vram"], json!("24 GiB"));
        assert_eq!(gpu["status"], json!("貸出可能"));
        assert_eq!(gpu["available"], json!(true));
        assert_eq!(gpu["provider"], json!("gpu-01"));
    }
    assert_eq!(gpus[0]["name"], json!("RTX 4080"));
    assert_eq!(gpus[1]["name"], json!("RTX 4090"));
    // 漏えい検査: IP・ノード ID・内部 GPU ID・内部キー・トークン。
    let text = as_text(&body);
    assert!(
        !text.contains("10.0.0.5"),
        "IP アドレスが公開されている: {text}"
    );
    assert!(
        !text.contains("direct_host"),
        "direct_host が公開されている"
    );
    assert!(!text.contains("node_id"), "node_id が公開されている");
    assert!(
        !text.contains("manual_enabled"),
        "snapshot 内部キーが公開されている"
    );
    assert!(!text.contains("gpu-0\""), "内部 GPU ID が公開されている");
    assert!(!text.contains("token"), "トークンが含まれている");
    let node_id = node.uuid_str();
    assert!(
        !text.contains(&node_id),
        "ノード ID {node_id} が公開されている"
    );
}

#[tokio::test]
async fn empty_inventory_returns_zeroes() {
    let (_dir, app) = temp_app();
    let (status, _headers, body) = fetch_inventory(&app, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["providers"], json!(0));
    assert_eq!(body["offered_gpus"], json!(0));
    assert_eq!(body["available_gpus"], json!(0));
    assert_eq!(body["gpus"], json!([]));
}

#[tokio::test]
async fn disabled_nodes_are_not_exposed() {
    let (_dir, app) = temp_app();
    register_node(&app, "gpu-01", "10.0.0.5", &["RTX 4090"], true);
    register_node(&app, "gpu-off", "172.16.9.9", &["Quadro P1000"], false);
    let (status, _headers, body) = fetch_inventory(&app, Some("203.0.113.11")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["providers"], json!(1));
    assert_eq!(body["offered_gpus"], json!(1));
    assert_eq!(body["available_gpus"], json!(1));
    let gpus = body["gpus"].as_array().expect("gpus array");
    assert_eq!(gpus.len(), 1);
    assert_eq!(gpus[0]["name"], json!("RTX 4090"));
    assert_eq!(gpus[0]["provider"], json!("gpu-01"));
    let text = as_text(&body);
    assert!(
        !text.contains("gpu-off"),
        "enabled=false の提供PC名が公開されている"
    );
    assert!(
        !text.contains("Quadro P1000"),
        "enabled=false のGPUが公開されている"
    );
    assert!(
        !text.contains("172.16.9.9"),
        "enabled=false のIP アドレスが公開されている"
    );
}

#[tokio::test]
async fn rate_limit_is_applied_per_ip() {
    let (_dir, app) = temp_app();
    register_node(&app, "gpu-01", "10.0.0.5", &["RTX 4090"], true);
    // 10 リクエスト/IP/秒。窓内なら 10 件まで 200、11 件目は 429。
    for _ in 0..10 {
        let (status, _headers, _body) = fetch_inventory(&app, Some("203.0.113.20")).await;
        assert_eq!(status, StatusCode::OK, "上限内のリクエストが拒否された");
    }
    let (status, headers, body) = fetch_inventory(&app, Some("203.0.113.20")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        headers
            .get(header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok()),
        Some("1"),
        "Retry-After がない"
    );
    assert!(
        body["error"].as_str().is_some(),
        "エラー応答が JSON ではない"
    );
    // 別 IP は別カウントなので 200。
    let (status, _headers, _body) = fetch_inventory(&app, Some("203.0.113.21")).await;
    assert_eq!(status, StatusCode::OK);
}
