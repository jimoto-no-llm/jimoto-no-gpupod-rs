//! 提供 PC 側エージェントの CLI。
//!
//! サブコマンドと引数名は Python 版 provider_agent.__main__ と同じ。

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use gpupod_agent::daemon::ProviderDaemon;
use gpupod_agent::{
    gpu_to_value, load_config, load_gpus, mock_gpus, Agent, BollardEngine, EngineOptions,
    StartRequest, StateStore,
};
use gpupod_common::resources::gib_bytes;
use gpupod_common::{load_templates, AgentError};
use serde_json::{json, Value};

#[derive(Parser)]
#[command(about = "Local GPU provider agent")]
struct Cli {
    #[arg(long, default_value = ".provider-state")]
    state_dir: String,
    #[arg(long, default_value = "examples/templates.json")]
    templates: String,
    #[arg(long)]
    devices: Option<String>,
    #[arg(long, default_value = "/var/run/docker.sock")]
    docker_socket: String,
    #[arg(long)]
    mock_gpu: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum OffMode {
    Drain,
    Stop,
}

impl OffMode {
    fn as_str(self) -> &'static str {
        match self {
            OffMode::Drain => "drain",
            OffMode::Stop => "stop",
        }
    }
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum Signal {
    Busy,
    Idle,
}

#[derive(Subcommand)]
enum Command {
    /// 現在の状態を表示する
    Status,
    /// GPU の在庫を表示する
    Devices,
    /// テンプレート一覧を表示する
    Templates,
    /// 貸出受付を ON にする
    On,
    /// 貸出受付を OFF にする
    Off {
        #[arg(long, value_enum, default_value = "drain")]
        mode: OffMode,
    },
    /// コンテナを起動して貸出を受ける
    Start {
        #[arg(long)]
        template: String,
        #[arg(long)]
        gpu: String,
        #[arg(long)]
        memory_gib: Option<String>,
        #[arg(long)]
        disk_gib: Option<String>,
    },
    /// 貸出を返却する
    Stop { lease_id: String },
    /// コンテナごとの使用量を表示する
    Stats,
    /// 状態と実態を突き合わせる
    Reconcile,
    /// RAM とディスクの貸出枠を設定する
    Resources {
        #[arg(long)]
        memory_gib: String,
        #[arg(long)]
        disk_gib: String,
        #[arg(long)]
        default_memory_gib: Option<String>,
        #[arg(long)]
        default_disk_gib: Option<String>,
    },
    /// 自動混雑検知の信号を記録する
    Auto {
        #[arg(value_enum)]
        signal: Signal,
        #[arg(long, value_enum, default_value = "drain")]
        mode: OffMode,
    },
    /// 状態を JSON 行で監視する
    Watch {
        #[arg(long, default_value_t = 10.0)]
        interval: f64,
    },
    /// 中央と常時同期する常駐プロセス
    Daemon {
        #[arg(long)]
        config: String,
        #[arg(long, default_value_t = 2.0)]
        interval: f64,
    },
}

fn main() {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(exc) => {
            let text = serde_json::to_string(&json!({ "error": exc.0 })).unwrap_or_default();
            eprintln!("{text}");
            std::process::exit(1);
        }
    }
}

