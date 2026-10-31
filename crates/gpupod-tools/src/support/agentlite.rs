//! 補助ツール向けの最小 Agent 実装（Python 版 agent.py の診断・自動貸出部分）。
//!
//! gpupod-agent crate の lib 実装が揃い次第、この重複は削除する（README 参照）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use chrono::Utc;
use gpupod_common::{resources, AgentError, Template, GPU};
use serde_json::json;

use super::docker::DockerEngine;
use super::state::{State, StateStore};
use super::telemetry::external_gpu_activity;

#[derive(Debug)]
pub struct Status {
    pub node_id: String,
    pub manual_enabled: bool,
    pub auto_busy: bool,
    pub accepting: bool,
    pub lease_ids: Vec<String>,
    pub active_containers: Vec<(String, String)>,
}

#[derive(Debug)]
pub struct Agent {
    pub store: StateStore,
    pub engine: DockerEngine,
    pub templates: BTreeMap<String, Template>,
    pub gpus: BTreeMap<String, GPU>,
}

impl Agent {
    pub fn new(
        state_dir: &Path,
        engine: DockerEngine,
        templates: BTreeMap<String, Template>,
        gpus: Vec<GPU>,
    ) -> Result<Agent> {
        Ok(Agent {
            store: StateStore::new(state_dir)?,
            engine,
            templates,
            gpus: gpus.into_iter().map(|gpu| (gpu.id.clone(), gpu)).collect(),
        })
    }

    pub fn status(&self) -> Result<Status> {
        let state = self.store.with_locked(|state| Ok(state.clone()))?;
        Ok(status_of(&state))
    }

    /// Python 版 Agent.set_auto_busy。stop モードでは稼働中レースを解消する。
    pub fn set_auto_busy(&mut self, busy: bool, mode: &str) -> Result<Status> {
        if !matches!(mode, "drain" | "stop") {
            bail!(AgentError::new("Invalid automatic detection signal"));
        }
        let stop = busy && mode == "stop";
        let leases = self.store.with_locked(|state| {
            state.auto_busy = busy;
            Ok(if stop { active_ids(state) } else { Vec::new() })
        })?;
        for lease_id in leases {
            self.cleanup(&lease_id)?;
        }
        self.status()
    }

    /// 診断レースを作成する。手動貸出が ON の間は実行しない。
    pub fn start_diagnostic(
        &mut self,
        template_id: &str,
        gpu_ids: &[String],
        ssh_public_key: &str,
    ) -> Result<serde_json::Value> {
        let Some(template) = self.templates.get(template_id).cloned() else {
            bail!(AgentError::new(
                "Only registered template IDs can be started"
            ));
        };
        super::devices::validate_gpu_group(
            &self.gpus.values().cloned().collect::<Vec<_>>(),
            &template.vendors,
            gpu_ids,
        )?;
        let gpus: Vec<GPU> = gpu_ids
            .iter()
            .map(|id| {
                self.gpus
                    .get(id)
                    .cloned()
                    .ok_or_else(|| AgentError::new("GPU is not registered on this node"))
            })
            .collect::<Result<_, _>>()?;
        let lease_id = uuid::Uuid::new_v4().simple().to_string();
        let (node_id, lease) = self
            .store
            .with_locked(|state| {
                if state.manual_enabled {
                    bail!(AgentError::new(
                        "Turn lending OFF before running diagnostics"
                    ));
                }
                let claimed = claimed_gpus(state);
                if gpu_ids.iter().any(|id| claimed.contains(id)) {
                    bail!(AgentError::new(
                        "GPU is already allocated or awaiting cleanup"
                    ));
                }
                let pool = state
                    .resource_pool
                    .clone()
                    .filter(|value| !value.is_null())
                    .map(resources::Pool::from_value)
                    .transpose()?;
                let allocations: Vec<Option<resources::Allocation>> = state
                    .leases
                    .values()
                    .filter(|lease| lease.get("status").and_then(|v| v.as_str()) != Some("stopped"))
                    .map(|lease| match lease.get("resources") {
                        None | Some(serde_json::Value::Null) => None,
                        Some(value) => serde_json::from_value(value.clone()).ok(),
                    })
                    .collect();
                let reserved = resources::claim(pool.as_ref(), &allocations, None, None)?;
                let lease = json!({
                    "id": lease_id,
                    "gpu_id": gpu_ids[0],
                    "gpu_ids": gpu_ids,
                    "template_id": template_id,
                    "image": template.image,
                    "container": format!("gpulend-{lease_id}"),
                    "volume": format!("gpulend-work-{lease_id}"),
                    "network": format!("gpulend-net-{lease_id}"),
                    "status": "starting",
                    "created_at": Utc::now().to_rfc3339(),
                    "ssh_public_key": ssh_public_key,
                    "diagnostic": true,
                });
                let mut lease = lease;
                if let Some(reserved) = reserved {
                    lease["resources"] = serde_json::json!({
                        "memory_bytes": reserved.memory_bytes,
                        "disk_bytes": reserved.disk_bytes,
                    });
                }
                state.leases.insert(lease_id.clone(), lease.clone());
                Ok((state.node_id.clone(), lease))
            })
            .map_err(|exc| anyhow::anyhow!(exc))?;
        let result = self.launch(&node_id, &lease, &template, &gpus);
        match result {
            Ok(value) => Ok(value),
            Err(exc) => {
                let message = format!("{exc:#}");
                self.mark_failed(&lease_id, &message)?;
                let _ = self.cleanup(&lease_id);
                Err(anyhow::anyhow!(
                    "Start failed for lease {lease_id}: {message}"
                ))
            }
        }
    }

