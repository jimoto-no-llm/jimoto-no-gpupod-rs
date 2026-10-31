//! Web 層（auth / web / pages / oauth / site）のセキュリティ回帰テスト。
//!
//! ネットワークも Discord も使わない。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::{Form, Query, State};
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::response::Response;
use gpupod_control::admin;
use gpupod_control::auth;
use gpupod_control::models;
use gpupod_control::oauth::{DiscordApi, DiscordUser};
use gpupod_control::{pages, web, App, Config};
use tower::ServiceExt;

/// Discord API は呼ばないので、常に失敗するモックにする。
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

/// 一時ディレクトリ上のアプリを作る（Host 検証の既定値を固定する）。
fn temp_app() -> (tempfile::TempDir, Arc<App>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::from_env();
    config.state_dir = dir.path().to_path_buf();
    config.allowed_hosts = vec!["localhost".to_string()];
    config.csrf_origins = Vec::new();
    config.trust_proxy = false;
    config.insecure_http = true;
    let app = App::with_discord(config, Arc::new(UnusedDiscord)).expect("app");
    (dir, app)
}

/// Cookie つきヘッダー。
fn cookie_headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let cookie = pairs
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ");
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, "localhost".parse().unwrap());
    headers.insert(header::COOKIE, cookie.parse().unwrap());
    headers
}

fn anon_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, "localhost".parse().unwrap());
    headers
}

async fn body_text(response: Response) -> String {
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    String::from_utf8_lossy(&bytes).to_string()
}

fn set_cookies(response: &Response) -> Vec<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok().map(str::to_string))
        .collect()
}

fn cookie_value_from(response: &Response, name: &str) -> Option<String> {
    set_cookies(response).into_iter().find_map(|cookie| {
        let first = cookie.split(';').next()?.trim();
        let (key, value) = first.split_once('=')?;
        if key == name {
            Some(value.to_string())
        } else {
            None
        }
    })
}

/// ログイン POST を組み立てる（CSRF は Cookie とフォームで一致させる）。
async fn login_post(
    app: &Arc<App>,
    headers: &HeaderMap,
    token: &str,
    username: &str,
    password: &str,
    next: &str,
) -> Response {
    let mut form = HashMap::new();
    form.insert("csrfmiddlewaretoken".to_string(), token.to_string());
    form.insert("username".to_string(), username.to_string());
    form.insert("password".to_string(), password.to_string());
    form.insert("next".to_string(), next.to_string());
    pages::login_post(State(app.clone()), headers.clone(), Form(form)).await
}

#[tokio::test]
async fn error_page_escapes_untrusted_text() {
    let response = web::error_page(
        StatusCode::INTERNAL_SERVER_ERROR,
        "題名 <script>alert(1)</script>",
        "<img src=x onerror=alert(2)>",
    );
    let body = body_text(response).await;
    assert!(
        !body.contains("<script>"),
        "title がエスケープされていない: {body}"
    );
    assert!(
        !body.contains("<img src=x"),
        "message がエスケープされていない: {body}"
    );
    assert!(body.contains("&lt;script&gt;"));
}

#[test]
fn csrf_input_escapes_token() {
    let html = web::csrf_input("\"><script>alert(1)</script>");
    assert!(
        !html.contains("<script>"),
        "トークンが素のまま埋め込まれている: {html}"
    );
    assert!(html.contains("&quot;&gt;&lt;script&gt;"));
}

#[tokio::test]
async fn login_page_sets_cookie_matching_form_token() {
    let (_dir, app) = temp_app();
    let response =
        pages::login_get(State(app.clone()), anon_headers(), Query(HashMap::new())).await;
    assert_eq!(response.status(), StatusCode::OK);
    let token = cookie_value_from(&response, "csrftoken").expect("csrftoken Cookie");
    let body = body_text(response).await;
    assert!(
        body.contains(&format!("name=\"csrfmiddlewaretoken\" value=\"{token}\"")),
        "フォームのトークンと Cookie が一致していない（初回ログインが CSRF で失敗する）"
    );
}

#[test]
fn safe_next_allows_only_site_paths() {
    assert_eq!(pages::safe_next("/account/").as_deref(), Some("/account/"));
    assert_eq!(pages::safe_next("/").as_deref(), Some("/"));
    // プロトコル相対・バックスラッシュ・絶対 URL・制御文字は拒否する。
    for bad in [
        "//evil.example",
        "/\\evil.example",
        "https://evil.example/",
        "/a\nb",
        "",
    ] {
        assert!(
            pages::safe_next(bad).is_none(),
            "危険な next を通している: {bad:?}"
        );
    }
}

