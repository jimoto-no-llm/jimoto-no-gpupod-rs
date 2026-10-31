//! 公開サイト（ランディングページ・料金・提供者募集・規約・プライバシー・使い方）。
//!
//! 未ログインでも閲覧できるページだけをここに置く。既存の利用パネル（web.rs）とは
//! 分離し、ルーターの統合は web::router 側で行う。
//!
//! home は / の入口で、ログイン済みなら既存のダッシュボード（pages::dashboard）を
//! そのまま返し、未ログインなら LP を返す。ダッシュボードの URL は / のまま変えない。

use std::sync::Arc;

use askama::Template;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, MethodRouter};
use axum::Router;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::auth;
use crate::db::{fmt_dt, now};
use crate::models;
use crate::services;
use crate::web;

/// site.css はバイナリに埋め込む（実行ディレクトリに依存しない）。
const SITE_CSS: &str = include_str!("../static/site.css");

/// Discord サーバー ID の既定値（docs/roadmap.md の決定事項）。
const DEFAULT_DISCORD_GUILD_ID: &str = "1551195569429422202";

/// リポジトリの URL（フッターの GitHub リンク）。
const GITHUB_URL: &str = "https://github.com/jimoto-no-llm/jimoto-no-gpupod-rs";

/// 規約・ポリシーの最終更新日（叩き台の作成日）。
const LEGAL_UPDATED: &str = "2026年10月31日";

// ---------------------------------------------------------------------------
// 環境変数
//
// config.rs は別メンバーが編集中のため、このモジュール内で直接読む。将来は
// Config へ移す前提なので、読み取りはこの小さなヘルパに集約しておく。
// ---------------------------------------------------------------------------

/// 空文字を未設定として扱う環境変数。
fn env_trimmed(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Discord サーバーの招待 URL（GPUPOD_DISCORD_INVITE_URL）。
pub fn discord_invite_url() -> Option<String> {
    env_trimmed("GPUPOD_DISCORD_INVITE_URL")
}

/// Discord サーバー ID（GPUPOD_DISCORD_GUILD_ID、未設定なら既定値）。
pub fn discord_guild_id() -> String {
    env_trimmed("GPUPOD_DISCORD_GUILD_ID").unwrap_or_else(|| DEFAULT_DISCORD_GUILD_ID.to_string())
}

// ---------------------------------------------------------------------------
// テンプレート共通の文脈
// ---------------------------------------------------------------------------

/// 全ページで共通に使う値（ヘッダー・フッター・参加案内）。
#[derive(Debug, Clone)]
pub struct SiteContext {
    /// 現在のページ（ナビの aria-current 用）: home / pricing / providers / how / terms / privacy。
    pub nav: &'static str,
    /// ログイン済みか（LP では常に false。フッター等の分岐にだけ使う）。
    pub signed_in: bool,
    /// Discord OAuth2 が設定されているか（ログインボタンの出し分け）。
    pub discord_login_enabled: bool,
    /// 招待 URL が設定されているか。
    pub has_invite: bool,
    /// 招待 URL（未設定なら空文字）。
    pub invite_url: String,
    /// Discord サーバー ID。
    pub guild_id: String,
    /// リポジトリの URL。
    pub github_url: &'static str,
    /// フッターの著作権年。
    pub year: String,
}

impl SiteContext {
    fn new(app: &crate::App, nav: &'static str, signed_in: bool) -> SiteContext {
        let invite = discord_invite_url();
        SiteContext {
            nav,
            signed_in,
            discord_login_enabled: app.cfg.discord_enabled(),
            has_invite: invite.is_some(),
            invite_url: invite.unwrap_or_default(),
            guild_id: discord_guild_id(),
            github_url: GITHUB_URL,
            year: now().format("%Y").to_string(),
        }
    }

    /// セッションを読んでログイン状態を判定する。
    fn for_page(app: &crate::App, headers: &HeaderMap, nav: &'static str) -> SiteContext {
        let signed_in = auth::identity(&app.db, headers).is_some();
        SiteContext::new(app, nav, signed_in)
    }
}

// ---------------------------------------------------------------------------
// ライブ在庫（公開してよい情報だけ）
// ---------------------------------------------------------------------------

/// ヒーロー下の在庫ストリップ。
#[derive(Debug, Clone, Default)]
pub struct InventoryStats {
    /// 提供中（enabled な提供PCが載せている）GPU 数。
    pub offered_gpus: usize,
    /// いま貸出可能な GPU 数。
    pub available_gpus: usize,
    /// 登録提供PC数。
    pub providers: usize,
}

/// 公開用の GPU 1 件。IP アドレス・ノード ID・内部パスは含めない。
#[derive(Debug, Clone)]
pub struct PublicGpu {
    pub name: String,
    pub vendor_label: String,
    pub vram: String,
    /// 提供者が設定した表示名。
    pub provider: String,
    pub status: &'static str,
    pub available: bool,
}

/// DB のスナップショットから在庫を集計する。壊れたデータは黙って読み飛ばす。
fn collect_inventory(app: &crate::App) -> (InventoryStats, Vec<PublicGpu>) {
    let now_text = fmt_dt(now());
    let conn = app.db.lock();
    let nodes = models::list_nodes(&conn).unwrap_or_default();
    let mut stats = InventoryStats::default();
    let mut gpus: Vec<PublicGpu> = Vec::new();
    for node in nodes.into_iter().filter(|node| node.enabled) {
        stats.providers += 1;
        let accepting = node.accepting(&now_text);
        let inventory = node
            .snapshot
            .get("gpus")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        for gpu in inventory {
            if !gpu.is_object() {
                continue;
            }
            let Some(gpu_id) = gpu.get("id").and_then(|value| value.as_str()) else {
                continue;
            };
            stats.offered_gpus += 1;
            let occupied = services::is_truthy(gpu.get("allocated"))
                || models::active_lease_exists(&conn, &node.id, gpu_id).unwrap_or(false);
            let available = accepting && !occupied;
            if available {
                stats.available_gpus += 1;
            }
            let vendor = gpu
                .get("vendor")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let memory_mib = gpu
                .get("memory_mib")
                .and_then(|value| value.as_i64())
                .unwrap_or(0);
            gpus.push(PublicGpu {
                name: gpu
                    .get("name")
                    .and_then(|value| value.as_str())
                    .filter(|value| !value.is_empty())
                    .unwrap_or("GPU")
                    .to_string(),
                vendor_label: vendor_label(vendor),
                vram: vram_label(memory_mib),
                provider: node.name.clone(),
                status: if available {
                    "貸出可能"
                } else if occupied {
                    "利用中"
                } else {
                    "受付停止"
                },
                available,
            });
        }
    }
    // 貸出可能なものを先に、同名は提供者名順に並べる。
    gpus.sort_by(|a, b| {
        b.available
            .cmp(&a.available)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.provider.cmp(&b.provider))
    });
    (stats, gpus)
}

