//! 中央への常時アウトバウンド同期と管理用 SSH 逆方向転送（Python 版 daemon.py の移植）。
//!
//! worker / maintain / poll / tunnels / watchdog の 5 タスクで構成し、
//! 同期は 2〜30 秒の指数バックオフ（ジッター付き）で再試行する。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use gpupod_common::protocol::{NodeCommand, RelayRoute, SyncResponse};
use gpupod_common::token::token_digest;
use gpupod_common::AgentError;
use rand::Rng;
use russh::client::{AuthResult, Config as SshConfig, Handle as SshHandle, Handler as SshHandler};
use russh::keys::PublicKey;
use russh::{Channel, Disconnect};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::{error, warn};

use crate::agent::{Agent, StartRequest};
use crate::benchmarks::read_results;
use crate::credentials::rotate_credentials;
use crate::files::atomic_json;
use crate::health::{notify_systemd, HealthMonitor};
use crate::telemetry;

pub type Result<T> = std::result::Result<T, AgentError>;

fn wrapped(exc: impl std::fmt::Display) -> AgentError {
    AgentError::new(format!("Invalid provider configuration: {exc}"))
}

fn default_relay_port() -> u16 {
    19000
}

/// 1 接続で同時に張る中継経路の上限。
const MAX_FORWARD_PORTS: usize = 256;

fn default_watchdog_timeout() -> f64 {
    180.0
}

/// プロバイダー認証情報 JSON（0600 権限で置く）。
///
/// Debug は機材キーを伏せる（ログやパニックメッセージへ鍵を出さない）。
#[derive(Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub url: String,
    pub node_id: String,
    pub token: String,
    pub relay_host: String,
    pub relay_host_key: String,
    #[serde(default = "default_relay_port")]
    pub relay_port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_bind: Option<String>,
    #[serde(default)]
    pub limits: BTreeMap<String, i64>,
    #[serde(default)]
    pub min_disk_free_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(default = "default_watchdog_timeout")]
    pub watchdog_timeout: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_path: Option<String>,
    /// 未知のキー（repository_url 等）は交換書き戻しで失わない。
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderConfig")
            .field("url", &self.url)
            .field("node_id", &self.node_id)
            .field("token", &"<redacted>")
            .field("relay_host", &self.relay_host)
            .field("relay_host_key", &self.relay_host_key)
            .field("relay_port", &self.relay_port)
            .field("direct_host", &self.direct_host)
            .field("direct_bind", &self.direct_bind)
            .field("limits", &self.limits)
            .field("min_disk_free_bytes", &self.min_disk_free_bytes)
            .field("runtime", &self.runtime)
            .field("watchdog_timeout", &self.watchdog_timeout)
            .field("disk_path", &self.disk_path)
            .finish()
    }
}

/// Python の urllib.parse.urlsplit 相当（検証用の軽い分解）。
#[derive(Debug, Default)]
struct UrlParts {
    scheme: String,
    username: Option<String>,
    host: Option<String>,
    query: Option<String>,
    fragment: Option<String>,
}

fn split_url(value: &str) -> UrlParts {
    let mut parts = UrlParts::default();
    let mut rest = value;
    if let Some(index) = value.find("://") {
        let scheme = &value[..index];
        if !scheme.is_empty()
            && scheme.as_bytes()[0].is_ascii_alphabetic()
            && scheme
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
        {
            parts.scheme = scheme.to_ascii_lowercase();
            rest = &value[index + 3..];
        }
    }
    if let Some(index) = rest.find('#') {
        parts.fragment = Some(rest[index + 1..].to_owned());
        rest = &rest[..index];
    }
    if let Some(index) = rest.find('?') {
        parts.query = Some(rest[index + 1..].to_owned());
        rest = &rest[..index];
    }
    let authority = match rest.find('/') {
        Some(index) => &rest[..index],
        None => rest,
    };
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(index) => (Some(&authority[..index]), &authority[index + 1..]),
        None => (None, authority),
    };
    if let Some(userinfo) = userinfo {
        let username = match userinfo.find(':') {
            Some(index) => &userinfo[..index],
            None => userinfo,
        };
        parts.username = Some(username.to_owned());
    }
    let host = if let Some(stripped) = hostport.strip_prefix('[') {
        stripped.split(']').next().unwrap_or("").to_owned()
    } else {
        match hostport.rfind(':') {
            Some(index) => hostport[..index].to_owned(),
            None => hostport.to_owned(),
        }
    };
    if !host.is_empty() {
        parts.host = Some(host.to_ascii_lowercase());
    }
    parts
}

fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

