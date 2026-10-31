//! Discord OAuth2 ログイン（認可コード + PKCE S256）。
//!
//! 認可 URL の生成、callback でのトークン交換、Discord ユーザー情報の取得、
//! portal_oauth_identity / portal_userprofile の更新、連携解除を扱う。
//! HTTP 通信は DiscordApi トレイトに閉じ込め、テストではモックに差し替える。

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use base64::Engine;
use chrono::Duration;

use crate::auth;
use crate::config::Config;
use crate::db::{fmt_dt, now, parse_dt};
use crate::models;
use crate::web;

/// portal_oauth_identity に保存する provider 名。
pub const PROVIDER: &str = "discord";
/// state の有効期間（秒）。
pub const STATE_TTL_SECONDS: i64 = 600;
/// state をブラウザへ束縛する Cookie 名（ログイン CSRF 対策）。
pub const STATE_COOKIE: &str = "oauth_state";

const AUTHORIZE_URL: &str = "https://discord.com/oauth2/authorize";
const TOKEN_URL: &str = "https://discord.com/api/oauth2/token";
const USER_URL: &str = "https://discord.com/api/users/@me";
const GUILDS_URL: &str = "https://discord.com/api/users/@me/guilds";
const AVATAR_BASE: &str = "https://cdn.discordapp.com/avatars";
const SCOPES: &str = "identify email guilds";

/// Discord のユーザー情報（GET /users/@me のうち使う項目）。
#[derive(Debug, Clone)]
pub struct DiscordUser {
    pub id: String,
    pub username: String,
    pub global_name: Option<String>,
    pub avatar: Option<String>,
    pub email: Option<String>,
    pub verified: bool,
}

/// Discord API へのアクセス（テストでモックに差し替える境界）。
pub trait DiscordApi: Send + Sync {
    /// 認可コードをアクセストークンへ交換する。
    fn exchange_code(
        &self,
        client_id: &str,
        client_secret: &str,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> Result<String, String>;
    /// GET /users/@me。
    fn fetch_user(&self, access_token: &str) -> Result<DiscordUser, String>;
    /// GET /users/@me/guilds（所属サーバー ID の一覧）。
    fn list_guilds(&self, access_token: &str) -> Result<Vec<String>, String>;
    /// GET /users/@me/guilds/{guild}/member（未所属は None）。
    fn guild_member_roles(
        &self,
        access_token: &str,
        guild_id: &str,
    ) -> Result<Option<Vec<String>>, String>;
}

/// reqwest（blocking）による実装。
///
/// axum の非同期ハンドラからは spawn_blocking 経由で呼ぶ。
pub struct HttpDiscord {
    client: reqwest::blocking::Client,
}

impl HttpDiscord {
    pub fn new() -> HttpDiscord {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());
        HttpDiscord { client }
    }
}

impl Default for HttpDiscord {
    fn default() -> HttpDiscord {
        HttpDiscord::new()
    }
}

#[derive(Debug, serde::Deserialize)]
struct TokenResponse {
    access_token: String,
}

impl DiscordApi for HttpDiscord {
    fn exchange_code(
        &self,
        client_id: &str,
        client_secret: &str,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> Result<String, String> {
        // form フィーチャーに依存しないよう、本文を自分で組み立てる。
        let body = format!(
            "client_id={}&client_secret={}&grant_type=authorization_code&code={}&redirect_uri={}&code_verifier={}",
            url_encode(client_id),
            url_encode(client_secret),
            url_encode(code),
            url_encode(redirect_uri),
            url_encode(code_verifier)
        );
        let response = self
            .client
            .post(TOKEN_URL)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .map_err(|exc| format!("token エンドポイントへの接続に失敗しました: {exc}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "token エンドポイントが {} を返しました",
                response.status()
            ));
        }
        let token: TokenResponse = response
            .json()
            .map_err(|exc| format!("token 応答の解析に失敗しました: {exc}"))?;
        if token.access_token.trim().is_empty() {
            return Err("token 応答に access_token がありません".to_string());
        }
        Ok(token.access_token)
    }

