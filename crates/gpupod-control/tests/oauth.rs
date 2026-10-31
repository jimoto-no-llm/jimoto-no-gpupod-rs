//! Discord OAuth2 ログインのテスト（ネットワーク不要）。
//!
//! DiscordApi をモックに差し替え、start → callback の流れを
//! ハンドラ関数（start_request / callback_request / unlink_request）で検証する。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::body::to_bytes;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use gpupod_control::auth;
use gpupod_control::db::{fmt_dt, now};
use gpupod_control::models;
use gpupod_control::oauth::{self, DiscordApi, DiscordUser};
use gpupod_control::{App, Config};

/// Discord API のモック。
struct MockDiscord {
    user: DiscordUser,
    guilds: Vec<String>,
    member_roles: Option<Vec<String>>,
    exchange_count: Mutex<u32>,
}

impl MockDiscord {
    fn new(user: DiscordUser) -> Arc<MockDiscord> {
        Arc::new(MockDiscord {
            user,
            guilds: Vec::new(),
            member_roles: None,
            exchange_count: Mutex::new(0),
        })
    }
}

impl DiscordApi for MockDiscord {
    fn exchange_code(
        &self,
        _client_id: &str,
        _client_secret: &str,
        _code: &str,
        _redirect_uri: &str,
        _code_verifier: &str,
    ) -> Result<String, String> {
        let mut count = self.exchange_count.lock().unwrap();
        *count += 1;
        Ok("test-access-token".to_string())
    }

    fn fetch_user(&self, _access_token: &str) -> Result<DiscordUser, String> {
        Ok(self.user.clone())
    }

    fn list_guilds(&self, _access_token: &str) -> Result<Vec<String>, String> {
        Ok(self.guilds.clone())
    }

    fn guild_member_roles(
        &self,
        _access_token: &str,
        _guild_id: &str,
    ) -> Result<Option<Vec<String>>, String> {
        Ok(self.member_roles.clone())
    }
}

/// テスト用のユーザー。
fn discord_user(id: &str, username: &str) -> DiscordUser {
    DiscordUser {
        id: id.to_string(),
        username: username.to_string(),
        global_name: Some("表示名".to_string()),
        avatar: Some("a_hash".to_string()),
        email: Some(format!("{username}@example.com")),
        verified: true,
    }
}

/// Discord 設定込みの Config（所属確認はテストごとに指定する）。
fn base_config(dir: &Path) -> Config {
    let mut config = Config::from_env();
    config.state_dir = dir.to_path_buf();
    config.public_base_url = Some("https://gpu.example.org".to_string());
    config.discord_client_id = Some("test-client".to_string());
    config.discord_client_secret = Some("test-secret".to_string());
    config.discord_guild_id = None;
    config.discord_allowed_role_ids = Vec::new();
    config.discord_invite_url = None;
    config.insecure_http = true;
    config
}

fn app_with(config: Config, mock: Arc<MockDiscord>) -> Arc<App> {
    App::with_discord(config, mock).expect("app")
}

fn anon_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, "gpu.example.org".parse().unwrap());
    headers
}

fn cookie_headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let cookie = pairs
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ");
    let mut headers = anon_headers();
    headers.insert(header::COOKIE, cookie.parse().unwrap());
    headers
}

fn set_cookies(response: &Response) -> Vec<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok().map(str::to_string))
        .collect()
}

