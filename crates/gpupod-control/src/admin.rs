//! 簡易管理画面（Django admin の簡易版）。
//!
//! staff のみ閲覧可能。利用者作成・WorkloadTemplate の有効切替・
//! AgentRelease の編集を minimal なフォームで行う。

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::extract::{Form, Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;

use crate::auth;
use crate::models::{self, ReleaseRow};
use crate::web::{
    self as web, error_page, flash_view, not_found_page, page_response, redirect, staff_identity,
    FlashView, UserNav,
};

/// /admin/
#[derive(Template)]
#[template(path = "admin/index.html")]
pub struct AdminIndex {
    pub user: UserNav,
    pub csrf: String,
    pub messages: Vec<FlashView>,
}

/// /admin/auth/user/
#[derive(Template)]
#[template(path = "admin/users.html")]
pub struct AdminUsers {
    pub user: UserNav,
    pub csrf: String,
    pub messages: Vec<FlashView>,
    pub users: Vec<UserRowView>,
    pub error: String,
}

/// 利用者 1 行。
#[derive(Debug, Clone)]
pub struct UserRowView {
    pub username: String,
    pub is_superuser: bool,
    pub is_staff: bool,
    pub is_active: bool,
    pub date_joined: String,
}

/// /admin/portal/workloadtemplate/
#[derive(Template)]
#[template(path = "admin/templates.html")]
pub struct AdminTemplates {
    pub user: UserNav,
    pub csrf: String,
    pub messages: Vec<FlashView>,
    pub templates: Vec<TemplateRowView>,
}

/// テンプレート 1 行。
#[derive(Debug, Clone)]
pub struct TemplateRowView {
    pub id: String,
    pub name: String,
    pub enabled: bool,
}

/// /admin/portal/agentrelease/
#[derive(Template)]
#[template(path = "admin/releases.html")]
pub struct AdminReleases {
    pub user: UserNav,
    pub csrf: String,
    pub messages: Vec<FlashView>,
    pub releases: Vec<ReleaseRowView>,
    pub form: ReleaseForm,
    pub error: String,
}

/// 更新 1 行。
#[derive(Debug, Clone)]
pub struct ReleaseRowView {
    pub sequence: String,
    pub version: String,
    pub target: String,
    pub channel: String,
    pub rollout_percent: String,
    pub enabled: bool,
}

/// 更新フォームの入力値。
#[derive(Debug, Clone)]
pub struct ReleaseForm {
    pub sequence: String,
    pub version: String,
    pub target: String,
    pub channel: String,
    pub rollout_percent: String,
    pub enabled: bool,
}

impl Default for ReleaseForm {
    fn default() -> ReleaseForm {
        ReleaseForm {
            sequence: String::new(),
            version: String::new(),
            target: String::new(),
            channel: "stable".to_string(),
            rollout_percent: "0".to_string(),
            enabled: false,
        }
    }
}

/// GET /admin/
pub async fn index(State(app): State<Arc<crate::App>>, headers: HeaderMap) -> Response {
    let (session, user) = match staff_identity(&app, &headers, "/admin/") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let messages: Vec<FlashView> = auth::take_messages(&app.db, &session)
        .into_iter()
        .map(flash_view)
        .collect();
    let page = AdminIndex {
        user: web::user_nav_profile(&app.db, Some(&user)),
        csrf: web::csrf_input(&session.data.csrf),
        messages,
    };
    page_response(&app, &session, &headers, page.render().unwrap_or_default())
}

/// GET /admin/auth/user/
pub async fn users(
    State(app): State<Arc<crate::App>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let (session, user) = match staff_identity(&app, &headers, "/admin/auth/user/") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let messages: Vec<FlashView> = auth::take_messages(&app.db, &session)
        .into_iter()
        .map(flash_view)
        .collect();
    let rows = {
        let conn = app.db.lock();
        models::list_users(&conn).unwrap_or_default()
    };
    let page = AdminUsers {
        user: web::user_nav_profile(&app.db, Some(&user)),
        csrf: web::csrf_input(&session.data.csrf),
        messages,
        users: rows
            .into_iter()
            .map(|row| UserRowView {
                username: row.username,
                is_superuser: row.is_superuser,
                is_staff: row.is_staff,
                is_active: row.is_active,
                date_joined: crate::db::dt_label(&row.date_joined),
            })
            .collect(),
        error: query.get("error").cloned().unwrap_or_default(),
    };
    page_response(&app, &session, &headers, page.render().unwrap_or_default())
}

/// POST /admin/auth/user/create/
pub async fn user_create(
    State(app): State<Arc<crate::App>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (session, actor) = match staff_identity(&app, &headers, "/admin/auth/user/") {
        Ok(value) => value,
        Err(response) => return response,
    };
    if let Err(response) = web::check_csrf(&app, &headers, &form) {
        return response;
    }
    let username = form
        .get("username")
        .map(String::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let password = form.get("password").cloned().unwrap_or_default();
    if username.is_empty() || password.is_empty() {
        auth::push_message(&app.db, &session, "error", "このフィールドは必須です。");
        return redirect("/admin/auth/user/");
    }
    let is_staff = form.contains_key("is_staff");
    let is_superuser = form.contains_key("is_superuser");
    // 管理者（スーパーユーザー）を付与できるのは管理者だけにする
    // （Django admin と同じく、権限昇格の経路を塞ぐ）。
    if is_superuser && !actor.is_superuser {
        auth::push_message(
            &app.db,
            &session,
            "error",
            "管理者を付与できるのは管理者だけです。",
        );
        return redirect("/admin/auth/user/");
    }
    let is_active = form.contains_key("is_active");
    let encoded = auth::hash_password(&password);
    let created = {
        let conn = app.db.lock();
        if models::find_user_by_username(&conn, &username)
            .ok()
            .flatten()
            .is_some()
        {
            None
        } else {
            models::insert_user(
                &conn,
                &username,
                &encoded,
                is_superuser,
                is_staff,
                is_active,
                "",
            )
            .ok()
        }
    };
    if created.is_none() {
        auth::push_message(
            &app.db,
            &session,
            "error",
            "このユーザー名は既に使われています。",
        );
        return redirect("/admin/auth/user/");
    }
    redirect("/admin/auth/user/")
}

/// GET /admin/portal/workloadtemplate/
pub async fn templates_list(State(app): State<Arc<crate::App>>, headers: HeaderMap) -> Response {
    let (session, user) = match staff_identity(&app, &headers, "/admin/portal/workloadtemplate/") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let messages: Vec<FlashView> = auth::take_messages(&app.db, &session)
        .into_iter()
        .map(flash_view)
        .collect();
    let rows = {
        let conn = app.db.lock();
        models::list_templates(&conn).unwrap_or_default()
    };
    let page = AdminTemplates {
        user: web::user_nav_profile(&app.db, Some(&user)),
        csrf: web::csrf_input(&session.data.csrf),
        messages,
        templates: rows
            .into_iter()
            .map(|row| TemplateRowView {
                id: row.id,
                name: row.name,
                enabled: row.enabled,
            })
            .collect(),
    };
    page_response(&app, &session, &headers, page.render().unwrap_or_default())
}

/// POST /admin/portal/workloadtemplate/<id>/toggle/
pub async fn template_toggle(
    State(app): State<Arc<crate::App>>,
    Path(template_id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (session, _user) = match staff_identity(&app, &headers, "/admin/portal/workloadtemplate/") {
        Ok(value) => value,
        Err(response) => return response,
    };
    if let Err(response) = web::check_csrf(&app, &headers, &form) {
        return response;
    }
    {
        let conn = app.db.lock();
        let found = models::get_template(&conn, &template_id).ok().flatten();
        match found {
            Some(template) => {
                let _ = conn.execute(
                    "UPDATE portal_workloadtemplate SET enabled = ?2 WHERE id = ?1",
                    rusqlite::params![template.id, !template.enabled],
                );
            }
            None => return not_found_page(),
        }
    }
    let _ = &session;
    redirect("/admin/portal/workloadtemplate/")
}

/// GET /admin/portal/agentrelease/
pub async fn releases(
    State(app): State<Arc<crate::App>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let (session, user) = match staff_identity(&app, &headers, "/admin/portal/agentrelease/") {
        Ok(value) => value,
        Err(response) => return response,
    };
    let messages: Vec<FlashView> = auth::take_messages(&app.db, &session)
        .into_iter()
        .map(flash_view)
        .collect();
    let mut form = ReleaseForm::default();
    if let Some(sequence) = query.get("seq").and_then(|value| value.parse::<i64>().ok()) {
        let conn = app.db.lock();
        if let Ok(Some(row)) = models::get_release(&conn, sequence) {
            form = ReleaseForm {
                sequence: row.sequence.to_string(),
                version: row.version,
                target: row.target,
                channel: row.channel,
                rollout_percent: row.rollout_percent.to_string(),
                enabled: row.enabled,
            };
        }
    }
    let rows = {
        let conn = app.db.lock();
        models::list_all_releases(&conn).unwrap_or_default()
    };
    let page = AdminReleases {
        user: web::user_nav_profile(&app.db, Some(&user)),
        csrf: web::csrf_input(&session.data.csrf),
        messages,
        releases: rows
            .into_iter()
            .map(|row| ReleaseRowView {
                sequence: row.sequence.to_string(),
                version: row.version,
                target: row.target,
                channel: row.channel,
                rollout_percent: row.rollout_percent.to_string(),
                enabled: row.enabled,
            })
            .collect(),
        form,
        error: query.get("error").cloned().unwrap_or_default(),
    };
    page_response(&app, &session, &headers, page.render().unwrap_or_default())
}

/// POST /admin/portal/agentrelease/save/
pub async fn release_save(
    State(app): State<Arc<crate::App>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let (session, _user) = match staff_identity(&app, &headers, "/admin/portal/agentrelease/") {
        Ok(value) => value,
        Err(response) => return response,
    };
    if let Err(response) = web::check_csrf(&app, &headers, &form) {
        return response;
    }
    let back = "/admin/portal/agentrelease/";
    let fail = |session: &auth::Session, message: &str| {
        auth::push_message(&app.db, session, "error", message);
        redirect(back)
    };
    let sequence = match form
        .get("sequence")
        .and_then(|value| value.trim().parse::<i64>().ok())
    {
        Some(value) if value >= 0 => value,
        _ => return fail(&session, "整数を入力してください。"),
    };
    let rollout = match form
        .get("rollout_percent")
        .and_then(|value| value.trim().parse::<i64>().ok())
    {
        Some(value) if value >= 0 => value,
        Some(_) => return fail(&session, "0 以上の値を入力してください。"),
        None => return fail(&session, "整数を入力してください。"),
    };
    let version = form.get("version").cloned().unwrap_or_default();
    let target = form.get("target").cloned().unwrap_or_default();
    let channel = form.get("channel").cloned().unwrap_or_default();
    if rollout > 100 || !valid_release_target(&target) {
        return fail(&session, "Invalid rollout percentage or TUF target path");
    }
    if channel != "stable" && channel != "candidate" {
        return fail(&session, "選択した値が正しくありません。");
    }
    if version.is_empty() || version.len() > 64 || target.len() > 200 {
        return fail(&session, "このフィールドは必須です。");
    }
    let release = ReleaseRow {
        sequence,
        version,
        target,
        channel,
        rollout_percent: rollout,
        enabled: form.contains_key("enabled"),
    };
    {
        let conn = app.db.lock();
        if let Err(exc) = models::upsert_release(&conn, &release) {
            tracing::error!("release save failed: {exc}");
            return error_page(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "サーバーで予期しないエラーが発生しました（500）",
                &exc.to_string(),
            );
        }
    }
    redirect(back)
}

/// AgentRelease.clean() のターゲット規則（provider/... .tar.gz）。
fn valid_release_target(target: &str) -> bool {
    let Some(rest) = target.strip_prefix("provider/") else {
        return false;
    };
    if !rest.ends_with(".tar.gz") {
        return false;
    }
    let body = &rest[..rest.len() - ".tar.gz".len()];
    !body.is_empty()
        && body
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && !body.starts_with('.')
        && !body.ends_with('.')
}
