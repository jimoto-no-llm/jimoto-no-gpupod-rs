//! DB 行の型とクエリ（portal/models.py に対応）。

use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};

use crate::db::{fmt_dt, now, parse_dt, parse_json};

/// auth_user の 1 行。
#[derive(Debug, Clone)]
pub struct UserRow {
    pub id: i64,
    pub password: String,
    pub last_login: Option<String>,
    pub is_superuser: bool,
    pub username: String,
    pub first_name: String,
    pub last_name: String,
    pub email: String,
    pub is_staff: bool,
    pub is_active: bool,
    pub date_joined: String,
}

const USER_COLUMNS: &str = "id, password, last_login, is_superuser, username, first_name, last_name, email, is_staff, is_active, date_joined";

fn user_from_row(row: &Row<'_>) -> rusqlite::Result<UserRow> {
    Ok(UserRow {
        id: row.get("id")?,
        password: row.get("password")?,
        last_login: row.get("last_login")?,
        is_superuser: row.get("is_superuser")?,
        username: row.get("username")?,
        first_name: row.get("first_name")?,
        last_name: row.get("last_name")?,
        email: row.get("email")?,
        is_staff: row.get("is_staff")?,
        is_active: row.get("is_active")?,
        date_joined: row.get("date_joined")?,
    })
}

pub fn find_user(conn: &Connection, user_id: i64) -> rusqlite::Result<Option<UserRow>> {
    conn.query_row(
        &format!("SELECT {USER_COLUMNS} FROM auth_user WHERE id = ?1"),
        params![user_id],
        user_from_row,
    )
    .optional()
}

pub fn find_user_by_username(
    conn: &Connection,
    username: &str,
) -> rusqlite::Result<Option<UserRow>> {
    conn.query_row(
        &format!("SELECT {USER_COLUMNS} FROM auth_user WHERE username = ?1"),
        params![username],
        user_from_row,
    )
    .optional()
}

pub fn list_users(conn: &Connection) -> rusqlite::Result<Vec<UserRow>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {USER_COLUMNS} FROM auth_user ORDER BY rowid"
    ))?;
    let rows = statement.query_map([], user_from_row)?;
    rows.collect()
}

#[allow(clippy::too_many_arguments)]
pub fn insert_user(
    conn: &Connection,
    username: &str,
    password: &str,
    is_superuser: bool,
    is_staff: bool,
    is_active: bool,
    email: &str,
) -> rusqlite::Result<i64> {
    let joined = fmt_dt(now());
    conn.execute(
        "INSERT INTO auth_user (password, last_login, is_superuser, username, first_name, last_name,
                                email, is_staff, is_active, date_joined)
         VALUES (?1, NULL, ?2, ?3, '', '', ?4, ?5, ?6, ?7)",
        params![password, is_superuser, username, email, is_staff, is_active, joined],
    )?;
    Ok(conn.last_insert_rowid())
}

/// portal_node の 1 行。
#[derive(Debug, Clone)]
pub struct NodeRow {
    /// UUID の hex 表現（32 文字）。
    pub id: String,
    pub name: String,
    pub token_hash: String,
    pub previous_token_hash: String,
    pub previous_token_until: Option<String>,
    pub pending_token_hash: String,
    pub pending_token_until: Option<String>,
    pub pending_rotation_id: Option<String>,
    pub rotation_id: Option<String>,
    pub update_channel: String,
    pub enabled: bool,
    pub desired_enabled: bool,
    pub last_seen: Option<String>,
    pub snapshot: serde_json::Value,
    pub statistics: serde_json::Value,
    pub benchmark_download_after: Option<String>,
    pub benchmark_upload_after: Option<String>,
}

impl NodeRow {
    /// ハイフン付き UUID 文字列（Python の str(node.pk)）。
    pub fn uuid_str(&self) -> String {
        hex_to_uuid(&self.id)
    }

