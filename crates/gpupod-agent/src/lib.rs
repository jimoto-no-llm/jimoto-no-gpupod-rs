//! Jimoto GPU Pod 提供 PC 側エージェント。
//!
//! Python 版 provider_agent を Rust (tokio) に移植したもの。
//! 状態は SQLite 単一行 JSON（v1）、Docker は bollard、
//! 中央同期は reqwest、中継は russh の remote port forwarding。

pub mod agent;
pub mod benchmarks;
pub mod credentials;
pub mod daemon;
pub mod devices;
pub mod docker;
pub mod files;
pub mod health;
pub mod mock;
pub mod state;
pub mod telemetry;

pub use agent::{gpu_to_value, now, validate_ssh_key, Agent, Lease, StartRequest};
pub use benchmarks::{public_results, read_results, save_result};
pub use credentials::rotate_credentials;
pub use daemon::{load_config, Forwarder, ProviderConfig, ProviderDaemon};
pub use devices::{discover_gpus, load_gpus};
pub use docker::{
    BollardEngine, DockerEngine, DockerError, EngineOptions, LEASE_LABEL, NODE_LABEL,
};
pub use files::{atomic_bytes, atomic_json};
pub use health::{notify_systemd, release_identity, HealthMonitor};
pub use mock::mock_gpus;
pub use state::{AgentState, OperationLock, StateGuard, StateStore};
pub use telemetry::{
    collect, disk_free, disk_usage, external_gpu_activity, process_is_borrower, IdlePolicy,
};
