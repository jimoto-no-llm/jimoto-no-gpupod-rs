//! 提供PC向け API（portal/views.py の node_* と portal/benchmarks.py）。
//!
//! これらの経路は CSRF 検証の対象外（Django の csrf_exempt 相当）。

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use serde_json::Value;

use crate::benchmarks::{self, MAX_NETWORK_BYTES, MIB};
use crate::jsonfmt::django_json;
use crate::models;
use crate::services::{self, ServiceError};

/// 同期 API のボディ上限（スナップショット + 結果）。
const SYNC_BODY_LIMIT: usize = 32 * 1024 * 1024;
/// 鍵交換 API のボディ上限。中身は id と鍵だけなので小さく保つ。
const ROTATION_BODY_LIMIT: usize = 8 * 1024;
/// ボディ読取の期限（遅い接続で作業スレッドを占有させない）。
const SYNC_BODY_DEADLINE: Duration = Duration::from_secs(60);
/// アップロードの読取期限（Python 版の deadline と同じ 60 秒）。
const UPLOAD_DEADLINE: Duration = Duration::from_secs(60);
/// 機材キーの最大長（受信側でハッシュ計算を膨らませない）。
const MAX_TOKEN_BYTES: usize = 512;

fn json_response(status: StatusCode, value: &Value, no_store: bool) -> Response {
    let mut response = Response::new(axum::body::Body::from(django_json(value)));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    if no_store {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    }
    response
}

fn error_response(status: StatusCode, message: &str) -> Response {
    json_response(status, &serde_json::json!({"error": message}), true)
}

/// Bearer トークンを取り出す。
///
/// スキーム名は大文字小文字を区別しない（RFC 7235）。空や過大な値は
/// 空トークンとして扱い、認証側で拒否させる。
fn bearer(headers: &HeaderMap) -> String {
    let raw = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let mut parts = raw.splitn(2, ' ');
    let (Some(scheme), Some(token)) = (parts.next(), parts.next()) else {
        return String::new();
    };
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return String::new();
    }
    let token = token.trim();
    if token.len() > MAX_TOKEN_BYTES {
        return String::new();
    }
    token.to_owned()
}

/// POST /api/nodes/<id>/sync/ （node_sync）
pub async fn node_sync(
    State(app): State<Arc<crate::App>>,
    Path(node_id): Path<String>,
    request: Request,
) -> Response {
    let Some(node_hex) = models::uuid_to_hex(&node_id) else {
        return error_response(StatusCode::FORBIDDEN, "Invalid node credentials");
    };
    let token = bearer(request.headers());
    let node = {
        let conn = app.db.lock();
        match services::authenticate_node(&conn, &node_hex, &token, true) {
            Some(node) => node,
            None => return error_response(StatusCode::FORBIDDEN, "Invalid node credentials"),
        }
    };

    let body = match tokio::time::timeout(
        SYNC_BODY_DEADLINE,
        axum::body::to_bytes(request.into_body(), SYNC_BODY_LIMIT),
    )
    .await
    {
        Ok(Ok(body)) => body,
        _ => return error_response(StatusCode::BAD_REQUEST, "Invalid payload"),
    };
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "Invalid payload"),
    };
    match services::sync_node(&app.db, &node, &payload) {
        Ok(result) => json_response(
            StatusCode::OK,
            &serde_json::to_value(&result).unwrap_or(Value::Null),
            true,
        ),
        Err(ServiceError::Agent(message)) => error_response(StatusCode::BAD_REQUEST, &message),
        Err(ServiceError::Payload) => error_response(StatusCode::BAD_REQUEST, "Invalid payload"),
        Err(ServiceError::Generic) => error_response(StatusCode::BAD_REQUEST, "Invalid payload"),
        Err(ServiceError::Db(message)) => {
            tracing::error!("sync failed: {message}");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "Invalid payload")
        }
    }
}

/// POST /api/nodes/<id>/token/prepare/ （node_token_prepare）
pub async fn node_token_prepare(
    State(app): State<Arc<crate::App>>,
    Path(node_id): Path<String>,
    request: Request,
) -> Response {
    let Some(node_hex) = models::uuid_to_hex(&node_id) else {
        return error_response(StatusCode::FORBIDDEN, "Invalid node credential");
    };
    let token = bearer(request.headers());
    let node = {
        let conn = app.db.lock();
        match services::authenticate_node(&conn, &node_hex, &token, false) {
            Some(node) => node,
            None => return error_response(StatusCode::FORBIDDEN, "Invalid node credential"),
        }
    };
    let body = match axum::body::to_bytes(request.into_body(), ROTATION_BODY_LIMIT).await {
        Ok(body) => body,
        Err(_) => {
            return error_response(StatusCode::CONFLICT, "Credential rotation was not accepted")
        }
    };
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => {
            return error_response(StatusCode::CONFLICT, "Credential rotation was not accepted")
        }
    };
    let rotation = payload
        .get("id")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let replacement = payload
        .get("token")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let conn = app.db.lock();
    match services::prepare_rotation(&conn, &node, rotation, replacement) {
        Ok(result) => json_response(StatusCode::OK, &result, true),
        Err(_) => error_response(StatusCode::CONFLICT, "Credential rotation was not accepted"),
    }
}