    /// online: 最終通信が NODE_TIMEOUT_SECONDS 以内。
    pub fn online(&self, now_text: &str) -> bool {
        match &self.last_seen {
            None => false,
            Some(seen) => match (parse_dt(seen), parse_dt(now_text)) {
                (Some(seen), Some(now_value)) => {
                    seen > now_value
                        - chrono::Duration::seconds(crate::config::NODE_TIMEOUT_SECONDS)
                }
                _ => false,
            },
        }
    }

    /// accepting: enabled && online && desired_enabled && snapshot.accepting。
    pub fn accepting(&self, now_text: &str) -> bool {
        self.enabled
            && self.online(now_text)
            && self.desired_enabled
            && self.snapshot.get("accepting").and_then(|v| v.as_bool()) == Some(true)
    }

    pub fn direct_host(&self) -> &str {
        self.snapshot
            .get("direct_host")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    }
}

const NODE_COLUMNS: &str =
    "id, name, token_hash, previous_token_hash, previous_token_until, pending_token_hash,
    pending_token_until, pending_rotation_id, rotation_id, update_channel, enabled, desired_enabled,
    last_seen, snapshot, statistics, benchmark_download_after, benchmark_upload_after";

fn node_from_row(row: &Row<'_>) -> rusqlite::Result<NodeRow> {
    Ok(NodeRow {
        id: row.get("id")?,
        name: row.get("name")?,
        token_hash: row.get("token_hash")?,
        previous_token_hash: row.get("previous_token_hash")?,
        previous_token_until: row.get("previous_token_until")?,
        pending_token_hash: row.get("pending_token_hash")?,
        pending_token_until: row.get("pending_token_until")?,
        pending_rotation_id: row.get("pending_rotation_id")?,
        rotation_id: row.get("rotation_id")?,
        update_channel: row.get("update_channel")?,
        enabled: row.get("enabled")?,
        desired_enabled: row.get("desired_enabled")?,
        last_seen: row.get("last_seen")?,
        snapshot: parse_json(&row.get::<_, String>("snapshot")?),
        statistics: parse_json(&row.get::<_, String>("statistics")?),
        benchmark_download_after: row.get("benchmark_download_after")?,
        benchmark_upload_after: row.get("benchmark_upload_after")?,
    })
}

/// URL の UUID を DB の hex 形式へ。
pub fn uuid_to_hex(value: &str) -> Option<String> {
    let trimmed = value.trim_matches(|c| c == '{' || c == '}');
    let compact: String = trimmed.chars().filter(|c| *c != '-').collect();
    if compact.len() != 32 || !compact.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(compact.to_ascii_lowercase())
}

/// hex を Python の str(uuid.UUID) 形式へ。
pub fn hex_to_uuid(value: &str) -> String {
    let compact: String = value.chars().filter(|c| *c != '-').collect();
    if compact.len() != 32 {
        return value.to_string();
    }
    format!(
        "{}-{}-{}-{}-{}",
        &compact[0..8],
        &compact[8..12],
        &compact[12..16],
        &compact[16..20],
        &compact[20..32]
    )
}

pub fn new_uuid_hex() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

pub fn get_node(conn: &Connection, hex_id: &str) -> rusqlite::Result<Option<NodeRow>> {
    conn.query_row(
        &format!("SELECT {NODE_COLUMNS} FROM portal_node WHERE id = ?1"),
        params![hex_id],
        node_from_row,
    )
    .optional()
}

pub fn get_node_enabled(conn: &Connection, hex_id: &str) -> rusqlite::Result<Option<NodeRow>> {
    conn.query_row(
        &format!("SELECT {NODE_COLUMNS} FROM portal_node WHERE id = ?1 AND enabled"),
        params![hex_id],
        node_from_row,
    )
    .optional()
}

/// Django は ORDER BY なし = テーブル走査順。
pub fn list_nodes(conn: &Connection) -> rusqlite::Result<Vec<NodeRow>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {NODE_COLUMNS} FROM portal_node ORDER BY rowid"
    ))?;
    let rows = statement.query_map([], node_from_row)?;
    rows.collect()
}

