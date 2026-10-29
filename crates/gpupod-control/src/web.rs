//! Web 層の共通部（ビュー型・応答ヘルパー・ルーター）。

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::routing::{any, get, post};
use axum::{middleware, Router};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::admin;
use crate::auth::{self, Flash, Session};
use crate::db::{fmt_dt, now};
use crate::fmt::{floatformat, gib, gib_label, py_float};
use crate::models::{self, LeaseRow, UserRow};
use crate::services::{Capacity, ServiceError};

/// ヘッダーの利用者表示。
#[derive(Debug, Clone)]
pub struct UserNav {
    pub is_authenticated: bool,
    pub is_staff: bool,
    pub username: String,
}

/// フラッシュメッセージ 1 件。
#[derive(Debug, Clone)]
pub struct FlashView {
    pub tags: String,
    pub text: String,
}

/// Flash → 表示。
pub fn flash_view(flash: Flash) -> FlashView {
    FlashView { tags: flash.tags, text: flash.text }
}

/// テンプレート候補 1 件。
#[derive(Debug, Clone)]
pub struct TemplateOption {
    pub id: String,
    pub name: String,
}

/// 資源の提供枠表示（resources.html）。
#[derive(Debug, Clone, Default)]
pub struct ResourcesView {
    pub error: bool,
    pub present: bool,
    pub memory_gib: String,
    pub disk_gib: String,
    pub allocated_memory_gib: String,
    pub allocated_disk_gib: String,
    pub available_memory_gib: String,
    pub available_disk_gib: String,
    pub available_memory_raw: String,
    pub available_disk_raw: String,
    pub default_memory_raw: String,
    pub default_disk_raw: String,
}

/// 測定結果の表示（benchmarks.html）。
#[derive(Debug, Clone, Default)]
pub struct BenchmarksView {
    pub present: bool,
    pub disk: Option<DiskView>,
    pub network: Option<NetworkView>,
}

/// ディスク測定の表示。
#[derive(Debug, Clone)]
pub struct DiskView {
    pub measured_at: String,
    pub size_mib: String,
    pub stale: bool,
    pub read_mib_s: String,
    pub write_mib_s: String,
}

/// 回線測定の表示。
#[derive(Debug, Clone)]
pub struct NetworkView {
    pub measured_at: String,
    pub size_mib: String,
    pub stale: bool,
    pub download_mbps: String,
    pub upload_mbps: String,
}

/// GPU ごとの貸出候補（dashboard.html の offer）。
#[derive(Debug, Clone)]
pub struct OfferView {
    pub node_pk: String,
    pub node_name: String,
    pub node_online: bool,
    pub direct_host: bool,
    pub gpu_id: String,
    pub gpu_name: String,
    pub gpu_vendor: String,
    pub gpu_memory_mib: i64,
    pub gpu_allocated: bool,
    pub available: bool,
    pub capacity_available: bool,
    pub has_resources: bool,
    pub resources: ResourcesView,
    pub benchmarks: BenchmarksView,
    pub templates: Vec<TemplateOption>,
}

/// ダッシュボードの貸出行。
#[derive(Debug, Clone)]
pub struct LeaseRowView {
    pub pk: String,
    pub node_name: String,
    pub template_name: String,
    pub status: String,
}

/// 管理パネルの貸出行。
#[derive(Debug, Clone)]
pub struct NodeLeaseView {
    pub pk: String,
    pub username: String,
    pub node_name: String,
    pub template_name: String,
    pub status: String,
}

/// 貸出の詳細表示。
#[derive(Debug, Clone)]
pub struct LeaseDetailView {
    pub pk: String,
    pub node_name: String,
    pub node_online: bool,
    pub template_name: String,
    pub gpu_id: String,
    pub has_resources: bool,
    pub memory_gib: String,
    pub disk_gib: String,
    pub status: String,
    pub has_error: bool,
    pub error: String,
    pub active: bool,
    pub show_connection: bool,
    pub connection_mode_display: String,
}

/// 接続先 1 件。
#[derive(Debug, Clone)]
pub struct RouteView {
    pub port: String,
    pub command: String,
}