    fn fetch_user(&self, access_token: &str) -> Result<DiscordUser, String> {
        let response = self
            .client
            .get(USER_URL)
            .bearer_auth(access_token)
            .send()
            .map_err(|exc| format!("users/@me への接続に失敗しました: {exc}"))?;
        if !response.status().is_success() {
            return Err(format!("users/@me が {} を返しました", response.status()));
        }
        let value: serde_json::Value = response
            .json()
            .map_err(|exc| format!("users/@me の解析に失敗しました: {exc}"))?;
        let id = value
            .get("id")
            .and_then(|item| item.as_str())
            .ok_or_else(|| "users/@me に id がありません".to_string())?
            .to_string();
        Ok(DiscordUser {
            id,
            username: value
                .get("username")
                .and_then(|item| item.as_str())
                .unwrap_or("")
                .to_string(),
            global_name: value
                .get("global_name")
                .and_then(|item| item.as_str())
                .map(str::to_string),
            avatar: value
                .get("avatar")
                .and_then(|item| item.as_str())
                .map(str::to_string),
            email: value
                .get("email")
                .and_then(|item| item.as_str())
                .map(str::to_string),
            verified: value
                .get("verified")
                .and_then(|item| item.as_bool())
                .unwrap_or(false),
        })
    }

    fn list_guilds(&self, access_token: &str) -> Result<Vec<String>, String> {
        let response = self
            .client
            .get(GUILDS_URL)
            .bearer_auth(access_token)
            .send()
            .map_err(|exc| format!("users/@me/guilds への接続に失敗しました: {exc}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "users/@me/guilds が {} を返しました",
                response.status()
            ));
        }
        let value: serde_json::Value = response
            .json()
            .map_err(|exc| format!("users/@me/guilds の解析に失敗しました: {exc}"))?;
        let guilds = value
            .as_array()
            .ok_or_else(|| "users/@me/guilds が配列ではありません".to_string())?;
        Ok(guilds
            .iter()
            .filter_map(|item| {
                item.get("id")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            })
            .collect())
    }

    fn guild_member_roles(
        &self,
        access_token: &str,
        guild_id: &str,
    ) -> Result<Option<Vec<String>>, String> {
        let url = format!("{GUILDS_URL}/{}/member", url_encode(guild_id));
        let response = self
            .client
            .get(&url)
            .bearer_auth(access_token)
            .send()
            .map_err(|exc| format!("guild member への接続に失敗しました: {exc}"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(format!(
                "guild member が {} を返しました",
                response.status()
            ));
        }
        let value: serde_json::Value = response
            .json()
            .map_err(|exc| format!("guild member の解析に失敗しました: {exc}"))?;
        let roles = value
            .get("roles")
            .and_then(|item| item.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Some(roles))
    }
}

/// OAuth の state と PKCE verifier を生成する。
pub fn new_state_and_verifier() -> (String, String) {
    (random_hex(32), code_verifier())
}

/// PKCE の code_verifier（43..=128 文字、base64url）。
fn code_verifier() -> String {
    let mut buffer = vec![0u8; 64];
    rand::Rng::fill(&mut rand::thread_rng(), &mut buffer[..]);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buffer)
}

/// PKCE の code_challenge（S256）。
pub fn code_challenge(verifier: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// RFC 3986 の unreserved 以外をパーセントエンコードする。
pub fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// リダイレクト URI（PUBLIC_BASE_URL 優先、未設定は Host から）。
pub fn redirect_uri(cfg: &Config, headers: &HeaderMap) -> String {
    format!("{}/oauth/discord/callback", cfg.public_base(headers))
}

/// Discord の認可 URL。
pub fn authorize_url(client_id: &str, redirect_uri: &str, state: &str, challenge: &str) -> String {
    format!(
        "{AUTHORIZE_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        url_encode(client_id),
        url_encode(redirect_uri),
        url_encode(SCOPES),
        url_encode(state),
        url_encode(challenge)
    )
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::Rng::fill(&mut rand::thread_rng(), &mut buffer[..]);
    hex::encode(buffer)
}

/// 文字数を上限で切り詰める。
fn truncate_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

/// Django の username 規則に寄せたサニタイズ（英数字と _- のみ、1..=150）。
fn sanitize_username(raw: &str) -> String {
    let base: String = raw
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '-')
        .collect();
    let base = truncate_chars(&base, 150);
    if base.is_empty() {
        "discord".to_string()
    } else {
        base
    }
}