/// ベンダー名の表示（不明な値はそのまま大文字化して出す）。
fn vendor_label(vendor: &str) -> String {
    match vendor.to_ascii_lowercase().as_str() {
        "nvidia" => "NVIDIA".to_string(),
        "amd" => "AMD".to_string(),
        "intel" => "Intel".to_string(),
        "" => "GPU".to_string(),
        other => other.to_ascii_uppercase(),
    }
}

/// VRAM 容量の表示（MiB → GiB）。0 は「—」。
fn vram_label(memory_mib: i64) -> String {
    if memory_mib <= 0 {
        return "—".to_string();
    }
    let gib = memory_mib as f64 / 1024.0;
    if (gib - gib.round()).abs() < 0.05 {
        format!("{} GiB", gib.round() as i64)
    } else {
        format!("{gib:.1} GiB")
    }
}

// ---------------------------------------------------------------------------
// ページ（askama）
// ---------------------------------------------------------------------------

/// ランディングページ。
#[derive(Template)]
#[template(path = "site/index.html")]
struct IndexPage {
    ctx: SiteContext,
    stats: InventoryStats,
    gpus: Vec<PublicGpu>,
    has_gpus: bool,
}

/// 料金。
#[derive(Template)]
#[template(path = "site/pricing.html")]
struct PricingPage {
    ctx: SiteContext,
    updated: &'static str,
}

/// 提供者募集。
#[derive(Template)]
#[template(path = "site/providers.html")]
struct ProvidersPage {
    ctx: SiteContext,
    updated: &'static str,
}

/// 利用規約（叩き台）。
#[derive(Template)]
#[template(path = "site/terms.html")]
struct TermsPage {
    ctx: SiteContext,
    updated: &'static str,
}

/// プライバシーポリシー（叩き台）。
#[derive(Template)]
#[template(path = "site/privacy.html")]
struct PrivacyPage {
    ctx: SiteContext,
    updated: &'static str,
}

