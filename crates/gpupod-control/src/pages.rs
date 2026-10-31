//! ページのビュー（portal/views.py の移植）。

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::extract::{Form, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;

use crate::auth;
use crate::benchmarks::results_from_statistics;
use crate::db::{dt_label, fmt_dt, now};
use crate::models::{self, LeaseRow};
use crate::pyrepr::py_str;
use crate::services::{self, ServiceError};
use crate::web::{
    self as web, benchmarks_view, check_csrf, error_page, flash_view, not_found_page,
    page_response, redirect, require_user, resources_view, staff_identity, visible_lease,
    FlashView, LeaseDetailView, LeaseRowView, NodeLeaseView, NodeView, OfferView, RouteView,
    TemplateOption, UserNav,
};

/// 利用パネル（dashboard.html）。
#[derive(Template)]
#[template(path = "portal/dashboard.html")]
pub struct DashboardPage {
    pub user: UserNav,
    pub csrf: String,
    pub messages: Vec<FlashView>,
    pub offers: Vec<OfferView>,
    pub leases: Vec<LeaseRowView>,
}

/// 貸出の詳細（lease.html）。
#[derive(Template)]
#[template(path = "portal/lease.html")]
pub struct LeasePage {
    pub user: UserNav,
    pub csrf: String,
    pub messages: Vec<FlashView>,
    pub lease: LeaseDetailView,
    pub routes: Vec<RouteView>,
}

/// ログイン（login.html）。
#[derive(Template)]
#[template(path = "portal/login.html")]
pub struct LoginPage {
    pub user: UserNav,
    pub csrf: String,
    pub messages: Vec<FlashView>,
    pub form_html: String,
    pub next: String,
    /// Discord ログインのボタンを表示するか。
    pub discord_enabled: bool,
}

/// アカウント（account.html）。
#[derive(Template)]
#[template(path = "portal/account.html")]
pub struct AccountPage {
    pub user: UserNav,
    pub csrf: String,
    pub messages: Vec<FlashView>,
    pub username: String,
    pub display_name: String,
    pub avatar_url: String,
    pub has_avatar: bool,
    pub email: String,
    pub has_email: bool,
    pub discord_enabled: bool,
    pub discord_linked: bool,
    pub discord_subject: String,
    pub can_unlink: bool,
    pub has_password: bool,
}

/// 提供PC管理（nodes.html）。
#[derive(Template)]
#[template(path = "portal/nodes.html")]
pub struct NodesPage {
    pub user: UserNav,
    pub csrf: String,
    pub messages: Vec<FlashView>,
    pub nodes: Vec<NodeView>,
    pub leases: Vec<NodeLeaseView>,
}

/// 500 ページ。
fn server_error(message: &str) -> Response {
    error_page(
        StatusCode::INTERNAL_SERVER_ERROR,
        "サーバーで予期しないエラーが発生しました（500）",
        message,
    )
}

/// HTML のエスケープ（Django の escape 相当）。
fn escape_html(value: &str) -> String {
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

/// 貸出行 1 件（dashboard）。
fn lease_row_view(conn: &rusqlite::Connection, lease: LeaseRow) -> LeaseRowView {
    let node_name = models::get_node(conn, &lease.node_id)
        .ok()
        .flatten()
        .map(|node| node.name)
        .unwrap_or_default();
    let template_name = models::get_template(conn, &lease.template_id)
        .ok()
        .flatten()
        .map(|template| template.name)
        .unwrap_or_default();
    LeaseRowView {
        pk: lease.uuid_str(),
        node_name,
        template_name,
        status: lease.status,
    }
}

/// GET / （dashboard）
pub async fn dashboard(State(app): State<Arc<crate::App>>, headers: HeaderMap) -> Response {
    let (session, user) = match require_user(&app, &headers, "/") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let messages: Vec<FlashView> = auth::take_messages(&app.db, &session)
        .into_iter()
        .map(flash_view)
        .collect();
    let csrf = web::csrf_input(&session.data.csrf);
    let now_text = fmt_dt(now());

    let (offers, leases) = {
        let conn = app.db.lock();
        let mut offers: Vec<OfferView> = Vec::new();
        for node in models::list_nodes(&conn)
            .unwrap_or_default()
            .into_iter()
            .filter(|node| node.enabled)
        {
            let capacity = services::node_resources(&conn, &node);
            let capacity_available = match &capacity {
                Ok(None) => true,
                Ok(Some(value)) => {
                    value.available_memory_bytes
                        >= gpupod_common::resources::minimum("memory_bytes")
                        && value.available_disk_bytes
                            >= gpupod_common::resources::minimum("disk_bytes")
                }
                Err(_) => false,
            };
            let resources = resources_view(&capacity);
            let node_benchmarks = benchmarks_view(results_from_statistics(&node.statistics));
            let node_online = node.online(&now_text);
            let accepting = node.accepting(&now_text);
            let gpus = node
                .snapshot
                .get("gpus")
                .and_then(|value| value.as_array())
                .cloned()
                .unwrap_or_default();
            for gpu in gpus {
                if !gpu.is_object() {
                    return server_error("Invalid GPU inventory");
                }
                let Some(gpu_id) = gpu.get("id").and_then(|value| value.as_str()) else {
                    return server_error("Invalid GPU inventory");
                };
                let templates = match services::compatible_templates(&conn, &node, &gpu) {
                    Ok(templates) => templates,
                    Err(ServiceError::Agent(message)) => return server_error(&message),
                    Err(other) => return server_error(&other.to_string()),
                };
                let occupied = services::is_truthy(gpu.get("allocated"))
                    || models::active_lease_exists(&conn, &node.id, gpu_id).unwrap_or(false);
                let available =
                    accepting && !occupied && !templates.is_empty() && capacity_available;
                offers.push(OfferView {
                    node_pk: node.uuid_str(),
                    node_name: node.name.clone(),
                    node_online,
                    direct_host: !node.direct_host().is_empty(),
                    gpu_id: gpu_id.to_string(),
                    gpu_name: gpu
                        .get("name")
                        .and_then(|value| value.as_str())
                        .unwrap_or("")
                        .to_string(),
                    gpu_vendor: gpu
                        .get("vendor")
                        .and_then(|value| value.as_str())
                        .unwrap_or("")
                        .to_string(),
                    gpu_memory_mib: gpu
                        .get("memory_mib")
                        .and_then(|value| value.as_i64())
                        .unwrap_or(0),
                    gpu_allocated: occupied,
                    available,
                    capacity_available,
                    has_resources: resources.present,
                    resources: resources.clone(),
                    benchmarks: node_benchmarks.clone(),
                    templates: templates
                        .into_iter()
                        .map(|item| TemplateOption {
                            id: item.id,
                            name: item.name,
                        })
                        .collect(),
                });
            }
        }
        let leases: Vec<LeaseRowView> = models::list_user_leases(&conn, user.id, 30)
            .unwrap_or_default()
            .into_iter()
            .map(|lease| lease_row_view(&conn, lease))
            .collect();
        (offers, leases)
    };

    let page = DashboardPage {
        user: web::user_nav_profile(&app.db, Some(&user)),
        csrf,
        messages,
        offers,
        leases,
    };
    let body = page.render().unwrap_or_default();
    page_response(&app, &session, &headers, body)
}

/// POST /leases/start/ （start）
pub async fn start(
    State(app): State<Arc<crate::App>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (session, user) = match require_user(&app, &headers, "/leases/start/") {
        Ok(value) => value,
        Err(response) => return response,
    };
    if let Err(response) = check_csrf(&app, &headers, &form) {
        return response;
    }

    let generic = "貸出要求を受け付けられませんでした。";
    let push_error = |text: String| {
        auth::push_message(&app.db, &session, "error", text);
    };

    let (node, gpu, template) = match (form.get("node"), form.get("gpu"), form.get("template")) {
        (Some(node), Some(gpu), Some(template)) => (node.clone(), gpu.clone(), template.clone()),
        _ => {
            push_error(generic.to_string());
            return redirect("/");
        }
    };
    let ssh_public_key = form.get("ssh_public_key").cloned().unwrap_or_default();
    let connection_mode = form
        .get("connection_mode")
        .cloned()
        .unwrap_or_else(|| "relay".to_string());

    let memory_bytes = match form.get("memory_gib").filter(|value| !value.is_empty()) {
        None => Ok(None),
        Some(value) => gpupod_common::resources::gib_bytes(value, "memory_bytes")
            .map(Some)
            .map_err(|exc| exc.0),
    };
    let memory_bytes = match memory_bytes {
        Ok(value) => value,
        Err(message) => {
            push_error(message);
            return redirect("/");
        }
    };
    let disk_bytes = match form.get("disk_gib").filter(|value| !value.is_empty()) {
        None => Ok(None),
        Some(value) => gpupod_common::resources::gib_bytes(value, "disk_bytes")
            .map(Some)
            .map_err(|exc| exc.0),
    };
    let disk_bytes = match disk_bytes {
        Ok(value) => value,
        Err(message) => {
            push_error(message);
            return redirect("/");
        }
    };

    match services::reserve(
        &app.db,
        &app.cfg,
        user.id,
        &node,
        &gpu,
        &template,
        &ssh_public_key,
        &connection_mode,
        memory_bytes,
        disk_bytes,
    ) {
        Ok(lease) => redirect(&format!("/leases/{}/", lease.uuid_str())),
        Err(ServiceError::Agent(message)) => {
            push_error(message);
            redirect("/")
        }
        Err(_) => {
            push_error(generic.to_string());
            redirect("/")
        }
    }
}

/// GET /leases/<uuid>/ （lease_detail）
pub async fn lease_detail(
    State(app): State<Arc<crate::App>>,
    Path(lease_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (session, user) = match require_user(&app, &headers, "/") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let messages: Vec<FlashView> = auth::take_messages(&app.db, &session)
        .into_iter()
        .map(flash_view)
        .collect();
    let csrf = web::csrf_input(&session.data.csrf);
    let Some(lease_hex) = models::uuid_to_hex(&lease_id) else {
        return not_found_page();
    };
    let lease = match visible_lease(&app, &user, &lease_hex) {
        Ok(lease) => lease,
        Err(response) => return response,
    };
    let now_text = fmt_dt(now());
    let (routes, detail) = {
        let conn = app.db.lock();
        let node = models::get_node(&conn, &lease.node_id).ok().flatten();
        let template = models::get_template(&conn, &lease.template_id)
            .ok()
            .flatten();
        let node_online = node
            .as_ref()
            .map(|node| node.online(&now_text))
            .unwrap_or(false);
        let mut routes: Vec<RouteView> = Vec::new();
        for route in models::routes_of_lease(&conn, &lease.id).unwrap_or_default() {
            let command = if route.container_port == "22/tcp" {
                format!("ssh -p {} pod@{}", route.relay_port, app.cfg.relay_host)
            } else {
                format!("{}:{}", app.cfg.relay_host, route.relay_port)
            };
            routes.push(RouteView {
                port: route.container_port,
                command,
            });
        }
        if lease.connection_mode == "direct" {
            let node = node.clone();
            if let Some(node) = node {
                let host = node.direct_host().to_string();
                let local = node
                    .snapshot
                    .get("leases")
                    .and_then(|value| value.get(&lease.id))
                    .and_then(|value| value.get("ports"))
                    .and_then(|value| value.as_object())
                    .cloned()
                    .unwrap_or_default();
                for (port, bindings) in local {
                    let items = match bindings.as_array() {
                        Some(items) => items.clone(),
                        None => continue,
                    };
                    for binding in items {
                        let number = match binding.get("HostPort").and_then(|value| value.as_str())
                        {
                            Some(number) => number,
                            None => continue,
                        };
                        let command = if port == "22/tcp" {
                            format!("ssh -p {} pod@{}", number, host)
                        } else {
                            format!("{}:{}", host, number)
                        };
                        routes.push(RouteView {
                            port: port.clone(),
                            command,
                        });
                    }
                }
            }
        }
        let resources = lease.resources();
        let detail = LeaseDetailView {
            pk: lease.uuid_str(),
            node_name: node
                .as_ref()
                .map(|node| node.name.clone())
                .unwrap_or_default(),
            node_online,
            template_name: template.map(|template| template.name).unwrap_or_default(),
            gpu_id: lease.gpu_id.clone(),
            has_resources: resources.is_some(),
            memory_gib: resources
                .map(|(memory, _)| crate::fmt::floatformat(crate::fmt::gib(memory), 2))
                .unwrap_or_default(),
            disk_gib: resources
                .map(|(_, disk)| crate::fmt::floatformat(crate::fmt::gib(disk), 2))
                .unwrap_or_default(),
            status: lease.status.clone(),
            has_error: !lease.error.is_empty(),
            error: lease.error.clone(),
            active: lease.active,
            show_connection: lease.status == "running" && node_online,
            connection_mode_display: if lease.connection_mode == "direct" {
                "直接接続".to_string()
            } else {
                "中央中継".to_string()
            },
        };
        (routes, detail)
    };

    let page = LeasePage {
        user: web::user_nav_profile(&app.db, Some(&user)),
        csrf,
        messages,
        lease: detail,
        routes,
    };
    let body = page.render().unwrap_or_default();
    page_response(&app, &session, &headers, body)
}

/// POST /leases/<uuid>/stop/ （stop）
pub async fn stop(
    State(app): State<Arc<crate::App>>,
    Path(lease_id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (session, user) = match require_user(&app, &headers, "/") {
        Ok(value) => value,
        Err(response) => return response,
    };
    if let Err(response) = check_csrf(&app, &headers, &form) {
        return response;
    }
    let Some(lease_hex) = models::uuid_to_hex(&lease_id) else {
        return not_found_page();
    };
    let lease = match visible_lease(&app, &user, &lease_hex) {
        Ok(lease) => lease,
        Err(response) => return response,
    };
    match services::stop_lease(&app.db, &lease.id) {
        Ok(_) => {
            auth::push_message(
                &app.db,
                &session,
                "success",
                "返却を要求しました。提供PCがデータの削除を確認すると返却が完了します。",
            );
            redirect(&format!("/leases/{}/", lease.uuid_str()))
        }
        Err(exc) => server_error(&exc.to_string()),
    }
}

/// GET /nodes/ （nodes）
pub async fn nodes(State(app): State<Arc<crate::App>>, headers: HeaderMap) -> Response {
    let (session, user) = match staff_identity(&app, &headers, "/nodes/") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let messages: Vec<FlashView> = auth::take_messages(&app.db, &session)
        .into_iter()
        .map(flash_view)
        .collect();
    let csrf = web::csrf_input(&session.data.csrf);
    let now_text = fmt_dt(now());

    let (node_views, lease_views) = {
        let conn = app.db.lock();
        let mut node_views: Vec<NodeView> = Vec::new();
        for node in models::list_nodes(&conn).unwrap_or_default() {
            let capacity = services::node_resources(&conn, &node);
            node_views.push(NodeView {
                pk: node.uuid_str(),
                name: node.name.clone(),
                online: node.online(&now_text),
                accepting: node.accepting(&now_text),
                last_seen: node
                    .last_seen
                    .as_deref()
                    .map(dt_label)
                    .unwrap_or_else(|| "未接続".to_string()),
                resources: resources_view(&capacity),
                benchmarks: benchmarks_view(results_from_statistics(&node.statistics)),
                statistics: py_str(&node.statistics),
                snapshot: py_str(&node.snapshot),
            });
        }
        let mut lease_views: Vec<NodeLeaseView> = Vec::new();
        for lease in models::list_active_leases(&conn).unwrap_or_default() {
            let username = models::find_user(&conn, lease.user_id)
                .ok()
                .flatten()
                .map(|user| user.username)
                .unwrap_or_default();
            let node_name = models::get_node(&conn, &lease.node_id)
                .ok()
                .flatten()
                .map(|node| node.name)
                .unwrap_or_default();
            let template_name = models::get_template(&conn, &lease.template_id)
                .ok()
                .flatten()
                .map(|template| template.name)
                .unwrap_or_default();
            lease_views.push(NodeLeaseView {
                pk: lease.uuid_str(),
                username,
                node_name,
                template_name,
                status: lease.status,
            });
        }
        (node_views, lease_views)
    };

    let page = NodesPage {
        user: web::user_nav_profile(&app.db, Some(&user)),
        csrf,
        messages,
        nodes: node_views,
        leases: lease_views,
    };
    let body = page.render().unwrap_or_default();
    page_response(&app, &session, &headers, body)
}

/// POST /nodes/<uuid>/control/ （node_control）
pub async fn node_control(
    State(app): State<Arc<crate::App>>,
    Path(node_id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (session, _user) = match staff_identity(&app, &headers, "/nodes/") {
        Ok(value) => value,
        Err(response) => return response,
    };
    if let Err(response) = check_csrf(&app, &headers, &form) {
        return response;
    }
    let Some(node_hex) = models::uuid_to_hex(&node_id) else {
        return not_found_page();
    };
    {
        let conn = app.db.lock();
        match models::get_node(&conn, &node_hex) {
            Ok(Some(_)) => {}
            _ => return not_found_page(),
        }
    }
    let action = form.get("action").map(String::as_str);
    match services::control_node(&app.db, &node_hex, action) {
        Ok(()) => {}
        Err(ServiceError::Agent(message)) => {
            auth::push_message(&app.db, &session, "error", message);
        }
        Err(exc) => return server_error(&exc.to_string()),
    }
    redirect("/nodes/")
}

/// POST /nodes/enroll/ （node_enroll）
pub async fn node_enroll(
    State(app): State<Arc<crate::App>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (session, _user) = match staff_identity(&app, &headers, "/nodes/") {
        Ok(value) => value,
        Err(response) => return response,
    };
    if let Err(response) = check_csrf(&app, &headers, &form) {
        return response;
    }
    let name = form
        .get("name")
        .map(String::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if name.is_empty() {
        auth::push_message(
            &app.db,
            &session,
            "error",
            "提供PCの名前を入力してください。",
        );
        return redirect("/nodes/");
    }
    let key_path = app.cfg.host_key_path();
    if !key_path.exists() {
        auth::push_message(
            &app.db,
            &session,
            "error",
            "先に init_control を実行してください。",
        );
        return redirect("/nodes/");
    }
    let enrolled = {
        let conn = app.db.lock();
        services::enroll_node(&conn, &name)
    };
    let (node, token) = match enrolled {
        Ok(value) => value,
        Err(exc) => return server_error(&exc.to_string()),
    };
    let public_key = match russh::keys::load_secret_key(&key_path, None) {
        Ok(key) => key
            .public_key()
            .to_openssh()
            .unwrap_or_default()
            .trim()
            .to_string(),
        Err(exc) => return server_error(&format!("relay host key: {exc}")),
    };
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let scheme = crate::auth::request_scheme(&headers, &app.cfg);
    let payload = serde_json::json!({
        "url": format!("{}://{}", scheme, host),
        "node_id": node.uuid_str(),
        "token": token,
        "relay_host": app.cfg.relay_host,
        "relay_port": app.cfg.relay_port,
        "relay_host_key": public_key,
    });
    let body = serde_json::to_string_pretty(&payload).unwrap_or_default();
    let mut response = Response::new(axum::body::Body::from(body));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    let disposition = format!(
        "attachment; filename=\"node-{}.secret.json\"",
        node.uuid_str()
    );
    if let Ok(value) = disposition.parse() {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, value);
    }
    response
}

/// Django の AuthenticationForm.as_p 相当。
fn login_form_html(
    username_error: Option<&str>,
    password_error: Option<&str>,
    nonfield: Option<&str>,
) -> String {
    let mut out = String::new();
    if let Some(message) = nonfield {
        out.push_str("<ul class=\"errorlist nonfield\"><li>");
        out.push_str(&escape_html(message));
        out.push_str("</li></ul>\n");
    }
    if let Some(message) = username_error {
        out.push_str("<ul class=\"errorlist\"><li>");
        out.push_str(&escape_html(message));
        out.push_str("</li></ul>\n");
    }
    out.push_str(
        "<p><label for=\"id_username\">ユーザー名:</label> <input type=\"text\" name=\"username\" \
         maxlength=\"150\" autofocus required id=\"id_username\"></p>\n",
    );
    if let Some(message) = password_error {
        out.push_str("<ul class=\"errorlist\"><li>");
        out.push_str(&escape_html(message));
        out.push_str("</li></ul>\n");
    }
    out.push_str(
        "<p><label for=\"id_password\">パスワード:</label> <input type=\"password\" name=\"password\" \
         autocomplete=\"current-password\" required id=\"id_password\"></p>",
    );
    out
}

/// ログイン画面の CSRF トークン。
///
/// フォームに埋め込む値と Cookie に保存する値を必ず一致させ、
/// ログイン済みならセッション固有のトークンに合わせる。
fn login_csrf_token(app: &crate::App, headers: &HeaderMap) -> String {
    match auth::load_session(&app.db, headers) {
        Some(session) => session.data.csrf,
        None => web::anonymous_csrf(app, headers),
    }
}

/// GET /login/ （LoginView）
pub async fn login_get(
    State(app): State<Arc<crate::App>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let next = query.get("next").cloned().unwrap_or_default();
    let csrf_token = login_csrf_token(&app, &headers);
    let csrf = web::csrf_input(&csrf_token);
    let page = LoginPage {
        user: web::user_nav(None),
        csrf,
        messages: Vec::new(),
        form_html: login_form_html(None, None, None),
        next,
        discord_enabled: app.cfg.discord_enabled(),
    };
    let body = page.render().unwrap_or_default();
    web::anon_page_response_with_token(&app, &headers, &csrf_token, body)
}

/// 遷移先の安全性（Django の url_has_allowed_host_and_scheme 相当）。
///
/// 自サイトの絶対パスだけを許可する。プロトコル相対（//evil.example）と、
/// ブラウザがスラッシュとして扱うバックスラッシュ（/\evil.example）、
/// 改行・制御文字を含む値は拒否する。
pub fn safe_next(next: &str) -> Option<String> {
    if !next.starts_with('/') || next.starts_with("//") {
        return None;
    }
    if next.chars().any(|ch| ch.is_control() || ch == '\\') {
        return None;
    }
    Some(next.to_string())
}

/// POST /login/ （LoginView）
pub async fn login_post(
    State(app): State<Arc<crate::App>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if let Err(response) = check_csrf(&app, &headers, &form) {
        return response;
    }
    let username = form.get("username").cloned().unwrap_or_default();
    let password = form.get("password").cloned().unwrap_or_default();
    let next = form.get("next").cloned().unwrap_or_default();

    let mut username_error = None;
    let mut password_error = None;
    if username.is_empty() {
        username_error = Some("このフィールドは必須です。");
    }
    if password.is_empty() {
        password_error = Some("このフィールドは必須です。");
    }
    let mut nonfield = None;
    let mut logged_in = None;
    if username_error.is_none() && password_error.is_none() {
        // 総当たり対策（IP + ユーザー名ごとの失敗回数）。
        let throttle_key = auth::login_throttle_key(&headers, &app.cfg, &username);
        if auth::throttle_blocked(
            "login",
            &throttle_key,
            auth::LOGIN_MAX_FAILURES,
            auth::LOGIN_FAILURE_WINDOW,
        ) {
            return web::too_many_requests_page();
        }
        // 既存セッションは破棄してから新しい鍵を発行する（Django の cycle_key 相当）。
        if let Some(existing) = auth::load_session(&app.db, &headers) {
            auth::drop_session(&app.db, &existing.key);
        }
        match auth::login(&app.db, &username, &password) {
            Some(session) => {
                auth::throttle_clear("login", &throttle_key);
                logged_in = Some(session);
            }
            None => {
                auth::throttle_hit(
                    "login",
                    &throttle_key,
                    auth::LOGIN_MAX_FAILURES,
                    auth::LOGIN_FAILURE_WINDOW,
                );
                nonfield = Some(
                    "正しいユーザー名とパスワードを入力してください。どちらのフィールドも大文字と小文字は区別されます。",
                );
            }
        }
    }

    if let Some(session) = logged_in {
        let mut response = redirect(&safe_next(&next).unwrap_or_else(|| "/".to_string()));
        let secure = !app.cfg.insecure_http;
        super::web::append_cookie(&mut response, auth::session_cookie(&session.key, secure));
        super::web::append_cookie(&mut response, auth::csrf_cookie(&session.data.csrf, secure));
        return response;
    }

    let csrf_token = login_csrf_token(&app, &headers);
    let csrf = web::csrf_input(&csrf_token);
    let page = LoginPage {
        user: web::user_nav(None),
        csrf,
        messages: Vec::new(),
        form_html: login_form_html(username_error, password_error, nonfield),
        next,
        discord_enabled: app.cfg.discord_enabled(),
    };
    let body = page.render().unwrap_or_default();
    web::anon_page_response_with_token(&app, &headers, &csrf_token, body)
}

/// POST /logout/ （LogoutView）
pub async fn logout(
    State(app): State<Arc<crate::App>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if let Err(response) = check_csrf(&app, &headers, &form) {
        return response;
    }
    if let Some(session) = auth::load_session(&app.db, &headers) {
        auth::drop_session(&app.db, &session.key);
    }
    let mut response = redirect("/login/");
    super::web::append_cookie(
        &mut response,
        auth::clear_session_cookie(!app.cfg.insecure_http),
    );
    response
}

/// アカウント画面を組み立てる。
fn render_account(
    app: &crate::App,
    session: &auth::Session,
    user: &models::UserRow,
    headers: &HeaderMap,
) -> Response {
    let messages: Vec<FlashView> = auth::take_messages(&app.db, session)
        .into_iter()
        .map(flash_view)
        .collect();
    let (profile, linked) = {
        let conn = app.db.lock();
        let profile = models::get_user_profile(&conn, user.id).ok().flatten();
        let linked = models::find_oauth_identity_by_user(&conn, crate::oauth::PROVIDER, user.id)
            .ok()
            .flatten();
        (profile, linked)
    };
    let display_name = profile
        .as_ref()
        .map(|item| item.display_name.clone())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| user.username.clone());
    let avatar_url = profile
        .as_ref()
        .map(|item| item.avatar_url.clone())
        .unwrap_or_default();
    let has_password = auth::has_usable_password(&user.password);
    let discord_subject = linked
        .as_ref()
        .map(|item| item.subject.clone())
        .unwrap_or_default();
    let page = AccountPage {
        user: web::user_nav_profile(&app.db, Some(user)),
        csrf: web::csrf_input(&session.data.csrf),
        messages,
        username: user.username.clone(),
        display_name,
        has_avatar: !avatar_url.is_empty(),
        avatar_url,
        email: user.email.clone(),
        has_email: !user.email.is_empty(),
        discord_enabled: app.cfg.discord_enabled(),
        discord_linked: linked.is_some(),
        discord_subject,
        can_unlink: linked.is_some() && has_password,
        has_password,
    };
    page_response(app, session, headers, page.render().unwrap_or_default())
}

/// GET /account/ （アカウント情報・連携・パスワード変更）。
pub async fn account(State(app): State<Arc<crate::App>>, headers: HeaderMap) -> Response {
    let (session, user) = match require_user(&app, &headers, "/account/") {
        Ok(value) => value,
        Err(response) => return response,
    };
    render_account(&app, &session, &user, &headers)
}

/// POST /account/password/ （パスワード変更）。
pub async fn password_change(
    State(app): State<Arc<crate::App>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (session, user) = match require_user(&app, &headers, "/account/") {
        Ok(value) => value,
        Err(response) => return response,
    };
    if let Err(response) = check_csrf(&app, &headers, &form) {
        return response;
    }
    let fail = |message: &str| {
        auth::push_message(&app.db, &session, "error", message);
        redirect("/account/")
    };
    if !auth::has_usable_password(&user.password) {
        return fail("パスワードが設定されていません。管理者にお問い合わせください。");
    }
    let current = form.get("current_password").cloned().unwrap_or_default();
    let new1 = form.get("new_password1").cloned().unwrap_or_default();
    let new2 = form.get("new_password2").cloned().unwrap_or_default();
    if !auth::verify_password(&current, &user.password) {
        return fail("現在のパスワードが正しくありません。");
    }
    if new1.chars().count() < 8 {
        return fail("新しいパスワードは 8 文字以上にしてください。");
    }
    if new1.chars().all(|ch| ch.is_ascii_digit()) {
        return fail("新しいパスワードを数字だけにすることはできません。");
    }
    if new1 != new2 {
        return fail("確認用のパスワードが一致しません。");
    }
    let encoded = auth::hash_password(&new1);
    let updated = {
        let conn = app.db.lock();
        conn.execute(
            "UPDATE auth_user SET password = ?2 WHERE id = ?1",
            rusqlite::params![user.id, encoded],
        )
    };
    if let Err(exc) = updated {
        tracing::error!("パスワード変更に失敗しました: {exc}");
        return fail("パスワードの変更に失敗しました。時間をおいて再度お試しください。");
    }
    auth::push_message(&app.db, &session, "success", "パスワードを変更しました。");
    redirect("/account/")
}
