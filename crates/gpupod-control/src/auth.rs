//! 認証・セッション・CSRF（Django の auth / sessions / CsrfViewMiddleware の移植）。
//!
//! パスワードは Django 形式の PBKDF2-SHA256
//! （"pbkdf2_sha256$600000$salt$hash"）で検証する。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::config::{Config, SESSION_COOKIE_AGE};
use crate::db::{fmt_dt, now};
use crate::models::{self, SessionRow};

/// Django 5.2 の既定 iterations。
pub const ITERATIONS: u32 = 600_000;

/// Django の get_random_string と同じ文字集合。
const RANDOM_CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// Django の get_random_string 相当（salt・トークン生成）。
fn random_string(length: usize) -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..length)
        .map(|_| RANDOM_CHARS[rng.gen_range(0..RANDOM_CHARS.len())] as char)
        .collect()
}

/// PBKDF2-HMAC-SHA256。
fn pbkdf2(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut output = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password, salt, iterations, &mut output);
    output
}

/// Django 形式のエンコードを生成する（make_password 相当）。
pub fn hash_password(password: &str) -> String {
    let salt = random_string(12);
    let derived = pbkdf2(password.as_bytes(), salt.as_bytes(), ITERATIONS);
    let checksum = base64::engine::general_purpose::STANDARD_NO_PAD.encode(derived);
    let mut encoded = String::from("pbkdf2_sha256$");
    encoded.push_str(&ITERATIONS.to_string());
    encoded.push('$');
    encoded.push_str(&salt);
    encoded.push('$');
    encoded.push_str(&checksum);
    encoded
}

/// Django 形式のパスワード検証（verify_password 相当）。
pub fn verify_password(password: &str, encoded: &str) -> bool {
    let parts: Vec<&str> = encoded.split('$').collect();
    if parts.len() != 4 || parts[0] != "pbkdf2_sha256" {
        return false;
    }
    let iterations = match parts[1].parse::<u32>() {
        Ok(value) if value > 0 && value <= 10_000_000 => value,
        _ => return false,
    };
    let expected = match decode_base64_padded(parts[3]) {
        Some(bytes) if bytes.len() == 32 => bytes,
        _ => return false,
    };
    let derived = pbkdf2(password.as_bytes(), parts[2].as_bytes(), iterations);
    let mut matched = true;
    for (left, right) in derived.iter().zip(expected.iter()) {
        matched &= left == right;
    }
    matched
}

fn decode_base64_padded(value: &str) -> Option<Vec<u8>> {
    let mut text = value.to_string();
    while text.len() % 4 != 0 {
        text.push('=');
    }
    base64::engine::general_purpose::STANDARD.decode(text).ok()
}

/// フラッシュメッセージ（django.contrib.messages）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Flash {
    /// テンプレートの class 名（error / success）。
    pub tags: String,
    pub text: String,
}

/// セッションの内容（django_session.session_data に JSON で保存）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionData {
    pub user_id: i64,
    pub csrf: String,
    #[serde(default)]
    pub messages: Vec<Flash>,
}

/// 読み込み済みセッション。
#[derive(Debug, Clone)]
pub struct Session {
    pub key: String,
    pub data: SessionData,
}

/// 新しいセッション鍵（Django の SESSION_KEY_CHARS）。
pub fn new_session_key() -> String {
    random_string(32)
}

/// 新しい CSRF トークン（Django の _get_new_csrf_token）。
pub fn new_csrf_token() -> String {
    random_string(32)
}