/// 使い方。
#[derive(Template)]
#[template(path = "site/how.html")]
struct HowPage {
    ctx: SiteContext,
    updated: &'static str,
}

// ---------------------------------------------------------------------------
// ハンドラ
// ---------------------------------------------------------------------------

/// / の入口。ログイン済みなら既存ダッシュボード、未ログインなら LP。
pub async fn home(State(app): State<Arc<crate::App>>, headers: HeaderMap) -> Response {
    if auth::identity(&app.db, &headers).is_some() {
        // 既存のダッシュボードをそのまま再利用する（URL は / のまま）。
        return crate::pages::dashboard(State(app.clone()), headers).await;
    }
    let ctx = SiteContext::new(&app, "home", false);
    let (stats, gpus) = collect_inventory(&app);
    let has_gpus = !gpus.is_empty();
    render(IndexPage {
        ctx,
        stats,
        gpus,
        has_gpus,
    })
}

/// GET /pricing/
async fn pricing(State(app): State<Arc<crate::App>>, headers: HeaderMap) -> Response {
    let ctx = SiteContext::for_page(&app, &headers, "pricing");
    render(PricingPage {
        ctx,
        updated: LEGAL_UPDATED,
    })
}

/// GET /providers/
async fn providers(State(app): State<Arc<crate::App>>, headers: HeaderMap) -> Response {
    let ctx = SiteContext::for_page(&app, &headers, "providers");
    render(ProvidersPage {
        ctx,
        updated: LEGAL_UPDATED,
    })
}

/// GET /terms/
async fn terms(State(app): State<Arc<crate::App>>, headers: HeaderMap) -> Response {
    let ctx = SiteContext::for_page(&app, &headers, "terms");
    render(TermsPage {
        ctx,
        updated: LEGAL_UPDATED,
    })
}

/// GET /privacy/
async fn privacy(State(app): State<Arc<crate::App>>, headers: HeaderMap) -> Response {
    let ctx = SiteContext::for_page(&app, &headers, "privacy");
    render(PrivacyPage {
        ctx,
        updated: LEGAL_UPDATED,
    })
}

/// GET /how/
async fn how(State(app): State<Arc<crate::App>>, headers: HeaderMap) -> Response {
    let ctx = SiteContext::for_page(&app, &headers, "how");
    render(HowPage {
        ctx,
        updated: LEGAL_UPDATED,
    })
}

/// GET /static/site.css
async fn site_css() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=300"),
        ],
        SITE_CSS,
    )
        .into_response()
}

/// GET /healthz（認証不要の死活監視）。
pub async fn healthz() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        r#"{"status":"ok"}"#,
    )
        .into_response()
}

/// テンプレートを描画して HTML で返す。
fn render(page: impl Template) -> Response {
    match page.render() {
        Ok(body) => web::html(body),
        Err(exc) => web::error_page(
            StatusCode::INTERNAL_SERVER_ERROR,
            "ページを表示できませんでした（500）",
            &format!("テンプレートの描画に失敗しました: {exc}"),
        ),
    }
}

/// 末尾スラッシュなしの URL を正規の URL へ恒久リダイレクトする。
fn redirect_route(target: &'static str) -> MethodRouter<Arc<crate::App>> {
    get(move || async move {
        let mut response = Response::new(axum::body::Body::empty());
        *response.status_mut() = StatusCode::PERMANENT_REDIRECT;
        response
            .headers_mut()
            .insert(header::LOCATION, header::HeaderValue::from_static(target));
        response
    })
}

// ---------------------------------------------------------------------------
// ルーター
// ---------------------------------------------------------------------------

/// 公開サイトのルーター。/ は含めない（web::router 側で site::home に差し替える）。
pub fn router(app: Arc<crate::App>) -> Router {
    Router::new()
        .route("/pricing/", get(pricing))
        .route("/pricing", redirect_route("/pricing/"))
        .route("/providers/", get(providers))
        .route("/providers", redirect_route("/providers/"))
        .route("/terms/", get(terms))
        .route("/terms", redirect_route("/terms/"))
        .route("/privacy/", get(privacy))
        .route("/privacy", redirect_route("/privacy/"))
        .route("/how/", get(how))
        .route("/how", redirect_route("/how/"))
        .route("/healthz", get(healthz))
        .route("/static/site.css", get(site_css))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            HeaderValue::from_static("strict-origin-when-cross-origin"),
        ))
        .with_state(app)
}
