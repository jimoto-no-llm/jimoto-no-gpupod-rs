//! portal/services.py の 1:1 移植。
//!
//! エラーメッセージは Python 版と同一文字列（日本語を含む）にする。

use std::collections::{BTreeMap, BTreeSet};

use gpupod_common::identifier::identifier;
use gpupod_common::protocol::{NodeCommand, RelayRoute, ReleaseAssignment, SyncResponse};
use gpupod_common::resources::{self, Allocation, Pool};
use gpupod_common::token::{digest_equals, token_digest};
use gpupod_common::Template;
use rusqlite::params;

use crate::config::Config;
use crate::db::{fmt_dt, immediate, now, parse_dt};
use crate::models::{self, hex_to_uuid, LeaseRow, NodeRow, TemplateRow};

/// サービス層の失敗。ビューはこれを見て文言とステータスを決める。
#[derive(Debug)]
pub enum ServiceError {
    /// AgentError（Python 版の AgentError と同一メッセージ）。
    Agent(String),
    /// KeyError / ValueError / TypeError 等（ビューは "Invalid payload" 等に置換）。
    Payload,
    /// ビューが汎用メッセージに置き換える失敗（KeyError / IntegrityError 等）。
    Generic,
    /// DB 障害（Python 版では 500 になる）。
    Db(String),
}

impl ServiceError {
    pub fn agent(message: impl Into<String>) -> ServiceError {
        ServiceError::Agent(message.into())
    }
}

impl From<rusqlite::Error> for ServiceError {
    fn from(value: rusqlite::Error) -> Self {
        match value {
            rusqlite::Error::SqliteFailure(info, _) if info.code == rusqlite::ErrorCode::ConstraintViolation => {
                ServiceError::Generic
            }
            other => ServiceError::Db(other.to_string()),
        }
    }
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceError::Agent(message) => write!(formatter, "{message}"),
            ServiceError::Payload => write!(formatter, "Invalid payload"),
            ServiceError::Generic => write!(formatter, "Invalid request"),
            ServiceError::Db(message) => write!(formatter, "{message}"),
        }
    }
}

pub type Result<T> = std::result::Result<T, ServiceError>;

/// JSON の真偽判定（Python の真偽値規則）。
pub fn is_truthy(value: Option<&serde_json::Value>) -> bool {
    match value {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(flag)) => *flag,
        Some(serde_json::Value::Number(number)) => number.as_f64().map(|v| v != 0.0).unwrap_or(true),
        Some(serde_json::Value::String(text)) => !text.is_empty(),
        Some(serde_json::Value::Array(items)) => !items.is_empty(),
        Some(serde_json::Value::Object(map)) => !map.is_empty(),
    }
}

/// token_allowed: 現在の鍵、または有効期限内の直前の鍵を認める。
pub fn token_allowed(node: &NodeRow, digest: &str, now_text: &str) -> bool {
    if digest_equals(&node.token_hash, digest) {
        return true;
    }
    match &node.previous_token_until {
        None => false,
        Some(until) => {
            let valid = match (parse_dt(until), parse_dt(now_text)) {
                (Some(until), Some(now_value)) => until > now_value,
                _ => false,
            };
            valid && digest_equals(&node.previous_token_hash, digest)
        }
    }
}

/// authenticate_node。
pub fn authenticate_node(
    conn: &rusqlite::Connection,
    node_id: &str,
    token: &str,
    allow_previous: bool,
) -> Option<NodeRow> {
    let node = models::get_node_enabled(conn, node_id).ok().flatten()?;
    if token.is_empty() {
        return None;
    }
    let digest = token_digest(token);
    let valid = if allow_previous {
        token_allowed(&node, &digest, &fmt_dt(now()))
    } else {
        digest_equals(&node.token_hash, &digest)
    };
    valid.then_some(node)
}

