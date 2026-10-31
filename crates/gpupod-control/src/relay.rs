//! 中央中継 SSH サーバー（portal/relay.py の移植、russh 実装）。
//!
//! 提供PCは "node:<uuid>" + 機材キーでパスワード認証し、貸出中ランニングの
//! ポートだけ逆向き転送を受け付ける。シェル・コマンド・SFTP・直接接続・
//! UNIX 転送は拒否する。

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::server::{Auth, Handler, Session};
use russh::{Disconnect, MethodKind, MethodSet};
use tokio::net::TcpListener;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

use crate::db::{fmt_dt, now};
use crate::models;
use crate::services;

/// 認証が終わるまでの猶予。無認証の接続で待ち続けない。
const PRE_AUTH_TIMEOUT: Duration = Duration::from_secs(30);
/// アイドル接続の切断（提供PCは 10 秒ごとに keepalive を送る）。
const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(180);
/// 中継で待ち受けてよい最小ポート（特権ポートを中継しない）。
const MIN_RELAY_PORT: u16 = 1024;
/// 1 接続が同時に待ち受けてよい転送ポート数。
const MAX_FORWARD_PORTS: usize = 256;
/// 中継ポート 1 本あたりの同時接続数の上限。
const MAX_FORWARDED_CONNECTIONS: usize = 256;

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
    /// 実際に待ち受けている中継ポートと、その accept ループ。
    listeners: HashMap<u16, JoinHandle<()>>,
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

    fn remove_port(&self, port: u16) {
        let mut inner = self.inner.lock().unwrap_or_else(|exc| exc.into_inner());
        inner.ports.remove(&port);
    }

    /// このポートを既に待ち受けているか。
    fn has_listener(&self, port: u16) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|exc| exc.into_inner());
        inner.listeners.contains_key(&port)
    }

    /// 待ち受けの上限に達しているか。
    fn listeners_full(&self) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|exc| exc.into_inner());
        inner.listeners.len() >= MAX_FORWARD_PORTS
    }

    /// accept ループを登録し、同名の古いループは止める。
    fn add_listener(&self, port: u16, task: JoinHandle<()>) {
        let mut inner = self.inner.lock().unwrap_or_else(|exc| exc.into_inner());
        if let Some(previous) = inner.listeners.insert(port, task) {
            previous.abort();
        }
    }

    /// 1 ポートの待ち受けを止める。止めたら true。
    fn remove_listener(&self, port: u16) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|exc| exc.into_inner());
        match inner.listeners.remove(&port) {
            Some(task) => {
                task.abort();
                true
            }
            None => false,
        }
    }

    fn mark_closing(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|exc| exc.into_inner());
        inner.closing = true;
        for (_, task) in inner.listeners.drain() {
            task.abort();
        }
    }

    fn attempts(&self) -> u32 {
        let inner = self.inner.lock().unwrap_or_else(|exc| exc.into_inner());
        inner.attempts
    }

    fn mark_finished(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|exc| exc.into_inner());
        inner.finished = true;
        for (_, task) in inner.listeners.drain() {
            task.abort();
        }
    }
}

/// 転送要求の (address, port) を検証し、待ち受けるポートを返す。
///
/// 中継が実際に待ち受けるホストか、提供PCが同期で受け取る
/// バインド先ホストと一致した場合だけ許可する。特権ポートと
/// 制御ポートは中継しない。
pub fn forward_port(
    address: &str,
    port: u32,
    listen_host: &str,
    listen_port: u16,
    relay_bind_host: &str,
) -> Option<u16> {
    if address != listen_host && address != relay_bind_host {
        return None;
    }
    let port = u16::try_from(port).ok()?;
    if port < MIN_RELAY_PORT || port == listen_port {
        return None;
    }
    Some(port)
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
    /// 中継が実際に待ち受けているホスト（転送ポートもここで待ち受ける）。
    listen_host: String,
    listen_port: u16,
}

