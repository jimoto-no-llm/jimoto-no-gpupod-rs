//! GET /api/public/inventory の公開範囲とレート制限の検証。

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use gpupod_control::{web, App, Config};

struct UnusedDiscord;

impl gpupod_control::oauth::DiscordApi for UnusedDiscord {
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

    fn fetch_user(
        &self,
        _access_token: &str,
    ) -> Result<gpupod_control::oauth::DiscordUser, String> {
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

fn temp_app() -> (tempfile::TempDir, Arc<App>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::from_env();
    config.state_dir = dir.path().to_path_buf();
    config.allowed_hosts = vec!["127.0.0.1".into()];
    config.insecure_http = true;
    let app = App::with_discord(config, Arc::new(UnusedDiscord)).expect("app");
    (dir, app)
}

#[tokio::test]
async fn public_inventory_is_open_and_clean() {
    let (_dir, app) = temp_app();
    let router = web::router(app);

    let request = Request::builder()
        .method("GET")
        .uri("/api/public/inventory")
        .header("host", "127.0.0.1")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8(body_bytes.to_vec()).unwrap();
    assert!(body.contains("\"providers\""));
    assert!(body.contains("\"gpus\""));
    // 秘密は絶対に含めない
    assert!(!body.contains("token"));
    assert!(!body.contains("secret"));
    assert!(!body.contains("192.168."));
}

#[tokio::test]
async fn rate_limit_kicks_in() {
    let (_dir, app) = temp_app();
    let mut saw_limited = false;
    for _ in 0..25 {
        let router = web::router(app.clone());
        let request = Request::builder()
            .method("GET")
            .uri("/api/public/inventory")
            .header("host", "127.0.0.1")
            .header("x-forwarded-for", "10.9.9.9")
            .body(Body::empty())
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            saw_limited = true;
            break;
        }
    }
    assert!(saw_limited, "429 が返っていない");
}