#[tokio::test]
async fn login_page_uses_session_csrf_when_signed_in() {
    let (_dir, app) = temp_app();
    let user_id = {
        let conn = app.db.lock();
        models::insert_user(
            &conn,
            "signed-in",
            &auth::unusable_password(),
            false,
            false,
            true,
            "",
        )
        .expect("user")
    };
    let session = auth::create_session(&app.db, user_id);
    // csrftoken を落とした状態でも、ログイン画面はセッションのトークンを配る。
    let headers = cookie_headers(&[("sessionid", &session.key)]);
    let response = pages::login_get(State(app.clone()), headers, Query(HashMap::new())).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        cookie_value_from(&response, "csrftoken").as_deref(),
        Some(session.data.csrf.as_str()),
        "セッションの CSRF トークンを Cookie に設定する"
    );
    let body = body_text(response).await;
    assert!(body.contains(&session.data.csrf));
}

#[tokio::test]
async fn login_rejects_external_next_redirect() {
    let (_dir, app) = temp_app();
    let user_id = {
        let conn = app.db.lock();
        models::insert_user(
            &conn,
            "redirect-user",
            &auth::hash_password("pw"),
            false,
            false,
            true,
            "",
        )
        .expect("user")
    };
    assert!(user_id > 0);
    let token = "0123456789abcdef0123456789abcdef";
    let headers = cookie_headers(&[("csrftoken", token)]);

    let response = login_post(
        &app,
        &headers,
        token,
        "redirect-user",
        "pw",
        "//evil.example",
    )
    .await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok()),
        Some("/"),
        "外部へのリダイレクトを拒否する"
    );
}

#[test]
fn csrf_accepts_same_origin_referer_and_rejects_cross_origin() {
    let mut config = Config::from_env();
    config.csrf_origins = Vec::new();
    config.trust_proxy = false;
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, "gpu.example.org".parse().unwrap());
    headers.insert(header::COOKIE, "csrftoken=abcdef".parse().unwrap());

    // Origin が同一なら通る。
    headers.insert(header::ORIGIN, "https://gpu.example.org".parse().unwrap());
    assert!(auth::verify_csrf(&headers, Some("abcdef"), &config).is_ok());

    // Referer は URL 全体で届く（パス付きでも同一オリジンとして通す）。
    headers.remove(header::ORIGIN);
    headers.insert(
        header::REFERER,
        "https://gpu.example.org/login/".parse().unwrap(),
    );
    assert!(
        auth::verify_csrf(&headers, Some("abcdef"), &config).is_ok(),
        "同一オリジンの Referer を拒否している"
    );

    // 別オリジンは拒否する。
    headers.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
    assert!(auth::verify_csrf(&headers, Some("abcdef"), &config).is_err());

    // 不透明な Origin も拒否する。
    headers.insert(header::ORIGIN, "null".parse().unwrap());
    assert!(auth::verify_csrf(&headers, Some("abcdef"), &config).is_err());

    // トークン不一致は拒否する。
    headers.insert(header::ORIGIN, "https://gpu.example.org".parse().unwrap());
    assert!(auth::verify_csrf(&headers, Some("wrong"), &config).is_err());
    assert!(auth::verify_csrf(&headers, None, &config).is_err());
}

#[tokio::test]
async fn authenticated_post_needs_session_csrf_token() {
    let (_dir, app) = temp_app();
    let user_id = {
        let conn = app.db.lock();
        models::insert_user(
            &conn,
            "csrf-user",
            &auth::unusable_password(),
            false,
            false,
            true,
            "",
        )
        .expect("user")
    };
    let session = auth::create_session(&app.db, user_id);
    let session_headers = cookie_headers(&[
        ("sessionid", &session.key),
        ("csrftoken", &session.data.csrf),
    ]);

    let mut form = HashMap::new();
    form.insert("csrfmiddlewaretoken".to_string(), session.data.csrf.clone());
    assert!(web::check_csrf(&app, &session_headers, &form).is_ok());

    // ダブルサブミットは成立するがセッションのトークンではない場合（Cookie 上書き攻撃）。
    let attacker = "f".repeat(32);
    let forged = cookie_headers(&[("sessionid", &session.key), ("csrftoken", &attacker)]);
    let mut forged_form = HashMap::new();
    forged_form.insert("csrfmiddlewaretoken".to_string(), attacker);
    assert!(
        web::check_csrf(&app, &forged, &forged_form).is_err(),
        "セッションに紐付かないトークンを通している"
    );
}

