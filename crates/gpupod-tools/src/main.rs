//! gpupod-tools: Jimoto GPU Pod の補助ツール群（単一バイナリ）。

mod auto_lend;
mod benchmark;
mod build_images;
mod collective_probe;
mod demo;
mod doctor;
mod gpu_diagnostics;
mod prepare_mirrors;
mod prepare_runtime;
mod prepare_template;
mod release_repository;
mod rotate_token;
mod support;
mod support_bundle;
mod systemd_units;
mod update_agent;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "gpupod-tools",
    about = "Jimoto GPU Pod の補助ツール（診断・単位生成・準備・測定・更新）",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 読み取り専用の提供 PC 診断。
    Doctor {
        #[arg(long)]
        devices: Option<PathBuf>,
        #[arg(long, default_value = "/var/run/docker.sock")]
        docker_socket: PathBuf,
        #[arg(long, default_value = ".")]
        disk_path: PathBuf,
        #[arg(long)]
        mock_gpu: bool,
    },
    /// systemd ユニット文面の生成（インストールは操作者の行う）。
    SystemdUnits {
        role: Role,
        #[arg(long)]
        user: String,
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long, default_value = ".provider-state/node.secret.json")]
        config: PathBuf,
        #[arg(long)]
        output_dir: PathBuf,
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// GPU 活動の検出と自動貸出制御。
    AutoLend {
        #[arg(long, default_value = ".provider-state")]
        state_dir: PathBuf,
        #[arg(long, default_value = "examples/templates.json")]
        templates: PathBuf,
        #[arg(long)]
        devices: Option<PathBuf>,
        #[arg(long, default_value = "/var/run/docker.sock")]
        docker_socket: PathBuf,
        #[arg(long, default_value_t = 60.0)]
        idle_seconds: f64,
        #[arg(long, default_value_t = 5.0)]
        interval: f64,
        #[arg(long, value_enum, default_value_t = LendMode::Drain)]
        mode: LendMode,
        #[arg(long)]
        signal_file: Option<PathBuf>,
        #[arg(long)]
        once: bool,
    },
    /// ノード機材キーのローテーション。
    RotateToken {
        #[arg(long)]
        config: PathBuf,
    },
    /// Docker Hub ミラー候補設定の生成。
    PrepareMirrors {
        #[arg(long = "mirror", required = true, num_args = 1..)]
        mirrors: Vec<String>,
        #[arg(long)]
        docker_config: Option<PathBuf>,
        #[arg(long)]
        output: PathBuf,
    },
    /// 承認済みテンプレート画像の取得。
    PrepareTemplate {
        template_id: String,
        #[arg(long)]
        templates: Option<PathBuf>,
        #[arg(long, default_value = "/var/run/docker.sock")]
        docker_socket: PathBuf,
    },
    /// gVisor バンドル取得と daemon.json 生成。
    PrepareRuntime {
        #[arg(long)]
        output_dir: PathBuf,
        #[arg(long)]
        docker_config: Option<PathBuf>,
        #[arg(long)]
        gpu: bool,
    },
    /// 伏字済みサポートアーカイブの作成（アップロードはしない）。
    SupportBundle {
        #[arg(long, default_value = ".provider-state")]
        state_dir: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        log: Option<PathBuf>,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long, default_value = "/var/run/docker.sock")]
        docker_socket: PathBuf,
        #[arg(long)]
        mock_gpu: bool,
    },
    /// 記録装置と中央間の測定。
    Benchmark {
        #[arg(long, default_value = ".provider-state")]
        state_dir: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        disk_path: Option<PathBuf>,
        #[arg(long, default_value_t = 256)]
        disk_size_mib: i64,
        #[arg(long, default_value = "fio")]
        fio: String,
        #[arg(long)]
        network: bool,
        #[arg(long, default_value_t = 16)]
        network_size_mib: i64,
    },
    /// ローカル画像のビルド。
    BuildImages {
        #[arg(long, value_enum, num_args = 0..)]
        pytorch: Vec<TorchVariant>,
    },
    /// 中央と提供 PC を同居させたデモ起動。
    Demo {
        #[arg(long)]
        pytorch: bool,
        #[arg(long, default_value_t = 8000)]
        port: u16,
        #[arg(long, default_value_t = 19000)]
        relay_port: u16,
        #[arg(long, default_value = ".demo-state")]
        state_dir: PathBuf,
    },
    /// オフライン署名リポジトリの管理。
    ReleaseRepository {
        #[arg(long)]
        repository: PathBuf,
        #[arg(long)]
        keys_dir: PathBuf,
        #[command(subcommand)]
        action: ReleaseAction,
    },
    /// 署名検証つきの更新適用。
    UpdateAgent {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        request: Option<PathBuf>,
    },
    /// 診断 GPU 予約と all-reduce 検査。
    GpuDiagnostics {
        #[arg(long)]
        mock_gpu: bool,
        #[arg(long, required = true)]
        template: String,
        #[arg(long = "gpu", required = true, num_args = 1..)]
        gpus: Vec<String>,
        #[arg(long, default_value = ".provider-state")]
        state_dir: PathBuf,
        #[arg(long)]
        templates: Option<PathBuf>,
        #[arg(long)]
        devices: Option<PathBuf>,
        #[arg(long, default_value = "/var/run/docker.sock")]
        docker_socket: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// コンテナ内プローブ呼び出し仕様の表示。
    CollectiveProbe {
        #[arg(long)]
        backend: Backend,
        #[arg(long, default_value_t = 2)]
        workers: i64,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum Role {
    Control,
    Provider,
    Updater,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::Control => "control",
            Role::Provider => "provider",
            Role::Updater => "updater",
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum LendMode {
    Drain,
    Stop,
}

impl LendMode {
    fn as_str(self) -> &'static str {
        match self {
            LendMode::Drain => "drain",
            LendMode::Stop => "stop",
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum TorchVariant {
    Cpu,
    Cuda,
    Rocm,
    Xpu,
}

impl TorchVariant {
    fn as_str(self) -> &'static str {
        match self {
            TorchVariant::Cpu => "cpu",
            TorchVariant::Cuda => "cuda",
            TorchVariant::Rocm => "rocm",
            TorchVariant::Xpu => "xpu",
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum Backend {
    Nccl,
    Xccl,
    Gloo,
}

impl Backend {
    fn as_str(self) -> &'static str {
        match self {
            Backend::Nccl => "nccl",
            Backend::Xccl => "xccl",
            Backend::Gloo => "gloo",
        }
    }
}

fn main() {
    let cli = Cli::parse();
    if let Err(exc) = dispatch(cli.command) {
        eprintln!("{exc:#}");
        std::process::exit(1);
    }
}

fn dispatch(command: Command) -> Result<()> {
    match command {
        Command::Doctor {
            devices,
            docker_socket,
            disk_path,
            mock_gpu,
        } => {
            let options = doctor::Options {
                devices,
                docker_socket,
                disk_path,
                mock_gpu,
            };
            let (report, code) = doctor::run(&options)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            if code != 0 {
                std::process::exit(code);
            }
        }
        Command::SystemdUnits {
            role,
            user,
            state_dir,
            config,
            output_dir,
            project,
        } => {
            let project = match project {
                Some(path) => path,
                None => {
                    crate::support::repo::repo_root().context("Cannot find the repository root")?
                }
            };
            let units =
                systemd_units::generate(&project, &state_dir, &config, &user, role.as_str())?;
            for path in systemd_units::write_units(&units, &output_dir)? {
                println!("{}", path.display());
            }
        }
        Command::AutoLend {
            state_dir,
            templates,
            devices,
            docker_socket,
            idle_seconds,
            interval,
            mode,
            signal_file,
            once,
        } => {
            let options = auto_lend::Options {
                state_dir,
                templates,
                devices,
                docker_socket,
                idle_seconds,
                interval,
                mode: mode.as_str().to_string(),
                signal_file,
                once,
            };
            let code = auto_lend::run(&options)?;
            if code != 0 {
                std::process::exit(code);
            }
        }
        Command::RotateToken { config } => {
            println!(
                "{}",
                serde_json::to_string(&rotate_token::rotate_credentials(&config)?)?
            );
        }
        Command::PrepareMirrors {
            mirrors,
            docker_config,
            output,
        } => {
            let written = prepare_mirrors::prepare(&output, &mirrors, docker_config.as_deref())
                .context("Could not prepare mirror configuration")?;
            println!("{}", written.display());
        }
        Command::PrepareTemplate {
            template_id,
            templates,
            docker_socket,
        } => {
            let templates = templates.unwrap_or_else(|| prepare_template::default_catalog(None));
            match prepare_template::run(&template_id, &templates, &docker_socket) {
                Ok(image) => println!("{image}"),
                Err(exc) => {
                    eprintln!("{exc}");
                    std::process::exit(1);
                }
            }
        }
        Command::PrepareRuntime {
            output_dir,
            docker_config,
            gpu,
        } => {
            let destination = prepare_runtime::prepare(&output_dir, docker_config.as_deref(), gpu)?;
            println!("{}", destination.display());
        }
        Command::SupportBundle {
            state_dir,
            config,
            log,
            output,
            docker_socket,
            mock_gpu,
        } => {
            let output = output.unwrap_or_else(|| {
                PathBuf::from("support-bundles").join(format!(
                    "provider-{}.tar.gz",
                    chrono::Utc::now().format("%Y%m%dT%H%M%S")
                ))
            });
            let created = support_bundle::create_bundle(
                &state_dir,
                &output,
                config.as_deref(),
                log.as_deref(),
                &docker_socket,
                mock_gpu,
            )
            .context("Could not create support archive")?;
            println!("{}", created.display());
        }
        Command::Benchmark {
            state_dir,
            config,
            disk_path,
            disk_size_mib,
            fio,
            network,
            network_size_mib,
        } => {
            let options = benchmark::Options {
                state_dir,
                config,
                disk_path,
                disk_size_mib,
                fio,
                network,
                network_size_mib,
            };
            benchmark::main(&options).context("Measurement failed")?;
        }
        Command::BuildImages { pytorch } => {
            let root = crate::support::repo::repo_root()?;
            let variants: Vec<String> = pytorch.iter().map(|v| v.as_str().to_string()).collect();
            build_images::run(&variants, &root)?;
        }
        Command::Demo {
            pytorch,
            port,
            relay_port,
            state_dir,
        } => {
            let options = demo::Options {
                pytorch,
                state_dir,
                port,
                relay_port,
            };
            demo::run(&options)?;
        }
        Command::ReleaseRepository {
            repository,
            keys_dir,
            action,
        } => match action {
            ReleaseAction::Init => {
                release_repository::initialize(&repository, &keys_dir)?;
            }
            ReleaseAction::Publish {
                source,
                version,
                sequence,
            } => {
                let source = match source {
                    Some(path) => path,
                    None => crate::support::repo::repo_root()?,
                };
                let result = release_repository::publish(
                    &repository,
                    &keys_dir,
                    &source,
                    &version,
                    sequence,
                )?;
                println!("{}", serde_json::to_string(&result)?);
            }
            ReleaseAction::Refresh => {
                release_repository::refresh(&repository, &keys_dir)?;
            }
        },
        Command::UpdateAgent { config, request } => {
            if let Some(result) = update_agent::run(&config, request.as_deref())? {
                println!("{}", serde_json::to_string(&result)?);
            }
        }
        Command::GpuDiagnostics {
            mock_gpu,
            template,
            gpus,
            state_dir,
            templates,
            devices,
            docker_socket,
            config,
        } => {
            let options = gpu_diagnostics::Options {
                state_dir,
                templates: templates.unwrap_or_else(|| prepare_template::default_catalog(None)),
                devices,
                docker_socket,
                config,
                mock_gpu,
                gpus,
                template,
            };
            let report = gpu_diagnostics::run(&options)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::CollectiveProbe { backend, workers } => {
            let report = collective_probe::run(backend.as_str(), workers)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(())
}

#[derive(Debug, Subcommand)]
enum ReleaseAction {
    /// 信頼ルートと署名鍵を作る。
    Init,
    /// リリースをパッケージして公開する。
    Publish {
        #[arg(long)]
        source: Option<PathBuf>,
        #[arg(long)]
        version: String,
        #[arg(long)]
        sequence: i64,
    },
    /// 有効期限だけ更新する。
    Refresh,
}