pub fn insert_node(conn: &Connection, name: &str, token_hash: &str) -> rusqlite::Result<NodeRow> {
    let id = new_uuid_hex();
    conn.execute(
        "INSERT INTO portal_node (id, name, token_hash, enabled, desired_enabled, last_seen, snapshot, statistics,
                                  previous_token_hash, previous_token_until, pending_token_hash, pending_token_until,
                                  pending_rotation_id, rotation_id, update_channel, benchmark_download_after,
                                  benchmark_upload_after)
         VALUES (?1, ?2, ?3, 1, 0, NULL, '{}', '{}', '', NULL, '', NULL, NULL, NULL, 'stable', NULL, NULL)",
        params![id, name, token_hash],
    )?;
    get_node(conn, &id)?.ok_or_else(|| rusqlite::Error::QueryReturnedNoRows)
}

pub fn save_node_snapshot(
    conn: &Connection,
    hex_id: &str,
    snapshot: &serde_json::Value,
    statistics: &serde_json::Value,
    last_seen: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE portal_node SET snapshot = ?2, statistics = ?3, last_seen = ?4 WHERE id = ?1",
        params![
            hex_id,
            snapshot.to_string(),
            statistics.to_string(),
            last_seen
        ],
    )?;
    Ok(())
}

/// portal_workloadtemplate の 1 行。
#[derive(Debug, Clone)]
pub struct TemplateRow {
    pub id: String,
    pub name: String,
    pub definition: serde_json::Value,
    pub enabled: bool,
}

fn template_from_row(row: &Row<'_>) -> rusqlite::Result<TemplateRow> {
    Ok(TemplateRow {
        id: row.get("id")?,
        name: row.get("name")?,
        definition: parse_json(&row.get::<_, String>("definition")?),
        enabled: row.get("enabled")?,
    })
}

pub fn get_template(conn: &Connection, id: &str) -> rusqlite::Result<Option<TemplateRow>> {
    conn.query_row(
        "SELECT id, name, definition, enabled FROM portal_workloadtemplate WHERE id = ?1",
        params![id],
        template_from_row,
    )
    .optional()
}

/// enabled なテンプレート（Python は ORDER BY なし）。
pub fn list_enabled_templates(conn: &Connection) -> rusqlite::Result<Vec<TemplateRow>> {
    let mut statement =
        conn.prepare("SELECT id, name, definition, enabled FROM portal_workloadtemplate WHERE enabled ORDER BY rowid")?;
    let rows = statement.query_map([], template_from_row)?;
    rows.collect()
}

pub fn list_templates(conn: &Connection) -> rusqlite::Result<Vec<TemplateRow>> {
    let mut statement = conn.prepare(
        "SELECT id, name, definition, enabled FROM portal_workloadtemplate ORDER BY rowid",
    )?;
    let rows = statement.query_map([], template_from_row)?;
    rows.collect()
}

/// sync-templates の upsert（既存は enabled を維持し、新規のみ enabled=1）。
pub fn upsert_template(
    conn: &Connection,
    id: &str,
    definition: &serde_json::Value,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO portal_workloadtemplate (id, name, definition, enabled) VALUES (?1, ?1, ?2, 1)
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, definition = excluded.definition",
        params![id, definition.to_string()],
    )?;
    Ok(())
}