impl Handler for RelayHandler {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        // Python 版: 3 回を超えた試行は検証前に切断する。
        if self.state.bump_attempts() > 3 {
            return Ok(Auth::reject());
        }
        // 巨大なパスワードでハッシュ計算を膨らませない。
        if password.len() > 512 {
            return Ok(Auth::reject());
        }
        let Some(node_text) = user.strip_prefix("node:") else {
            return Ok(Auth::reject());
        };
        let Some(node_hex) = models::uuid_to_hex(node_text) else {
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
    ///
    /// 許可されたポートだけを実際に待ち受け、届いた TCP 接続を
    /// forwarded-tcpip チャネルで提供PCへ渡す。
    async fn tcpip_forward(
        &mut self,
        address: &str,
        port: &mut u32,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        let (node_id, digest, _, _) = self.state.snapshot();
        let Some(node_id) = node_id else {
            return Ok(false);
        };
        let Some(relay_port) = forward_port(
            address,
            *port,
            &self.listen_host,
            self.listen_port,
            &self.app.cfg.relay_bind_host,
        ) else {
            return Ok(false);
        };
        let allowed = allowed_ports(&self.app, &node_id, digest.as_deref());
        if !allowed.contains(&(relay_port as i64)) {
            return Ok(false);
        }
        if self.state.has_listener(relay_port) {
            return Ok(true);
        }
        if self.state.listeners_full() {
            tracing::warn!("中継: 転送ポートの上限を超えたため転送を拒否しました");
            return Ok(false);
        }
        let listener = match TcpListener::bind((self.listen_host.as_str(), relay_port)).await {
            Ok(listener) => listener,
            Err(exc) => {
                tracing::debug!("中継: {relay_port} を待ち受けできません: {exc}");
                return Ok(false);
            }
        };
        let handle = session.handle();
        let task = tokio::spawn(serve_forwarded(
            listener,
            handle,
            address.to_owned(),
            relay_port,
        ));
        self.state.add_listener(relay_port, task);
        self.state.add_port(relay_port);
        Ok(true)
    }

    /// 転送の取り消し（提供PCの切断時に届く）。
    async fn cancel_tcpip_forward(
        &mut self,
        address: &str,
        port: u32,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        let Ok(relay_port) = u16::try_from(port) else {
            return Ok(false);
        };
        if address != self.listen_host && address != self.app.cfg.relay_bind_host {
            return Ok(false);
        }
        // 失効判定に残さない（取り消したポートで接続全体を切らない）。
        self.state.remove_port(relay_port);
        Ok(self.state.remove_listener(relay_port))
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
        anyhow::bail!(
            "{} がありません。先に init-control を実行してください。",
            key_path.display()
        );
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
    config.inactivity_timeout = Some(INACTIVITY_TIMEOUT);
    // Python 版: 3 回を超えた認証試行で切断する。
    config.max_auth_attempts = 4;
    let config = Arc::new(config);

    let registry: Registry = Arc::default();
    tokio::spawn(revoke_loop(app.clone(), registry.clone()));

    let listener = TcpListener::bind((host.as_str(), port)).await?;
    tracing::info!("Relay listening on {host}:{port}");
    loop {
        let (socket, peer) = listener.accept().await?;
        // ハンドシェイクを accept ループで待たない。1 本の停滞で全提供PCを
        // 締め出さないよう、接続ごとに独立したタスクで処理する。
        let app = app.clone();
        let registry = registry.clone();
        let config = config.clone();
        let listen_host = host.clone();
        let listen_port = port;
        tokio::spawn(async move {
            let state = Arc::new(ConnState::default());
            let handler = RelayHandler {
                app,
                state: state.clone(),
                registry: registry.clone(),
                listen_host,
                listen_port,
            };
            let session = tokio::time::timeout(
                PRE_AUTH_TIMEOUT,
                russh::server::run_stream(config, socket, handler),
            )
            .await;
            let session = match session {
                Ok(Ok(session)) => session,
                Ok(Err(exc)) => {
                    tracing::debug!("relay handshake failed from {peer}: {exc}");
                    return;
                }
                Err(_) => {
                    tracing::debug!("relay handshake timed out from {peer}");
                    return;
                }
            };
            let handle = session.handle();
            registry.lock().await.push(ConnectionEntry {
                handle: handle.clone(),
                state: state.clone(),
            });
            // 認証が終わらない接続は期限で切る（認証後は keepalive で維持される）。
            tokio::spawn({
                let state = state.clone();
                let handle = handle.clone();
                async move {
                    tokio::time::sleep(PRE_AUTH_TIMEOUT).await;
                    let (node_id, _, _, _) = state.snapshot();
                    if node_id.is_none() {
                        let _ = handle
                            .disconnect(Disconnect::ByApplication, String::new(), String::new())
                            .await;
                    }
                }
            });
            let _ = session.await;
            state.mark_finished();
        });
    }
}

/// 1 本の転送ポートを待ち受け、届いた TCP 接続を forwarded-tcpip で提供PCへ流す。
async fn serve_forwarded(
    listener: TcpListener,
    handle: russh::server::Handle,
    bind_address: String,
    relay_port: u16,
) {
    let limit = Arc::new(tokio::sync::Semaphore::new(MAX_FORWARDED_CONNECTIONS));
    loop {
        let Ok((mut socket, peer)) = listener.accept().await else {
            return;
        };
        // 1 ポートあたりの同時接続数を抑え、fd とメモリを守る。
        let Ok(permit) = limit.clone().try_acquire_owned() else {
            tracing::debug!("中継: {relay_port} の同時接続が上限に達したため切断します");
            continue;
        };
        let handle = handle.clone();
        let bind_address = bind_address.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let Ok(channel) = handle
                .channel_open_forwarded_tcpip(
                    bind_address,
                    relay_port as u32,
                    peer.ip().to_string(),
                    peer.port() as u32,
                )
                .await
            else {
                return;
            };
            let mut stream = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
        });
    }
}

