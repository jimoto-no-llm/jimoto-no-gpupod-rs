//! gpupod-tools 内部で共有する最小限の基盤。
//!
//! Python 版 `provider_agent` のうち補助ツールが使う部分を最小限で移植したもの。
//! gpupod-agent crate の lib 実装が揃い次第、こちらの重複は削減する（README 参照）。

pub mod agentlite;
pub mod benchmarks;
pub mod config;
pub mod devices;
pub mod docker;
pub mod files;
pub mod repo;
pub mod sshkey;
pub mod state;
pub mod telemetry;
pub mod tuf;
pub mod url;
