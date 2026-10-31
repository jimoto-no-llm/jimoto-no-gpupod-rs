//! 中継（relay.rs）の統合テスト。
//!
//! 実際の SSH クライアント（提供PCの代役）で中継へ接続し、
//! 認証・転送ポートの許可判定・forwarded-tcpip のデータ経路を確かめる。
//! ネットワークは 127.0.0.1 のループバックだけを使う。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gpupod_common::Template;
use gpupod_control::db::{fmt_dt, now};
use gpupod_control::models;
use gpupod_control::oauth::{DiscordApi, DiscordUser};
use gpupod_control::services;
use gpupod_control::{relay, App, Config};
use russh::client::{Config as SshConfig, Handler as ClientHandler};
use russh::keys::PublicKey;
use russh::Channel;
use serde_json::json;

/// Discord は呼ばない（ブロッキングクライアントを非同期テスト内で作らない）。
struct UnusedDiscord;

impl DiscordApi for UnusedDiscord {
    fn exchange_code(
        &self,
        _client_id: &str,
        _client_secret: &str,
        _code: &str,
        _redirect_uri: &str,
        _code_verifier: &str,
    ) -> Result<String, String> {
        Err("unused".to_string())
    }

    fn fetch_user(&self, _access_token: &str) -> Result<DiscordUser, String> {
        Err("unused".to_string())
    }

    fn list_guilds(&self, _access_token: &str) -> Result<Vec<String>, String> {
        Err("unused".to_string())
    }

    fn guild_member_roles(
        &self,
        _access_token: &str,
        _guild_id: &str,
    ) -> Result<Option<Vec<String>>, String> {
        Err("unused".to_string())
    }
}

/// 空いているループバックポートを取る。
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

/// 中継と、転送先が 1 本ある提供PCを用意する。
async fn setup(
    relay_port: u16,
    forward_port: u16,
) -> (tempfile::TempDir, Arc<App>, String, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::from_env();
    config.state_dir = dir.path().to_path_buf();
    config.relay_host = "127.0.0.1".to_string();
    config.relay_bind_host = "127.0.0.1".to_string();
    config.relay_port = relay_port;
    let app = App::with_discord(config, Arc::new(UnusedDiscord)).expect("app");
    relay::init_host_key(&app.cfg.host_key_path()).expect("host key");

    let (node, token) = {
        let conn = app.db.lock();
        services::enroll_node(&conn, "provider-a").expect("enroll")
    };
    {
        let conn = app.db.lock();
        let template = Template::from_value(json!({
            "id": "ubuntu-24.04",
            "image": "gpupod/ubuntu:24.04",
            "vendors": ["nvidia"],
            "ports": ["22/tcp"],
        }))
        .expect("template");
        models::upsert_template(&conn, &template.id, &template.to_value()).expect("template row");
        conn.execute(
            "UPDATE portal_node SET last_seen = ?2, enabled = 1, desired_enabled = 1 WHERE id = ?1",
            rusqlite::params![node.id, fmt_dt(now())],
        )
        .expect("online node");
        let user_id = models::insert_user(
            &conn,
            "tester",
            &gpupod_control::auth::hash_password("pw"),
            false,
            false,
            true,
            "",
        )
        .expect("user");
        let lease = models::insert_lease(
            &conn,
            user_id,
            &node.id,
            "gpu-0",
            &template.id,
            "",
            "relay",
            0,
            0,
        )
        .expect("lease");
        conn.execute(
            "UPDATE portal_lease SET status = 'running', active = 1 WHERE id = ?1",
            rusqlite::params![lease.id],
        )
        .expect("running lease");
        models::insert_route(&conn, &lease.id, "22/tcp", forward_port as i64).expect("route");
    }

    let handle = tokio::spawn(relay::run_relay(
        app.clone(),
        "127.0.0.1".to_string(),
        relay_port,
    ));
    // 中継のリスン開始を待つ。
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", relay_port))
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!handle.is_finished(), "relay exited early");
    (dir, app, node.uuid_str(), token)
}

/// 提供PCの代役。forwarded-tcpip を手元のエコーサーバーへ繋ぐ。
struct NodeClient {
    expected_key: PublicKey,
    forwarded_port: u16,
    echo_addr: SocketAddr,
}

impl ClientHandler for NodeClient {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
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
    ) -> Result<(), Self::Error> {
        if connected_port as u16 != self.forwarded_port {
            let _ = channel.close().await;
            return Ok(());
        }
        let echo = self.echo_addr;
        tokio::spawn(async move {
            let Ok(mut stream) = tokio::net::TcpStream::connect(echo).await else {
                return;
            };
            let mut tunnel = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut stream, &mut tunnel).await;
        });
        Ok(())
    }
}