/// 衝突しない username を作る。
fn unique_username(conn: &rusqlite::Connection, raw: &str) -> rusqlite::Result<String> {
    let base = truncate_chars(&sanitize_username(raw), 140);
    if models::find_user_by_username(conn, &base)?.is_none() {
        return Ok(base);
    }
    for suffix in 2..1000 {
        let candidate = format!("{base}-{suffix}");
        if models::find_user_by_username(conn, &candidate)?.is_none() {
            return Ok(candidate);
        }
    }
    loop {
        let candidate = format!("{base}-{}", random_hex(4));
        if models::find_user_by_username(conn, &candidate)?.is_none() {
            return Ok(candidate);
        }
    }
}

/// プロフィール表示名（global_name 優先、無ければ username）。
fn display_name(identity: &DiscordUser) -> String {
    let candidate = identity
        .global_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            let username = identity.username.trim();
            if username.is_empty() {
                None
            } else {
                Some(username)
            }
        })
        .unwrap_or(&identity.id);
    let cleaned: String = candidate.chars().filter(|ch| !ch.is_control()).collect();
    truncate_chars(&cleaned, 150)
}

/// アイコン URL（未設定は空文字）。
fn avatar_url(identity: &DiscordUser) -> String {
    match &identity.avatar {
        Some(hash) if !hash.is_empty() => {
            let extension = if hash.starts_with("a_") { "gif" } else { "png" };
            truncate_chars(
                &format!("{AVATAR_BASE}/{}/{}.{}", identity.id, hash, extension),
                200,
            )
        }
        _ => String::new(),
    }
}

/// メールアドレス（未確認なら保存しない）。
fn account_email(identity: &DiscordUser) -> String {
    if !identity.verified {
        return String::new();
    }
    identity
        .email
        .clone()
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// 連携解決の失敗理由。
#[derive(Debug, Clone, Copy)]
enum LinkError {
    /// すでに別ユーザーへ連携済み。
    Conflict,
    /// 連携先ユーザーが無効。
    Inactive,
    /// DB エラー（詳細はログにだけ出す）。
    Database,
}

/// ログイン先ユーザーを決める（既存の紐付け → 連携 → 新規作成）。
fn resolve_identity(
    conn: &rusqlite::Connection,
    identity: &DiscordUser,
    link_user_id: Option<i64>,
) -> Result<i64, LinkError> {
    let db_error = |exc: rusqlite::Error| {
        tracing::error!("discord oauth の DB 操作に失敗しました: {exc}");
        LinkError::Database
    };

    if let Some(existing) =
        models::find_oauth_identity(conn, PROVIDER, &identity.id).map_err(db_error)?
    {
        if let Some(link) = link_user_id {
            if link != existing.user_id {
                return Err(LinkError::Conflict);
            }
        }
        models::upsert_user_profile(
            conn,
            existing.user_id,
            &display_name(identity),
            &avatar_url(identity),
            &fmt_dt(now()),
        )
        .map_err(db_error)?;
        return Ok(existing.user_id);
    }

    match link_user_id {
        Some(link) => {
            let target = models::find_user(conn, link)
                .map_err(db_error)?
                .ok_or(LinkError::Inactive)?;
            if !target.is_active {
                return Err(LinkError::Inactive);
            }
            if models::find_oauth_identity_by_user(conn, PROVIDER, link)
                .map_err(db_error)?
                .is_some()
            {
                return Err(LinkError::Conflict);
            }
            models::insert_oauth_identity(conn, PROVIDER, &identity.id, link, &fmt_dt(now()))
                .map_err(db_error)?;
            models::upsert_user_profile(
                conn,
                link,
                &display_name(identity),
                &avatar_url(identity),
                &fmt_dt(now()),
            )
            .map_err(db_error)?;
            Ok(link)
        }
        None => {
            let username = unique_username(conn, &identity.username).map_err(db_error)?;
            let password = auth::unusable_password();
            let email = truncate_chars(&account_email(identity), 254);
            let user_id =
                models::insert_user(conn, &username, &password, false, false, true, &email)
                    .map_err(db_error)?;
            models::insert_oauth_identity(conn, PROVIDER, &identity.id, user_id, &fmt_dt(now()))
                .map_err(db_error)?;
            models::upsert_user_profile(
                conn,
                user_id,
                &display_name(identity),
                &avatar_url(identity),
                &fmt_dt(now()),
            )
            .map_err(db_error)?;
            Ok(user_id)
        }
    }
}

fn oauth_error_page(status: StatusCode, title: &str, message: &str) -> Response {
    web::error_page(status, title, message)
}

fn not_configured_page() -> Response {
    oauth_error_page(
        StatusCode::NOT_FOUND,
        "Discord ログインは設定されていません",
        "このサーバーでは Discord ログインが有効になっていません。管理者にお問い合わせください。",
    )
}

fn invalid_state_page() -> Response {
    oauth_error_page(
        StatusCode::BAD_REQUEST,
        "ログインを続行できません（400）",
        "認証情報が無効か、有効期限が切れています。お手数ですが、もう一度ログインからお試しください。",
    )
}

fn denied_page() -> Response {
    oauth_error_page(
        StatusCode::FORBIDDEN,
        "Discord での認証がキャンセルされました",
        "ログインをやり直してください。",
    )
}

/// state 束縛 Cookie（HttpOnly・SameSite=Lax・10 分）。
fn state_cookie(state: &str, secure: bool) -> String {
    let mut cookie = format!(
        "{STATE_COOKIE}={state}; Path=/oauth/discord/; Max-Age={STATE_TTL_SECONDS}; HttpOnly; SameSite=Lax"
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// state 束縛 Cookie の削除。
fn clear_state_cookie(secure: bool) -> String {
    let mut cookie =
        format!("{STATE_COOKIE}=; Path=/oauth/discord/; Max-Age=0; HttpOnly; SameSite=Lax");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// HTML 属性・本文用のエスケープ。
fn html_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            other => out.push(other),
        }
    }
    out
}

/// 参加案内に使える招待 URL だけを返す（http/https 以外は無視する）。
fn safe_invite_url(app: &crate::App) -> Option<&str> {
    let value = app.cfg.discord_invite_url.as_deref()?.trim();
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") {
        Some(value)
    } else {
        tracing::warn!("GPUPOD_DISCORD_INVITE_URL が http(s) の URL ではないため表示しません");
        None
    }
}