/// 中継ホスト鍵の用意（init_control）。既存の鍵は作り替えない。
pub fn init_host_key(path: &Path) -> anyhow::Result<String> {
    if path.exists() {
        // 既存の鍵も他ユーザーから読めないよう 0600 に寄せる。
        crate::config::restrict_private(path);
        let key = russh::keys::load_secret_key(path, None)
            .map_err(|exc| anyhow::anyhow!("中継ホスト鍵を読めません: {exc}"))?;
        return Ok(key
            .public_key()
            .fingerprint(russh::keys::HashAlg::Sha256)
            .to_string());
    }
    if let Some(parent) = path.parent() {
        crate::config::create_dir_private(parent)?;
    }
    let key =
        russh::keys::PrivateKey::random(&mut rand::rngs::OsRng, russh::keys::Algorithm::Ed25519)
            .map_err(|exc| anyhow::anyhow!("鍵を生成できません: {exc}"))?;
    let encoded = key
        .to_openssh(russh::keys::ssh_key::LineEnding::LF)
        .map_err(|exc| anyhow::anyhow!("{exc}"))?;
    crate::config::write_private(path, encoded.as_bytes()).map_err(anyhow::Error::from)?;
    Ok(key
        .public_key()
        .fingerprint(russh::keys::HashAlg::Sha256)
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_port_requires_the_relay_bind_address() {
        // 設定値（提供PCが同期で受け取る値）と一致すれば許可。
        assert_eq!(
            forward_port("0.0.0.0", 19001, "0.0.0.0", 19000, "0.0.0.0"),
            Some(19001)
        );
        // 実際のリスン先が別でも、その値なら許可。
        assert_eq!(
            forward_port("127.0.0.1", 19001, "127.0.0.1", 19000, "0.0.0.0"),
            Some(19001)
        );
        // それ以外のバインド先は拒否（任意ホスト・任意 IF を開かせない）。
        assert_eq!(
            forward_port("10.0.0.1", 19001, "0.0.0.0", 19000, "0.0.0.0"),
            None
        );
        assert_eq!(forward_port("", 19001, "0.0.0.0", 19000, "0.0.0.0"), None);
    }

    #[test]
    fn forward_port_refuses_privileged_and_control_ports() {
        assert_eq!(
            forward_port("0.0.0.0", 22, "0.0.0.0", 19000, "0.0.0.0"),
            None
        );
        assert_eq!(
            forward_port("0.0.0.0", 1023, "0.0.0.0", 19000, "0.0.0.0"),
            None
        );
        assert_eq!(
            forward_port("0.0.0.0", 1024, "0.0.0.0", 19000, "0.0.0.0"),
            Some(1024)
        );
        // 制御ポート自身は中継しない。
        assert_eq!(
            forward_port("0.0.0.0", 19000, "0.0.0.0", 19000, "0.0.0.0"),
            None
        );
        // u16 を超える値は拒否。
        assert_eq!(
            forward_port("0.0.0.0", 70000, "0.0.0.0", 19000, "0.0.0.0"),
            None
        );
    }
}
