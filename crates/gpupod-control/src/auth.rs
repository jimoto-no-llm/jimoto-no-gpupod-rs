//! 認証・セッション・CSRF（Django の auth / sessions / CsrfViewMiddleware の移植）。
//!
//! パスワードは Django 形式の PBKDF2-SHA256
//! （"pbkdf2_sha256$600000$salt$hash"）で検証する。

use std::sync::Arc;

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
    let mut cookie = format!("csrftoken={}; Path=/; Max-Age={}; SameSite=Lax", token, SESSION_COOKIE_AGE);
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
    let _ = models::write_session(&conn, key, &serde_json::to_string(data).unwrap_or_default(), &expire);
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
        data: SessionData { user_id, csrf: new_csrf_token(), messages: Vec::new() },
    };
    store_session(db, &session.key, &session.data);
    session
}

/// フラッシュメッセージを追加する。
pub fn push_message(db: &crate::db::SharedDb, session: &Session, tags: &str, text: impl Into<String>) {
    let mut data = session.data.clone();
    data.messages.push(Flash { tags: tags.to_string(), text: text.into() });
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
    models::find_user(&conn, session.data.user_id).ok().flatten()
}

/// リクエストの Origin / Referer 検証（Django の _origin_verified 相当）。
fn origin_allowed(headers: &HeaderMap, cfg: &Config, origin: &str) -> bool {
    if cfg.csrf_origins.iter().any(|item| item == origin) {
        return true;
    }
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if host.is_empty() {
        return false;
    }
    let scheme = request_scheme(headers, cfg);
    origin.trim_end_matches('/') == format!("{}://{}", scheme, host)
}

/// リクエストのスケーム（GPUPOD_TRUST_PROXY で X-Forwarded-Proto を信頼）。
pub fn request_scheme(headers: &HeaderMap, cfg: &Config) -> String {
    if cfg.trust_proxy {
        if let Some(value) = headers.get("x-forwarded-proto").and_then(|value| value.to_str().ok()) {
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
    if let Some(origin) = headers.get(axum::http::header::ORIGIN).and_then(|value| value.to_str().ok()) {
        if !origin_allowed(headers, cfg, origin) {
            return Err(());
        }
    } else if let Some(referer) = headers.get(axum::http::header::REFERER).and_then(|value| value.to_str().ok()) {
        if request_scheme(headers, cfg) == "https" && !origin_allowed(headers, cfg, referer) {
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

/// ログイン処理（authenticate + セッション作成）。失敗時は None。
pub fn login(db: &crate::db::SharedDb, username: &str, password: &str) -> Option<Session> {
    let user = {
        let conn = db.lock();
        models::find_user_by_username(&conn, username).ok().flatten()?
    };
    if !user.is_active || !verify_password(password, &user.password) {
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