fn valid_rotation_token(token: &str) -> bool {
    (40..=128).contains(&token.len())
        && token.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// prepare_rotation。
pub fn prepare_rotation(
    conn: &rusqlite::Connection,
    node: &NodeRow,
    rotation_id: &str,
    token: &str,
) -> Result<serde_json::Value> {
    let rotation = uuid::Uuid::parse_str(rotation_id).map_err(|_| ServiceError::Payload)?;
    // Python 版の select_for_update 相当: 呼び出し元の古い行ではなく直近の状態を読む。
    let node = models::get_node(conn, &node.id)?.ok_or(ServiceError::Payload)?;
    if !valid_rotation_token(token) {
        return Err(ServiceError::agent("Invalid replacement credential"));
    }
    let digest = token_digest(token);
    if digest_equals(&node.token_hash, &digest) {
        return Err(ServiceError::agent("The replacement must be a fresh credential"));
    }
    let now_value = now();
    let pending_valid = match &node.pending_token_until {
        Some(until) => match parse_dt(until) {
            Some(until) => until > now_value,
            None => false,
        },
        None => false,
    };
    if pending_valid {
        let rotation_hex = rotation.simple().to_string();
        let same_rotation = node.pending_rotation_id.as_deref() == Some(rotation_hex.as_str());
        let same_digest = digest_equals(&node.pending_token_hash, &digest);
        if !same_rotation || !same_digest {
            return Err(ServiceError::agent("Another credential rotation is pending"));
        }
    }
    conn.execute(
        "UPDATE portal_node SET pending_token_hash = ?2, pending_rotation_id = ?3, pending_token_until = ?4
         WHERE id = ?1",
        params![
            node.id,
            digest,
            rotation.simple().to_string(),
            fmt_dt(now_value + chrono::Duration::hours(24))
        ],
    )?;
    Ok(serde_json::json!({"id": rotation.to_string(), "status": "prepared"}))
}

/// commit_rotation。
pub fn commit_rotation(
    conn: &rusqlite::Connection,
    node_id: &str,
    rotation_id: &str,
    token: &str,
) -> Result<serde_json::Value> {
    let rotation = uuid::Uuid::parse_str(rotation_id).map_err(|_| ServiceError::Payload)?;
    let node = models::get_node_enabled(conn, node_id)?.ok_or(ServiceError::Payload)?;
    let digest = token_digest(token);
    let rotation_hex = rotation.simple().to_string();
    if node.rotation_id.as_deref() == Some(rotation_hex.as_str()) && digest_equals(&node.token_hash, &digest) {
        return Ok(serde_json::json!({"id": rotation.to_string(), "status": "complete"}));
    }
    let now_value = now();
    let pending_valid = match &node.pending_token_until {
        Some(until) => match parse_dt(until) {
            Some(until) => until > now_value,
            None => false,
        },
        None => false,
    };
    let pending_ok = node.pending_rotation_id.as_deref() == Some(rotation_hex.as_str())
        && pending_valid
        && digest_equals(&node.pending_token_hash, &digest);
    if !pending_ok {
        return Err(ServiceError::agent("Replacement credential was not prepared"));
    }
    conn.execute(
        "UPDATE portal_node SET previous_token_hash = ?2, previous_token_until = ?3, token_hash = ?4,
                                rotation_id = ?5, pending_token_hash = '', pending_rotation_id = NULL,
                                pending_token_until = NULL
         WHERE id = ?1",
        params![
            node.id,
            node.token_hash,
            fmt_dt(now_value + chrono::Duration::seconds(120)),
            digest,
            rotation_hex,
        ],
    )?;
    Ok(serde_json::json!({"id": rotation.to_string(), "status": "complete"}))
}

/// assigned_release: cohort は sha256(str(node.pk)) の先頭 8hex % 10000（安定順）。
pub fn assigned_release(conn: &rusqlite::Connection, node: &NodeRow) -> Option<ReleaseAssignment> {
    let bucket = cohort_bucket(&node.uuid_str());
    let releases = models::list_releases(conn, &node.update_channel).ok()?;
    for release in releases {
        if release.rollout_percent > 0
            && release.rollout_percent <= 100
            && bucket < (release.rollout_percent * 100) as u32
        {
            return Some(ReleaseAssignment {
                sequence: release.sequence as u32,
                version: release.version,
                target: release.target,
            });
        }
    }
    None
}

/// cohort bucket（sha256(str(node.pk)) 先頭 8hex % 10000）。
pub fn cohort_bucket(uuid_text: &str) -> u32 {
    use sha2::{Digest, Sha256};
    let digest = hex::encode(Sha256::digest(uuid_text.as_bytes()));
    let head = u32::from_str_radix(&digest[..8], 16).unwrap_or(0);
    head % 10000
}

/// enroll_node: 新しい機材キーを発行する。
pub fn enroll_node(
    conn: &rusqlite::Connection,
    name: &str,
) -> std::result::Result<(NodeRow, String), rusqlite::Error> {
    let token = crate::config::random_token(48);
    let trimmed: String = name.chars().take(100).collect();
    let node = models::insert_node(conn, &trimmed, &token_digest(&token))?;
    Ok((node, token))
}

/// compatible_templates: 中央で有効で、指紋が一致し vendor が合うテンプレート。
pub fn compatible_templates(
    conn: &rusqlite::Connection,
    node: &NodeRow,
    gpu: &serde_json::Value,
) -> Result<Vec<TemplateRow>> {
    let advertised = node
        .snapshot
        .get("templates")
        .and_then(|value| value.as_object())
        .cloned()
        .unwrap_or_default();
    let vendor = match gpu.get("vendor").and_then(|value| value.as_str()) {
        Some(vendor) => vendor.to_string(),
        None => return Ok(Vec::new()),
    };
    let mut result = Vec::new();
    for item in models::list_enabled_templates(conn)? {
        let template = Template::from_value(item.definition.clone()).map_err(|exc| ServiceError::Agent(exc.0))?;
        let fingerprint = template.fingerprint();
        if template.vendors.contains(&vendor)
            && advertised.get(&item.id).and_then(|value| value.as_str()) == Some(fingerprint.as_str())
        {
            result.push(item);
        }
    }
    Ok(result)
}

/// 提供枠と予約状況（Node.resources プロパティの実体）。
#[derive(Debug, Clone, Copy)]
pub struct Capacity {
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub default_memory_bytes: u64,
    pub default_disk_bytes: u64,
    pub allocated_memory_bytes: u64,
    pub allocated_disk_bytes: u64,
    pub available_memory_bytes: u64,
    pub available_disk_bytes: u64,
}

/// validate_pool（Python 版はキー集合が完全一致であることを要求する）。
pub fn validate_pool(value: &serde_json::Value) -> Result<Pool> {
    let map = value.as_object().ok_or_else(|| ServiceError::agent("Invalid resource pool configuration"))?;
    let expected = ["memory_bytes", "disk_bytes", "default_memory_bytes", "default_disk_bytes"];
    if map.len() != expected.len() || expected.iter().any(|key| !map.contains_key(*key)) {
        return Err(ServiceError::agent("Invalid resource pool configuration"));
    }
    let field = |key: &str| -> Result<u64> {
        let raw = map.get(key).and_then(|value| value.as_u64()).ok_or_else(|| {
            ServiceError::agent("RAM must be at least 64 MiB and disk at least 16 MiB, in whole MiB")
        })?;
        resources::size_bytes(raw, key).map_err(|exc| ServiceError::Agent(exc.0))
    };
    let memory_bytes = field("memory_bytes")?;
    let disk_bytes = field("disk_bytes")?;
    let default_memory_bytes = field("default_memory_bytes")?;
    let default_disk_bytes = field("default_disk_bytes")?;
    if default_memory_bytes > memory_bytes || default_disk_bytes > disk_bytes {
        return Err(ServiceError::agent("Default lease capacity exceeds the configured pool"));
    }
    Ok(Pool { memory_bytes, disk_bytes, default_memory_bytes, default_disk_bytes })
}

/// allocation（キー集合が memory_bytes / disk_bytes の完全一致を要求）。
pub fn parse_allocation(value: &serde_json::Value) -> Result<Allocation> {
    let map = value.as_object().ok_or_else(|| ServiceError::agent("Invalid lease resource allocation"))?;
    if map.len() != 2 || !map.contains_key("memory_bytes") || !map.contains_key("disk_bytes") {
        return Err(ServiceError::agent("Invalid lease resource allocation"));
    }
    let field = |key: &str| -> Result<u64> {
        let raw = map.get(key).and_then(|value| value.as_u64()).ok_or_else(|| {
            ServiceError::agent("RAM must be at least 64 MiB and disk at least 16 MiB, in whole MiB")
        })?;
        resources::size_bytes(raw, key).map_err(|exc| ServiceError::Agent(exc.0))
    };
    Ok(Allocation { memory_bytes: field("memory_bytes")?, disk_bytes: field("disk_bytes")? })
}

/// used_resources: 未停止の貸出の合計（require_all=true で欠落を拒否）。
fn used_resources(items: Vec<Option<Allocation>>) -> Result<Allocation> {
    let mut used = Allocation { memory_bytes: 0, disk_bytes: 0 };
    for item in items {
        match item {
            None => {
                return Err(ServiceError::agent(
                    "An active lease has no resource allocation; return it before configuring a pool",
                ))
            }
            Some(value) => {
                used.memory_bytes = used.memory_bytes.saturating_add(value.memory_bytes);
                used.disk_bytes = used.disk_bytes.saturating_add(value.disk_bytes);
            }
        }
    }
    Ok(used)
}

/// snapshot の leases から生の資源値を収集（stopped は除外）。
fn snapshot_allocations(snapshot: &serde_json::Value) -> BTreeMap<String, serde_json::Value> {
    let mut allocations = BTreeMap::new();
    if let Some(leases) = snapshot.get("leases").and_then(|value| value.as_object()) {
        for (key, lease) in leases {
            if lease.get("status").and_then(|value| value.as_str()) == Some("stopped") {
                continue;
            }
            allocations
                .insert(key.clone(), lease.get("resources").cloned().unwrap_or(serde_json::Value::Null));
        }
    }
    allocations
}

/// 生の資源値を合計対象へ変換する。
fn allocation_of(value: &serde_json::Value) -> Result<Option<Allocation>> {
    match value {
        serde_json::Value::Null => Ok(None),
        other => Ok(Some(parse_allocation(other)?)),
    }
}

/// node_resources。
pub fn node_resources(conn: &rusqlite::Connection, node: &NodeRow) -> Result<Option<Capacity>> {
    let raw = match node.snapshot.get("resource_pool") {
        None | Some(serde_json::Value::Null) => return Ok(None),
        Some(value) => value.clone(),
    };
    let pool = validate_pool(&raw)?;
    let mut allocations = snapshot_allocations(&node.snapshot);
    for lease in models::list_node_active_leases(conn, &node.id)? {
        let expected = serde_json::json!({
            "memory_bytes": lease.memory_bytes,
            "disk_bytes": lease.disk_bytes,
        });
        if let Some(current) = allocations.get(&lease.id) {
            if !current.is_null() && current != &expected {
                return Err(ServiceError::agent(
                    "貸出の資源予約を確認中です。提供PCの同期を待ってください。",
                ));
            }
        }
        allocations.insert(lease.id.clone(), expected);
    }
    let mut items = Vec::new();
    for value in allocations.values() {
        items.push(allocation_of(value)?);
    }
    let used = used_resources(items)?;
    if used.memory_bytes > pool.memory_bytes || used.disk_bytes > pool.disk_bytes {
        return Err(ServiceError::agent(
            "資源予約が提供枠を超えています。新規貸出を停止しています。",
        ));
    }
    let accepting = node.accepting(&fmt_dt(now()));
    Ok(Some(Capacity {
        memory_bytes: pool.memory_bytes,
        disk_bytes: pool.disk_bytes,
        default_memory_bytes: pool.default_memory_bytes,
        default_disk_bytes: pool.default_disk_bytes,
        allocated_memory_bytes: used.memory_bytes,
        allocated_disk_bytes: used.disk_bytes,
        available_memory_bytes: if accepting { pool.memory_bytes - used.memory_bytes } else { 0 },
        available_disk_bytes: if accepting { pool.disk_bytes - used.disk_bytes } else { 0 },
    }))
}

/// SSH 公開鍵の検証と正規化（validate_ssh_key）。
pub fn validate_ssh_key(value: &str) -> Result<String> {
    if value.len() > 4096 || value.trim().contains('\n') {
        return Err(ServiceError::agent("A single SSH public key is required"));
    }
    let parts: Vec<&str> = value.split_whitespace().collect();
    if parts.len() < 2 {
        return Err(ServiceError::agent("Invalid SSH public key"));
    }
    let algorithm = parts[0];
    let encoded = parts[1];
    let allowed = algorithm.starts_with("ssh-") || algorithm.starts_with("ecdsa-") || algorithm.starts_with("sk-");
    if !allowed {
        return Err(ServiceError::agent("Invalid SSH public key"));
    }
    let decoded = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        normalize_base64(encoded),
    )
    .map_err(|_| ServiceError::agent("Invalid SSH public key"))?;
    if decoded.len() < 4 {
        return Err(ServiceError::agent("Invalid SSH public key"));
    }
    let mut length = [0u8; 4];
    length.copy_from_slice(&decoded[0..4]);
    let blob_length = u32::from_be_bytes(length) as usize;
    if blob_length == 0 || decoded.len() < 4 + blob_length {
        return Err(ServiceError::agent("Invalid SSH public key"));
    }
    if &decoded[4..4 + blob_length] != algorithm.as_bytes() {
        return Err(ServiceError::agent("Invalid SSH public key"));
    }
    Ok(format!("{algorithm} {encoded}"))
}