/// 認証情報ファイルの権限を 0600 に寄せる（他ユーザーから鍵を読ませない）。
fn harden_config_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                warn!(
                    "Provider config {} is readable by other users (mode {:04o}); restricting to 0600",
                    path.display(),
                    mode
                );
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// 認証情報ファイルを検証する（Python 版 load_config）。
pub fn load_config(path: impl AsRef<Path>) -> Result<ProviderConfig> {
    let text = std::fs::read_to_string(path.as_ref()).map_err(wrapped)?;
    harden_config_permissions(path.as_ref());
    let value: Value = serde_json::from_str(&text).map_err(wrapped)?;
    let object = value
        .as_object()
        .ok_or_else(|| wrapped("expected a JSON object"))?;

    let field = |name: &str| -> Result<String> {
        match object.get(name) {
            Some(Value::String(text)) => Ok(text.clone()),
            _ => Err(wrapped(format!("'{name}'"))),
        }
    };

    let url = field("url")?;
    let parts = split_url(&url);
    let local = parts.host.as_deref().map(is_loopback).unwrap_or(false);
    let insecure = parts.scheme != "https" && !(parts.scheme == "http" && local);
    if insecure || parts.username.is_some() || parts.query.is_some() || parts.fragment.is_some() {
        return Err(AgentError::new(
            "Central URL must use HTTPS (HTTP is allowed only for loopback demos)",
        ));
    }

    let node_id = field("node_id")?;
    let token = field("token")?;
    let relay_host = field("relay_host")?;
    let relay_host_key = field("relay_host_key")?;
    for (name, value) in [
        ("node_id", &node_id),
        ("token", &token),
        ("relay_host", &relay_host),
        ("relay_host_key", &relay_host_key),
    ] {
        if value.is_empty() {
            return Err(AgentError::new(format!("Missing {name}")));
        }
    }

    russh::keys::ssh_key::PublicKey::from_openssh(&relay_host_key).map_err(wrapped)?;
    gpupod_common::identifier::endpoint_host(&relay_host)?;

    let direct_host = match object.get("direct_host") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if text.is_empty() => None,
        Some(Value::String(text)) => {
            gpupod_common::identifier::endpoint_host(text)?;
            Some(text.clone())
        }
        Some(_) => return Err(AgentError::new("Invalid endpoint hostname")),
    };
    let direct_bind = match object.get("direct_bind") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if text.is_empty() => None,
        Some(Value::String(text)) => {
            if text.parse::<IpAddr>().is_err() {
                return Err(wrapped(format!(
                    "{text} does not appear to be an IPv4 or IPv6 address"
                )));
            }
            Some(text.clone())
        }
        Some(_) => return Err(wrapped("expected an IP address")),
    };
    if direct_host.is_some() && direct_bind.is_none() {
        return Err(AgentError::new(
            "Direct connections require both direct_host and direct_bind",
        ));
    }

    let mut limits = BTreeMap::new();
    if let Some(raw) = object.get("limits") {
        let map = raw
            .as_object()
            .ok_or_else(|| AgentError::new("Unknown resource budget"))?;
        let allowed = ["nano_cpus", "mem_limit", "pids_limit", "shm_size"];
        for (key, value) in map {
            if !allowed.contains(&key.as_str()) {
                return Err(AgentError::new("Unknown resource budget"));
            }
            let number = value.as_i64().filter(|number| *number > 0).ok_or_else(|| {
                AgentError::new(
                    "Resource budgets must be positive integers in bytes/nanocpus/count",
                )
            })?;
            limits.insert(key.clone(), number);
        }
    }

    let min_disk_free_bytes = match object.get("min_disk_free_bytes") {
        None => 0,
        Some(value) => match value.as_u64() {
            Some(number) => number,
            None => return Err(AgentError::new("Invalid disk reserve")),
        },
    };

    let runtime = match object.get("runtime") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if text.is_empty() => None,
        Some(Value::String(text)) => {
            if text != "runc" && text != "runsc" {
                return Err(AgentError::new("Runtime must be runc or runsc"));
            }
            Some(text.clone())
        }
        Some(_) => return Err(AgentError::new("Runtime must be runc or runsc")),
    };

    let watchdog_timeout = match object.get("watchdog_timeout") {
        None => default_watchdog_timeout(),
        Some(value) => {
            let number = value
                .as_f64()
                .ok_or_else(|| AgentError::new("Watchdog timeout must be at least 30 seconds"))?;
            if number < 30.0 {
                return Err(AgentError::new(
                    "Watchdog timeout must be at least 30 seconds",
                ));
            }
            number
        }
    };

    let disk_path = match object.get("disk_path") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return Err(wrapped("expected a string")),
    };

    let relay_port = match object.get("relay_port") {
        None => default_relay_port(),
        Some(value) => value
            .as_u64()
            .and_then(|number| u16::try_from(number).ok())
            .filter(|port| *port != 0)
            .ok_or_else(|| wrapped("expected a port number between 1 and 65535"))?,
    };

    let known: HashSet<&str> = [
        "url",
        "node_id",
        "token",
        "relay_host",
        "relay_host_key",
        "relay_port",
        "direct_host",
        "direct_bind",
        "limits",
        "min_disk_free_bytes",
        "runtime",
        "watchdog_timeout",
        "disk_path",
    ]
    .into_iter()
    .collect();
    let extra = object
        .iter()
        .filter(|(key, _)| !known.contains(key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();

    Ok(ProviderConfig {
        url: url.trim_end_matches('/').to_owned(),
        node_id,
        token,
        relay_host,
        relay_host_key,
        relay_port,
        direct_host,
        direct_bind,
        limits,
        min_disk_free_bytes,
        runtime,
        watchdog_timeout,
        disk_path,
        extra,
    })
}

/// 中継側で受け取る接続を手元のコンテナへ繋ぐハンドラ。
struct RelayHandler {
    expected_key: PublicKey,
    targets: Arc<Mutex<HashMap<u32, u16>>>,
}

impl SshHandler for RelayHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        // 設定の relay_host_key に固定検証する（known_hosts は使わない）。
        Ok(*server_public_key == self.expected_key)
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<russh::client::Msg>,
        _connected_address: &str,
        connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        _session: &mut russh::client::Session,
    ) -> std::result::Result<(), Self::Error> {
        let target = self
            .targets
            .lock()
            .unwrap_or_else(|exc| exc.into_inner())
            .get(&connected_port)
            .copied();
        match target {
            Some(local_port) => {
                tokio::spawn(pump(channel, local_port));
            }
            None => {
                let _ = channel.close().await;
            }
        }
        Ok(())
    }
}