/// サーバー未所属時の案内ページ（参加手順と再確認ボタン）。
fn guild_required_page(app: &crate::App) -> Response {
    let invite = safe_invite_url(app);
    let invite_html = match invite {
        Some(url) => format!(
            "<p><a class=\"button\" href=\"{}\">Discord サーバーに参加する</a></p>",
            html_escape(url)
        ),
        None => "<p class=\"muted\">招待リンクが分からない場合は、運営に招待リンクを確認してください。</p>"
            .to_string(),
    };
    let body = format!(
        "<!doctype html><html lang=\"ja\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>Discord サーバーへの参加が必要です（403）</title><style>\
         :root{{font:16px/1.6 system-ui,sans-serif;color:#202e3a;background:#f4f6f8}}\
         *{{box-sizing:border-box}}body{{margin:0}}\
         main{{max-width:720px;margin:3rem auto;padding:0 1rem}}\
         section{{background:white;border:1px solid #dbe2e6;border-radius:9px;padding:1.4rem;margin:1rem 0}}\
         h1{{font-size:1.5rem}}a{{color:#125e88}}\
         .button{{display:inline-block;background:#166589;color:white;padding:.55rem 1rem;border-radius:5px;text-decoration:none;margin:.3rem .4rem .3rem 0}}\
         .secondary{{background:white;color:#234c61;border:1px solid #bac7cf}}\
         .muted{{color:#586d79}}ol{{padding-left:1.3rem}}\
         </style></head><body><main>\
         <h1>Discord サーバーへの参加が必要です（403）</h1>\
         <section><p>このサービスの利用には、Discord サーバーへの参加が必要です。参加は誰でも可能です。</p>\
         <ol><li>招待リンクから Discord サーバーに参加します。</li>\
         <li>参加したら「再確認する」を押してください。</li></ol>\
         {invite_html}\
         <p><a class=\"button secondary\" href=\"/oauth/discord/start\">再確認する</a></p>\
         </section>\
         <p><a href=\"/login/\">ログイン画面に戻る</a></p></main></body></html>"
    );
    let mut response = web::html(body);
    *response.status_mut() = StatusCode::FORBIDDEN;
    response
}

