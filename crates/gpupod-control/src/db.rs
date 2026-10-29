//! SQLite 接続とマイグレーション（Django の最終スキーマを忠実に再現）。

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use rusqlite::{Connection, TransactionBehavior};

pub type SharedDb = Arc<Db>;

/// portal/migrations の全マイグレーションを反映した最終スキーマ。
/// Django が生成する DDL に合わせ、部分 UNIQUE INDEX は
/// one_active_gpu_lease (node_id, gpu_id) WHERE active とする。
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS auth_user (
    id integer NOT NULL PRIMARY KEY AUTOINCREMENT,
    password varchar(128) NOT NULL,
    last_login datetime NULL,
    is_superuser bool NOT NULL,
    username varchar(150) NOT NULL UNIQUE,
    first_name varchar(150) NOT NULL,
    last_name varchar(150) NOT NULL,
    email varchar(254) NOT NULL,
    is_staff bool NOT NULL,
    is_active bool NOT NULL,
    date_joined datetime NOT NULL
);

CREATE TABLE IF NOT EXISTS django_session (
    session_key varchar(40) NOT NULL PRIMARY KEY,
    session_data text NOT NULL,
    expire_date datetime NOT NULL
);

CREATE INDEX IF NOT EXISTS django_session_expire_date_id ON django_session (expire_date);

CREATE TABLE IF NOT EXISTS portal_workloadtemplate (
    id varchar(128) NOT NULL PRIMARY KEY,
    name varchar(100) NOT NULL,
    definition text NOT NULL,
    enabled bool NOT NULL
);

CREATE TABLE IF NOT EXISTS portal_node (
    id char(32) NOT NULL PRIMARY KEY,
    name varchar(100) NOT NULL,
    token_hash varchar(64) NOT NULL,
    enabled bool NOT NULL,
    desired_enabled bool NOT NULL,
    last_seen datetime NULL,
    snapshot text NOT NULL,
    statistics text NOT NULL,
    previous_token_hash varchar(64) NOT NULL,
    previous_token_until datetime NULL,
    pending_token_hash varchar(64) NOT NULL,
    pending_token_until datetime NULL,
    pending_rotation_id char(32) NULL,
    rotation_id char(32) NULL,
    update_channel varchar(16) NOT NULL,
    benchmark_download_after datetime NULL,
    benchmark_upload_after datetime NULL
);

CREATE TABLE IF NOT EXISTS portal_lease (
    id char(32) NOT NULL PRIMARY KEY,
    user_id integer NOT NULL REFERENCES auth_user (id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
    node_id char(32) NOT NULL REFERENCES portal_node (id) ON DELETE RESTRICT,
    gpu_id varchar(128) NOT NULL,
    template_id varchar(128) NOT NULL REFERENCES portal_workloadtemplate (id) ON DELETE RESTRICT,
    status varchar(24) NOT NULL,
    active bool NOT NULL,
    ssh_public_key text NOT NULL,
    connection_mode varchar(8) NOT NULL,
    error text NOT NULL,
    created_at datetime NOT NULL,
    updated_at datetime NOT NULL,
    memory_bytes bigint unsigned NOT NULL,
    disk_bytes bigint unsigned NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS one_active_gpu_lease ON portal_lease (node_id, gpu_id) WHERE active;

CREATE TABLE IF NOT EXISTS portal_nodecommand (
    id char(32) NOT NULL PRIMARY KEY,
    node_id char(32) NOT NULL REFERENCES portal_node (id) ON DELETE CASCADE,
    lease_id char(32) NULL REFERENCES portal_lease (id) ON DELETE CASCADE,
    kind varchar(16) NOT NULL,
    payload text NOT NULL,
    done bool NOT NULL,
    result text NOT NULL,
    created_at datetime NOT NULL
);

CREATE TABLE IF NOT EXISTS portal_relayroute (
    id integer NOT NULL PRIMARY KEY AUTOINCREMENT,
    lease_id char(32) NOT NULL REFERENCES portal_lease (id) ON DELETE CASCADE,
    container_port varchar(12) NOT NULL,
    relay_port integer unsigned NOT NULL UNIQUE
);

CREATE UNIQUE INDEX IF NOT EXISTS one_route_per_lease_port ON portal_relayroute (lease_id, container_port);

CREATE TABLE IF NOT EXISTS portal_agentrelease (
    sequence integer unsigned NOT NULL PRIMARY KEY,
    version varchar(64) NOT NULL,
    target varchar(200) NOT NULL,
    channel varchar(16) NOT NULL,
    rollout_percent smallint unsigned NOT NULL,
    enabled bool NOT NULL
);
"#;

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Db> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(Duration::from_secs(30))?;
        Ok(Db { conn: Mutex::new(conn) })
    }

    /// メモリ上の一時 DB（テスト用）。
    pub fn open_memory() -> rusqlite::Result<Db> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(Duration::from_secs(30))?;
        Ok(Db { conn: Mutex::new(conn) })
    }

    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|exc| exc.into_inner())
    }

    /// 起動時にマイグレーションを適用する（全て CREATE ... IF NOT EXISTS）。
    pub fn migrate(&self) -> rusqlite::Result<()> {
        let conn = self.lock();
        conn.execute_batch(DDL)?;
        Ok(())
    }
}

/// UTC の現在時刻。
pub fn now() -> DateTime<Utc> {
    Utc::now()
}

/// Django の SQLite 保存形式（秒単位なら小数なし、それ以外は 6 桁）。
pub fn fmt_dt(value: DateTime<Utc>) -> String {
    let micros = value.timestamp_subsec_micros();
    let base = value.format("%Y-%m-%d %H:%M:%S").to_string();
    if micros == 0 {
        base
    } else {
        format!("{base}.{micros:06}")
    }
}

/// 上記形式の読み込み。
pub fn parse_dt(value: &str) -> Option<DateTime<Utc>> {
    let naive = if let Some((head, fraction)) = value.split_once('.') {
        let micros: u32 = format!("{:0<6}", &fraction[..fraction.len().min(6)]).parse().ok()?;
        let head = NaiveDateTime::parse_from_str(head, "%Y-%m-%d %H:%M:%S").ok()?;
        head + chrono::Duration::microseconds(micros as i64)
    } else {
        NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S").ok()?
    };
    Some(Utc.from_utc_datetime(&naive))
}

/// テンプレート表示用（Django は tz 情報つきで表示する）。
pub fn dt_label(value: &str) -> String {
    format!("{value}+00:00")
}

/// 文字列として保存された JSON を読む。
pub fn parse_json(value: &str) -> serde_json::Value {
    serde_json::from_str(value).unwrap_or(serde_json::Value::Null)
}

/// トランザクション（Django の transaction.atomic / IMMEDIATE 相当）。
pub fn immediate(conn: &mut Connection) -> rusqlite::Result<rusqlite::Transaction<'_>> {
    conn.transaction_with_behavior(TransactionBehavior::Immediate)
}