pub fn disable_templates_except(conn: &Connection, keep: &[String]) -> rusqlite::Result<usize> {
    if keep.is_empty() {
        return conn.execute("UPDATE portal_workloadtemplate SET enabled = 0", []);
    }
    let placeholders = (1..=keep.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    conn.execute(
        &format!("UPDATE portal_workloadtemplate SET enabled = 0 WHERE id NOT IN ({placeholders})"),
        params_from_iter(keep.iter()),
    )
}

/// portal_lease の 1 行。
#[derive(Debug, Clone)]
pub struct LeaseRow {
    /// UUID の hex 表現。
    pub id: String,
    pub user_id: i64,
    pub node_id: String,
    pub gpu_id: String,
    pub template_id: String,
    pub status: String,
    pub active: bool,
    pub ssh_public_key: String,
    pub connection_mode: String,
    pub error: String,
    pub created_at: String,
    pub updated_at: String,
    pub memory_bytes: i64,
    pub disk_bytes: i64,
}

impl LeaseRow {
    pub fn uuid_str(&self) -> String {
        hex_to_uuid(&self.id)
    }

    /// Lease.resources プロパティ相当（0 なら None）。
    pub fn resources(&self) -> Option<(u64, u64)> {
        if self.memory_bytes == 0 || self.disk_bytes == 0 {
            None
        } else {
            Some((self.memory_bytes as u64, self.disk_bytes as u64))
        }
    }
}

const LEASE_COLUMNS: &str =
    "id, user_id, node_id, gpu_id, template_id, status, active, ssh_public_key,
    connection_mode, error, created_at, updated_at, memory_bytes, disk_bytes";

fn lease_from_row(row: &Row<'_>) -> rusqlite::Result<LeaseRow> {
    Ok(LeaseRow {
        id: row.get("id")?,
        user_id: row.get("user_id")?,
        node_id: row.get("node_id")?,
        gpu_id: row.get("gpu_id")?,
        template_id: row.get("template_id")?,
        status: row.get("status")?,
        active: row.get("active")?,
        ssh_public_key: row.get("ssh_public_key")?,
        connection_mode: row.get("connection_mode")?,
        error: row.get("error")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        memory_bytes: row.get("memory_bytes")?,
        disk_bytes: row.get("disk_bytes")?,
    })
}

pub fn get_lease(conn: &Connection, hex_id: &str) -> rusqlite::Result<Option<LeaseRow>> {
    conn.query_row(
        &format!("SELECT {LEASE_COLUMNS} FROM portal_lease WHERE id = ?1"),
        params![hex_id],
        lease_from_row,
    )
    .optional()
}

/// 自分の貸出（-created_at、30 件）。
pub fn list_user_leases(
    conn: &Connection,
    user_id: i64,
    limit: i64,
) -> rusqlite::Result<Vec<LeaseRow>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {LEASE_COLUMNS} FROM portal_lease WHERE user_id = ?1 ORDER BY created_at DESC, rowid DESC LIMIT ?2"
    ))?;
    let rows = statement.query_map(params![user_id, limit], lease_from_row)?;
    rows.collect()
}

/// 実行中・処理中の貸出（active、-created_at）。
pub fn list_active_leases(conn: &Connection) -> rusqlite::Result<Vec<LeaseRow>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {LEASE_COLUMNS} FROM portal_lease WHERE active ORDER BY created_at DESC, rowid DESC"
    ))?;
    let rows = statement.query_map([], lease_from_row)?;
    rows.collect()
}

pub fn list_node_active_leases(
    conn: &Connection,
    node_id: &str,
) -> rusqlite::Result<Vec<LeaseRow>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {LEASE_COLUMNS} FROM portal_lease WHERE node_id = ?1 AND active ORDER BY created_at DESC, rowid DESC"
    ))?;
    let rows = statement.query_map(params![node_id], lease_from_row)?;
    rows.collect()
}