/// POST /api/nodes/<id>/token/commit/ （node_token_commit）
pub async fn node_token_commit(
    State(app): State<Arc<crate::App>>,
    Path(node_id): Path<String>,
    request: Request,
) -> Response {
    let node_hex = match models::uuid_to_hex(&node_id) {
        Some(hex) => hex,
        None => {
            return error_response(
                StatusCode::FORBIDDEN,
                "Credential rotation was not confirmed",
            )
        }
    };
    let token = bearer(request.headers());
    let body = match axum::body::to_bytes(request.into_body(), ROTATION_BODY_LIMIT).await {
        Ok(body) => body,
        Err(_) => {
            return error_response(
                StatusCode::FORBIDDEN,
                "Credential rotation was not confirmed",
            )
        }
    };
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => {
            return error_response(
                StatusCode::FORBIDDEN,
                "Credential rotation was not confirmed",
            )
        }
    };
    let rotation = payload
        .get("id")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let conn = app.db.lock();
    match services::commit_rotation(&conn, &node_hex, rotation, &token) {
        Ok(result) => json_response(StatusCode::OK, &result, true),
        Err(_) => error_response(
            StatusCode::FORBIDDEN,
            "Credential rotation was not confirmed",
        ),
    }
}

/// GET/POST /api/nodes/<id>/benchmark/ （node_benchmark）
pub async fn node_benchmark(
    State(app): State<Arc<crate::App>>,
    Path(node_id): Path<String>,
    query: Query<std::collections::HashMap<String, String>>,
    request: Request,
) -> Response {
    let download = request.method() == axum::http::Method::GET;
    let Some(node_hex) = models::uuid_to_hex(&node_id) else {
        return error_response(StatusCode::FORBIDDEN, "Invalid node credentials");
    };
    let token = bearer(request.headers());
    let node = {
        let conn = app.db.lock();
        match services::authenticate_node(&conn, &node_hex, &token, true) {
            Some(node) => node,
            None => return error_response(StatusCode::FORBIDDEN, "Invalid node credentials"),
        }
    };

    let raw_size = if download {
        query.get("size").cloned().unwrap_or_default()
    } else {
        request
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let size: u64 = match raw_size.parse() {
        Ok(value) if (MIB..=MAX_NETWORK_BYTES).contains(&value) => value,
        _ => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "Sample must have a length between 1 and 64 MiB",
            )
        }
    };

    if !download {
        let content_type = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if !content_type.starts_with("application/octet-stream")
            || request.headers().contains_key("content-encoding")
        {
            return error_response(
                StatusCode::BAD_REQUEST,
                "Upload must be uncompressed application/octet-stream",
            );
        }
    }

    let claimed = {
        let conn = app.db.lock();
        benchmarks::claim(
            &conn,
            &node.id,
            if download { "download" } else { "upload" },
        )
    };
    if claimed != Ok(true) {
        let mut response = error_response(StatusCode::TOO_MANY_REQUESTS, "Measurement cooldown");
        response.headers_mut().insert(
            header::RETRY_AFTER,
            crate::config::BENCHMARK_COOLDOWN_SECONDS
                .to_string()
                .parse()
                .unwrap(),
        );
        return response;
    }

    if download {
        let block: Bytes = {
            use rand::RngCore;
            let mut buffer = vec![0u8; 65536];
            rand::thread_rng().fill_bytes(&mut buffer);
            Bytes::from(buffer)
        };
        let mut payload = Vec::with_capacity(size as usize);
        while (payload.len() as u64) < size {
            let remaining = (size - payload.len() as u64) as usize;
            let take = remaining.min(block.len());
            payload.extend_from_slice(&block[..take]);
        }
        let mut response = Response::new(axum::body::Body::from(payload));
        *response.status_mut() = StatusCode::OK;
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            "application/octet-stream".parse().unwrap(),
        );
        response
            .headers_mut()
            .insert(header::CONTENT_LENGTH, size.to_string().parse().unwrap());
        response
            .headers_mut()
            .insert("x-accel-buffering", "no".parse().unwrap());
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            "no-store, no-transform".parse().unwrap(),
        );
        return response;
    }

    // アップロード: Python 版と同じく size バイトだけ読み切ることを確認する。
    let body = request.into_body();
    let bytes = match tokio::time::timeout(
        UPLOAD_DEADLINE,
        axum::body::to_bytes(body, size as usize),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        _ => return error_response(StatusCode::BAD_REQUEST, "Incomplete or timed-out sample"),
    };
    if bytes.len() as u64 != size {
        return error_response(StatusCode::BAD_REQUEST, "Incomplete or timed-out sample");
    }
    let mut response = json_response(
        StatusCode::OK,
        &serde_json::json!({"received_bytes": bytes.len()}),
        false,
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-store, no-transform".parse().unwrap(),
    );
    response
}