/// 提供PC 1 台の表示（nodes.html の node）。
#[derive(Debug, Clone)]
pub struct NodeView {
    pub pk: String,
    pub name: String,
    pub online: bool,
    pub accepting: bool,
    pub last_seen: String,
    pub resources: ResourcesView,
    pub benchmarks: BenchmarksView,
    pub statistics: String,
    pub snapshot: String,
}

/// 提供枠の表示を組み立てる（Node.resources プロパティ相当）。
pub fn resources_view(capacity: &Result<Option<Capacity>, ServiceError>) -> ResourcesView {
    match capacity {
        Err(_) => ResourcesView { error: true, ..ResourcesView::default() },
        Ok(None) => ResourcesView::default(),
        Ok(Some(value)) => ResourcesView {
            present: true,
            error: false,
            memory_gib: gib_label(value.memory_bytes, 2),
            disk_gib: gib_label(value.disk_bytes, 2),
            allocated_memory_gib: gib_label(value.allocated_memory_bytes, 2),
            allocated_disk_gib: gib_label(value.allocated_disk_bytes, 2),
            available_memory_gib: gib_label(value.available_memory_bytes, 2),
            available_disk_gib: gib_label(value.available_disk_bytes, 2),
            available_memory_raw: py_float(gib(value.available_memory_bytes)),
            available_disk_raw: py_float(gib(value.available_disk_bytes)),
            default_memory_raw: py_float(gib(value.default_memory_bytes.min(value.available_memory_bytes))),
            default_disk_raw: py_float(gib(value.default_disk_bytes.min(value.available_disk_bytes))),
        },
    }
}

/// 測定結果の表示を組み立てる。
pub fn benchmarks_view(results: crate::benchmarks::PublicResults) -> BenchmarksView {
    let present = results.disk.is_some() || results.network.is_some();
    BenchmarksView {
        present,
        disk: results.disk.map(|item| DiskView {
            measured_at: item.measured_at,
            size_mib: py_float(item.size_mib),
            stale: item.stale,
            read_mib_s: floatformat(item.read_mib_s, 1),
            write_mib_s: floatformat(item.write_mib_s, 1),
        }),
        network: results.network.map(|item| NetworkView {
            measured_at: item.measured_at,
            size_mib: py_float(item.size_mib),
            stale: item.stale,
            download_mbps: floatformat(item.download_mbps, 1),
            upload_mbps: floatformat(item.upload_mbps, 1),
        }),
    }
}

/// ヘッダーの利用者欄。
pub fn user_nav(user: Option<&UserRow>) -> UserNav {
    match user {
        Some(user) => UserNav {
            is_authenticated: true,
            is_staff: user.is_staff,
            username: user.username.clone(),
        },
        None => UserNav { is_authenticated: false, is_staff: false, username: String::new() },
    }
}

/// HTML レスポンス。
pub fn html(body: String) -> Response {
    let mut response = Response::new(axum::body::Body::from(body));
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, "text/html; charset=utf-8".parse().unwrap());
    response
}

/// 302 リダイレクト。
pub fn redirect(location: &str) -> Response {
    let mut response = Response::new(axum::body::Body::empty());
    *response.status_mut() = StatusCode::FOUND;
    response.headers_mut().insert(header::LOCATION, location.parse().unwrap());
    response
}

/// Django 風のエラーページ。
pub fn error_page(status: StatusCode, title: &str, message: &str) -> Response {
    let body = format!(
        "<!doctype html><html lang=\"ja\"><head><meta charset=\"utf-8\"><title>{0}</title></head>\
         <body><h1>{0}</h1><p>{1}</p></body></html>",
        title, message
    );
    let mut response = html(body);
    *response.status_mut() = status;
    response
}

/// 404 ページ。
pub fn not_found_page() -> Response {
    error_page(
        StatusCode::NOT_FOUND,
        "ページが見つかりません（404）",
        "申し訳ありません。要求されたページは見つかりませんでした。",
    )
}

/// 403 ページ。
pub fn forbidden_page() -> Response {
    error_page(
        StatusCode::FORBIDDEN,
        "拒否されました（403）",
        "申し訳ありません。アクセスが許可されていません。",
    )
}

/// CSRF 検証失敗。
pub fn csrf_failed_page() -> Response {
    error_page(
        StatusCode::FORBIDDEN,
        "CSRF 検証に失敗しました",
        "リクエストを中止しました。ページを再読み込みしてから再度お試しください。",
    )
}