pub fn active_lease_exists(
    conn: &Connection,
    node_id: &str,
    gpu_id: &str,
) -> rusqlite::Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM portal_lease WHERE node_id = ?1 AND gpu_id = ?2 AND active",
            params![node_id, gpu_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

pub fn insert_lease(
    conn: &Connection,
    user_id: i64,
    node_id: &str,
    gpu_id: &str,
    template_id: &str,
    ssh_public_key: &str,
    connection_mode: &str,
    memory_bytes: u64,
    disk_bytes: u64,
) -> rusqlite::Result<LeaseRow> {
    let id = new_uuid_hex();
    let stamp = fmt_dt(now());
    conn.execute(
        &format!(
            "INSERT INTO portal_lease ({LEASE_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, 'queued', 1, ?6, ?7, '', ?8, ?8, ?9, ?10)"
        ),
        params![
            id,
            user_id,
            node_id,
            gpu_id,
            template_id,
            ssh_public_key,
            connection_mode,
            stamp,
            memory_bytes as i64,
            disk_bytes as i64
        ],
    )?;
    get_lease(conn, &id)?.ok_or_else(|| rusqlite::Error::QueryReturnedNoRows)
}

pub fn save_lease(conn: &Connection, lease: &LeaseRow) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE portal_lease SET user_id = ?2, node_id = ?3, gpu_id = ?4, template_id = ?5,
                status = ?6, active = ?7, ssh_public_key = ?8, connection_mode = ?9, error = ?10,
                created_at = ?11, updated_at = ?12, memory_bytes = ?13, disk_bytes = ?14
         WHERE id = ?1",
        params![
            lease.id,
            lease.user_id,
            lease.node_id,
            lease.gpu_id,
            lease.template_id,
            lease.status,
            lease.active,
            lease.ssh_public_key,
            lease.connection_mode,
            lease.error,
            lease.created_at,
            lease.updated_at,
            lease.memory_bytes,
            lease.disk_bytes
        ],
    )?;
    Ok(())
}

/// updated_at を現在時刻に更新しつつ状態を書き込む。
pub fn touch_lease(conn: &Connection, lease: &LeaseRow) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE portal_lease SET status = ?2, active = ?3, error = ?4, memory_bytes = ?5, disk_bytes = ?6,
                                 updated_at = ?7 WHERE id = ?1",
        params![
            lease.id,
            lease.status,
            lease.active,
            lease.error,
            lease.memory_bytes,
            lease.disk_bytes,
            fmt_dt(now())
        ],
    )?;
    Ok(())
}

/// portal_nodecommand の 1 行。
#[derive(Debug, Clone)]
pub struct CommandRow {
    pub id: String,
    pub node_id: String,
    pub lease_id: Option<String>,
    pub kind: String,
    pub payload: serde_json::Value,
    pub done: bool,
    pub result: serde_json::Value,
    pub created_at: String,
}

fn command_from_row(row: &Row<'_>) -> rusqlite::Result<CommandRow> {
    Ok(CommandRow {
        id: row.get("id")?,
        node_id: row.get("node_id")?,
        lease_id: row.get("lease_id")?,
        kind: row.get("kind")?,
        payload: parse_json(&row.get::<_, String>("payload")?),
        done: row.get("done")?,
        result: parse_json(&row.get::<_, String>("result")?),
        created_at: row.get("created_at")?,
    })
}

pub fn insert_command(
    conn: &Connection,
    node_id: &str,
    lease_id: Option<&str>,
    kind: &str,
    payload: &serde_json::Value,
) -> rusqlite::Result<String> {
    let id = new_uuid_hex();
    conn.execute(
        "INSERT INTO portal_nodecommand (id, node_id, lease_id, kind, payload, done, result, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 0, '{}', ?6)",
        params![id, node_id, lease_id, kind, payload.to_string(), fmt_dt(now())],
    )?;
    Ok(id)
}

pub fn get_pending_command(
    conn: &Connection,
    id: &str,
    node_id: &str,
) -> rusqlite::Result<Option<CommandRow>> {
    conn.query_row(
        "SELECT id, node_id, lease_id, kind, payload, done, result, created_at
         FROM portal_nodecommand WHERE id = ?1 AND node_id = ?2 AND NOT done",
        params![id, node_id],
        command_from_row,
    )
    .optional()
}

/// 未完了コマンド（created_at, id 順、先頭 20 件）。
pub fn pending_commands(
    conn: &Connection,
    node_id: &str,
    limit: i64,
) -> rusqlite::Result<Vec<CommandRow>> {
    let mut statement = conn.prepare(
        "SELECT id, node_id, lease_id, kind, payload, done, result, created_at
         FROM portal_nodecommand WHERE node_id = ?1 AND NOT done ORDER BY created_at, id LIMIT ?2",
    )?;
    let rows = statement.query_map(params![node_id, limit], command_from_row)?;
    rows.collect()
}