/// 中継から届いた接続を 127.0.0.1:local_port へ中継する。
async fn pump(channel: Channel<russh::client::Msg>, local_port: u16) {
    let stream = match tokio::net::TcpStream::connect(("127.0.0.1", local_port)).await {
        Ok(stream) => stream,
        Err(_) => {
            let _ = channel.close().await;
            return;
        }
    };
    let mut tunnel = channel.into_stream();
    let mut stream = stream;
    let _ = tokio::io::copy_bidirectional(&mut tunnel, &mut stream).await;
}

/// 既存の監視ポート一覧。
#[derive(Debug)]
struct Listener {
    local_port: u16,
    address: String,
}

/// russh による SSH 接続と remote port forwarding。
pub struct Forwarder {
    config: ProviderConfig,
    host_key: PublicKey,
    token: Arc<RwLock<String>>,
    connection: Option<SshHandle<RelayHandler>>,
    listeners: HashMap<u16, Listener>,
    targets: Arc<Mutex<HashMap<u32, u16>>>,
    authenticated_token_hash: Option<String>,
}

impl Forwarder {
    pub fn new(config: ProviderConfig, token: Arc<RwLock<String>>) -> Result<Self> {
        let host_key = russh::keys::ssh_key::PublicKey::from_openssh(&config.relay_host_key)
            .map_err(|exc| AgentError::new(format!("{exc}")))?;
        Ok(Self {
            config,
            host_key,
            token,
            connection: None,
            listeners: HashMap::new(),
            targets: Arc::new(Mutex::new(HashMap::new())),
            authenticated_token_hash: None,
        })
    }

    /// 認証済みトークンのダイジェスト（Python 版と同じ記録）。
    pub fn authenticated_token_hash(&self) -> Option<&str> {
        self.authenticated_token_hash.as_deref()
    }

