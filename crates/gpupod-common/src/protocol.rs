//! 中央 <-> 提供PC の同期 API で交換する型（portal.services / provider_agent.daemon の移植）。
//!
//! すべて JSON でやり取りする。フィールド名は Python 版と同一に保つ。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{AgentError, Result};
use crate::gpu::GPU;
use crate::identifier::{endpoint_host, identifier};
use crate::resources::{self, Allocation, Pool};

/// 提供PCが中央へ送るスナップショット（Agent.status() の結果 + テンプレート指紋）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeSnapshot {
    pub node_id: String,
    #[serde(default)]
    pub manual_enabled: bool,
    #[serde(default)]
    pub auto_busy: bool,
    #[serde(default)]
    pub accepting: bool,
    #[serde(default)]
    pub gpus: Vec<serde_json::Value>,
    #[serde(default)]
    pub leases: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub templates: BTreeMap<String, String>,
    #[serde(default)]
    pub resource_pool: Option<Pool>,
    #[serde(default)]
    pub direct_host: String,
    #[serde(default)]
    pub mock_gpu: bool,
}

/// 中央が提供PCへ送るコマンド。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeCommand {
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// 中継経路（中央が用意したリスンポート）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayRoute {
    pub lease_id: String,
    pub container_port: String,
    pub relay_port: u16,
}

/// 同期レスポンス。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResponse {
    #[serde(default)]
    pub commands: Vec<NodeCommand>,
    #[serde(default)]
    pub routes: Vec<RelayRoute>,
    #[serde(default)]
    pub acknowledged: Vec<String>,
    pub relay_bind_host: String,
    #[serde(default)]
    pub release: Option<ReleaseAssignment>,
}

/// 更新の割当（TUF リポジトリ内の成果物）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseAssignment {
    pub sequence: u32,
    pub version: String,
    pub target: String,
}

/// 同期リクエスト。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRequest {
    pub state: NodeSnapshot,
    #[serde(default)]
    pub statistics: serde_json::Value,
    #[serde(default)]
    pub results: Vec<serde_json::Value>,
}

/// Python 版 validate_snapshot と同じ検証。
pub fn validate_snapshot(value: &NodeSnapshot) -> Result<()> {
    identifier(&value.node_id)?;
    if value.gpus.len() > 128 {
        return Err(AgentError::new("Invalid inventory"));
    }
    if value.leases.len() > 4096 {
        return Err(AgentError::new("Invalid inventory"));
    }
    for fingerprint in value.templates.values() {
        if fingerprint.len() != 64 {
            return Err(AgentError::new("Invalid template fingerprints"));
        }
    }
    let mut seen = std::collections::HashSet::new();
    for gpu in &value.gpus {
        let id = gpu
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::new("Invalid or duplicate GPU"))?;
        identifier(id)?;
        if !seen.insert(id.to_owned()) {
            return Err(AgentError::new("Invalid or duplicate GPU"));
        }
        if gpu.get("vendor").and_then(|v| v.as_str()).is_none() {
            return Err(AgentError::new("Invalid or duplicate GPU"));
        }
    }
    for (key, lease) in &value.leases {
        identifier(key)?;
        let status = lease.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if !matches!(
            status,
            "starting" | "running" | "cleanup_pending" | "stopped"
        ) {
            return Err(AgentError::new("Invalid lease state"));
        }
        let ports = lease.get("ports");
        if let Some(ports) = ports {
            let ports = ports
                .as_object()
                .ok_or_else(|| AgentError::new("Invalid port bindings"))?;
            for bindings in ports.values() {
                if bindings.is_null() {
                    continue;
                }
                let bindings = bindings
                    .as_array()
                    .ok_or_else(|| AgentError::new("Invalid port bindings"))?;
                for binding in bindings {
                    let number = binding
                        .get("HostPort")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !number.bytes().all(|b| b.is_ascii_digit())
                        || !(1..=65535).contains(&number.parse::<u32>().unwrap_or(0))
                    {
                        return Err(AgentError::new("Invalid endpoint port"));
                    }
                }
            }
        }
    }
    if !value.direct_host.is_empty() {
        endpoint_host(&value.direct_host)?;
    }
    if let Some(pool) = &value.resource_pool {
        pool.validate()?;
        if !value.manual_enabled && !value.auto_busy {
            // 両方 false でも構造上は許容（Python 版は型のみ検査）。
            let _ = pool;
        }
        let leases: Vec<Option<Allocation>> = value
            .leases
            .values()
            .filter(|lease| lease.get("status").and_then(|v| v.as_str()) != Some("stopped"))
            .map(|lease| {
                serde_json::from_value::<Allocation>(
                    lease.get("resources").cloned().unwrap_or_default(),
                )
                .ok()
            })
            .collect();
        resources::summary(
            Some(pool),
            &leases,
            value.manual_enabled && !value.auto_busy,
        )?;
    }
    let _ = GPU::from_value;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn minimal() -> NodeSnapshot {
        serde_json::from_value(json!({
            "node_id": "abc123",
            "accepting": false,
            "gpus": [],
            "leases": {},
            "templates": {}
        }))
        .unwrap()
    }

    #[test]
    fn accepts_minimal_snapshot() {
        assert!(validate_snapshot(&minimal()).is_ok());
    }

    #[test]
    fn rejects_bad_lease_status_and_ports() {
        let mut snapshot = minimal();
        snapshot
            .leases
            .insert("lease1".into(), json!({"status": "unknown"}));
        assert!(validate_snapshot(&snapshot).is_err());

        let mut snapshot = minimal();
        snapshot.leases.insert(
            "lease1".into(),
            json!({"status": "running", "ports": {"22/tcp": [{"HostPort": "99999"}]}}),
        );
        assert!(validate_snapshot(&snapshot).is_err());

        let mut snapshot = minimal();
        snapshot.leases.insert(
            "lease1".into(),
            json!({"status": "running", "ports": {"22/tcp": [{"HostPort": "32768"}]}}),
        );
        assert!(validate_snapshot(&snapshot).is_ok());
    }

    #[test]
    fn rejects_duplicate_gpus_and_bad_fingerprints() {
        let mut snapshot = minimal();
        snapshot.gpus = vec![
            json!({"id": "g1", "vendor": "nvidia"}),
            json!({"id": "g1", "vendor": "amd"}),
        ];
        assert!(validate_snapshot(&snapshot).is_err());

        let mut snapshot = minimal();
        snapshot.templates.insert("t1".into(), "short".into());
        assert!(validate_snapshot(&snapshot).is_err());
    }
}