fn cookie_from(response: &Response, name: &str) -> Option<String> {
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

fn location(response: &Response) -> String {
    response
        .headers()
        .get(header::LOCATION)
        .expect("location")
        .to_str()
        .expect("location text")
        .to_string()
}

fn query_param(location: &str, key: &str) -> String {
    let query = location.split_once('?').map(|(_, rest)| rest).unwrap_or("");
    for pair in query.split('&') {
        if let Some((name, value)) = pair.split_once('=') {
            if name == key {
                return value.to_string();
            }
        }
    }
    panic!("クエリ {key} がありません: {location}");
}

/// start を実行して state を取り出す。
async fn start(app: &Arc<App>, headers: &HeaderMap) -> String {
    let response = oauth::start_request(app.clone(), headers).await;
    assert_eq!(response.status(), StatusCode::FOUND, "start は 302 を返す");
    let location = location(&response);
    assert!(
        location.starts_with("https://discord.com/oauth2/authorize?"),
        "認可 URL へリダイレクトする: {location}"
    );
    assert!(location.contains("code_challenge_method=S256"));
    assert!(location.contains("response_type=code"));
    assert!(location.contains("client_id=test-client"));
    assert!(
        location
            .contains("redirect_uri=https%3A%2F%2Fgpu.example.org%2Foauth%2Fdiscord%2Fcallback"),
        "PUBLIC_BASE_URL から redirect_uri を組み立てる: {location}"
    );
    assert!(location.contains("scope=identify%20email%20guilds"));
    query_param(&location, "state")
}

/// start が発行する state 束縛 Cookie を足したヘッダーを返す。
fn with_state_cookie(headers: &HeaderMap, state: &str) -> HeaderMap {
    let mut headers = headers.clone();
    let existing = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let cookie = if existing.is_empty() {
        format!("{}={state}", oauth::STATE_COOKIE)
    } else {
        format!("{existing}; {}={state}", oauth::STATE_COOKIE)
    };
    headers.insert(header::COOKIE, cookie.parse().expect("cookie"));
    headers
}

/// 指定 state で callback を呼ぶ（ブラウザが state Cookie を持っている想定）。
async fn callback(app: &Arc<App>, headers: &HeaderMap, code: &str, state: &str) -> Response {
    let mut query = HashMap::new();
    query.insert("code".to_string(), code.to_string());
    query.insert("state".to_string(), state.to_string());
    oauth::callback_request(app.clone(), &with_state_cookie(headers, state), &query).await
}

fn user_count(app: &App) -> i64 {
    let conn = app.db.lock();
    conn.query_row("SELECT COUNT(*) FROM auth_user", [], |row| row.get(0))
        .unwrap()
}

fn user_id_by_name(app: &App, username: &str) -> i64 {
    let conn = app.db.lock();
    models::find_user_by_username(&conn, username)
        .unwrap()
        .unwrap_or_else(|| panic!("ユーザー {username} がいません"))
        .id
}

async fn body_text(response: Response) -> String {
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    String::from_utf8_lossy(&bytes).to_string()
}

fn file_config(dir: &Path) -> Config {
    let mut config = base_config(dir);
    config.discord_guild_id = None;
    config.discord_allowed_role_ids = Vec::new();
    config
}

#[tokio::test]
async fn start_and_callback_create_user_and_session() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mock = MockDiscord::new(discord_user("100000000000000001", "alice"));
    let app = app_with(file_config(dir.path()), mock);

    let headers = anon_headers();
    let state = start(&app, &headers).await;
    let response = callback(&app, &headers, "test-code", &state).await;
    assert_eq!(
        response.status(),
        StatusCode::FOUND,
        "callback は 302 を返す"
    );
    assert_eq!(location(&response), "/", "ログイン後は / へ");
    let session_key = cookie_from(&response, "sessionid").expect("sessionid Cookie");
    let csrf_cookie = cookie_from(&response, "csrftoken").expect("csrftoken Cookie");
    assert!(!csrf_cookie.is_empty());

    let session = auth::load_session(&app.db, &cookie_headers(&[("sessionid", &session_key)]))
        .expect("セッションが保存されている");
    let user_id = user_id_by_name(&app, "alice");
    assert_eq!(session.data.user_id, user_id);

    {
        let conn = app.db.lock();
        let user = models::find_user(&conn, user_id).unwrap().unwrap();
        assert_eq!(user.email, "alice@example.com");
        assert!(!user.is_staff && !user.is_superuser && user.is_active);
        assert!(
            auth::has_usable_password(&user.password) == false,
            "パスワードは未設定扱い"
        );

        let linked = models::find_oauth_identity(&conn, oauth::PROVIDER, "100000000000000001")
            .unwrap()
            .expect("紐付けが保存されている");
        assert_eq!(linked.user_id, user_id);

        let profile = models::get_user_profile(&conn, user_id)
            .unwrap()
            .expect("プロフィール");
        assert_eq!(profile.display_name, "表示名");
        assert_eq!(
            profile.avatar_url,
            "https://cdn.discordapp.com/avatars/100000000000000001/a_hash.gif"
        );

        // state は単回使用で削除されている。
        assert!(models::take_oauth_state(&conn, &state).unwrap().is_none());
    }

    // 同じ state の再生は拒否する。
    let replay = callback(&app, &headers, "test-code", &state).await;
    assert_eq!(
        replay.status(),
        StatusCode::BAD_REQUEST,
        "state の再利用は 400"
    );
}