    /// 経路と状態に合わせて転送を作る／壊す（Python 版 Forwarder.update）。
    pub async fn update(
        &mut self,
        routes: &[RelayRoute],
        state: &Value,
        bind_host: &str,
    ) -> std::result::Result<(), anyhow::Error> {
        let mut wanted: BTreeMap<u16, u16> = BTreeMap::new();
        let leases = state.get("leases").and_then(|v| v.as_object());
        for route in routes {
            let Some(lease) = leases.and_then(|leases| leases.get(&route.lease_id)) else {
                continue;
            };
            if lease.get("status").and_then(|v| v.as_str()) != Some("running") {
                continue;
            }
            let mode = lease
                .get("connection_mode")
                .and_then(|v| v.as_str())
                .unwrap_or("relay");
            if mode != "relay" {
                continue;
            }
            let Some(bindings) = lease
                .get("ports")
                .and_then(|ports| ports.get(&route.container_port))
                .and_then(|value| value.as_array())
            else {
                continue;
            };
            let Some(first) = bindings.first() else {
                continue;
            };
            if first.get("HostIp").and_then(|v| v.as_str()) != Some("127.0.0.1") {
                continue;
            }
            let Some(host_port) = first.get("HostPort").and_then(|v| v.as_str()) else {
                continue;
            };
            if let Ok(host_port) = host_port.parse::<u16>() {
                // 中継も特権ポートと制御ポートは受け付けない。数も上限で抑える。
                if route.relay_port < 1024 {
                    continue;
                }
                if wanted.len() >= MAX_FORWARD_PORTS && !wanted.contains_key(&route.relay_port) {
                    warn!("Relay routes exceed the supported limit; extra routes ignored");
                    continue;
                }
                wanted.insert(route.relay_port, host_port);
            }
        }

        if wanted.is_empty() {
            self.close().await;
            return Ok(());
        }
        if self
            .connection
            .as_ref()
            .map(|handle| handle.is_closed())
            .unwrap_or(false)
        {
            self.close().await;
        }
        // 変わった経路は再接続前に閉じ、古い TCP 流れが復帰後も生きないようにする。
        let changed = self
            .listeners
            .iter()
            .any(|(port, listener)| wanted.get(port).copied() != Some(listener.local_port));
        if changed {
            self.close().await;
        }
        if self.connection.is_none() {
            self.connect().await?;
        }
        {
            let mut targets = self.targets.lock().unwrap_or_else(|exc| exc.into_inner());
            *targets = wanted
                .iter()
                .map(|(relay, local)| (*relay as u32, *local))
                .collect();
        }
        let Some(handle) = self.connection.as_mut() else {
            return Ok(());
        };
        for (relay_port, local_port) in &wanted {
            if self.listeners.contains_key(relay_port) {
                continue;
            }
            handle
                .tcpip_forward(bind_host.to_owned(), *relay_port as u32)
                .await?;
            self.listeners.insert(
                *relay_port,
                Listener {
                    local_port: *local_port,
                    address: bind_host.to_owned(),
                },
            );
        }
        Ok(())
    }

    /// 中継へ接続し、機材キーで認証する。
    async fn connect(&mut self) -> std::result::Result<(), anyhow::Error> {
        let mut ssh_config = SshConfig::default();
        ssh_config.keepalive_interval = Some(Duration::from_secs(10));
        ssh_config.keepalive_max = 3;
        let handler = RelayHandler {
            expected_key: self.host_key.clone(),
            targets: self.targets.clone(),
        };
        let address = (self.config.relay_host.as_str(), self.config.relay_port);
        let mut handle = tokio::time::timeout(
            Duration::from_secs(10),
            russh::client::connect(Arc::new(ssh_config), address, handler),
        )
        .await
        .map_err(|_| anyhow::anyhow!("SSH connect timed out"))??;
        let token = self
            .token
            .read()
            .unwrap_or_else(|exc| exc.into_inner())
            .clone();
        let username = format!("node:{}", self.config.node_id);
        let auth = tokio::time::timeout(
            Duration::from_secs(10),
            handle.authenticate_password(username, token.clone()),
        )
        .await
        .map_err(|_| anyhow::anyhow!("SSH login timed out"))??;
        if !matches!(auth, AuthResult::Success) {
            return Err(anyhow::anyhow!("SSH authentication was not accepted"));
        }
        self.authenticated_token_hash = Some(token_digest(&token));
        self.connection = Some(handle);
        Ok(())
    }

    /// 転送を全部閉じ、必要なら接続も閉じる（Python 版 Forwarder.close）。
    pub async fn close(&mut self) {
        if let Some(handle) = self.connection.take() {
            for (relay_port, listener) in self.listeners.drain() {
                let _ = handle
                    .cancel_tcpip_forward(listener.address, relay_port as u32)
                    .await;
            }
            let _ = handle.disconnect(Disconnect::ByApplication, "", "").await;
            let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
        } else {
            self.listeners.clear();
        }
        self.targets
            .lock()
            .unwrap_or_else(|exc| exc.into_inner())
            .clear();
        self.authenticated_token_hash = None;
    }
}

/// 同期応答から受け取る先の情報。
#[derive(Debug, Default)]
struct SyncOutcome {
    routes: Vec<RelayRoute>,
    bind_host: String,
    release: Value,
    commands: Vec<NodeCommand>,
}

/// 初回同期で消す、割当の未初期化状態。
#[derive(Debug, PartialEq)]
enum DesiredRelease {
    Unset,
    Set(Value),
}

/// ブロッキング処理（同期・コマンド実行）に必要な所有権の詰め合わせ。
#[derive(Clone)]
struct WorkerContext {
    agent: Arc<Agent>,
    config: ProviderConfig,
    token: Arc<RwLock<String>>,
    statistics: Arc<Mutex<Value>>,
    config_path: Option<PathBuf>,
    mock: bool,
    credentials_changed: Arc<AtomicBool>,
}

