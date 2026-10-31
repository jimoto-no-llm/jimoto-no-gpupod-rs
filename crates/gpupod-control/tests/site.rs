//! 公開サイト（site.rs / templates/site/）のテスト。
//!
//! 実際に Router を組み立て、tower::ServiceExt::oneshot でリクエストを流す。
//! ネットワークも Discord も使わない。

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::routing::get;
use axum::Router;
use gpupod_control::db::{fmt_dt, now};
use gpupod_control::models;
use gpupod_control::oauth::{DiscordApi, DiscordUser};
use gpupod_control::services;
use gpupod_control::site;
use gpupod_control::{auth, App, Config};
use rusqlite::params;
use serde_json::json;
use tower::ServiceExt;

/// 公開サイトのテストでは Discord API を呼ばないので、常に失敗するモックにする。
///
/// （HttpDiscord は reqwest の blocking クライアントを持ち、非同期テストの中で
///   ドロップすると runtime の破棄で panic するため使わない。）
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

/// 一時ディレクトリ上のアプリを作る。
fn temp_app() -> (tempfile::TempDir, Arc<App>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::from_env();
    config.state_dir = dir.path().to_path_buf();
    let app = App::with_discord(config, Arc::new(UnusedDiscord)).expect("app");
    (dir, app)
}

/// 本番の統合と同じ形（web.rs の / を site::home に差し替えた状態）のルーター。
fn test_router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(site::home))
        .with_state(app.clone())
        .merge(site::router(app))
}

/// 利用者を作ってログイン済みの Cookie を返す。
fn login_cookie(app: &App, username: &str) -> String {
    let user_id = {
        let conn = app.db.lock();
        models::insert_user(
            &conn,
            username,
            &auth::hash_password("pw"),
            false,
            false,
            true,
            "",
        )
        .expect("user")
    };
    let session = auth::login_user(&app.db, user_id);
    format!("sessionid={}", session.key)
}

/// 受付中の提供 PC（GPU 2 枚）を登録する。direct_host には公開してはいけない値を入れる。
fn accepting_node(app: &App) {
    let node = {
        let conn = app.db.lock();
        let (node, _token) = services::enroll_node(&conn, "gpu-01").expect("enroll");
        node
    };
    let snapshot = json!({
        "node_id": node.uuid_str(),
        "accepting": true,
        "manual_enabled": true,
        "gpus": [
            {"id": "gpu-0", "vendor": "nvidia", "name": "RTX 4090", "memory_mib": 24576},
            {"id": "gpu-1", "vendor": "amd", "name": "Radeon RX 7900 XTX", "memory_mib": 24576},
        ],
        "leases": {},
        "templates": {},
        "direct_host": "10.0.0.5",
    });
    let conn = app.db.lock();
    conn.execute(
        "UPDATE portal_node SET last_seen = ?2, desired_enabled = 1, snapshot = ?3 WHERE id = ?1",
        params![node.id, fmt_dt(now()), snapshot.to_string()],
    )
    .expect("update node");
}

/// GET リクエストを流し、(status, headers, body) を返す。
async fn fetch(
    router: &Router,
    uri: &str,
    cookie: Option<&str>,
) -> (StatusCode, HeaderMap, String) {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    let request = builder.body(Body::empty()).expect("request");
    let response = router.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, headers, String::from_utf8_lossy(&bytes).to_string())
}

#[tokio::test]
async fn anonymous_home_serves_landing_page() {
    let (_dir, app) = temp_app();
    let router = test_router(app);
    let (status, headers, body) = fetch(&router, "/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/html; charset=utf-8")
    );
    assert!(
        body.contains("地域のGPUを、") && body.contains("時間で貸し出します"),
        "ヒーローの見出しが出ていない"
    );
    assert!(body.contains("いまの在庫"), "在庫ボードが出ていない");
    assert!(
        body.contains("Discord サーバーへの参加が必要です"),
        "Discord 参加の案内が出ていない"
    );
    assert!(
        body.contains("いま貸し出せるGPUはありません"),
        "在庫 0 件の表示が出ていない"
    );
}