#[tokio::test]
async fn second_login_reuses_same_user() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mock = MockDiscord::new(discord_user("200000000000000001", "bob"));
    let app = app_with(file_config(dir.path()), mock);

    let headers = anon_headers();
    let first_state = start(&app, &headers).await;
    let first = callback(&app, &headers, "code-1", &first_state).await;
    assert_eq!(first.status(), StatusCode::FOUND);
    let first_session = cookie_from(&first, "sessionid").expect("sessionid");
    let first_user = auth::load_session(&app.db, &cookie_headers(&[("sessionid", &first_session)]))
        .unwrap()
        .data
        .user_id;

    let second_state = start(&app, &headers).await;
    let second = callback(&app, &headers, "code-2", &second_state).await;
    assert_eq!(second.status(), StatusCode::FOUND);
    let second_session = cookie_from(&second, "sessionid").expect("sessionid");
    let second_user =
        auth::load_session(&app.db, &cookie_headers(&[("sessionid", &second_session)]))
            .unwrap()
            .data
            .user_id;

    assert_eq!(
        first_user, second_user,
        "同じ Discord アカウントは同じユーザー"
    );
    assert_eq!(user_count(&app), 1, "ユーザーを重複作成しない");
}

#[tokio::test]
async fn invalid_and_expired_state_are_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mock = MockDiscord::new(discord_user("300000000000000001", "carol"));
    let app = app_with(file_config(dir.path()), mock);
    let headers = anon_headers();

    // 未知の state。
    let response = callback(&app, &headers, "code", "unknown-state").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // state 無し。
    let mut empty = HashMap::new();
    empty.insert("code".to_string(), "code".to_string());
    let response = oauth::callback_request(app.clone(), &headers, &empty).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // 期限切れの state。
    let state = start(&app, &headers).await;
    {
        let conn = app.db.lock();
        conn.execute(
            "UPDATE portal_oauth_state SET expires_at = ?2 WHERE state = ?1",
            rusqlite::params![state, fmt_dt(now() - chrono::Duration::seconds(1))],
        )
        .unwrap();
    }
    let response = callback(&app, &headers, "code", &state).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST, "期限切れは 400");
    assert_eq!(user_count(&app), 0, "期限切れではユーザーを作らない");
    {
        let conn = app.db.lock();
        assert!(models::take_oauth_state(&conn, &state).unwrap().is_none());
    }
}

#[tokio::test]
async fn guild_membership_is_required() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = base_config(dir.path());
    config.discord_guild_id = Some("424242424242424242".to_string());
    let mock = MockDiscord::new(discord_user("400000000000000001", "dave"));
    let app = app_with(config, mock.clone());
    let headers = anon_headers();

    let state = start(&app, &headers).await;
    let response = callback(&app, &headers, "code", &state).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN, "未所属は 403");
    assert_eq!(user_count(&app), 0);
}

#[tokio::test]
async fn allowed_role_is_required() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = base_config(dir.path());
    config.discord_guild_id = Some("424242424242424242".to_string());
    config.discord_allowed_role_ids = vec!["role-allowed".to_string()];
    let mock = MockDiscord {
        user: discord_user("410000000000000001", "erin"),
        guilds: vec!["424242424242424242".to_string()],
        member_roles: Some(vec!["role-other".to_string()]),
        exchange_count: Mutex::new(0),
    };
    let app = app_with(config, Arc::new(mock));
    let headers = anon_headers();

    let state = start(&app, &headers).await;
    let response = callback(&app, &headers, "code", &state).await;
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "ロール未保持は 403"
    );
    assert_eq!(user_count(&app), 0);
}