/// パディングを補った base64 文字列にする。
fn normalize_base64(value: &str) -> String {
    let mut trimmed: String = value.chars().filter(|ch| !ch.is_whitespace()).collect();
    while trimmed.len() % 4 != 0 {
        trimmed.push('=');
    }
    trimmed
}

/// reserve: GPU の予約と中継ポート割当。
#[allow(clippy::too_many_arguments)]
pub fn reserve(
    db: &crate::db::SharedDb,
    cfg: &Config,
    user_id: i64,
    node_id: &str,
    gpu_id: &str,
    template_id: &str,
    ssh_public_key: &str,
    connection_mode: &str,
    memory_bytes: Option<u64>,
    disk_bytes: Option<u64>,
) -> Result<LeaseRow> {
    let node_hex = models::uuid_to_hex(node_id).ok_or(ServiceError::Generic)?;
    let mut guard = db.lock();
    let tx = immediate(&mut guard)?;
    let node = models::get_node(&tx, &node_hex)?.ok_or(ServiceError::Generic)?;
    if !node.accepting(&fmt_dt(now())) {
        return Err(ServiceError::agent("この提供PCは現在、新規の貸出を受け付けていません。"));
    }
    let gpus = node
        .snapshot
        .get("gpus")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let mut found: Option<serde_json::Value> = None;
    for gpu in &gpus {
        let id = gpu.get("id").ok_or(ServiceError::Generic)?;
        if id.as_str() == Some(gpu_id) {
            found = Some(gpu.clone());
            break;
        }
    }
    let gpu = found.ok_or_else(|| ServiceError::agent("このGPUは貸出中、または後片付け中です。"))?;
    if is_truthy(gpu.get("allocated")) || models::active_lease_exists(&tx, &node_hex, gpu_id)? {
        return Err(ServiceError::agent("このGPUは貸出中、または後片付け中です。"));
    }
    let compatible = compatible_templates(&tx, &node, &gpu)?;
    let template = match compatible.into_iter().find(|item| item.id == template_id) {
        Some(template) => template,
        None => {
            return Err(ServiceError::agent(
                "提供PCと一致する承認済みテンプレートを選択してください。",
            ))
        }
    };
    let key = validate_ssh_key(ssh_public_key)?;
    if connection_mode != "relay" && connection_mode != "direct" {
        return Err(ServiceError::agent("Invalid connection mode"));
    }
    if connection_mode == "direct" && node.direct_host().is_empty() {
        return Err(ServiceError::agent("この提供PCでは直接接続を利用できません。"));
    }
    let capacity = node_resources(&tx, &node)?;
    let mut requested = Allocation { memory_bytes: 0, disk_bytes: 0 };
    if let Some(capacity) = capacity {
        requested = parse_allocation(&serde_json::json!({
            "memory_bytes": memory_bytes.unwrap_or(capacity.default_memory_bytes),
            "disk_bytes": disk_bytes.unwrap_or(capacity.default_disk_bytes),
        }))?;
        if requested.memory_bytes > capacity.available_memory_bytes
            || requested.disk_bytes > capacity.available_disk_bytes
        {
            return Err(ServiceError::agent("予約可能なRAMまたはディスク容量が不足しています。"));
        }
    } else if memory_bytes.is_some() || disk_bytes.is_some() {
        return Err(ServiceError::agent("この提供PCには資源の提供枠が設定されていません。"));
    }

    // 中継ポートの割当（貸出ごとに最大 4 ポート、使用中ポートは除外）。
    let mut tcp_ports: Vec<String> = Vec::new();
    let mut chosen_ports: Vec<i64> = Vec::new();
    if connection_mode == "relay" {
        let definition =
            Template::from_value(template.definition.clone()).map_err(|exc| ServiceError::Agent(exc.0))?;
        if definition.ports.len() > 4 {
            return Err(ServiceError::agent("中央中継は貸出ごとに最大4ポートです。"));
        }
        tcp_ports = definition.ports.into_iter().filter(|port| port.ends_with("/tcp")).collect();
        let used = models::used_relay_ports(&tx)?;
        let available: Vec<i64> = (cfg.relay_port_min as i64..=cfg.relay_port_max as i64)
            .filter(|port| !used.contains(port) && *port != cfg.relay_port as i64)
            .collect();
        if available.len() < tcp_ports.len() {
            return Err(ServiceError::agent("中継用ポートが不足しています。"));
        }
        chosen_ports = sample_ports(&available, tcp_ports.len());
    }

    let lease = models::insert_lease(
        &tx,
        user_id,
        &node_hex,
        gpu_id,
        &template.id,
        &key,
        connection_mode,
        requested.memory_bytes,
        requested.disk_bytes,
    )?;
    for (port, relay_port) in tcp_ports.iter().zip(chosen_ports.iter()) {
        models::insert_route(&tx, &lease.id, port, *relay_port)?;
    }
    let mut payload = serde_json::json!({
        "lease_id": lease.id.clone(),
        "template_id": template.id,
        "gpu_id": gpu_id,
        "ssh_public_key": key,
        "connection_mode": connection_mode,
    });
    if capacity.is_some() {
        payload["memory_bytes"] = serde_json::json!(requested.memory_bytes);
        payload["disk_bytes"] = serde_json::json!(requested.disk_bytes);
    }
    models::insert_command(&tx, &node_hex, Some(&lease.id), "start", &payload)?;
    tx.commit()?;
    Ok(lease)
}

