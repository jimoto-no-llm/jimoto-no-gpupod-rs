//! 中央中継 SSH サーバー（portal/relay.py の移植、russh 実装）。
//!
//! 提供PCは "node:<uuid>" + 機材キーでパスワード認証し、貸出中ランニングの
//! ポートだけ逆向き転送を受け付ける。シェル・コマンド・SFTP・直接接続・
//! UNIX 転送は拒否する。

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use russh::server::{Auth, Handler, Session};
use russh::{Disconnect, MethodKind, MethodSet};
use tokio::sync::Mutex as AsyncMutex;

use crate::db::{fmt_dt, now};
use crate::models;
use crate::services;

/// 接続 1 本の状態（Python 版 RelayServer の属性に相当）。
#[derive(Default)]
struct ConnState {
    inner: Mutex<ConnStateInner>,
}

#[derive(Default)]
struct ConnStateInner {
    node_id: Option<String>,
    credential_digest: Option<String>,
    attempts: u32,
    ports: BTreeSet<u16>,
    finished: bool,
    closing: bool,
}

impl ConnState {
    fn snapshot(&self) -> (Option<String>, Option<String>, BTreeSet<u16>, bool) {
        let inner = self.inner.lock().unwrap_or_else(|exc| exc.into_inner());
        (
            inner.node_id.clone(),
            inner.credential_digest.clone(),
            inner.ports.clone(),
            inner.finished || inner.closing,
        )
    }

    fn bump_attempts(&self) -> u32 {
        let mut inner = self.inner.lock().unwrap_or_else(|exc| exc.into_inner());
        inner.attempts += 1;
        inner.attempts
    }

    fn authenticated(&self, node_id: String, digest: String) {
        let mut inner = self.inner.lock().unwrap_or_else(|exc| exc.into_inner());
        inner.node_id = Some(node_id);
        inner.credential_digest = Some(digest);
    }

    fn add_port(&self, port: u16) {
        let mut inner = self.inner.lock().unwrap_or_else(|exc| exc.into_inner());
        inner.ports.insert(port);
    }

    fn mark_closing(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|exc| exc.into_inner());
        inner.closing = true;
    }

    fn attempts(&self) -> u32 {
        let inner = self.inner.lock().unwrap_or_else(|exc| exc.into_inner());
        inner.attempts
    }

    fn mark_finished(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|exc| exc.into_inner());
        inner.finished = true;
    }
}

/// 登録済み接続。
struct ConnectionEntry {
    handle: russh::server::Handle,
    state: Arc<ConnState>,
}

type Registry = Arc<AsyncMutex<Vec<ConnectionEntry>>>;

/// 許可ポート（DB の RelayRoute、node が online で token が有効なものだけ）。
pub fn allowed_ports(app: &crate::App, node_hex: &str, digest: Option<&str>) -> BTreeSet<i64> {
    let conn = app.db.lock();
    let Ok(Some(node)) = models::get_node_enabled(&conn, node_hex) else {
        return BTreeSet::new();
    };
    let now_text = fmt_dt(now());
    if !node.online(&now_text) {
        return BTreeSet::new();
    }
    if let Some(digest) = digest {
        if !services::token_allowed(&node, digest, &now_text) {
            return BTreeSet::new();
        }
    }
    models::relay_ports_for_node(&conn, node_hex).unwrap_or_default()
}

/// 中継サーバーのハンドラー。
struct RelayHandler {
    app: Arc<crate::App>,
    state: Arc<ConnState>,
    registry: Registry,
}

impl Handler for RelayHandler {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        // Python 版: 3 回を超えた試行は検証前に切断する。
        if self.state.bump_attempts() > 3 {
            return Ok(Auth::reject());
        }
        if !user.starts_with("node:") {
            return Ok(Auth::reject());
        }
        let Some(node_hex) = models::uuid_to_hex(&user["node:".len()..]) else {
            return Ok(Auth::reject());
        };
        let node = {
            let conn = self.app.db.lock();
            services::authenticate_node(&conn, &node_hex, password, true)
        };
        match node {
            Some(node) => {
                self.state
                    .authenticated(node.id, gpupod_common::token::token_digest(password));
                Ok(Auth::Accept)
            }
            None => Ok(Auth::reject()),
        }
    }

    async fn auth_succeeded(&mut self, session: &mut Session) -> Result<(), Self::Error> {
        if self.state.attempts() > 3 {
            let _ = session.disconnect(Disconnect::ByApplication, "", "");
        }
        Ok(())
    }

    /// 逆向き転送（python 版 server_requested）。
    async fn tcpip_forward(
        &mut self,
        address: &str,
        port: &mut u32,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        let (node_id, digest, _, _) = self.state.snapshot();
        let Some(node_id) = node_id else {
            return Ok(false);
        };
        if address != self.app.cfg.relay_bind_host {
            return Ok(false);
        }
        let allowed = allowed_ports(&self.app, &node_id, digest.as_deref());
        if !allowed.contains(&(*port as i64)) {
            return Ok(false);
        }
        self.state.add_port(*port as u16);
        Ok(true)
    }

    async fn channel_open_session(
        &mut self,
        _channel: russh::Channel<russh::server::Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        // シェル・コマンド・SFTP の前提となるセッションチャネル自体を開けない。
        Ok(false)
    }

    async fn shell_request(
        &mut self,
        channel: russh::ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let _ = session.channel_failure(channel);
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: russh::ChannelId,
        _data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let _ = session.channel_failure(channel);
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: russh::ChannelId,
        _name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let _ = session.channel_failure(channel);
        Ok(())
    }
}