fn run(cli: Cli) -> Result<i32, AgentError> {
    let templates = load_templates(&cli.templates)?;
    let gpus = if cli.mock_gpu && cli.devices.is_none() {
        mock_gpus()
    } else {
        load_gpus(cli.devices.as_deref())?
    };

    match &cli.command {
        Command::Devices => {
            let result: Vec<Value> = gpus.iter().map(gpu_to_value).collect();
            print_pretty(&Value::Array(result))?;
            return Ok(0);
        }
        Command::Templates => {
            let result: Vec<Value> = templates.values().map(|item| item.to_value()).collect();
            print_pretty(&Value::Array(result))?;
            return Ok(0);
        }
        _ => {}
    }

    let config = match &cli.command {
        Command::Daemon { config, .. } => Some(load_config(config)?),
        _ => None,
    };
    let store = Arc::new(StateStore::new(&cli.state_dir)?);
    let engine_options = EngineOptions {
        socket_path: cli.docker_socket.clone(),
        limits: config
            .as_ref()
            .map(|config| config.limits.clone())
            .unwrap_or_default(),
        direct_bind: config
            .as_ref()
            .and_then(|config| config.direct_bind.clone()),
        mock_gpu: cli.mock_gpu,
        runtime: config.as_ref().and_then(|config| config.runtime.clone()),
        ..Default::default()
    };
    let engine = Arc::new(
        BollardEngine::new(engine_options).map_err(|exc| AgentError::new(exc.to_string()))?,
    );
    let agent = Arc::new(Agent::new(store, engine, templates, gpus));

    match &cli.command {
        Command::Status => print_pretty(&agent.status()?)?,
        Command::On => print_pretty(&agent.on()?)?,
        Command::Off { mode } => print_pretty(&agent.off(mode.as_str())?)?,
        Command::Auto { signal, mode } => {
            let busy = matches!(signal, Signal::Busy);
            print_pretty(&agent.set_auto_busy(busy, mode.as_str())?)?
        }
        Command::Resources {
            memory_gib,
            disk_gib,
            default_memory_gib,
            default_disk_gib,
        } => {
            let summary = agent.configure_resources(
                gib_bytes(memory_gib, "memory_bytes")?,
                gib_bytes(disk_gib, "disk_bytes")?,
                default_memory_gib
                    .as_deref()
                    .map(|value| gib_bytes(value, "memory_bytes"))
                    .transpose()?,
                default_disk_gib
                    .as_deref()
                    .map(|value| gib_bytes(value, "disk_bytes"))
                    .transpose()?,
            )?;
            print_pretty(&summary)?;
        }
        Command::Start {
            template,
            gpu,
            memory_gib,
            disk_gib,
        } => {
            let memory_bytes = memory_gib
                .as_deref()
                .map(|value| gib_bytes(value, "memory_bytes"))
                .transpose()?;
            let disk_bytes = disk_gib
                .as_deref()
                .map(|value| gib_bytes(value, "disk_bytes"))
                .transpose()?;
            let mut request = StartRequest::new(template, gpu);
            request.memory_bytes = memory_bytes;
            request.disk_bytes = disk_bytes;
            print_pretty(&agent.start(request)?)?;
        }
        Command::Stop { lease_id } => print_pretty(&agent.stop(lease_id)?)?,
        Command::Stats => print_pretty(&agent.statistics()?)?,
        Command::Reconcile => print_pretty(&agent.reconcile()?)?,
        Command::Watch { interval } => {
            if !interval.is_finite() || *interval < 1.0 {
                return Err(AgentError::new(
                    "Watch interval must be a finite number of at least one second",
                ));
            }
            loop {
                agent.reconcile()?;
                let event = match agent.statistics() {
                    Ok(statistics) => {
                        json!({ "state": agent.status()?, "statistics": statistics })
                    }
                    Err(exc) => json!({ "state": agent.status()?, "statistics_error": exc.0 }),
                };
                print_line(&event)?;
                std::thread::sleep(Duration::from_secs_f64(*interval));
            }
        }
        Command::Daemon { config, interval } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .with_target(false)
                .init();
            let path = PathBuf::from(config);
            let provider_config = load_config(&path)?;
            let daemon = std::sync::Arc::new(ProviderDaemon::new(
                agent,
                provider_config,
                cli.mock_gpu,
                *interval,
                Some(path),
            )?);
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|exc| AgentError::new(format!("{exc}")))?;
            let interrupted = runtime.block_on(daemon.run())?;
            return Ok(if interrupted { 130 } else { 0 });
        }
        // 上で先に返す。
        Command::Devices | Command::Templates => {}
    }
    Ok(0)
}

fn print_pretty(value: &Value) -> Result<(), AgentError> {
    let text =
        serde_json::to_string_pretty(value).map_err(|exc| AgentError::new(format!("{exc}")))?;
    let mut stdout = std::io::stdout();
    stdout
        .write_all(text.as_bytes())
        .map_err(|exc| AgentError::new(format!("{exc}")))?;
    stdout
        .write_all(b"\n")
        .map_err(|exc| AgentError::new(format!("{exc}")))?;
    let _ = stdout.flush();
    Ok(())
}

fn print_line(value: &Value) -> Result<(), AgentError> {
    let text = serde_json::to_string(value).map_err(|exc| AgentError::new(format!("{exc}")))?;
    let mut stdout = std::io::stdout();
    stdout
        .write_all(text.as_bytes())
        .map_err(|exc| AgentError::new(format!("{exc}")))?;
    stdout
        .write_all(b"\n")
        .map_err(|exc| AgentError::new(format!("{exc}")))?;
    let _ = stdout.flush();
    Ok(())
}