/// 127.0.0.1 のエコーサーバー。
async fn echo_server() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind echo");
    let address = listener.local_addr().expect("echo addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let (mut read, mut write) = socket.split();
                let _ = tokio::io::copy(&mut read, &mut write).await;
            });
        }
    });
    address
}

/// 中継へ接続してノード認証する。
async fn connect_node(
    app: &App,
    node_text: &str,
    token: &str,
    forwarded_port: u16,
    echo_addr: SocketAddr,
) -> russh::client::Handle<NodeClient> {
    let expected_key = russh::keys::load_secret_key(&app.cfg.host_key_path(), None)
        .expect("host key")
        .public_key()
        .clone();
    let client = NodeClient {
        expected_key,
        forwarded_port,
        echo_addr,
    };
    let mut handle = russh::client::connect(
        Arc::new(SshConfig::default()),
        ("127.0.0.1", app.cfg.relay_port),
        client,
    )
    .await
    .expect("ssh connect");
    let auth = handle
        .authenticate_password(format!("node:{node_text}"), token)
        .await
        .expect("auth");
    assert!(matches!(auth, russh::client::AuthResult::Success));
    handle
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwards_allowed_port_to_the_node() {
    let relay_port = free_port();
    let forward_port = free_port();
    let (_dir, app, node_text, token) = setup(relay_port, forward_port).await;
    let echo = echo_server().await;

    let mut handle = connect_node(&app, &node_text, &token, forward_port, echo).await;
    // 明示ポートの要求では russh は 0 を返す（拒否なら Err）。
    assert_eq!(
        handle
            .tcpip_forward("127.0.0.1", forward_port as u32)
            .await
            .expect("granted"),
        0
    );

    // 中継ポートへ TCP 接続し、提供PC（エコー）まで往復する。
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", forward_port))
        .await
        .expect("connect forwarded port");
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    socket
        .write_all(b"ping-through-relay")
        .await
        .expect("write");
    let mut buffer = vec![0u8; b"ping-through-relay".len()];
    tokio::time::timeout(Duration::from_secs(5), socket.read_exact(&mut buffer))
        .await
        .expect("echo timeout")
        .expect("echo read");
    assert_eq!(&buffer, b"ping-through-relay");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refuses_ports_that_are_not_assigned() {
    let relay_port = free_port();
    let forward_port = free_port();
    let (_dir, app, node_text, token) = setup(relay_port, forward_port).await;
    let echo = echo_server().await;
    let mut handle = connect_node(&app, &node_text, &token, forward_port, echo).await;

    // 未割当のポート。
    let other = free_port();
    assert!(handle
        .tcpip_forward("127.0.0.1", other as u32)
        .await
        .is_err());
    // 特権ポート。
    assert!(handle.tcpip_forward("127.0.0.1", 22).await.is_err());
    // 中継の制御ポート。
    assert!(handle
        .tcpip_forward("127.0.0.1", relay_port as u32)
        .await
        .is_err());
    // 許可されていないバインド先。
    assert!(handle
        .tcpip_forward("0.0.0.0", forward_port as u32)
        .await
        .is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_handshake_does_not_block_other_nodes() {
    let relay_port = free_port();
    let forward_port = free_port();
    let (_dir, app, node_text, token) = setup(relay_port, forward_port).await;
    let echo = echo_server().await;

    // 何も送らない接続を 1 本開きっぱなしにする（従来はここで accept が止まった）。
    let stalled = tokio::net::TcpStream::connect(("127.0.0.1", relay_port))
        .await
        .expect("stalled connect");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 別の提供PCは期限内に認証・転送まで到達できる。
    let connecting = tokio::time::timeout(
        Duration::from_secs(5),
        connect_node(&app, &node_text, &token, forward_port, echo),
    )
    .await
    .expect("accept loop was blocked by a stalled handshake");
    let mut handle = connecting;
    assert!(handle
        .tcpip_forward("127.0.0.1", forward_port as u32)
        .await
        .is_ok());
    drop(stalled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_bad_credentials() {
    let relay_port = free_port();
    let forward_port = free_port();
    let (_dir, app, node_text, _token) = setup(relay_port, forward_port).await;
    let echo = echo_server().await;
    let expected_key = russh::keys::load_secret_key(&app.cfg.host_key_path(), None)
        .expect("host key")
        .public_key()
        .clone();
    let client = NodeClient {
        expected_key,
        forwarded_port: forward_port,
        echo_addr: echo,
    };
    let mut handle = russh::client::connect(
        Arc::new(SshConfig::default()),
        ("127.0.0.1", app.cfg.relay_port),
        client,
    )
    .await
    .expect("ssh connect");
    let auth = handle
        .authenticate_password(format!("node:{node_text}"), "wrong-token")
        .await
        .expect("auth reply");
    assert!(!matches!(auth, russh::client::AuthResult::Success));
}