/// 重複なしのランダム抽出（secrets.SystemRandom().sample 相当）。
fn sample_ports(available: &[i64], count: usize) -> Vec<i64> {
    use rand::seq::SliceRandom;
    let mut generator = rand::thread_rng();
    let mut pool = available.to_vec();
    pool.shuffle(&mut generator);
    pool.into_iter().take(count).collect()
}

/// stop_lease。
pub fn stop_lease(db: &crate::db::SharedDb, lease_hex: &str) -> Result<LeaseRow> {
    let mut guard = db.lock();
    let tx = immediate(&mut guard)?;
    let mut lease = models::get_lease(&tx, lease_hex)?.ok_or(ServiceError::Payload)?;
    if lease.active && lease.status != "stopping" {
        lease.status = "stopping".to_string();
        models::touch_lease(&tx, &lease)?;
        models::insert_command(
            &tx,
            &lease.node_id,
            Some(&lease.id),
            "stop",
            &serde_json::json!({"lease_id": lease.id.clone()}),
        )?;
    }
    tx.commit()?;
    Ok(lease)
}

/// control_node。
pub fn control_node(db: &crate::db::SharedDb, node_hex: &str, action: Option<&str>) -> Result<()> {
    let action = action.unwrap_or("");
    if action == "rotate" {
        let guard = db.lock();
        models::insert_command(&guard, node_hex, None, "rotate_token", &serde_json::json!({}))?;
        return Ok(());
    }
    if !matches!(action, "on" | "drain" | "stop") {
        return Err(ServiceError::agent("Invalid node action"));
    }
    let mut guard = db.lock();
    let tx = immediate(&mut guard)?;
    let node = models::get_node(&tx, node_hex)?.ok_or(ServiceError::Payload)?;
    tx.execute(
        "UPDATE portal_node SET desired_enabled = ?2 WHERE id = ?1",
        params![node.id, action == "on"],
    )?;
    let payload = if action == "on" {
        serde_json::json!({})
    } else {
        serde_json::json!({"mode": action})
    };
    let kind = if action == "on" { "on" } else { "off" };
    models::insert_command(&tx, &node.id, None, kind, &payload)?;
    if action == "stop" {
        tx.execute(
            "UPDATE portal_lease SET status = 'stopping' WHERE node_id = ?1 AND active",
            params![node.id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Python 版 validate_snapshot（生 JSON に対する忠実な検証）。
pub fn validate_snapshot(value: &serde_json::Value) -> Result<()> {
    let object = value.as_object().ok_or_else(|| ServiceError::agent("Invalid node snapshot"))?;
    if object.get("accepting").and_then(|value| value.as_bool()).is_none() {
        return Err(ServiceError::agent("Invalid node snapshot"));
    }
    let node_id = object
        .get("node_id")
        .and_then(|value| value.as_str())
        .ok_or_else(|| ServiceError::agent("Invalid identifier"))?;
    identifier(node_id).map_err(|exc| ServiceError::Agent(exc.0))?;

    let gpus = object.get("gpus").and_then(|value| value.as_array());
    let leases = object.get("leases").and_then(|value| value.as_object());
    let gpus = match gpus {
        Some(gpus) if gpus.len() <= 128 => gpus,
        _ => return Err(ServiceError::agent("Invalid inventory")),
    };
    let leases = match leases {
        Some(leases) if leases.len() <= 4096 => leases,
        _ => return Err(ServiceError::agent("Invalid inventory")),
    };
    let templates = object
        .get("templates")
        .and_then(|value| value.as_object())
        .ok_or_else(|| ServiceError::agent("Invalid template fingerprints"))?;
    if templates
        .values()
        .any(|value| value.as_str().map(|text| text.len() != 64).unwrap_or(true))
    {
        return Err(ServiceError::agent("Invalid template fingerprints"));
    }

    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for gpu in gpus {
        if !gpu.is_object() {
            return Err(ServiceError::agent("Invalid or duplicate GPU"));
        }
        let id = gpu
            .get("id")
            .and_then(|value| value.as_str())
            .ok_or_else(|| ServiceError::agent("Invalid identifier"))?;
        identifier(id).map_err(|exc| ServiceError::Agent(exc.0))?;
        if !seen.insert(id) {
            return Err(ServiceError::agent("Invalid or duplicate GPU"));
        }
        if gpu.get("vendor").and_then(|value| value.as_str()).is_none() {
            return Err(ServiceError::agent("Invalid or duplicate GPU"));
        }
    }

    for (key, lease) in leases {
        identifier(key).map_err(|exc| ServiceError::Agent(exc.0))?;
        if !lease.is_object() {
            return Err(ServiceError::agent("Invalid lease state"));
        }
        let status = lease.get("status").and_then(|value| value.as_str()).unwrap_or("");
        if !matches!(status, "starting" | "running" | "cleanup_pending" | "stopped") {
            return Err(ServiceError::agent("Invalid lease state"));
        }
        let ports = match lease.get("ports") {
            None | Some(serde_json::Value::Null) => continue,
            Some(serde_json::Value::Object(map)) => map,
            Some(_) => return Err(ServiceError::agent("Invalid port bindings")),
        };
        for bindings in ports.values() {
            if bindings.is_null() {
                continue;
            }
            let bindings =
                bindings.as_array().ok_or_else(|| ServiceError::agent("Invalid port bindings"))?;
            for binding in bindings {
                if !binding.is_object() {
                    return Err(ServiceError::Payload);
                }
                let number = match binding.get("HostPort") {
                    None => "",
                    Some(serde_json::Value::String(text)) => text,
                    Some(_) => return Err(ServiceError::agent("Invalid endpoint port")),
                };
                let valid = !number.is_empty()
                    && number.bytes().all(|byte| byte.is_ascii_digit())
                    && number
                        .parse::<u32>()
                        .map(|value| (1..=65535).contains(&value))
                        .unwrap_or(false);
                if !valid {
                    return Err(ServiceError::agent("Invalid endpoint port"));
                }
            }
        }
    }

    if is_truthy(object.get("direct_host")) {
        let host = object
            .get("direct_host")
            .and_then(|value| value.as_str())
            .ok_or_else(|| ServiceError::agent("Invalid endpoint hostname"))?;
        gpupod_common::identifier::endpoint_host(host).map_err(|exc| ServiceError::Agent(exc.0))?;
    }

    if !object.get("resource_pool").map(|value| value.is_null()).unwrap_or(true) {
        let manual = object.get("manual_enabled").and_then(|value| value.as_bool()).is_some();
        let auto_busy = object.get("auto_busy").and_then(|value| value.as_bool()).is_some();
        if !manual || !auto_busy {
            return Err(ServiceError::agent("Invalid resource availability state"));
        }
        let pool = validate_pool(object.get("resource_pool").unwrap())?;
        let allocations = snapshot_allocations(value);
        let mut items = Vec::new();
        for item in allocations.values() {
            items.push(allocation_of(item)?);
        }
        let used = used_resources(items)?;
        if used.memory_bytes > pool.memory_bytes || used.disk_bytes > pool.disk_bytes {
            return Err(ServiceError::agent(
                "Existing allocations exceed the resource pool; reservations retained",
            ));
        }
    }
    Ok(())
}

/// sync_node。
pub fn sync_node(
    db: &crate::db::SharedDb,
    node: &NodeRow,
    payload: &serde_json::Value,
) -> Result<SyncResponse> {
    let state = payload.get("state").ok_or(ServiceError::Payload)?;
    validate_snapshot(state)?;
    let results = match payload.get("results") {
        None => Vec::new(),
        Some(serde_json::Value::Array(items)) if items.len() <= 100 => items.clone(),
        Some(_) => return Err(ServiceError::agent("Invalid command results")),
    };
    let statistics = payload
        .get("statistics")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));

    let mut guard = db.lock();
    let tx = immediate(&mut guard)?;
    let node = models::get_node(&tx, &node.id)?.ok_or(ServiceError::Payload)?;
    let stored_empty = node.snapshot.as_object().map(|map| map.is_empty()).unwrap_or(true);
    let stored_id = node.snapshot.get("node_id").cloned();
    let new_id = state.get("node_id").cloned();
    if !stored_empty && stored_id != new_id {
        return Err(ServiceError::agent(
            "Local node identity changed; enrol this installation as a new node",
        ));
    }
    models::save_node_snapshot(&tx, &node.id, state, &statistics, &fmt_dt(now()))?;

    for result in &results {
        let raw_id = result.get("id").and_then(|value| value.as_str()).ok_or(ServiceError::Payload)?;
        // エージェントは str(uuid) 形式（ハイフン付き）で返す。DB は 32 桁 hex。
        let id = models::uuid_to_hex(raw_id).ok_or(ServiceError::Payload)?;
        let command = match models::get_pending_command(&tx, &id, &node.id)? {
            None => continue,
            Some(command) => command,
        };
        models::mark_command_done(&tx, &id, result)?;
        if command.kind == "start" && !is_truthy(result.get("ok")) {
            if let Some(lease_hex) = command.lease_id.clone() {
                let local = state.get("leases").and_then(|value| value.get(&lease_hex)).cloned();
                if local.is_none() {
                    let error_value = match result.get("error") {
                        None | Some(serde_json::Value::Null) => "Start failed".to_string(),
                        Some(value) => crate::pyrepr::py_str(value),
                    };
                    if let Some(mut lease) = models::get_lease(&tx, &lease_hex)? {
                        lease.status = "failed".to_string();
                        lease.active = false;
                        lease.error = truncate_chars(&error_value, 2000);
                        models::touch_lease(&tx, &lease)?;
                        models::delete_routes_of_lease(&tx, &lease.id)?;
                    }
                }
            }
        }
    }

    let state_leases = state
        .get("leases")
        .and_then(|value| value.as_object())
        .cloned()
        .unwrap_or_default();
    let has_pool = !state.get("resource_pool").map(|value| value.is_null()).unwrap_or(true);
    for mut lease in models::list_node_active_leases(&tx, &node.id)? {
        let local = match state_leases.get(&lease.id) {
            None => continue,
            Some(local) => local.clone(),
        };
        if local.get("gpu_id").and_then(|value| value.as_str()) != Some(lease.gpu_id.as_str())
            || local.get("template_id").and_then(|value| value.as_str()) != Some(lease.template_id.as_str())
        {
            return Err(ServiceError::agent("Lease identity mismatch"));
        }
        if lease.memory_bytes == 0 && lease.disk_bytes == 0 && has_pool && is_truthy(local.get("resources")) {
            let observed = parse_allocation(local.get("resources").unwrap())?;
            lease.memory_bytes = observed.memory_bytes as i64;
            lease.disk_bytes = observed.disk_bytes as i64;
        }
        if lease.memory_bytes != 0 || lease.disk_bytes != 0 {
            let expected =
                serde_json::json!({"memory_bytes": lease.memory_bytes, "disk_bytes": lease.disk_bytes});
            if local.get("resources") != Some(&expected) {
                return Err(ServiceError::agent("Lease resource allocation mismatch"));
            }
        }
        let status = local
            .get("status")
            .and_then(|value| value.as_str())
            .ok_or(ServiceError::Payload)?;
        let error_value = match local.get("error") {
            None | Some(serde_json::Value::Null) => String::new(),
            Some(value) => crate::pyrepr::py_str(value),
        };
        lease.error = truncate_chars(&error_value, 2000);
        if status == "stopped" {
            lease.status = "stopped".to_string();
            lease.active = false;
            models::delete_routes_of_lease(&tx, &lease.id)?;
        } else if lease.status != "stopping" {
            lease.status = status.to_string();
        }
        models::touch_lease(&tx, &lease)?;
    }

    let commands: Vec<NodeCommand> = models::pending_commands(&tx, &node.id, 20)?
        .into_iter()
        .map(|command| NodeCommand {
            id: hex_to_uuid(&command.id),
            kind: command.kind,
            payload: command.payload,
        })
        .collect();
    let routes: Vec<RelayRoute> = models::routes_for_node(&tx, &node.id)?
        .into_iter()
        .map(|route| RelayRoute {
            lease_id: route.lease_id,
            container_port: route.container_port,
            relay_port: route.relay_port as u16,
        })
        .collect();
    let acknowledged: Vec<String> = results
        .iter()
        .map(|result| match result.get("id") {
            Some(serde_json::Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        })
        .collect();
    let release = assigned_release(&tx, &node);
    tx.commit()?;
    Ok(SyncResponse {
        commands,
        routes,
        acknowledged,
        relay_bind_host: crate::config::relay_bind_host(),
        release,
    })
}

/// Python の文字列スライス（文字単位で先頭 n 文字）。
pub fn truncate_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}