/// Cookie ヘッダーから値を取り出す。
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some((key, value)) = part.split_once('=') {
            if key.trim() == name {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

/// セッション Cookie（sessionid、HttpOnly・SameSite=Lax・12 時間）。
pub fn session_cookie(key: &str, secure: bool) -> String {
    let mut cookie = format!(
        "sessionid={}; Path=/; Max-Age={}; HttpOnly; SameSite=Lax",
        key, SESSION_COOKIE_AGE
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// ログアウト時のセッション Cookie 削除。
pub fn clear_session_cookie(secure: bool) -> String {
    let mut cookie = String::from("sessionid=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// CSRF Cookie（Django は HttpOnly にしない）。
pub fn csrf_cookie(token: &str, secure: bool) -> String {
    let mut cookie = format!(
        "csrftoken={}; Path=/; Max-Age={}; SameSite=Lax",
        token, SESSION_COOKIE_AGE
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// セッションを読み込む（有効期限切れは無視）。
pub fn load_session(db: &crate::db::SharedDb, headers: &HeaderMap) -> Option<Session> {
    let key = cookie_value(headers, "sessionid")?;
    if key.is_empty() {
        return None;
    }
    let conn = db.lock();
    let row: SessionRow = models::get_session(&conn, &key).ok().flatten()?;
    let expired = match crate::db::parse_dt(&row.expire_date) {
        Some(expire) => expire <= now(),
        None => true,
    };
    if expired {
        return None;
    }
    let data: SessionData = serde_json::from_str(&row.session_data).ok()?;
    Some(Session { key, data })
}

/// セッションを書き込む。
pub fn store_session(db: &crate::db::SharedDb, key: &str, data: &SessionData) {
    let conn = db.lock();
    let expire = fmt_dt(now() + chrono::Duration::seconds(SESSION_COOKIE_AGE));
    let _ = models::write_session(
        &conn,
        key,
        &serde_json::to_string(data).unwrap_or_default(),
        &expire,
    );
}

/// セッションを削除する。
pub fn drop_session(db: &crate::db::SharedDb, key: &str) {
    let conn = db.lock();
    let _ = models::delete_session(&conn, key);
}

/// 新規セッションを作る。
pub fn create_session(db: &crate::db::SharedDb, user_id: i64) -> Session {
    let session = Session {
        key: new_session_key(),
        data: SessionData {
            user_id,
            csrf: new_csrf_token(),
            messages: Vec::new(),
        },
    };
    store_session(db, &session.key, &session.data);
    session
}

/// ログインに使えないパスワード（Django の set_unusable_password 相当）。
///
/// `!` で始まる値は Django の has_usable_password() が false を返す。
pub fn unusable_password() -> String {
    format!("!{}", random_string(40))
}

/// パスワードが設定されているか（Django の has_usable_password 相当）。
pub fn has_usable_password(encoded: &str) -> bool {
    !encoded.is_empty() && !encoded.starts_with('!')
}

/// 指定ユーザーでセッションを作り直す（OAuth ログイン用）。
pub fn login_user(db: &crate::db::SharedDb, user_id: i64) -> Session {
    let session = create_session(db, user_id);
    let conn = db.lock();
    let _ = conn.execute(
        "UPDATE auth_user SET last_login = ?2 WHERE id = ?1",
        rusqlite::params![user_id, fmt_dt(now())],
    );
    session
}

/// フラッシュメッセージを追加する。
pub fn push_message(
    db: &crate::db::SharedDb,
    session: &Session,
    tags: &str,
    text: impl Into<String>,
) {
    let mut data = session.data.clone();
    data.messages.push(Flash {
        tags: tags.to_string(),
        text: text.into(),
    });
    store_session(db, &session.key, &data);
}

/// メッセージを取り出す（Django と同じく一度だけ表示される）。
pub fn take_messages(db: &crate::db::SharedDb, session: &Session) -> Vec<Flash> {
    let messages = session.data.messages.clone();
    if !messages.is_empty() {
        let mut data = session.data.clone();
        data.messages.clear();
        store_session(db, &session.key, &data);
    }
    messages
}

/// 現在のユーザーを取得する。
pub fn current_user(db: &crate::db::SharedDb, session: &Session) -> Option<models::UserRow> {
    let conn = db.lock();
    models::find_user(&conn, session.data.user_id)
        .ok()
        .flatten()
}

/// "scheme://authority/path..." から scheme と authority を取り出す（Origin / Referer 用）。
fn split_origin(value: &str) -> Option<(String, String)> {
    let (scheme, rest) = value.trim().split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // 認証情報つき（user@host）や空の authority は受けない。
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    Some((scheme, authority.to_ascii_lowercase()))
}

/// authority（host[:port]）を host と port に分ける（IPv6 の [::1]:8080 にも対応）。
fn split_authority(value: &str) -> (String, Option<String>) {
    if let Some(rest) = value.strip_prefix('[') {
        if let Some((head, tail)) = rest.split_once(']') {
            let port = tail.strip_prefix(':').map(str::to_string);
            return (format!("[{head}]"), port);
        }
        return (value.to_string(), None);
    }
    match value.rsplit_once(':') {
        Some((head, tail))
            if !tail.is_empty() && tail.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            (head.to_string(), Some(tail.to_string()))
        }
        _ => (value.to_string(), None),
    }
}

/// 同一の authority か（既定ポートの明示は同一視する）。
fn authority_matches(left: &str, right: &str) -> bool {
    let (left_host, left_port) = split_authority(left);
    let (right_host, right_port) = split_authority(right);
    if left_host != right_host {
        return false;
    }
    let default_port = |port: &str| port == "80" || port == "443";
    match (left_port, right_port) {
        (Some(a), Some(b)) => a == b,
        (Some(a), None) | (None, Some(a)) => default_port(&a),
        (None, None) => true,
    }
}

/// リクエストの Origin / Referer 検証（Django の _origin_verified 相当）。
///
/// Referer は URL 全体で届くため、scheme と authority だけを取り出して Host と比べる。
fn origin_allowed(headers: &HeaderMap, cfg: &Config, origin: &str) -> bool {
    let origin = origin.trim();
    if cfg
        .csrf_origins
        .iter()
        .any(|item| item.trim().eq_ignore_ascii_case(origin))
    {
        return true;
    }
    let Some((_scheme, authority)) = split_origin(origin) else {
        return false;
    };
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    authority_matches(&authority, &host)
}

/// リクエストのスケーム（GPUPOD_TRUST_PROXY で X-Forwarded-Proto を信頼）。
pub fn request_scheme(headers: &HeaderMap, cfg: &Config) -> String {
    if cfg.trust_proxy {
        if let Some(value) = headers
            .get("x-forwarded-proto")
            .and_then(|value| value.to_str().ok())
        {
            let first = value.split(',').next().unwrap_or("").trim();
            if !first.is_empty() {
                return first.to_string();
            }
        }
    }
    "http".to_string()
}

/// ダブルサブミット + Origin 検証（POST のみ、対象外パスは views が省略する）。
pub fn verify_csrf(headers: &HeaderMap, form_token: Option<&str>, cfg: &Config) -> Result<(), ()> {
    if let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    {
        if !origin_allowed(headers, cfg, origin) {
            return Err(());
        }
    } else if let Some(referer) = headers
        .get(axum::http::header::REFERER)
        .and_then(|value| value.to_str().ok())
    {
        // Origin が無い場合は Referer の scheme と authority を Host と比べる
        // （Referer は URL 全体なので、パスを取り除いてから比較する）。
        if !origin_allowed(headers, cfg, referer) {
            return Err(());
        }
    }
    let cookie = cookie_value(headers, "csrftoken").ok_or(())?;
    let form = form_token.ok_or(())?;
    if cookie.len() == form.len() && gpupod_common::token::digest_equals(&cookie, form) {
        Ok(())
    } else {
        Err(())
    }
}

// ---------------------------------------------------------------------------
// ブルートフォース対策（プロセス内の簡易レート制限）
// ---------------------------------------------------------------------------

/// レート制限テーブルの上限（超えたら全消ししてメモリを守る）。
const THROTTLE_MAX_ENTRIES: usize = 10_000;
/// ログイン失敗の上限（[LOGIN_FAILURE_WINDOW] あたり）。
pub const LOGIN_MAX_FAILURES: usize = 10;
/// ログイン失敗を数える期間。
pub const LOGIN_FAILURE_WINDOW: Duration = Duration::from_secs(600);
/// OAuth callback の失敗上限（IP が分かるときだけ使う）。
pub const OAUTH_CALLBACK_MAX_FAILURES: usize = 30;
/// OAuth callback の失敗を数える期間。
pub const OAUTH_CALLBACK_FAILURE_WINDOW: Duration = Duration::from_secs(300);

fn throttle_table() -> &'static Mutex<HashMap<String, Vec<Instant>>> {
    static TABLE: OnceLock<Mutex<HashMap<String, Vec<Instant>>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn throttle_lock() -> std::sync::MutexGuard<'static, HashMap<String, Vec<Instant>>> {
    match throttle_table().lock() {
        Ok(table) => table,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn prune_window(entries: &mut Vec<Instant>, now: Instant, window: Duration) {
    entries.retain(|stamp| now.duration_since(*stamp) <= window);
}

fn throttle_slot(bucket: &str, key: &str) -> String {
    format!("{bucket}:{key}")
}

/// 失敗を 1 件記録し、上限を超えたかを返す。
pub fn throttle_hit(bucket: &str, key: &str, limit: usize, window: Duration) -> bool {
    let now = Instant::now();
    let mut table = throttle_lock();
    if table.len() > THROTTLE_MAX_ENTRIES {
        table.clear();
    }
    let entries = table.entry(throttle_slot(bucket, key)).or_default();
    prune_window(entries, now, window);
    entries.push(now);
    entries.len() > limit
}

/// 失敗履歴が上限に達しているか（記録はしない）。
pub fn throttle_blocked(bucket: &str, key: &str, limit: usize, window: Duration) -> bool {
    let now = Instant::now();
    let mut table = throttle_lock();
    let entries = table.entry(throttle_slot(bucket, key)).or_default();
    prune_window(entries, now, window);
    entries.len() >= limit
}

/// 成功したときに失敗履歴を消す。
pub fn throttle_clear(bucket: &str, key: &str) {
    let mut table = throttle_lock();
    table.remove(&throttle_slot(bucket, key));
    if table.len() > THROTTLE_MAX_ENTRIES {
        table.clear();
    }
}

/// レート制限用のクライアント識別子（信頼するプロキシ設定のときだけ X-Forwarded-For を使う）。
pub fn client_ip(headers: &HeaderMap, cfg: &Config) -> String {
    if !cfg.trust_proxy {
        return String::new();
    }
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .split(',')
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// ログイン失敗のレート制限キー（IP が分かれば IP + ユーザー名、無ければユーザー名）。
pub fn login_throttle_key(headers: &HeaderMap, cfg: &Config, username: &str) -> String {
    let ip = client_ip(headers, cfg);
    let name = username.trim().to_ascii_lowercase();
    if ip.is_empty() {
        name
    } else {
        format!("{ip}|{name}")
    }
}

/// 存在しないユーザーでも同じ計算量にするためのダミーハッシュ。
fn dummy_password_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| hash_password("gpupod-dummy-password"))
}

/// ログイン処理（authenticate + セッション作成）。失敗時は None。
pub fn login(db: &crate::db::SharedDb, username: &str, password: &str) -> Option<Session> {
    let user = {
        let conn = db.lock();
        models::find_user_by_username(&conn, username)
            .ok()
            .flatten()
    };
    // ユーザーが存在しない・パスワード未設定でもダミー値を検証し、
    // 応答時間からアカウントの有無を推測されないようにする。
    let stored = match &user {
        Some(row) if has_usable_password(&row.password) => row.password.clone(),
        _ => dummy_password_hash().to_string(),
    };
    let matched = verify_password(password, &stored);
    let user = user?;
    if !matched || !user.is_active {
        return None;
    }
    let session = create_session(db, user.id);
    let conn = db.lock();
    let _ = conn.execute(
        "UPDATE auth_user SET last_login = ?2 WHERE id = ?1",
        rusqlite::params![user.id, fmt_dt(now())],
    );
    Some(session)
}

/// 権限チェック用の小さな構造体。
pub struct Identity {
    pub session: Session,
    pub user: models::UserRow,
}

impl Identity {
    pub fn staff(&self) -> bool {
        self.user.is_staff
    }
}

/// セッションとユーザーをまとめて読む。
pub fn identity(db: &crate::db::SharedDb, headers: &HeaderMap) -> Option<Identity> {
    let session = load_session(db, headers)?;
    let user = current_user(db, &session)?;
    if !user.is_active {
        return None;
    }
    Some(Identity { session, user })
}

/// 共有アプリからセッションを読むショートカット。
pub fn session_of(app: &Arc<crate::App>, headers: &HeaderMap) -> Option<Session> {
    load_session(&app.db, headers)
}