fn role_required_page() -> Response {
    oauth_error_page(
        StatusCode::FORBIDDEN,
        "このサービスは利用できません（403）",
        "このサービスを利用するためのロールが付与されていません。管理者にお問い合わせください。",
    )
}

fn conflict_page() -> Response {
    oauth_error_page(
        StatusCode::CONFLICT,
        "Discord アカウントを連携できません（409）",
        "この Discord アカウントは、すでに別のユーザーに連携されています。",
    )
}

fn inactive_page() -> Response {
    oauth_error_page(
        StatusCode::FORBIDDEN,
        "このアカウントは利用できません（403）",
        "アカウントが無効になっています。管理者にお問い合わせください。",
    )
}

fn upstream_error_page() -> Response {
    oauth_error_page(
        StatusCode::BAD_GATEWAY,
        "Discord との通信に失敗しました（502）",
        "時間をおいて、もう一度お試しください。",
    )
}

fn internal_error_page() -> Response {
    oauth_error_page(
        StatusCode::INTERNAL_SERVER_ERROR,
        "サーバーで予期しないエラーが発生しました（500）",
        "処理を完了できませんでした。時間をおいて再度お試しください。",
    )
}

/// spawn_blocking で DiscordApi を呼ぶ（非同期ランタイムを塞がない）。
async fn api_call<T, F>(api: Arc<dyn DiscordApi>, work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&dyn DiscordApi) -> Result<T, String> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || work(api.as_ref())).await {
        Ok(result) => result,
        Err(exc) => Err(format!("Discord API のタスクが失敗しました: {exc}")),
    }
}

/// ログイン成功時の応答（セッション Cookie と CSRF Cookie を発行する）。
fn login_response(app: &crate::App, headers: &HeaderMap, user_id: i64) -> Response {
    if let Some(existing) = auth::load_session(&app.db, headers) {
        auth::drop_session(&app.db, &existing.key);
    }
    let session = auth::login_user(&app.db, user_id);
    let secure = !app.cfg.insecure_http;
    let mut response = web::redirect("/");
    web::append_cookie(&mut response, auth::session_cookie(&session.key, secure));
    web::append_cookie(&mut response, auth::csrf_cookie(&session.data.csrf, secure));
    // 使用済みの state 束縛 Cookie を消す。
    web::append_cookie(&mut response, clear_state_cookie(secure));
    response
}

/// GET /oauth/discord/start。
pub async fn start_request(app: Arc<crate::App>, headers: &HeaderMap) -> Response {
    if !app.cfg.discord_enabled() {
        return not_configured_page();
    }
    let link_user_id = auth::identity(&app.db, headers).map(|identity| identity.user.id);
    let (state, verifier) = new_state_and_verifier();
    let challenge = code_challenge(&verifier);
    let created = fmt_dt(now());
    let expires = fmt_dt(now() + Duration::seconds(STATE_TTL_SECONDS));
    {
        let conn = app.db.lock();
        let _ = models::purge_expired_oauth_states(&conn, &created);
        if let Err(exc) =
            models::insert_oauth_state(&conn, &state, &verifier, link_user_id, &expires, &created)
        {
            tracing::error!("discord oauth の state 保存に失敗しました: {exc}");
            return internal_error_page();
        }
    }
    let redirect_uri = redirect_uri(&app.cfg, headers);
    let client_id = app.cfg.discord_client_id.as_deref().unwrap_or_default();
    let location = authorize_url(client_id, &redirect_uri, &state, &challenge);
    let mut response = web::redirect(&location);
    // state を発行元ブラウザへ束縛する（ログイン CSRF 対策）。
    web::append_cookie(&mut response, state_cookie(&state, !app.cfg.insecure_http));
    response
}