impl WorkerContext {
    /// 送信待ちのコマンド結果（最大 100 件）。
    fn results(&self) -> Vec<Value> {
        match self.agent.store.locked() {
            Ok(guard) => guard.command_results.values().take(100).cloned().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// 中央コマンド 1 件の実行（Python 版 _execute）。
    fn execute(&self, command: &NodeCommand) {
        match self.agent.store.locked() {
            Ok(guard) if guard.command_results.contains_key(&command.id) => return,
            Ok(_) => {}
            Err(_) => return,
        }
        let mut result = json!({ "id": command.id, "ok": true });
        let payload = command.payload.clone();
        let failure = match command.kind.as_str() {
            "start" => self.start_from_payload(&payload),
            "stop" => match payload.get("lease_id").and_then(|v| v.as_str()) {
                Some(lease_id) => self.agent.stop(lease_id).map(|_| ()),
                None => Err(AgentError::new("'lease_id'")),
            },
            "on" => self.agent.on().map(|_| ()),
            "off" => match payload.get("mode").and_then(|v| v.as_str()) {
                Some(mode) => self.agent.off(mode).map(|_| ()),
                None => Err(AgentError::new("'mode'")),
            },
            "rotate_token" => match &self.config_path {
                None => Err(AgentError::new(
                    "Credential rotation requires a persistent provider config path",
                )),
                Some(path) => rotate_credentials(path).map(|rotation| {
                    result["rotation"] = rotation;
                }),
            },
            _ => Err(AgentError::new("Unknown central command")),
        };
        if let Err(exc) = failure {
            result["ok"] = json!(false);
            result["error"] = json!(exc.0);
        }
        if let Ok(mut guard) = self.agent.store.locked() {
            guard.command_results.insert(command.id.clone(), result);
            let _ = guard.save();
        }
    }

    /// start コマンドの引数を検証して起動する。
    fn start_from_payload(&self, payload: &Value) -> Result<()> {
        let object = payload
            .as_object()
            .ok_or_else(|| AgentError::new("start payload must be an object"))?;
        let allowed = [
            "template_id",
            "gpu_id",
            "lease_id",
            "ssh_public_key",
            "connection_mode",
            "gpu_ids",
            "diagnostic",
            "memory_bytes",
            "disk_bytes",
        ];
        for key in object.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(AgentError::new(format!(
                    "start() got an unexpected keyword argument '{key}'"
                )));
            }
        }
        let disk_path = self
            .config
            .disk_path
            .clone()
            .unwrap_or_else(|| self.agent.store.directory.to_string_lossy().into_owned());
        let free =
            telemetry::disk_free(&disk_path).map_err(|exc| AgentError::new(format!("{exc}")))?;
        if free < self.config.min_disk_free_bytes {
            return Err(AgentError::new("Provider disk reserve would be exceeded"));
        }
        if object.get("connection_mode").and_then(|v| v.as_str()) == Some("direct")
            && self.config.direct_host.is_none()
        {
            return Err(AgentError::new(
                "Provider has not enabled direct connections",
            ));
        }
        let template_id = string_field(object, "template_id")?;
        let gpu_id = string_field(object, "gpu_id")?;
        let lease_id = optional_string(object, "lease_id")?;
        let ssh_public_key = optional_string(object, "ssh_public_key")?;
        let connection_mode =
            optional_string(object, "connection_mode")?.unwrap_or_else(|| "relay".to_owned());
        let diagnostic = match object.get("diagnostic") {
            None => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => return Err(AgentError::new("diagnostic must be a boolean")),
        };
        let gpu_ids = match object.get("gpu_ids") {
            None | Some(Value::Null) => None,
            Some(Value::Array(items)) => {
                let mut result = Vec::new();
                for item in items {
                    let value = item
                        .as_str()
                        .ok_or_else(|| AgentError::new("gpu_ids must be strings"))?;
                    result.push(value.to_owned());
                }
                Some(result)
            }
            Some(_) => return Err(AgentError::new("gpu_ids must be a list")),
        };
        let memory_bytes = optional_u64(object, "memory_bytes")?;
        let disk_bytes = optional_u64(object, "disk_bytes")?;
        let ssh_public_key = ssh_public_key.unwrap_or_default();
        let mut request = StartRequest::new(&template_id, &gpu_id);
        request.connection_mode = &connection_mode;
        request.lease_id = lease_id.as_deref();
        request.ssh_public_key = &ssh_public_key;
        request.gpu_ids = gpu_ids;
        request.diagnostic = diagnostic;
        request.memory_bytes = memory_bytes;
        request.disk_bytes = disk_bytes;
        self.agent.start(request)?;
        Ok(())
    }