#[tokio::test]
async fn signed_in_home_serves_existing_dashboard() {
    let (_dir, app) = temp_app();
    let cookie = login_cookie(&app, "tester");
    let router = test_router(app);
    let (status, _headers, body) = fetch(&router, "/", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("GPUを借りる"),
        "ログイン済みでダッシュボードが出ていない"
    );
    assert!(
        !body.contains("時間で貸し出します"),
        "ログイン済みなのに LP が出ている"
    );
}

#[tokio::test]
async fn public_pages_return_ok() {
    let (_dir, app) = temp_app();
    let router = test_router(app);
    let pages = [
        ("/pricing/", "料金"),
        ("/providers/", "提供者募集"),
        ("/terms/", "利用規約"),
        ("/privacy/", "プライバシーポリシー"),
        ("/how/", "使い方"),
    ];
    for (uri, expected) in pages {
        let (status, headers, body) = fetch(&router, uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uri} が 200 ではない");
        assert!(
            body.contains(expected),
            "{uri} に {expected} が含まれていない"
        );
        // 公開ページにも安全ヘッダーを付ける（site::router のレイヤー）。
        assert_eq!(
            headers
                .get(header::X_CONTENT_TYPE_OPTIONS)
                .and_then(|value| value.to_str().ok()),
            Some("nosniff"),
            "{uri} に X-Content-Type-Options がない"
        );
        assert_eq!(
            headers
                .get(header::X_FRAME_OPTIONS)
                .and_then(|value| value.to_str().ok()),
            Some("DENY"),
            "{uri} に X-Frame-Options がない"
        );
    }
}

#[tokio::test]
async fn missing_trailing_slash_redirects() {
    let (_dir, app) = temp_app();
    let router = test_router(app);
    let (status, headers, _body) = fetch(&router, "/pricing", None).await;
    assert_eq!(status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(
        headers
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok()),
        Some("/pricing/")
    );
}

#[tokio::test]
async fn healthz_returns_json() {
    let (_dir, app) = temp_app();
    let router = test_router(app);
    let (status, headers, body) = fetch(&router, "/healthz", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json; charset=utf-8")
    );
    assert_eq!(body, r#"{"status":"ok"}"#);
}

#[tokio::test]
async fn static_css_is_served() {
    let (_dir, app) = temp_app();
    let router = test_router(app);
    let (status, headers, body) = fetch(&router, "/static/site.css", None).await;
    assert_eq!(status, StatusCode::OK);
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert!(
        content_type.starts_with("text/css"),
        "unexpected: {content_type}"
    );
    assert!(body.contains("--accent"));
}

#[tokio::test]
async fn inventory_with_no_nodes_does_not_panic() {
    let (_dir, app) = temp_app();
    let router = test_router(app);
    let (status, _headers, body) = fetch(&router, "/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("いま貸し出せるGPUはありません"));
    // ステータス行は 0 件でも自然に表示される。
    assert!(body.contains("提供PC"));
    assert!(body.contains("料金 <b>無料</b>"));
}

#[tokio::test]
async fn inventory_shows_public_fields_only() {
    let (_dir, app) = temp_app();
    accepting_node(&app);
    let router = test_router(app);
    let (status, _headers, body) = fetch(&router, "/", None).await;
    assert_eq!(status, StatusCode::OK);
    // GPU 名・ベンダー・VRAM・提供者名は公開する。
    assert!(body.contains("RTX 4090"));
    assert!(body.contains("Radeon RX 7900 XTX"));
    assert!(body.contains("24 GiB"));
    assert!(body.contains("gpu-01"));
    assert!(body.contains("貸出可能"));
    // IP アドレスや内部 ID は公開しない。
    assert!(
        !body.contains("10.0.0.5"),
        "提供PCの IP アドレスが公開されている"
    );
    assert!(!body.contains("gpu-0\""), "内部 GPU ID が公開されている");
}