/// GET /oauth/discord/callback。
pub async fn callback_request(
    app: Arc<crate::App>,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
) -> Response {
    if !app.cfg.discord_enabled() {
        return not_configured_page();
    }
    if let Some(error) = query.get("error") {
        tracing::warn!(
            "Discord 認可が拒否されました: {}",
            truncate_chars(error, 200)
        );
        return denied_page();
    }
    let Some(state_value) = query
        .get("state")
        .map(String::as_str)
        .filter(|value| !value.is_empty())
    else {
        return invalid_state_page();
    };
    let Some(code) = query
        .get("code")
        .map(String::as_str)
        .filter(|value| !value.is_empty())
    else {
        return invalid_state_page();
    };

    // state の失敗が続く場合はレート制限する（IP が分かるときだけ）。
    let client_ip = auth::client_ip(headers, &app.cfg);
    if !client_ip.is_empty()
        && auth::throttle_blocked(
            "oauth_callback",
            &client_ip,
            auth::OAUTH_CALLBACK_MAX_FAILURES,
            auth::OAUTH_CALLBACK_FAILURE_WINDOW,
        )
    {
        return web::too_many_requests_page();
    }

    // state は発行元のブラウザにだけ有効（ログイン CSRF 対策）。
    // 攻撃者が用意した callback URL を被害者に踏ませても、Cookie が無いため拒否する。
    let state_bound = auth::cookie_value(headers, STATE_COOKIE)
        .map(|value| {
            value.len() == state_value.len()
                && gpupod_common::token::digest_equals(&value, state_value)
        })
        .unwrap_or(false);
    if !state_bound {
        tracing::warn!("discord oauth: state がブラウザの Cookie と一致しません");
        if !client_ip.is_empty() {
            auth::throttle_hit(
                "oauth_callback",
                &client_ip,
                auth::OAUTH_CALLBACK_MAX_FAILURES,
                auth::OAUTH_CALLBACK_FAILURE_WINDOW,
            );
        }
        return invalid_state_page();
    }

    // state は単回使用（取り出しと同時に削除する）。
    let stored = {
        let conn = app.db.lock();
        match models::take_oauth_state(&conn, state_value) {
            Ok(value) => value,
            Err(exc) => {
                tracing::error!("discord oauth の state 取得に失敗しました: {exc}");
                return internal_error_page();
            }
        }
    };
    let Some(stored) = stored else {
        return invalid_state_page();
    };
    let expired = match parse_dt(&stored.expires_at) {
        Some(expires) => expires <= now(),
        None => true,
    };
    if expired {
        return invalid_state_page();
    }

    let redirect_uri = redirect_uri(&app.cfg, headers);
    let client_id = app.cfg.discord_client_id.clone().unwrap_or_default();
    let client_secret = app.cfg.discord_client_secret.clone().unwrap_or_default();
    let code = code.to_string();
    let verifier = stored.code_verifier.clone();
    let access_token = match api_call(app.discord.clone(), move |api| {
        api.exchange_code(&client_id, &client_secret, &code, &redirect_uri, &verifier)
    })
    .await
    {
        Ok(token) => token,
        Err(message) => {
            tracing::warn!("discord oauth のトークン交換に失敗しました: {message}");
            return upstream_error_page();
        }
    };

    let identity = {
        let token = access_token.clone();
        match api_call(app.discord.clone(), move |api| api.fetch_user(&token)).await {
            Ok(user) => user,
            Err(message) => {
                tracing::warn!("discord oauth のユーザー取得に失敗しました: {message}");
                return upstream_error_page();
            }
        }
    };

    // サーバー所属の確認。
    if let Some(guild_id) = app.cfg.discord_guild_id.clone() {
        let token = access_token.clone();
        let guilds = match api_call(app.discord.clone(), move |api| api.list_guilds(&token)).await {
            Ok(guilds) => guilds,
            Err(message) => {
                tracing::warn!("discord oauth のサーバー一覧取得に失敗しました: {message}");
                return upstream_error_page();
            }
        };
        if !guilds.iter().any(|id| id == &guild_id) {
            tracing::info!("discord oauth: サーバー未所属のため拒否しました");
            return guild_required_page(&app);
        }
    }

    // ロールの確認（サーバー ID が無いと確認できないため拒否する）。
    if !app.cfg.discord_allowed_role_ids.is_empty() {
        let Some(guild_id) = app.cfg.discord_guild_id.clone() else {
            tracing::error!(
                "discord oauth: GPUPOD_DISCORD_GUILD_ID が無いためロールを確認できません"
            );
            return role_required_page();
        };
        let token = access_token.clone();
        let roles = match api_call(app.discord.clone(), move |api| {
            api.guild_member_roles(&token, &guild_id)
        })
        .await
        {
            Ok(value) => value,
            Err(message) => {
                tracing::warn!("discord oauth のメンバー情報取得に失敗しました: {message}");
                return upstream_error_page();
            }
        };
        let Some(roles) = roles else {
            return guild_required_page(&app);
        };
        let allowed = app
            .cfg
            .discord_allowed_role_ids
            .iter()
            .any(|allowed_id| roles.iter().any(|role| role == allowed_id));
        if !allowed {
            tracing::info!("discord oauth: 許可ロール未保持のため拒否しました");
            return role_required_page();
        }
    }

    // ユーザーの特定（既存の紐付け → ログイン中の連携 → 新規作成）。
    let resolution = {
        let mut guard = app.db.lock();
        let tx = match crate::db::immediate(&mut guard) {
            Ok(tx) => tx,
            Err(exc) => {
                tracing::error!("discord oauth のトランザクション開始に失敗しました: {exc}");
                return internal_error_page();
            }
        };
        match resolve_identity(&tx, &identity, stored.link_user_id) {
            Ok(user_id) => match tx.commit() {
                Ok(()) => Ok(user_id),
                Err(exc) => {
                    tracing::error!("discord oauth のコミットに失敗しました: {exc}");
                    Err(LinkError::Database)
                }
            },
            Err(error) => Err(error),
        }
    };
    let user_id = match resolution {
        Ok(user_id) => user_id,
        Err(LinkError::Conflict) => return conflict_page(),
        Err(LinkError::Inactive) => return inactive_page(),
        Err(LinkError::Database) => return internal_error_page(),
    };

    let user = {
        let conn = app.db.lock();
        models::find_user(&conn, user_id).ok().flatten()
    };
    let Some(user) = user else {
        return internal_error_page();
    };
    if !user.is_active {
        return inactive_page();
    }
    login_response(&app, headers, user.id)
}