/// 1 秒毎に許可外ポートを持つ接続を閉じる（Python 版 revoke ループ）。
async fn revoke_loop(app: Arc<crate::App>, registry: Registry) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let mut guard = registry.lock().await;
        guard.retain(|entry| {
            let (_, _, _, finished) = entry.state.snapshot();
            !finished
        });
        for entry in guard.iter() {
            let (node_id, digest, ports, _) = entry.state.snapshot();
            let Some(node_id) = node_id else { continue };
            if ports.is_empty() {
                continue;
            }
            let allowed = allowed_ports(&app, &node_id, digest.as_deref());
            if ports.iter().any(|port| !allowed.contains(&(*port as i64))) {
                entry.state.mark_closing();
                // 切断すれば既存のデータチャンネルも同時に閉じる。
                let _ = entry
                    .handle
                    .disconnect(Disconnect::ByApplication, String::new(), String::new())
                    .await;
            }
        }
    }
}

/// 中継 SSH サーバーを起動する。
pub async fn run_relay(app: Arc<crate::App>, host: String, port: u16) -> anyhow::Result<()> {
    let key_path = app.cfg.host_key_path();
    if !key_path.exists() {
        anyhow::bail!("{} がありません。先に init-control を実行してください。", key_path.display());
    }
    let private_key = russh::keys::load_secret_key(&key_path, None)
        .map_err(|exc| anyhow::anyhow!("中継ホスト鍵を読めません: {exc}"))?;

    let mut config = russh::server::Config::default();
    config.keys = vec![private_key];
    let mut methods = MethodSet::empty();
    methods.push(MethodKind::Password);
    config.methods = methods;
    config.keepalive_interval = Some(std::time::Duration::from_secs(10));
    config.keepalive_max = 3;
    // Python 版: 3 回を超えた認証試行で切断する。
    config.max_auth_attempts = 4;
    let config = Arc::new(config);

    let registry: Registry = Arc::default();
    tokio::spawn(revoke_loop(app.clone(), registry.clone()));

    let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
    tracing::info!("Relay listening on {host}:{port}");
    loop {
        let (socket, peer) = listener.accept().await?;
        let state = Arc::new(ConnState::default());
        let handler =
            RelayHandler { app: app.clone(), state: state.clone(), registry: registry.clone() };
        match russh::server::run_stream(config.clone(), socket, handler).await {
            Ok(session) => {
                let handle = session.handle();
                registry
                    .lock()
                    .await
                    .push(ConnectionEntry { handle, state: state.clone() });
                tokio::spawn(async move {
                    let _ = session.await;
                    state.mark_finished();
                });
            }
            Err(exc) => tracing::debug!("relay handshake failed from {peer}: {exc}"),
        }
    }
}

/// 中継ホスト鍵の用意（init_control）。既存の鍵は作り替えない。
pub fn init_host_key(path: &Path) -> anyhow::Result<String> {
    if path.exists() {
        let key = russh::keys::load_secret_key(path, None)
            .map_err(|exc| anyhow::anyhow!("中継ホスト鍵を読めません: {exc}"))?;
        return Ok(key.public_key().fingerprint(russh::keys::HashAlg::Sha256).to_string());
    }
    if let Some(parent) = path.parent() {
        crate::config::create_dir_private(parent)?;
    }
    let key = russh::keys::PrivateKey::random(&mut rand::rngs::OsRng, russh::keys::Algorithm::Ed25519)
        .map_err(|exc| anyhow::anyhow!("鍵を生成できません: {exc}"))?;
    let encoded =
        key.to_openssh(russh::keys::ssh_key::LineEnding::LF).map_err(|exc| anyhow::anyhow!("{exc}"))?;
    crate::config::write_private(path, encoded.as_bytes()).map_err(anyhow::Error::from)?;
    Ok(key.public_key().fingerprint(russh::keys::HashAlg::Sha256).to_string())
}
