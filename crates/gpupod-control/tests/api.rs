//! 提供PC向け API（api.rs）の認証と入力上限のテスト。
//!
//! Router を組み立てて tower::ServiceExt::oneshot で流す。外部通信はしない。

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use gpupod_control::db::{fmt_dt, now};
use gpupod_control::oauth::{DiscordApi, DiscordUser};
use gpupod_control::services;
use gpupod_control::{web, App, Config};
use serde_json::json;
use tower::ServiceExt;

struct UnusedDiscord;

impl DiscordApi for UnusedDiscord {
    fn exchange_code(
        &self,
        _client_id: &str,
        _client_secret: &str,
        _code: &str,
        _redirect_uri: &str,
        _code_verifier: &str,
    ) -> Result<String, String> {
        Err("unused".to_string())
    }

    fn fetch_user(&self, _access_token: &str) -> Result<DiscordUser, String> {
        Err("unused".to_string())
    }

    fn list_guilds(&self, _access_token: &str) -> Result<Vec<String>, String> {
        Err("unused".to_string())
    }

    fn guild_member_roles(
        &self,
        _access_token: &str,
        _guild_id: &str,
    ) -> Result<Option<Vec<String>>, String> {
        Err("unused".to_string())
    }
}

fn temp_app() -> (tempfile::TempDir, Arc<App>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::from_env();
    config.state_dir = dir.path().to_path_buf();
    let app = App::with_discord(config, Arc::new(UnusedDiscord)).expect("app");
    (dir, app)
}

fn enroll(app: &App) -> (String, String) {
    let (node, token) = {
        let conn = app.db.lock();
        services::enroll_node(&conn, "provider-a").expect("enroll")
    };
    {
        let conn = app.db.lock();
        conn.execute(
            "UPDATE portal_node SET last_seen = ?2 WHERE id = ?1",
            rusqlite::params![node.id, fmt_dt(now())],
        )
        .expect("last seen");
    }
    (node.uuid_str(), token)
}

fn sync_payload(node_text: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "state": {
            "node_id": node_text,
            "manual_enabled": false,
            "auto_busy": false,
            "accepting": false,
            "gpus": [],
            "leases": {},
            "templates": {},
            "resource_pool": null,
            "direct_host": "",
        },
        "statistics": {},
        "results": [],
    }))
    .expect("json")
}

async fn send(
    app: Arc<App>,
    method: &str,
    uri: &str,
    authorization: Option<&str>,
    body: Vec<u8>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "localhost")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(value) = authorization {
        builder = builder.header(header::AUTHORIZATION, value);
    }
    let request = builder.body(Body::from(body)).expect("request");
    let response = web::router(app).oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.expect("body");
    let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
}

#[tokio::test]
async fn sync_accepts_a_valid_bearer_token() {
    let (_dir, app) = temp_app();
    let (node_text, token) = enroll(&app);
    let uri = format!("/api/nodes/{node_text}/sync/");
    let payload = sync_payload(&node_text);

    // スキーム名は大文字小文字を区別しない。
    let (status, _) = send(
        app.clone(),
        "POST",
        &uri,
        Some(&format!("bearer {token}")),
        payload.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = send(app, "POST", &uri, Some(&format!("Bearer {token}")), payload).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn sync_rejects_missing_or_wrong_credentials() {
    let (_dir, app) = temp_app();
    let (node_text, _token) = enroll(&app);
    let uri = format!("/api/nodes/{node_text}/sync/");
    let payload = sync_payload(&node_text);

    let (status, _) = send(app.clone(), "POST", &uri, None, payload.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(
        app.clone(),
        "POST",
        &uri,
        Some("Bearer wrong-token"),
        payload.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(
        app.clone(),
        "POST",
        &uri,
        Some("Basic dXNlcjpwYXNz"),
        payload.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // ノード ID が UUID でなければ認証前に 403。
    let (status, _) = send(
        app,
        "POST",
        "/api/nodes/not-a-uuid/sync/",
        Some("Bearer wrong-token"),
        payload,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn rotation_rejects_oversized_bodies_before_processing() {
    let (_dir, app) = temp_app();
    let (node_text, token) = enroll(&app);
    let uri = format!("/api/nodes/{node_text}/token/prepare/");
    let oversized = serde_json::to_vec(&json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "token": "N".repeat(40),
        "padding": "x".repeat(9000),
    }))
    .expect("json");
    let (status, _) = send(
        app.clone(),
        "POST",
        &uri,
        Some(&format!("Bearer {token}")),
        oversized,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // 小さな本文なら従来どおり処理される。
    let rotation = uuid::Uuid::new_v4().to_string();
    let fresh = "N".repeat(40);
    let (status, value) = send(
        app,
        "POST",
        &uri,
        Some(&format!("Bearer {token}")),
        serde_json::to_vec(&json!({"id": rotation, "token": fresh})).expect("json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["status"], json!("prepared"));
}

#[tokio::test]
async fn commit_requires_the_pending_credential() {
    let (_dir, app) = temp_app();
    let (node_text, token) = enroll(&app);
    let rotation = uuid::Uuid::new_v4().to_string();
    let fresh = "R".repeat(40);
    let prepare_uri = format!("/api/nodes/{node_text}/token/prepare/");
    let (status, _) = send(
        app.clone(),
        "POST",
        &prepare_uri,
        Some(&format!("Bearer {token}")),
        serde_json::to_vec(&json!({"id": rotation, "token": fresh})).expect("json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let commit_uri = format!("/api/nodes/{node_text}/token/commit/");
    let (status, _) = send(
        app.clone(),
        "POST",
        &commit_uri,
        Some(&format!("Bearer {token}")),
        serde_json::to_vec(&json!({"id": rotation})).expect("json"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, value) = send(
        app,
        "POST",
        &commit_uri,
        Some(&format!("Bearer {fresh}")),
        serde_json::to_vec(&json!({"id": rotation})).expect("json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["status"], json!("complete"));
}

#[tokio::test]
async fn benchmark_requires_node_credentials() {
    let (_dir, app) = temp_app();
    let (node_text, _token) = enroll(&app);
    let uri = format!("/api/nodes/{node_text}/benchmark/?size=1048576");
    let (status, _) = send(app, "GET", &uri, None, Vec::new()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