/// POST /oauth/discord/unlink/（ログイン中の連携解除）。
pub fn unlink_request(
    app: &crate::App,
    headers: &HeaderMap,
    form: &HashMap<String, String>,
) -> Response {
    let (session, user) = match web::require_user(app, headers, "/account/") {
        Ok(value) => value,
        Err(response) => return response,
    };
    if let Err(response) = web::check_csrf(app, headers, form) {
        return response;
    }
    if !auth::has_usable_password(&user.password) {
        auth::push_message(
            &app.db,
            &session,
            "error",
            "パスワードが設定されていないため、Discord 連携を解除できません。先にパスワードを設定してください。",
        );
        return web::redirect("/account/");
    }
    let removed = {
        let conn = app.db.lock();
        models::delete_oauth_identity_for_user(&conn, PROVIDER, user.id)
    };
    match removed {
        Ok(count) if count > 0 => {
            auth::push_message(&app.db, &session, "success", "Discord 連携を解除しました。");
        }
        Ok(_) => {
            auth::push_message(&app.db, &session, "error", "Discord と連携していません。");
        }
        Err(exc) => {
            tracing::error!("discord 連携の解除に失敗しました: {exc}");
            auth::push_message(
                &app.db,
                &session,
                "error",
                "連携の解除に失敗しました。時間をおいて再度お試しください。",
            );
        }
    }
    web::redirect("/account/")
}

/// GET /oauth/discord/start。
pub async fn start(State(app): State<Arc<crate::App>>, headers: HeaderMap) -> Response {
    start_request(app, &headers).await
}

/// GET /oauth/discord/callback。
pub async fn callback(
    State(app): State<Arc<crate::App>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    callback_request(app, &headers, &query).await
}

/// POST /oauth/discord/unlink/。
pub async fn unlink(
    State(app): State<Arc<crate::App>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    unlink_request(&app, &headers, &form)
}