#[tokio::test]
async fn host_validation_covers_public_site_routes() {
    let (_dir, app) = temp_app();
    let router = web::router(app.clone());

    // Host ヘッダーが無い / 許可外なら 400（公開サイトも含む）。
    let request = Request::builder()
        .method("GET")
        .uri("/pricing/")
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.expect("response");
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "Host 未設定を拒否する"
    );

    let request = Request::builder()
        .method("GET")
        .uri("/pricing/")
        .header(header::HOST, "evil.example")
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.expect("response");
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "許可外ホストを拒否する"
    );

    // 許可ホストなら公開ページを返す。
    let request = Request::builder()
        .method("GET")
        .uri("/pricing/")
        .header(header::HOST, "localhost")
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    // 死活監視はコンテナ内プローブのため Host 検証の対象外にする。
    let request = Request::builder()
        .method("GET")
        .uri("/healthz")
        .header(header::HOST, "127.0.0.1:8000")
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.expect("response");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "healthz は Host 検証の除外対象"
    );
}

#[test]
fn login_throttle_blocks_after_repeated_failures() {
    let bucket = "test-login-throttle";
    let key = format!("key-{}", std::process::id());
    let window = Duration::from_secs(60);
    assert!(!auth::throttle_blocked(bucket, &key, 2, window));
    assert!(!auth::throttle_hit(bucket, &key, 2, window));
    assert!(!auth::throttle_blocked(bucket, &key, 2, window));
    assert!(!auth::throttle_hit(bucket, &key, 2, window));
    assert!(
        auth::throttle_blocked(bucket, &key, 2, window),
        "上限到達を検知できない"
    );
    assert!(auth::throttle_hit(bucket, &key, 2, window));
    auth::throttle_clear(bucket, &key);
    assert!(
        !auth::throttle_blocked(bucket, &key, 2, window),
        "成功時に履歴を消せない"
    );
}

#[tokio::test]
async fn repeated_login_failures_return_429() {
    let (_dir, app) = temp_app();
    let token = "0123456789abcdef0123456789abcdef";
    let headers = cookie_headers(&[("csrftoken", token)]);
    let username = format!("bruteforce-{}", std::process::id());
    let key = auth::login_throttle_key(&headers, &app.cfg, &username);
    let window = auth::LOGIN_FAILURE_WINDOW;

    // 失敗が記録されること（1 回だけ実リクエストを流す）。
    let response = login_post(&app, &headers, token, &username, "wrong-password", "").await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "1 回目はフォームを再表示する"
    );
    assert!(
        auth::throttle_blocked("login", &key, 1, window),
        "失敗を記録していない"
    );

    // 上限まで失敗した状態では、パスワード検証まで進まず 429 を返す。
    for _ in 1..auth::LOGIN_MAX_FAILURES {
        auth::throttle_hit("login", &key, auth::LOGIN_MAX_FAILURES, window);
    }
    let response = login_post(&app, &headers, token, &username, "wrong-password", "").await;
    assert_eq!(
        response.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "総当たりを 429 で止める"
    );
}

#[tokio::test]
async fn only_superuser_can_grant_superuser() {
    let (_dir, app) = temp_app();

    let make_user = |name: &str, superuser: bool, staff: bool| {
        let conn = app.db.lock();
        models::insert_user(
            &conn,
            name,
            &auth::unusable_password(),
            superuser,
            staff,
            true,
            "",
        )
        .expect("user")
    };

    let staff_id = make_user("plain-staff", false, true);
    let staff_session = auth::create_session(&app.db, staff_id);
    let staff_headers = cookie_headers(&[
        ("sessionid", &staff_session.key),
        ("csrftoken", &staff_session.data.csrf),
    ]);
    let mut form = HashMap::new();
    form.insert(
        "csrfmiddlewaretoken".to_string(),
        staff_session.data.csrf.clone(),
    );
    form.insert("username".to_string(), "escalated".to_string());
    form.insert("password".to_string(), "pw-12345678".to_string());
    form.insert("is_superuser".to_string(), "1".to_string());
    form.insert("is_active".to_string(), "1".to_string());

    // スタッフ（非管理者）は管理者アカウントを作れない。
    let response = admin::user_create(State(app.clone()), staff_headers, Form(form.clone())).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    {
        let conn = app.db.lock();
        assert!(
            models::find_user_by_username(&conn, "escalated")
                .unwrap()
                .is_none(),
            "スタッフが管理者アカウントを作れてしまう"
        );
    }

    // 管理者なら作成できる。
    let root_id = make_user("root-admin", true, true);
    let root_session = auth::create_session(&app.db, root_id);
    let root_headers = cookie_headers(&[
        ("sessionid", &root_session.key),
        ("csrftoken", &root_session.data.csrf),
    ]);
    let mut root_form = form;
    root_form.insert(
        "csrfmiddlewaretoken".to_string(),
        root_session.data.csrf.clone(),
    );
    let response = admin::user_create(State(app.clone()), root_headers, Form(root_form)).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    {
        let conn = app.db.lock();
        let created = models::find_user_by_username(&conn, "escalated")
            .unwrap()
            .expect("管理者は作成できる");
        assert!(created.is_superuser);
    }
}
