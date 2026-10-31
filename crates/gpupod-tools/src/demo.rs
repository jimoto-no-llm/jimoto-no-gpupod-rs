//! 同居デモの起動（Python 版 demo.py の移植）。
//!
//! 中央パネルと提供 PC を同一プロセスで起動する。gpupod-control / gpupod-agent の
//! lib 実装を `--features full-stack` で使う。

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::build_images;

#[derive(Debug, Default)]
pub struct Options {
    pub pytorch: bool,
    pub state_dir: PathBuf,
    pub port: u16,
    pub relay_port: u16,
}

pub fn run(options: &Options) -> Result<()> {
    let root = crate::support::repo::repo_root()?;
    if options.pytorch {
        build_images::run(&["cpu".to_string()], &root)?;
    }
    let state = root.join(&options.state_dir);
    std::fs::create_dir_all(&state)?;
    crate::support::files::fs_set_mode(&state, 0o700)?;
    start_in_process(&state, options)
}

/// 中央（web + relay）と提供（ダミー GPU デーモン）を同一プロセスで起動する。
#[cfg(feature = "full-stack")]
fn start_in_process(state: &std::path::Path, options: &Options) -> Result<()> {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use gpupod_agent::{
        mock_gpus, Agent, BollardEngine, EngineOptions, ProviderConfig as AgentConfig,
        ProviderDaemon, StateStore,
    };
    use gpupod_control::{relay, services, web, App, Config as ControlConfig};

    let control = ControlConfig {
        state_dir: state.join("control"),
        allowed_hosts: vec![
            format!("127.0.0.1:{}", options.port),
            format!("localhost:{}", options.port),
        ],
        csrf_origins: vec![format!("http://127.0.0.1:{}", options.port)],
        relay_host: "127.0.0.1".to_string(),
        relay_port: options.relay_port,
        relay_port_min: options.relay_port.saturating_add(1),
        relay_port_max: options.relay_port.saturating_add(999),
        relay_bind_host: "127.0.0.1".to_string(),
        trust_proxy: false,
        insecure_http: true,
        discord_client_id: None,
        discord_client_secret: None,
        public_base_url: Some(format!("http://127.0.0.1:{}", options.port)),
        discord_guild_id: None,
        discord_allowed_role_ids: Vec::new(),
        discord_invite_url: None,
    };
    let app = App::new(control)?;
    let (node, token) = {
        let connection = app.db.lock();
        services::enroll_node(&connection, "MOCK provider (GPUなし)")
            .map_err(|exc| anyhow::anyhow!("Cannot enroll the demo provider: {exc}"))?
    };
    let credentials = AgentConfig {
        url: format!("http://127.0.0.1:{}", options.port),
        node_id: node.id.clone(),
        token,
        relay_host: "127.0.0.1".to_string(),
        relay_host_key: crate::support::sshkey::generate_public_key()?,
        relay_port: options.relay_port,
        direct_host: None,
        direct_bind: None,
        limits: BTreeMap::new(),
        min_disk_free_bytes: 0,
        runtime: None,
        watchdog_timeout: 180.0,
        disk_path: None,
        extra: BTreeMap::new(),
    };
    let credentials_path = state.join("provider.secret.json");
    crate::support::files::atomic_json(&credentials_path, &serde_json::to_value(&credentials)?)?;
    let templates = gpupod_common::load_templates(crate::prepare_template::default_catalog(None))?;
    let store = Arc::new(StateStore::new(state.join("provider"))?);
    let engine_options = EngineOptions {
        mock_gpu: true,
        ..Default::default()
    };
    let engine =
        Arc::new(BollardEngine::new(engine_options).map_err(|exc| anyhow::anyhow!("{exc}"))?);
    let agent = Arc::new(Agent::new(store, engine, templates, mock_gpus()));
    let daemon = Arc::new(
        ProviderDaemon::new(agent, credentials, true, 5.0, Some(credentials_path))
            .map_err(|exc| anyhow::anyhow!("{exc}"))?,
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Cannot start the demo runtime")?;
    let port = options.port;
    let relay_port = options.relay_port;
    runtime.block_on(async move {
        let provider = tokio::spawn(async move {
            daemon.run().await.map(|_| ()).map_err(|exc| anyhow::anyhow!("{exc}"))
        });
        let central = tokio::spawn(async move {
            let bind = format!("127.0.0.1:{port}");
            let relay_host = "127.0.0.1".to_string();
            let web_task = tokio::spawn({
                let app = app.clone();
                async move { web::serve(app, &bind).await }
            });
            let relay_task = tokio::spawn(async move {
                relay::run_relay(app, relay_host, relay_port).await
            });
            let _ = web_task.await;
            let _ = relay_task.await;
            Ok::<(), anyhow::Error>(())
        });
        println!(
            "\nPanel: http://127.0.0.1:{port}/\nDummy GPU inventory. Docker, SSH, relay and workspaces are real.\nAccounts: gpupod-control create-superuser --help\nPress Ctrl+C to stop the demo.\n"
        );
        let _ = central.await;
        let _ = provider.await;
        Ok::<(), anyhow::Error>(())
    })
}

#[cfg(not(feature = "full-stack"))]
fn start_in_process(_state: &std::path::Path, _options: &Options) -> Result<()> {
    bail!("demo requires the full-stack feature: build with --features full-stack")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(feature = "full-stack"))]
    #[test]
    fn demo_reports_the_missing_stack_clearly() {
        let options = Options {
            pytorch: false,
            state_dir: PathBuf::from(".demo-state"),
            port: 8000,
            relay_port: 19000,
        };
        // リポジトリルートが見つかっても、feature が無ければ明示的なエラーになる。
        match run(&options) {
            Err(exc) => {
                let message = format!("{exc:#}");
                assert!(
                    message.contains("full-stack") || message.contains("repository root"),
                    "{message}"
                );
            }
            Ok(()) => panic!("demo must not start without the stack"),
        }
    }
}