    /// 中央と同期する（Python 版 sync、ブロッキング）。
    fn sync(&self) -> Result<SyncOutcome> {
        if let Some(path) = &self.config_path {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(updated) = serde_json::from_str::<Value>(&text) {
                    if let Some(token) = updated.get("token").and_then(|v| v.as_str()) {
                        if !token.is_empty() {
                            let mut current =
                                self.token.write().unwrap_or_else(|exc| exc.into_inner());
                            if *current != token {
                                *current = token.to_owned();
                                self.credentials_changed.store(true, Ordering::SeqCst);
                            }
                        }
                    }
                }
            }
        }
        let mut snapshot = self.agent.status()?;
        let fingerprints: BTreeMap<String, String> = self
            .agent
            .templates
            .iter()
            .map(|(id, template)| (id.clone(), template.fingerprint()))
            .collect();
        snapshot["templates"] = json!(fingerprints);
        snapshot["direct_host"] = json!(self.config.direct_host.clone().unwrap_or_default());
        snapshot["mock_gpu"] = json!(self.mock);
        let statistics = self
            .statistics
            .lock()
            .unwrap_or_else(|exc| exc.into_inner())
            .clone();
        let results = self.results();
        let token = self
            .token
            .read()
            .unwrap_or_else(|exc| exc.into_inner())
            .clone();
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|exc| AgentError::new(format!("{exc}")))?;
        let url = format!(
            "{}/api/nodes/{}/sync/",
            self.config.url, self.config.node_id
        );
        let response = client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .json(&json!({
                "state": snapshot,
                "statistics": statistics,
                "results": results,
            }))
            .send()
            .map_err(|exc| AgentError::new(format!("{exc}")))?;
        if !response.status().is_success() {
            return Err(AgentError::new(format!(
                "Central server rejected the sync: {}",
                response.status()
            )));
        }
        let raw: Value = response
            .json()
            .map_err(|exc| AgentError::new(format!("{exc}")))?;
        let reply: SyncResponse =
            serde_json::from_value(raw.clone()).map_err(|exc| AgentError::new(format!("{exc}")))?;
        let release = raw.get("release").cloned().unwrap_or(Value::Null);
        for command_id in &reply.acknowledged {
            if let Ok(mut guard) = self.agent.store.locked() {
                guard.command_results.remove(command_id);
                let _ = guard.save();
            }
        }
        Ok(SyncOutcome {
            routes: reply.routes,
            bind_host: reply.relay_bind_host,
            release,
            commands: reply.commands,
        })
    }
}

fn string_field(object: &serde_json::Map<String, Value>, key: &str) -> Result<String> {
    match object.get(key) {
        Some(Value::String(value)) => Ok(value.clone()),
        _ => Err(AgentError::new(format!("'{key}'"))),
    }
}

fn optional_string(object: &serde_json::Map<String, Value>, key: &str) -> Result<Option<String>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        _ => Err(AgentError::new(format!("'{key}'"))),
    }
}

fn optional_u64(object: &serde_json::Map<String, Value>, key: &str) -> Result<Option<u64>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(value)) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| AgentError::new(format!("'{key}'"))),
        _ => Err(AgentError::new(format!("'{key}'"))),
    }
}

/// 提供 PC 側の常駐デーモン。
pub struct ProviderDaemon {
    pub agent: Arc<Agent>,
    pub config: ProviderConfig,
    pub token: Arc<RwLock<String>>,
    pub mock: bool,
    pub interval: f64,
    pub forwarder: tokio::sync::Mutex<Forwarder>,
    pub statistics: Arc<Mutex<Value>>,
    pub health: Arc<Mutex<HealthMonitor>>,
    pub config_path: Option<PathBuf>,
    context: WorkerContext,
    credentials_changed: Arc<AtomicBool>,
    routes: Arc<Mutex<(Vec<RelayRoute>, String)>>,
    inflight: Arc<Mutex<HashSet<String>>>,
    desired_release: Mutex<DesiredRelease>,
    queue_tx: mpsc::UnboundedSender<NodeCommand>,
    queue_rx: Mutex<Option<mpsc::UnboundedReceiver<NodeCommand>>>,
}

impl ProviderDaemon {
    pub fn new(
        agent: Arc<Agent>,
        config: ProviderConfig,
        mock: bool,
        interval: f64,
        config_path: Option<PathBuf>,
    ) -> Result<Self> {
        let token = Arc::new(RwLock::new(config.token.clone()));
        let forwarder = tokio::sync::Mutex::new(Forwarder::new(config.clone(), token.clone())?);
        let (queue_tx, queue_rx) = mpsc::unbounded_channel();
        let statistics = Arc::new(Mutex::new(json!({ "mock_gpu": mock })));
        let credentials_changed = Arc::new(AtomicBool::new(false));
        let context = WorkerContext {
            agent: agent.clone(),
            config: config.clone(),
            token: token.clone(),
            statistics: statistics.clone(),
            config_path: config_path.clone(),
            mock,
            credentials_changed: credentials_changed.clone(),
        };
        let health = HealthMonitor::new(config.watchdog_timeout);
        Ok(Self {
            agent,
            token,
            config,
            mock,
            interval,
            forwarder,
            statistics,
            health: Arc::new(Mutex::new(health)),
            config_path,
            context,
            credentials_changed,
            routes: Arc::new(Mutex::new((Vec::new(), "0.0.0.0".to_owned()))),
            inflight: Arc::new(Mutex::new(HashSet::new())),
            desired_release: Mutex::new(DesiredRelease::Unset),
            queue_tx,
            queue_rx: Mutex::new(Some(queue_rx)),
        })
    }