pub fn mark_command_done(
    conn: &Connection,
    id: &str,
    result: &serde_json::Value,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE portal_nodecommand SET done = 1, result = ?2 WHERE id = ?1",
        params![id, result.to_string()],
    )?;
    Ok(())
}

/// portal_relayroute の 1 行。
#[derive(Debug, Clone)]
pub struct RouteRow {
    pub id: i64,
    pub lease_id: String,
    pub container_port: String,
    pub relay_port: i64,
}

fn route_from_row(row: &Row<'_>) -> rusqlite::Result<RouteRow> {
    Ok(RouteRow {
        id: row.get("id")?,
        lease_id: row.get("lease_id")?,
        container_port: row.get("container_port")?,
        relay_port: row.get("relay_port")?,
    })
}

pub fn routes_of_lease(conn: &Connection, lease_id: &str) -> rusqlite::Result<Vec<RouteRow>> {
    let mut statement = conn.prepare(
        "SELECT id, lease_id, container_port, relay_port FROM portal_relayroute WHERE lease_id = ?1 ORDER BY rowid",
    )?;
    let rows = statement.query_map(params![lease_id], route_from_row)?;
    rows.collect()
}

/// 予約済みの中継ポート（全レコード）。
pub fn used_relay_ports(conn: &Connection) -> rusqlite::Result<std::collections::BTreeSet<i64>> {
    let mut statement = conn.prepare("SELECT relay_port FROM portal_relayroute")?;
    let rows = statement.query_map([], |row| row.get(0))?;
    rows.collect()
}

pub fn insert_route(
    conn: &Connection,
    lease_id: &str,
    container_port: &str,
    relay_port: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO portal_relayroute (lease_id, container_port, relay_port) VALUES (?1, ?2, ?3)",
        params![lease_id, container_port, relay_port],
    )?;
    Ok(())
}

pub fn delete_routes_of_lease(conn: &Connection, lease_id: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM portal_relayroute WHERE lease_id = ?1",
        params![lease_id],
    )
}

/// 同期応答に入れる経路（active かつ running の貸出のみ）。
pub fn routes_for_node(conn: &Connection, node_id: &str) -> rusqlite::Result<Vec<RouteRow>> {
    let mut statement = conn.prepare(
        "SELECT r.id, r.lease_id, r.container_port, r.relay_port
         FROM portal_relayroute r JOIN portal_lease l ON l.id = r.lease_id
         WHERE l.node_id = ?1 AND l.active AND l.status = 'running' ORDER BY r.rowid",
    )?;
    let rows = statement.query_map(params![node_id], route_from_row)?;
    rows.collect()
}

/// 中継が許可してよいポート（active かつ running の貸出のみ）。
pub fn relay_ports_for_node(
    conn: &Connection,
    node_id: &str,
) -> rusqlite::Result<std::collections::BTreeSet<i64>> {
    let mut statement = conn.prepare(
        "SELECT r.relay_port FROM portal_relayroute r JOIN portal_lease l ON l.id = r.lease_id
         WHERE l.node_id = ?1 AND l.active AND l.status = 'running'",
    )?;
    let rows = statement.query_map(params![node_id], |row| row.get(0))?;
    rows.collect()
}

/// portal_agentrelease の 1 行。
#[derive(Debug, Clone)]
pub struct ReleaseRow {
    pub sequence: i64,
    pub version: String,
    pub target: String,
    pub channel: String,
    pub rollout_percent: i64,
    pub enabled: bool,
}

fn release_from_row(row: &Row<'_>) -> rusqlite::Result<ReleaseRow> {
    Ok(ReleaseRow {
        sequence: row.get("sequence")?,
        version: row.get("version")?,
        target: row.get("target")?,
        channel: row.get("channel")?,
        rollout_percent: row.get("rollout_percent")?,
        enabled: row.get("enabled")?,
    })
}