// ---------------------------------------------------------------------------
// 公開在庫 API（GET /api/public/inventory）
//
// 認証・CSRF の対象外。IP ベースの簡易レート制限（1 秒あたり 10 リクエスト、
// メモリ内・単一プロセス前提）をかけ、レスポンスは公開してよい情報だけを
// 明示的に組み立てる（IP・ノード ID・トークン・テナント情報は載せない）。
// ---------------------------------------------------------------------------

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// レート制限の窓（1 秒）。
const PUBLIC_RATE_WINDOW: Duration = Duration::from_secs(1);
/// 窓あたりの許可数（10 リクエスト/IP/秒）。
const PUBLIC_RATE_MAX: u32 = 10;
/// 期限切れの鍵を掃くしきい値（これだけ溜まったら一斉に掃く）。
const PUBLIC_RATE_PRUNE_KEYS: usize = 4096;
/// 上限到達時に案内する再試行までの秒数。
const PUBLIC_RATE_RETRY_AFTER: &str = "1";

/// 鍵ごとに直近リクエスト時刻を覚えるスライディングウィンドウ。
pub struct RateLimiter {
    window: Duration,
    max: u32,
    hits: Mutex<HashMap<String, Vec<Instant>>>,
}

impl RateLimiter {
    pub fn new(window: Duration, max: u32) -> Self {
        RateLimiter {
            window,
            max,
            hits: Mutex::new(HashMap::new()),
        }
    }

    /// この鍵のリクエストを通すか。通すときは時刻を記録する。
    pub fn allow(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut map = self.hits.lock().expect("rate limiter poisoned");
        // キー数がしきい値を越えたときだけ期限切れの鍵を掃く（正常系は O(1)）。
        if map.len() > PUBLIC_RATE_PRUNE_KEYS {
            map.retain(|_, times| {
                times
                    .last()
                    .is_some_and(|last| now.duration_since(*last) < self.window)
            });
        }
        let times = map.entry(key.to_owned()).or_default();
        times.retain(|t| now.duration_since(*t) < self.window);
        if times.len() >= self.max as usize {
            return false;
        }
        times.push(now);
        true
    }
}

/// 公開在庫のレート制限（プロセス内で共有）。
static PUBLIC_INVENTORY_RATE: LazyLock<RateLimiter> =
    LazyLock::new(|| RateLimiter::new(PUBLIC_RATE_WINDOW, PUBLIC_RATE_MAX));

/// レート制限の鍵（クライアント IP）。
///
/// リバースプロキシの後ろの運用を前提に、プロキシが付与する
/// X-Forwarded-For（先頭）・X-Real-IP を鍵に使う。どちらも無い接続
/// （直アクセス・テスト）は共有の "local" 鍵にまとめる。
fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get(header::HeaderName::from_static("x-forwarded-for"))
        .and_then(|value| value.to_str().ok())
        .and_then(|list| list.split(',').next())
        .map(|ip| ip.trim().to_owned())
        .filter(|ip| !ip.is_empty())
        .or_else(|| {
            headers
                .get(header::HeaderName::from_static("x-real-ip"))
                .and_then(|value| value.to_str().ok())
                .map(|ip| ip.trim().to_owned())
                .filter(|ip| !ip.is_empty())
        })
        .unwrap_or_else(|| "local".to_owned())
}

/// 上限に達したときの 429 応答。
fn rate_limited_response() -> Response {
    let mut response = error_response(StatusCode::TOO_MANY_REQUESTS, "Too many requests");
    response.headers_mut().insert(
        header::RETRY_AFTER,
        PUBLIC_RATE_RETRY_AFTER.parse().unwrap(),
    );
    response
}

/// GET /api/public/inventory （認証不要・CSRF 不要・公開情報だけ）。
pub async fn public_inventory(State(app): State<Arc<crate::App>>, headers: HeaderMap) -> Response {
    if !PUBLIC_INVENTORY_RATE.allow(&client_ip(&headers)) {
        return rate_limited_response();
    }
    // DB の読み取りはブロッキングのためワーカースレッドを占有しない場所へ。
    let (stats, gpus): (crate::site::InventoryStats, Vec<crate::site::PublicGpu>) =
        tokio::task::spawn_blocking({
            let app = app.clone();
            move || crate::site::collect_inventory(&app)
        })
        .await
        .unwrap_or_else(|exc| {
            tracing::warn!("inventory aggregation failed: {exc}");
            (Default::default(), Vec::new())
        });
    let body = serde_json::json!({
        "providers": stats.providers,
        "offered_gpus": stats.offered_gpus,
        "available_gpus": stats.available_gpus,
        "gpus": gpus.iter()
            .map(|gpu| serde_json::json!({
                "name": gpu.name,
                "vendor": gpu.vendor_label,
                "vram": gpu.vram,
                "provider": gpu.provider,
                "status": gpu.status,
                "available": gpu.available,
            }))
            .collect::<Vec<_>>(),
    });
    let mut response = json_response(StatusCode::OK, &body, false);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "public, max-age=5".parse().unwrap());
    response
}