    fn begin(&self, name: &str) {
        self.health
            .lock()
            .unwrap_or_else(|exc| exc.into_inner())
            .begin(name);
    }

    fn beat(&self, name: &str) {
        self.health
            .lock()
            .unwrap_or_else(|exc| exc.into_inner())
            .beat(name);
    }

    /// コマンドを片付けるワーカー（Python 版 worker）。
    pub async fn worker(self: Arc<Self>) {
        let Some(mut receiver) = self
            .queue_rx
            .lock()
            .unwrap_or_else(|exc| exc.into_inner())
            .take()
        else {
            return;
        };
        while let Some(command) = receiver.recv().await {
            self.begin("worker");
            let id = command.id.clone();
            let context = self.context.clone();
            let _ = tokio::task::spawn_blocking(move || context.execute(&command)).await;
            self.inflight
                .lock()
                .unwrap_or_else(|exc| exc.into_inner())
                .remove(&id);
            self.beat("worker");
        }
    }

    /// 再調和と計測（Python 版 maintain）。
    pub async fn maintain(self: Arc<Self>) {
        loop {
            let context = self.context.clone();
            let gpus = self.agent.gpus.clone();
            let disk_path = self
                .config
                .disk_path
                .clone()
                .unwrap_or_else(|| self.agent.store.directory.to_string_lossy().into_owned());
            let collected = tokio::task::spawn_blocking(move || -> Result<(Value, Value)> {
                context.agent.reconcile()?;
                let metrics = context.agent.statistics()?;
                let host = telemetry::collect(&gpus, Path::new(&disk_path))
                    .map_err(|exc| AgentError::new(format!("{exc}")))?;
                Ok((metrics, host))
            })
            .await;
            match collected {
                Ok(Ok((mut metrics, host))) => {
                    if let (Some(object), Some(host)) = (metrics.as_object_mut(), host.as_object())
                    {
                        for (key, value) in host {
                            object.insert(key.clone(), value.clone());
                        }
                    }
                    metrics["mock_gpu"] = json!(self.mock);
                    metrics["limits"] = json!(self.config.limits);
                    metrics["min_disk_free_bytes"] = json!(self.config.min_disk_free_bytes);
                    metrics["runtime"] = json!(self
                        .config
                        .runtime
                        .clone()
                        .unwrap_or_else(|| "runc".to_owned()));
                    let release = self
                        .health
                        .lock()
                        .unwrap_or_else(|exc| exc.into_inner())
                        .identity()
                        .clone();
                    metrics["release"] = release;
                    let mut benchmarks = serde_json::Map::new();
                    benchmarks.insert("version".into(), json!(1));
                    if let Some(results) = read_results(&self.agent.store.directory).as_object() {
                        for (key, value) in results {
                            benchmarks.insert(key.clone(), value.clone());
                        }
                    }
                    metrics["benchmarks"] = Value::Object(benchmarks);
                    *self
                        .statistics
                        .lock()
                        .unwrap_or_else(|exc| exc.into_inner()) = metrics;
                }
                Ok(Err(exc)) => warn!("Reconciliation/metrics unavailable: {}", exc.0),
                Err(exc) => warn!("Reconciliation/metrics unavailable: {exc}"),
            }
            self.beat("maintenance");
            let interval = if self.interval < 2.0 {
                2.0
            } else {
                self.interval
            };
            tokio::time::sleep(Duration::from_secs_f64(interval)).await;
        }
    }

    /// 中央へ同期する（Python 版 poll）。
    pub async fn poll(self: Arc<Self>) {
        let mut backoff = self.interval;
        loop {
            let context = self.context.clone();
            let outcome = tokio::task::spawn_blocking(move || context.sync()).await;
            match outcome {
                Ok(Ok(data)) => {
                    backoff = self.interval;
                    {
                        let mut routes = self.routes.lock().unwrap_or_else(|exc| exc.into_inner());
                        routes.0 = data.routes;
                        routes.1 = data.bind_host;
                    }
                    {
                        let mut desired = self
                            .desired_release
                            .lock()
                            .unwrap_or_else(|exc| exc.into_inner());
                        if *desired != DesiredRelease::Set(data.release.clone()) {
                            *desired = DesiredRelease::Set(data.release.clone());
                            let path = self.agent.store.directory.join("desired-release.json");
                            if let Err(exc) = atomic_json(path, &data.release) {
                                warn!("Cannot write desired-release.json: {}", exc.0);
                            }
                        }
                    }
                    for command in data.commands {
                        let queued = {
                            let mut inflight =
                                self.inflight.lock().unwrap_or_else(|exc| exc.into_inner());
                            if inflight.contains(&command.id) {
                                false
                            } else {
                                inflight.insert(command.id.clone());
                                true
                            }
                        };
                        if queued {
                            let _ = self.queue_tx.send(command);
                        }
                    }
                }
                Ok(Err(exc)) => {
                    warn!("Central connection unavailable: {}", exc.0);
                    backoff = clamp_backoff(backoff);
                }
                Err(exc) => {
                    warn!("Central connection unavailable: {exc}");
                    backoff = clamp_backoff(backoff);
                }
            }
            self.beat("poll");
            let jitter = rand::thread_rng().gen_range(0.85..1.15);
            tokio::time::sleep(Duration::from_secs_f64(backoff * jitter)).await;
        }
    }