/// 有効な更新（channel 一致、-sequence 順）。
pub fn list_releases(conn: &Connection, channel: &str) -> rusqlite::Result<Vec<ReleaseRow>> {
    let mut statement = conn.prepare(
        "SELECT sequence, version, target, channel, rollout_percent, enabled
         FROM portal_agentrelease WHERE enabled AND channel = ?1 ORDER BY sequence DESC",
    )?;
    let rows = statement.query_map(params![channel], release_from_row)?;
    rows.collect()
}

pub fn list_all_releases(conn: &Connection) -> rusqlite::Result<Vec<ReleaseRow>> {
    let mut statement = conn.prepare(
        "SELECT sequence, version, target, channel, rollout_percent, enabled
         FROM portal_agentrelease ORDER BY sequence DESC",
    )?;
    let rows = statement.query_map([], release_from_row)?;
    rows.collect()
}

pub fn get_release(conn: &Connection, sequence: i64) -> rusqlite::Result<Option<ReleaseRow>> {
    conn.query_row(
        "SELECT sequence, version, target, channel, rollout_percent, enabled
         FROM portal_agentrelease WHERE sequence = ?1",
        params![sequence],
        release_from_row,
    )
    .optional()
}

pub fn upsert_release(conn: &Connection, release: &ReleaseRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO portal_agentrelease (sequence, version, target, channel, rollout_percent, enabled)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(sequence) DO UPDATE SET version = excluded.version, target = excluded.target,
             channel = excluded.channel, rollout_percent = excluded.rollout_percent, enabled = excluded.enabled",
        params![
            release.sequence,
            release.version,
            release.target,
            release.channel,
            release.rollout_percent,
            release.enabled
        ],
    )?;
    Ok(())
}

/// セッション 1 行（django_session）。
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub session_key: String,
    pub session_data: String,
    pub expire_date: String,
}

pub fn get_session(conn: &Connection, key: &str) -> rusqlite::Result<Option<SessionRow>> {
    conn.query_row(
        "SELECT session_key, session_data, expire_date FROM django_session WHERE session_key = ?1",
        params![key],
        |row| {
            Ok(SessionRow {
                session_key: row.get("session_key")?,
                session_data: row.get("session_data")?,
                expire_date: row.get("expire_date")?,
            })
        },
    )
    .optional()
}

pub fn write_session(
    conn: &Connection,
    key: &str,
    data: &str,
    expire: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO django_session (session_key, session_data, expire_date) VALUES (?1, ?2, ?3)
         ON CONFLICT(session_key) DO UPDATE SET session_data = excluded.session_data,
                                                expire_date = excluded.expire_date",
        params![key, data, expire],
    )?;
    Ok(())
}

pub fn delete_session(conn: &Connection, key: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM django_session WHERE session_key = ?1",
        params![key],
    )
}

pub fn purge_expired_sessions(conn: &Connection, now_text: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM django_session WHERE expire_date <= ?1",
        params![now_text],
    )
}

/// portal_oauth_identity の 1 行。
#[derive(Debug, Clone)]
pub struct OAuthIdentityRow {
    pub provider: String,
    pub subject: String,
    pub user_id: i64,
    pub created_at: String,
}

fn oauth_identity_from_row(row: &Row<'_>) -> rusqlite::Result<OAuthIdentityRow> {
    Ok(OAuthIdentityRow {
        provider: row.get("provider")?,
        subject: row.get("subject")?,
        user_id: row.get("user_id")?,
        created_at: row.get("created_at")?,
    })
}

pub fn find_oauth_identity(
    conn: &Connection,
    provider: &str,
    subject: &str,
) -> rusqlite::Result<Option<OAuthIdentityRow>> {
    conn.query_row(
        "SELECT provider, subject, user_id, created_at FROM portal_oauth_identity
         WHERE provider = ?1 AND subject = ?2",
        params![provider, subject],
        oauth_identity_from_row,
    )
    .optional()
}