#[tokio::test]
async fn linking_uses_logged_in_user_and_rejects_other_user() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mock = MockDiscord::new(discord_user("500000000000000001", "frank"));
    let app = app_with(file_config(dir.path()), mock);
    let first_user = {
        let conn = app.db.lock();
        models::insert_user(
            &conn,
            "existing",
            &auth::hash_password("pw"),
            false,
            false,
            true,
            "",
        )
        .unwrap()
    };
    let first_session = auth::create_session(&app.db, first_user);
    let headers = cookie_headers(&[("sessionid", &first_session.key)]);

    let state = start(&app, &headers).await;
    let response = callback(&app, &headers, "code", &state).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        user_count(&app),
        1,
        "ログイン中の連携では新規ユーザーを作らない"
    );
    {
        let conn = app.db.lock();
        let linked = models::find_oauth_identity(&conn, oauth::PROVIDER, "500000000000000001")
            .unwrap()
            .expect("紐付け");
        assert_eq!(linked.user_id, first_user);
    }

    // 別ユーザーが同じ Discord アカウントを連携しようとすると 409。
    let second_user = {
        let conn = app.db.lock();
        models::insert_user(
            &conn,
            "other",
            &auth::hash_password("pw"),
            false,
            false,
            true,
            "",
        )
        .unwrap()
    };
    let second_session = auth::create_session(&app.db, second_user);
    let second_headers = cookie_headers(&[("sessionid", &second_session.key)]);
    let state = start(&app, &second_headers).await;
    let response = callback(&app, &second_headers, "code", &state).await;
    assert_eq!(response.status(), StatusCode::CONFLICT, "二重連携は 409");
    {
        let conn = app.db.lock();
        let linked = models::find_oauth_identity(&conn, oauth::PROVIDER, "500000000000000001")
            .unwrap()
            .unwrap();
        assert_eq!(linked.user_id, first_user, "元の紐付けは変わらない");
        assert!(
            models::find_oauth_identity_by_user(&conn, oauth::PROVIDER, second_user)
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn unlink_removes_identity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mock = MockDiscord::new(discord_user("600000000000000001", "grace"));
    let app = app_with(file_config(dir.path()), mock);
    let user_id = {
        let conn = app.db.lock();
        let user_id = models::insert_user(
            &conn,
            "linked",
            &auth::hash_password("pw"),
            false,
            false,
            true,
            "",
        )
        .unwrap();
        models::insert_oauth_identity(
            &conn,
            oauth::PROVIDER,
            "600000000000000001",
            user_id,
            &fmt_dt(now()),
        )
        .unwrap();
        user_id
    };
    let session = auth::create_session(&app.db, user_id);
    let headers = cookie_headers(&[
        ("sessionid", &session.key),
        ("csrftoken", &session.data.csrf),
    ]);
    let mut form = HashMap::new();
    form.insert("csrfmiddlewaretoken".to_string(), session.data.csrf.clone());

    let response = oauth::unlink_request(&app, &headers, &form);
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(location(&response), "/account/");
    {
        let conn = app.db.lock();
        assert!(
            models::find_oauth_identity_by_user(&conn, oauth::PROVIDER, user_id)
                .unwrap()
                .is_none(),
            "紐付けが削除される"
        );
        assert!(
            models::find_user(&conn, user_id).unwrap().is_some(),
            "ユーザーは残る"
        );
    }

    // CSRF トークンが無い場合は拒否。
    let mut bad_form = HashMap::new();
    bad_form.insert("csrfmiddlewaretoken".to_string(), "wrong".to_string());
    let response = oauth::unlink_request(&app, &headers, &bad_form);
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn unlink_is_refused_without_password() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mock = MockDiscord::new(discord_user("700000000000000001", "heidi"));
    let app = app_with(file_config(dir.path()), mock);
    let user_id = {
        let conn = app.db.lock();
        let user_id = models::insert_user(
            &conn,
            "oauth-only",
            &auth::unusable_password(),
            false,
            false,
            true,
            "",
        )
        .unwrap();
        models::insert_oauth_identity(
            &conn,
            oauth::PROVIDER,
            "700000000000000001",
            user_id,
            &fmt_dt(now()),
        )
        .unwrap();
        user_id
    };
    let session = auth::create_session(&app.db, user_id);
    let headers = cookie_headers(&[
        ("sessionid", &session.key),
        ("csrftoken", &session.data.csrf),
    ]);
    let mut form = HashMap::new();
    form.insert("csrfmiddlewaretoken".to_string(), session.data.csrf.clone());

    let response = oauth::unlink_request(&app, &headers, &form);
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(location(&response), "/account/");
    {
        let conn = app.db.lock();
        assert!(
            models::find_oauth_identity_by_user(&conn, oauth::PROVIDER, user_id)
                .unwrap()
                .is_some(),
            "パスワード未設定では解除できない"
        );
    }
}

#[test]
fn default_guild_id_is_configured() {
    assert_eq!(
        gpupod_control::config::DEFAULT_DISCORD_GUILD_ID,
        "1551195569429422202"
    );
    // 環境変数で上書きされていない場合は既定のサーバー ID が入る。
    let overridden = std::env::var("GPUPOD_DISCORD_GUILD_ID")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    if !overridden {
        let config = Config::from_env();
        assert_eq!(
            config.discord_guild_id.as_deref(),
            Some(gpupod_control::config::DEFAULT_DISCORD_GUILD_ID)
        );
    }
}

#[tokio::test]
async fn guild_guidance_page_with_invite_link() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = base_config(dir.path());
    config.discord_guild_id = Some(gpupod_control::config::DEFAULT_DISCORD_GUILD_ID.to_string());
    config.discord_invite_url = Some("https://discord.gg/example".to_string());
    let app = app_with(
        config,
        MockDiscord::new(discord_user("800000000000000001", "ivan")),
    );

    let headers = anon_headers();
    let state = start(&app, &headers).await;
    let response = callback(&app, &headers, "code", &state).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = body_text(response).await;
    assert!(
        body.contains("Discord サーバーへの参加が必要です"),
        "参加案内を表示する"
    );
    assert!(
        body.contains("招待リンクから Discord サーバーに参加します。"),
        "参加手順を表示する"
    );
    assert!(body.contains("再確認する"), "再確認ボタンを表示する");
    assert!(
        body.contains("href=\"/oauth/discord/start\""),
        "再確認は start へ戻す"
    );
    assert!(
        body.contains("href=\"https://discord.gg/example\""),
        "招待リンクを表示する"
    );
    assert!(!body.contains("運営に招待リンクを確認してください"));
}