    fn launch(
        &mut self,
        node_id: &str,
        lease: &serde_json::Value,
        template: &Template,
        gpus: &[GPU],
    ) -> Result<serde_json::Value> {
        let container = lease["container"].as_str().unwrap_or_default().to_string();
        let volume = lease["volume"].as_str().unwrap_or_default().to_string();
        let network = lease["network"].as_str().unwrap_or_default().to_string();
        let lease_id = lease["id"].as_str().unwrap_or_default().to_string();
        let key = lease["ssh_public_key"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let memory = lease
            .get("resources")
            .and_then(|value| value.get("memory_bytes"))
            .and_then(|value| value.as_i64());
        self.engine.create_diagnostic(
            &lease_id, node_id, &container, &volume, &network, template, gpus, &key, memory,
        )?;
        self.engine.start(&container)?;
        let value = self.engine.describe(&container, &lease_id, node_id)?;
        let running = value
            .as_ref()
            .and_then(|value| value.get("State"))
            .and_then(|state| state.get("Running"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if !running {
            bail!(AgentError::new("Container exited before becoming ready"));
        }
        self.store.with_locked(|state| {
            if let Some(current) = state.leases.get_mut(&lease_id) {
                current["status"] = json!("running");
                current["started_at"] = json!(Utc::now().to_rfc3339());
            }
            Ok(())
        })?;
        Ok(lease.clone())
    }

    fn mark_failed(&mut self, lease_id: &str, message: &str) -> Result<()> {
        self.store.with_locked(|state| {
            if let Some(lease) = state.leases.get_mut(lease_id) {
                lease["error"] = json!(message);
                lease["status"] = json!("cleanup_pending");
            }
            Ok(())
        })
    }

    /// レースの解消（Python 版 _cleanup の移植）。
    pub fn cleanup(&mut self, lease_id: &str) -> Result<String> {
        let (node_id, lease) = self.store.with_locked(|state| {
            let Some(lease) = state.leases.get(lease_id).cloned() else {
                bail!(AgentError::new("Unknown lease ID"));
            };
            if lease.get("status").and_then(|v| v.as_str()) == Some("stopped") {
                return Ok((state.node_id.clone(), lease));
            }
            if let Some(current) = state.leases.get_mut(lease_id) {
                current["status"] = json!("cleanup_pending");
            }
            Ok((state.node_id.clone(), lease))
        })?;
        if lease.get("status").and_then(|v| v.as_str()) == Some("stopped") {
            return Ok("stopped".to_string());
        }
        let container = lease["container"].as_str().unwrap_or_default();
        let volume = lease["volume"].as_str().unwrap_or_default();
        let network = lease["network"].as_str().unwrap_or_default();
        match self
            .engine
            .cleanup(container, volume, network, lease_id, &node_id)
        {
            Ok(()) => {}
            Err(exc) => {
                self.store.with_locked(|state| {
                    if let Some(current) = state.leases.get_mut(lease_id) {
                        current["error"] = json!(format!("{exc:#}"));
                    }
                    Ok(())
                })?;
                return Err(exc);
            }
        }
        self.store.with_locked(|state| {
            if let Some(current) = state.leases.get_mut(lease_id) {
                current["status"] = json!("stopped");
                current["stopped_at"] = json!(Utc::now().to_rfc3339());
            }
            Ok(())
        })?;
        Ok("stopped".to_string())
    }

    /// 診断 GPU 群の候補（自動貸出用）。
    pub fn gpu_list(&self) -> Vec<GPU> {
        self.gpus.values().cloned().collect()
    }

    /// 実行中レースのコンテナ名とレース ID。
    pub fn running_containers(&self) -> Result<Vec<(String, String)>> {
        let state = self.store.with_locked(|state| Ok(state.clone()))?;
        Ok(state
            .leases
            .values()
            .filter(|lease| lease.get("status").and_then(|v| v.as_str()) == Some("running"))
            .filter_map(|lease| {
                Some((
                    lease.get("container")?.as_str()?.to_string(),
                    lease.get("id")?.as_str()?.to_string(),
                ))
            })
            .collect())
    }

    /// 借手以外の GPU 利用があるか。
    pub fn external_activity(&self) -> super::telemetry::Activity {
        match self.running_containers() {
            Ok(containers) => {
                let ids: Vec<String> = containers.iter().map(|(_, id)| id.clone()).collect();
                external_gpu_activity(&self.gpu_list(), &ids, Path::new("/proc"))
            }
            Err(_) => None,
        }
    }

    pub fn state_dir(&self) -> &Path {
        &self.store.directory
    }
}

pub fn status_of(state: &State) -> Status {
    let lease_ids: Vec<String> = state.leases.keys().cloned().collect();
    let active_containers = state
        .leases
        .values()
        .filter(|lease| lease.get("status").and_then(|v| v.as_str()) != Some("stopped"))
        .filter_map(|lease| {
            Some((
                lease.get("container")?.as_str()?.to_string(),
                lease.get("id")?.as_str()?.to_string(),
            ))
        })
        .collect();
    Status {
        node_id: state.node_id.clone(),
        manual_enabled: state.manual_enabled,
        auto_busy: state.auto_busy,
        accepting: state.manual_enabled && !state.auto_busy,
        lease_ids,
        active_containers,
    }
}

fn active_ids(state: &State) -> Vec<String> {
    state
        .leases
        .iter()
        .filter(|(_, lease)| lease.get("status").and_then(|v| v.as_str()) != Some("stopped"))
        .map(|(id, _)| id.clone())
        .collect()
}

fn claimed_gpus(state: &State) -> Vec<String> {
    let mut claimed = Vec::new();
    for lease in state.leases.values() {
        if lease.get("status").and_then(|v| v.as_str()) == Some("stopped") {
            continue;
        }
        match lease.get("gpu_ids").and_then(|v| v.as_array()) {
            Some(items) => {
                claimed.extend(items.iter().filter_map(|v| v.as_str().map(str::to_string)))
            }
            None => claimed.extend(
                lease
                    .get("gpu_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            ),
        }
    }
    claimed
}

/// 状態ディレクトリの既定値。
pub fn default_state_dir() -> PathBuf {
    PathBuf::from(".provider-state")
}