pub fn find_oauth_identity_by_user(
    conn: &Connection,
    provider: &str,
    user_id: i64,
) -> rusqlite::Result<Option<OAuthIdentityRow>> {
    conn.query_row(
        "SELECT provider, subject, user_id, created_at FROM portal_oauth_identity
         WHERE provider = ?1 AND user_id = ?2",
        params![provider, user_id],
        oauth_identity_from_row,
    )
    .optional()
}

pub fn insert_oauth_identity(
    conn: &Connection,
    provider: &str,
    subject: &str,
    user_id: i64,
    created_at: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO portal_oauth_identity (provider, subject, user_id, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![provider, subject, user_id, created_at],
    )?;
    Ok(())
}

pub fn delete_oauth_identity_for_user(
    conn: &Connection,
    provider: &str,
    user_id: i64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM portal_oauth_identity WHERE provider = ?1 AND user_id = ?2",
        params![provider, user_id],
    )
}

/// portal_userprofile の 1 行。
#[derive(Debug, Clone)]
pub struct UserProfileRow {
    pub user_id: i64,
    pub display_name: String,
    pub avatar_url: String,
    pub updated_at: String,
}

pub fn get_user_profile(
    conn: &Connection,
    user_id: i64,
) -> rusqlite::Result<Option<UserProfileRow>> {
    conn.query_row(
        "SELECT user_id, display_name, avatar_url, updated_at FROM portal_userprofile WHERE user_id = ?1",
        params![user_id],
        |row| {
            Ok(UserProfileRow {
                user_id: row.get("user_id")?,
                display_name: row.get("display_name")?,
                avatar_url: row.get("avatar_url")?,
                updated_at: row.get("updated_at")?,
            })
        },
    )
    .optional()
}

pub fn upsert_user_profile(
    conn: &Connection,
    user_id: i64,
    display_name: &str,
    avatar_url: &str,
    updated_at: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO portal_userprofile (user_id, display_name, avatar_url, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(user_id) DO UPDATE SET display_name = excluded.display_name,
                                            avatar_url = excluded.avatar_url,
                                            updated_at = excluded.updated_at",
        params![user_id, display_name, avatar_url, updated_at],
    )?;
    Ok(())
}

/// portal_oauth_state の 1 行（OAuth の一時 state）。
#[derive(Debug, Clone)]
pub struct OAuthStateRow {
    pub state: String,
    pub code_verifier: String,
    pub link_user_id: Option<i64>,
    pub expires_at: String,
    pub created_at: String,
}

fn oauth_state_from_row(row: &Row<'_>) -> rusqlite::Result<OAuthStateRow> {
    Ok(OAuthStateRow {
        state: row.get("state")?,
        code_verifier: row.get("code_verifier")?,
        link_user_id: row.get("link_user_id")?,
        expires_at: row.get("expires_at")?,
        created_at: row.get("created_at")?,
    })
}

pub fn insert_oauth_state(
    conn: &Connection,
    state: &str,
    code_verifier: &str,
    link_user_id: Option<i64>,
    expires_at: &str,
    created_at: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO portal_oauth_state (state, code_verifier, link_user_id, expires_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![state, code_verifier, link_user_id, expires_at, created_at],
    )?;
    Ok(())
}

/// state を取り出して同時に削除する（単回使用の保証）。
pub fn take_oauth_state(conn: &Connection, state: &str) -> rusqlite::Result<Option<OAuthStateRow>> {
    conn.query_row(
        "DELETE FROM portal_oauth_state WHERE state = ?1
         RETURNING state, code_verifier, link_user_id, expires_at, created_at",
        params![state],
        oauth_state_from_row,
    )
    .optional()
}

pub fn purge_expired_oauth_states(conn: &Connection, now_text: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM portal_oauth_state WHERE expires_at <= ?1",
        params![now_text],
    )
}