/// ログインページへの誘導。
pub fn login_redirect(next: &str) -> Response {
    redirect(&format!("/login/?next={next}"))
}

/// Cookie を 1 つ追記する。
pub fn append_cookie(response: &mut Response, value: String) {
    if let Ok(parsed) = value.parse::<HeaderValue>() {
        response.headers_mut().append(header::SET_COOKIE, parsed);
    }
}

/// ログイン済みページの応答（Vary: Cookie と CSRF Cookie の更新を含む）。
pub fn page_response(app: &crate::App, session: &Session, headers: &HeaderMap, body: String) -> Response {
    let mut response = html(body);
    response.headers_mut().insert(header::VARY, "Cookie".parse().unwrap());
    let secure = !app.cfg.insecure_http;
    let current = auth::cookie_value(headers, "csrftoken");
    if current.as_deref() != Some(session.data.csrf.as_str()) {
        append_cookie(&mut response, auth::csrf_cookie(&session.data.csrf, secure));
    }
    response
}

/// 匿名ページの応答。
pub fn anon_page_response(app: &crate::App, headers: &HeaderMap, body: String) -> Response {
    let mut response = html(body);
    response.headers_mut().insert(header::VARY, "Cookie".parse().unwrap());
    if auth::cookie_value(headers, "csrftoken").is_none() {
        let token = auth::new_csrf_token();
        let secure = !app.cfg.insecure_http;
        append_cookie(&mut response, auth::csrf_cookie(&token, secure));
    }
    response
}

/// 匿名ページ用の CSRF トークン。
pub fn anonymous_csrf(_app: &crate::App, headers: &HeaderMap) -> String {
    match auth::cookie_value(headers, "csrftoken") {
        Some(token) => token,
        None => auth::new_csrf_token(),
    }
}

/// ログイン必須（未ログインはログインへ）。
pub fn require_session(app: &crate::App, headers: &HeaderMap, next: &str) -> Result<Session, Response> {
    match auth::load_session(&app.db, headers) {
        Some(session) if auth::current_user(&app.db, &session).is_some() => Ok(session),
        _ => Err(login_redirect(next)),
    }
}

/// スタッフ専用。
pub fn require_staff(app: &crate::App, headers: &HeaderMap, next: &str) -> Result<Session, Response> {
    let session = require_session(app, headers, next)?;
    let user = auth::current_user(&app.db, &session).ok_or_else(|| login_redirect(next))?;
    if !user.is_staff {
        return Err(login_redirect(next));
    }
    Ok(session)
}

/// スタッフのセッションと利用者をまとめて取得する。
pub fn staff_identity(
    app: &crate::App,
    headers: &HeaderMap,
    next: &str,
) -> Result<(Session, UserRow), Response> {
    let session = require_staff(app, headers, next)?;
    let user = auth::current_user(&app.db, &session).ok_or_else(|| login_redirect(next))?;
    Ok((session, user))
}

/// CSRF 検検証（対象外 API では呼ばない）。
pub fn check_csrf(
    app: &crate::App,
    headers: &HeaderMap,
    form: &HashMap<String, String>,
) -> Result<(), Response> {
    auth::verify_csrf(headers, form.get("csrfmiddlewaretoken").map(|value| value.as_str()), &app.cfg)
        .map_err(|_| csrf_failed_page())
}

use std::collections::HashMap;

/// 貸出の表示（スタッフは全員分を閲覧できる）。
pub fn visible_lease(app: &crate::App, user: &UserRow, lease_hex: &str) -> Result<LeaseRow, Response> {
    let conn = app.db.lock();
    match models::get_lease(&conn, lease_hex).map_err(|_| not_found_page())? {
        Some(lease) if user.is_staff || lease.user_id == user.id => Ok(lease),
        _ => Err(not_found_page()),
    }
}

/// 現在の利用者（無ければログインへ）。
pub fn require_user(
    app: &crate::App,
    headers: &HeaderMap,
    next: &str,
) -> Result<(Session, UserRow), Response> {
    let session = require_session(app, headers, next)?;
    let user = auth::current_user(&app.db, &session).ok_or_else(|| login_redirect(next))?;
    Ok((session, user))
}

