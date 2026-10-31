//! Jimoto GPU Pod 共通型・検証ロジック。
//!
//! Python 版 `provider_agent.models` / `provider_agent.resources` /
//! `portal.services` のうち、中央と提供PCの両方から使われる検証・計算を移植したもの。

pub mod error;
pub mod gpu;
pub mod identifier;
pub mod protocol;
pub mod resources;
pub mod template;
pub mod token;

pub use error::AgentError;
pub use gpu::GPU;
pub use identifier::{endpoint_host, identifier};
pub use protocol::{
    validate_snapshot, NodeCommand, NodeSnapshot, RelayRoute, ReleaseAssignment, SyncRequest,
    SyncResponse,
};
pub use resources::{
    claim, gib_bytes, size_bytes, summary, used_resources, Allocation, Pool, FIELDS, GIB, MAXIMUM,
    MIB,
};
pub use template::{load_templates, Template};
pub use token::{digest_equals, token_digest};