#[tokio::test]
async fn guild_guidance_page_without_invite_link() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = base_config(dir.path());
    config.discord_guild_id = Some(gpupod_control::config::DEFAULT_DISCORD_GUILD_ID.to_string());
    config.discord_invite_url = None;
    let app = app_with(
        config,
        MockDiscord::new(discord_user("810000000000000001", "judy")),
    );

    let headers = anon_headers();
    let state = start(&app, &headers).await;
    let response = callback(&app, &headers, "code", &state).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = body_text(response).await;
    assert!(body.contains("Discord サーバーへの参加が必要です"));
    assert!(
        body.contains("運営に招待リンクを確認してください"),
        "招待 URL 未設定の案内を表示する"
    );
    assert!(!body.contains("Discord サーバーに参加する"));
}

#[tokio::test]
async fn login_page_shows_participation_notice() {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = app_with(
        base_config(dir.path()),
        MockDiscord::new(discord_user("820000000000000001", "kim")),
    );

    let response =
        gpupod_control::pages::login_get(State(app.clone()), anon_headers(), Query(HashMap::new()))
            .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_text(response).await;
    assert!(
        body.contains("Discord サーバーへの参加が必要です"),
        "ログイン画面にも参加案内を表示する"
    );
    assert!(
        body.contains("Discord でログイン"),
        "Discord ログインボタンを表示する"
    );
    assert!(
        body.contains("csrfmiddlewaretoken"),
        "CSRF hidden input を出力する"
    );
}

/// state がブラウザの Cookie に束縛され、他人が用意した callback URL では
/// ログインできない（ログイン CSRF 対策）ことを確認する。
#[tokio::test]
async fn state_is_bound_to_browser_cookie() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mock = MockDiscord::new(discord_user("900000000000000001", "mallory"));
    let app = app_with(file_config(dir.path()), mock);
    let headers = anon_headers();

    let response = oauth::start_request(app.clone(), &headers).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    let location = location(&response);
    let state = query_param(&location, "state");
    let cookie = cookie_from(&response, oauth::STATE_COOKIE).expect("state Cookie");
    assert_eq!(cookie, state, "Cookie には同じ state を入れる");
    let set_cookie = set_cookies(&response)
        .into_iter()
        .find(|value| value.starts_with("oauth_state="))
        .expect("Set-Cookie");
    assert!(set_cookie.contains("HttpOnly"), "HttpOnly を付ける");
    assert!(set_cookie.contains("SameSite=Lax"), "SameSite=Lax を付ける");
    assert!(
        set_cookie.contains("Path=/oauth/discord/"),
        "callback へ送る Path にする"
    );

    let mut query = HashMap::new();
    query.insert("code".to_string(), "code".to_string());
    query.insert("state".to_string(), state.clone());

    // Cookie を持たないブラウザ（被害者に踏ませた callback URL）は拒否する。
    let response = oauth::callback_request(app.clone(), &headers, &query).await;
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "Cookie 無しは 400"
    );
    assert_eq!(user_count(&app), 0, "ユーザーを作らない");

    // 別の state Cookie を持つブラウザでも拒否する。
    let other_state = "0".repeat(64);
    let wrong = with_state_cookie(&headers, &other_state);
    let response = oauth::callback_request(app.clone(), &wrong, &query).await;
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "別の Cookie は 400"
    );
    assert_eq!(user_count(&app), 0);

    // 正しい Cookie があれば成功し、使用後に Cookie を消す。
    let response = callback(&app, &headers, "code", &state).await;
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(user_count(&app), 1);
    let cleared = set_cookies(&response)
        .into_iter()
        .find(|value| value.starts_with("oauth_state="))
        .expect("削除用 Set-Cookie");
    assert!(cleared.contains("Max-Age=0"), "使用済み Cookie を消す");
}