/// Host ヘッダーの検証（Django の DisallowedHost 相当）。
fn host_allowed(host: &str, allowed: &[String]) -> bool {
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    // ポートを取り除く（IPv6 の [::1]:8000 にも対応）。
    let hostname = match host.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((head, _)) => format!("[{head}]"),
            None => host.clone(),
        },
        None => match host.rsplit_once(':') {
            Some((head, tail)) if tail.bytes().all(|byte| byte.is_ascii_digit()) => head.to_string(),
            _ => host.clone(),
        },
    };
    allowed.iter().any(|entry| {
        let entry = entry.trim().to_ascii_lowercase();
        if let Some(suffix) = entry.strip_prefix('.') {
            hostname == suffix || hostname.ends_with(&entry)
        } else {
            hostname == entry
        }
    })
}

/// Host 検証ミドルウェア。
async fn host_guard(State(app): State<Arc<crate::App>>, request: Request, next: axum::middleware::Next) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if !host_allowed(host, &app.cfg.allowed_hosts) {
        return error_page(StatusCode::BAD_REQUEST, "不正なリクエストです（400）", "許可されていないホスト名です。");
    }
    next.run(request).await
}

/// APPEND_SLASH 相当（Django はスラッシュなしへ 301 で誘導する）。
async fn slash_redirect(request: Request) -> Response {
    let path = request.uri().path().to_string();
    let known = matches!(
        path.as_str(),
        "/" | "/login/" | "/nodes/" | "/admin/" | "/admin/auth/user/"
            | "/admin/portal/workloadtemplate/" | "/admin/portal/agentrelease/"
    );
    let lease_like = path.starts_with("/leases/") && path.len() > 8 && !path[8..].contains('/');
    if request.method() == Method::GET && (known || lease_like) {
        redirect(&format!("{path}/"))
    } else {
        not_found_page()
    }
}

/// ルーターを組み立てる。
pub fn router(app: Arc<crate::App>) -> Router {
    Router::new()
        .route("/", get(crate::pages::dashboard))
        .route("/login/", get(crate::pages::login_get).post(crate::pages::login_post))
        .route("/logout/", post(crate::pages::logout))
        .route("/leases/start/", post(crate::pages::start))
        .route("/leases/{lease_id}/", get(crate::pages::lease_detail))
        .route("/leases/{lease_id}/stop/", post(crate::pages::stop))
        .route("/nodes/", get(crate::pages::nodes))
        .route("/nodes/enroll/", post(crate::pages::node_enroll))
        .route("/nodes/{node_id}/control/", post(crate::pages::node_control))
        .route("/api/nodes/{node_id}/sync/", post(crate::api::node_sync))
        .route(
            "/api/nodes/{node_id}/benchmark/",
            get(crate::api::node_benchmark).post(crate::api::node_benchmark),
        )
        .route("/api/nodes/{node_id}/token/prepare/", post(crate::api::node_token_prepare))
        .route("/api/nodes/{node_id}/token/commit/", post(crate::api::node_token_commit))
        .route("/admin/", get(admin::index))
        .route("/admin/auth/user/", get(admin::users))
        .route("/admin/auth/user/create/", post(admin::user_create))
        .route("/admin/portal/workloadtemplate/", get(admin::templates_list))
        .route(
            "/admin/portal/workloadtemplate/{template_id}/toggle/",
            post(admin::template_toggle),
        )
        .route("/admin/portal/agentrelease/", get(admin::releases))
        .route("/admin/portal/agentrelease/save/", post(admin::release_save))
        .fallback(any(slash_redirect))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(middleware::from_fn_with_state(app.clone(), host_guard))
        .with_state(app)
}

/// HTTP サーバーを起動する（serve）。
pub async fn serve(app: Arc<crate::App>, bind: &str) -> anyhow::Result<()> {
    let relay = tokio::spawn(crate::relay::run_relay(
        app.clone(),
        app.cfg.relay_bind_host.clone(),
        app.cfg.relay_port,
    ));
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!("Serving on http://{bind}/");
    let result = axum::serve(listener, router(app)).await.map_err(anyhow::Error::from);
    relay.abort();
    result
}

/// ヘルパー再エクスポート（他モジュールから利用）。
pub use crate::db::dt_label;

/// 現在時刻の文字列。
pub fn stamp() -> String {
    fmt_dt(now())
}
