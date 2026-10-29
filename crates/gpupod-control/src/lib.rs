//! 中央サーバー（gpupod-control）。
//!
//! Python 版 `portal` / `control_panel`（Django）の移植。HTTP は axum、
//! 永続化は SQLite（rusqlite）、中継は russh、テンプレートは askama。

pub mod admin;
pub mod api;
pub mod auth;
pub mod benchmarks;
pub mod config;
pub mod db;
pub mod fmt;
pub mod jsonfmt;
pub mod models;
pub mod pages;
pub mod pyrepr;
pub mod relay;
pub mod services;
pub mod web;

pub use config::Config;
pub use db::{Db, SharedDb};

use std::sync::Arc;

/// サーバー全体の共有状態（DB と設定）。
pub struct App {
    pub db: SharedDb,
    pub cfg: Arc<Config>,
}

impl App {
    /// 状態ディレクトリを用意し、DB を開いてマイグレーションを適用する。
    pub fn new(cfg: Config) -> anyhow::Result<Arc<App>> {
        cfg.ensure_state_dir()?;
        let db = Db::open(&cfg.db_path())?;
        db.migrate()?;
        Ok(Arc::new(App { db: Arc::new(db), cfg: Arc::new(cfg) }))
    }
}