    /// 中継経路の維持（Python 版 tunnels）。
    pub async fn tunnels(self: Arc<Self>) {
        loop {
            if self.credentials_changed.swap(false, Ordering::SeqCst) {
                self.forwarder.lock().await.close().await;
            }
            let (routes, bind_host) = {
                let guard = self.routes.lock().unwrap_or_else(|exc| exc.into_inner());
                (guard.0.clone(), guard.1.clone())
            };
            let agent = self.agent.clone();
            let status = tokio::task::spawn_blocking(move || agent.status()).await;
            match status {
                Ok(Ok(status)) => {
                    let result = self
                        .forwarder
                        .lock()
                        .await
                        .update(&routes, &status, &bind_host)
                        .await;
                    if let Err(exc) = result {
                        warn!("Relay connection unavailable: {exc}");
                        self.forwarder.lock().await.close().await;
                    }
                }
                Ok(Err(exc)) => {
                    warn!("Relay connection unavailable: {}", exc.0);
                    self.forwarder.lock().await.close().await;
                }
                Err(exc) => {
                    warn!("Relay connection unavailable: {exc}");
                    self.forwarder.lock().await.close().await;
                }
            }
            self.beat("tunnels");
            tokio::time::sleep(Duration::from_secs_f64(self.interval)).await;
        }
    }

    /// health.json と systemd 通知（Python 版 watchdog）。
    pub async fn watchdog(self: Arc<Self>) {
        let mut ready = false;
        loop {
            let snapshot = {
                let health = self.health.lock().unwrap_or_else(|exc| exc.into_inner());
                health.snapshot()
            };
            let path = self.agent.store.directory.join("health.json");
            if let Err(exc) = atomic_json(path, &snapshot) {
                warn!("Cannot write health.json: {}", exc.0);
            }
            let healthy = snapshot
                .get("healthy")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let stalled: Vec<String> = snapshot
                .get("stalled")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            if healthy {
                if !ready {
                    notify_systemd("READY=1");
                    ready = true;
                }
                notify_systemd("WATCHDOG=1");
            } else if !stalled.is_empty() {
                error!("Progress watchdog expired: {}", stalled.join(", "));
                notify_systemd("STATUS=Progress stalled\nWATCHDOG=trigger");
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    /// 5 タスクを立ち上げ、停止まで待つ（Python 版 run）。
    pub async fn run(self: Arc<Self>) -> Result<bool> {
        if !self.interval.is_finite() || self.interval < 0.1 {
            return Err(AgentError::new(
                "Polling interval must be a finite number of at least 0.1 seconds",
            ));
        }
        let lock_path = self.agent.store.directory.join("daemon.lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|exc| AgentError::new(format!("{exc}")))?;
        if fs4::FileExt::try_lock(&lock).is_err() {
            return Err(AgentError::new(
                "Another provider operation is still running",
            ));
        }

        let mut worker = tokio::spawn(self.clone().worker());
        let mut maintain = tokio::spawn(self.clone().maintain());
        let mut poll = tokio::spawn(self.clone().poll());
        let mut tunnels = tokio::spawn(self.clone().tunnels());
        let mut watchdog = tokio::spawn(self.clone().watchdog());

        let interrupted = tokio::select! {
            _ = &mut worker => false,
            _ = &mut maintain => false,
            _ = &mut poll => false,
            _ = &mut tunnels => false,
            _ = &mut watchdog => false,
            _ = tokio::signal::ctrl_c() => true,
        };

        for task in [
            &mut worker,
            &mut maintain,
            &mut poll,
            &mut tunnels,
            &mut watchdog,
        ] {
            task.abort();
        }
        for task in [worker, maintain, poll, tunnels, watchdog] {
            let _ = task.await;
        }
        self.forwarder.lock().await.close().await;
        notify_systemd("STOPPING=1");
        // コンテナはサービス再起動を越えて残る。
        Ok(interrupted)
    }
}

/// バックオフを 2〜30 秒の範囲へ収める。
fn clamp_backoff(backoff: f64) -> f64 {
    let doubled = backoff * 2.0;
    let doubled = if doubled < 2.0 { 2.0 } else { doubled };
    if doubled > 30.0 {
        30.0
    } else {
        doubled
    }
}
