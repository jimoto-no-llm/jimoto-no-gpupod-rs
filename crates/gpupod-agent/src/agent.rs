//! 貸出の受付・起動・返却（Python 版 agent.py の移植）。
//!
//! 状態遷移は starting → running、cleanup_pending → stopped。
//! Docker 操作が不確実な失敗を返した場合は予約を保持したまま再調和に委ねる。

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::Utc;
use gpupod_common::resources::{self, Allocation, Pool};
use gpupod_common::{AgentError, Template, GPU};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::docker::{DockerEngine, DockerError};
use crate::state::{AgentState, StateStore};

pub type Result<T> = std::result::Result<T, AgentError>;

/// 現在時刻（UTC、RFC3339）。Python 版 datetime.now(timezone.utc).isoformat()。
pub fn now() -> String {
    Utc::now().to_rfc3339()
}

/// agent_state 内の 1 件の貸出。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Lease {
    pub id: String,
    pub gpu_id: String,
    pub template_id: String,
    pub image: String,
    pub container: String,
    pub volume: String,
    pub status: String,
    pub created_at: String,
    #[serde(default)]
    pub ssh_public_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpu_ids: Vec<String>,
    #[serde(default)]
    pub diagnostic: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Allocation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<String>,
}

impl Lease {
    /// gpu_ids が無い古い貸出は主 GPU を返す（Python 版 lease.get の既定値）。
    pub fn gpu_ids_or_primary(&self) -> Vec<String> {
        if self.gpu_ids.is_empty() {
            vec![self.gpu_id.clone()]
        } else {
            self.gpu_ids.clone()
        }
    }

    /// 中継経路の判定に使う接続モード（既定 relay）。
    pub fn mode(&self) -> &str {
        self.connection_mode.as_deref().unwrap_or("relay")
    }

    pub fn is_active(&self) -> bool {
        self.status != "stopped"
    }
}

/// start への引数（Python 版 Agent.start のキーワード引数）。
#[derive(Debug, Clone)]
pub struct StartRequest<'a> {
    pub template_id: &'a str,
    pub gpu_id: &'a str,
    pub lease_id: Option<&'a str>,
    pub ssh_public_key: &'a str,
    pub connection_mode: &'a str,
    pub gpu_ids: Option<Vec<String>>,
    pub diagnostic: bool,
    pub memory_bytes: Option<u64>,
    pub disk_bytes: Option<u64>,
}

impl<'a> StartRequest<'a> {
    pub fn new(template_id: &'a str, gpu_id: &'a str) -> Self {
        Self {
            template_id,
            gpu_id,
            lease_id: None,
            ssh_public_key: "",
            connection_mode: "relay",
            gpu_ids: None,
            diagnostic: false,
            memory_bytes: None,
            disk_bytes: None,
        }
    }

    pub fn lease_id(mut self, value: &'a str) -> Self {
        self.lease_id = Some(value);
        self
    }

    pub fn ssh_public_key(mut self, value: &'a str) -> Self {
        self.ssh_public_key = value;
        self
    }

    pub fn connection_mode(mut self, value: &'a str) -> Self {
        self.connection_mode = value;
        self
    }

    pub fn gpu_ids(mut self, value: Option<Vec<String>>) -> Self {
        self.gpu_ids = value;
        self
    }

    pub fn diagnostic(mut self, value: bool) -> Self {
        self.diagnostic = value;
        self
    }

    pub fn memory_bytes(mut self, value: Option<u64>) -> Self {
        self.memory_bytes = value;
        self
    }

    pub fn disk_bytes(mut self, value: Option<u64>) -> Self {
        self.disk_bytes = value;
        self
    }
}

/// エンジン起因の失敗。uncertain は予約保持の判断に使う。
enum StartFailure {
    Agent(AgentError),
    Docker(DockerError),
}

impl StartFailure {
    fn message(&self) -> String {
        match self {
            StartFailure::Agent(exc) => exc.0.clone(),
            StartFailure::Docker(exc) => exc.to_string(),
        }
    }

    fn uncertain(&self) -> bool {
        match self {
            StartFailure::Agent(_) => false,
            StartFailure::Docker(exc) => exc.uncertain,
        }
    }
}

impl From<AgentError> for StartFailure {
    fn from(exc: AgentError) -> Self {
        StartFailure::Agent(exc)
    }
}

impl From<DockerError> for StartFailure {
    fn from(exc: DockerError) -> Self {
        StartFailure::Docker(exc)
    }
}

/// 永続的な予約台帳。Docker を呼ぶ間はグローバルロックを保持しない。
pub struct Agent {
    pub store: Arc<StateStore>,
    pub engine: Arc<dyn DockerEngine>,
    pub templates: BTreeMap<String, Template>,
    /// 発見順を保つ（Python 版の dict 挿入順と一致させる）。
    pub gpus: Vec<GPU>,
}

impl Agent {
    pub fn new(
        store: Arc<StateStore>,
        engine: Arc<dyn DockerEngine>,
        templates: BTreeMap<String, Template>,
        gpus: Vec<GPU>,
    ) -> Self {
        Self {
            store,
            engine,
            templates,
            gpus,
        }
    }

    /// ID で GPU を引く。
    pub fn gpu(&self, id: &str) -> Option<&GPU> {
        self.gpus.iter().find(|gpu| gpu.id == id)
    }

    fn active(state: &AgentState) -> Vec<&Lease> {
        state
            .leases
            .values()
            .filter(|lease| lease.is_active())
            .collect()
    }

    /// 予約計算用の配列（停止分は None 扱いにせず除外）。
    fn allocations(state: &AgentState) -> Vec<Option<Allocation>> {
        state
            .leases
            .values()
            .filter(|lease| lease.is_active())
            .map(|lease| lease.resources)
            .collect()
    }

    fn active_gpu_ids(state: &AgentState) -> Vec<String> {
        let mut claimed = Vec::new();
        for lease in Self::active(state) {
            for gpu_id in lease.gpu_ids_or_primary() {
                if !claimed.contains(&gpu_id) {
                    claimed.push(gpu_id);
                }
            }
        }
        claimed
    }

    /// ノードの現在地図（Python 版 Agent.status）。
    pub fn status(&self) -> Result<Value> {
        let (snapshot, accepting, summary, claimed) = {
            let guard = self.store.locked()?;
            let accepting = guard.manual_enabled && !guard.auto_busy;
            let summary = resources::summary(
                guard.resource_pool.as_ref(),
                &Self::allocations(&guard),
                accepting,
            )?;
            let claimed = Self::active_gpu_ids(&guard);
            let snapshot = serde_json::to_value(&*guard).map_err(|exc| {
                AgentError::new(format!(
                    "Cannot read or save state; existing reservations retained: {exc}"
                ))
            })?;
            (snapshot, accepting, summary, claimed)
        };

        let mut result = snapshot;
        let object = result.as_object_mut().expect("state is an object");
        object.insert("accepting".into(), json!(accepting));
        let gpus: Vec<Value> = self
            .gpus
            .iter()
            .map(|gpu| {
                let mut value = gpu_to_value(gpu);
                value
                    .as_object_mut()
                    .expect("gpu object")
                    .insert("allocated".into(), json!(claimed.contains(&gpu.id)));
                value
            })
            .collect();
        object.insert("gpus".into(), json!(gpus));
        object.insert(
            "resources".into(),
            serde_json::to_value(summary).map_err(|exc| {
                AgentError::new(format!(
                    "Cannot read or save state; existing reservations retained: {exc}"
                ))
            })?,
        );
        Ok(result)
    }

    /// 貸出枠の設定（Python 版 configure_resources）。
    pub fn configure_resources(
        &self,
        memory_bytes: u64,
        disk_bytes: u64,
        default_memory_bytes: Option<u64>,
        default_disk_bytes: Option<u64>,
    ) -> Result<Value> {
        let count = std::cmp::max(1, self.gpus.len()) as u64;
        resources::Allocation {
            memory_bytes,
            disk_bytes,
        }
        .validate()?;
        let default_memory = default_memory_bytes.unwrap_or_else(|| {
            std::cmp::max(
                resources::minimum("memory_bytes"),
                memory_bytes / count / resources::MIB * resources::MIB,
            )
        });
        let default_disk = default_disk_bytes.unwrap_or_else(|| {
            std::cmp::max(
                resources::minimum("disk_bytes"),
                disk_bytes / count / resources::MIB * resources::MIB,
            )
        });
        let pool = Pool {
            memory_bytes,
            disk_bytes,
            default_memory_bytes: default_memory,
            default_disk_bytes: default_disk,
        }
        .validate()?;
        {
            let mut guard = self.store.locked()?;
            let has_active = guard.leases.values().any(|lease| lease.is_active());
            if guard.manual_enabled || has_active {
                return Err(AgentError::new(
                    "Turn lending OFF and return all leases before changing the resource pool",
                ));
            }
            guard.resource_pool = Some(pool);
            guard.save()?;
        }
        let status = self.status()?;
        Ok(status.get("resources").cloned().unwrap_or(Value::Null))
    }

    /// 貸出受付の ON（Python 版 on）。
    pub fn on(&self) -> Result<Value> {
        {
            let mut guard = self.store.locked()?;
            guard.manual_enabled = true;
            guard.save()?;
        }
        self.status()
    }

    /// 貸出受付の OFF（Python 版 off / _control）。
    pub fn off(&self, mode: &str) -> Result<Value> {
        if mode != "drain" && mode != "stop" {
            return Err(AgentError::new("OFF mode must be drain or stop"));
        }
        self.control("manual_enabled", false, mode == "stop")
    }

    /// 自動混雑検知の信号（Python 版 set_auto_busy）。
    pub fn set_auto_busy(&self, busy: bool, mode: &str) -> Result<Value> {
        if mode != "drain" && mode != "stop" {
            return Err(AgentError::new("Invalid automatic detection signal"));
        }
        self.control("auto_busy", busy, busy && mode == "stop")
    }

    fn control(&self, field: &str, value: bool, stop: bool) -> Result<Value> {
        let leases: Vec<String> = {
            let mut guard = self.store.locked()?;
            match field {
                "manual_enabled" => guard.manual_enabled = value,
                _ => guard.auto_busy = value,
            }
            let mut leases = Vec::new();
            if stop {
                for lease in Self::active(&guard) {
                    leases.push(lease.id.clone());
                }
                for lease_id in &leases {
                    if let Some(lease) = guard.leases.get_mut(lease_id) {
                        lease.status = "cleanup_pending".into();
                    }
                }
            }
            guard.save()?;
            leases
        };
        for lease_id in leases {
            let lock = self.store.operation(&lease_id)?;
            if lock.acquired() {
                self.cleanup(&lease_id)?;
            }
        }
        self.status()
    }

    /// 返却処理（Python 版 _cleanup）。呼び出し側が操作ロックを持つ。
    pub fn cleanup(&self, lease_id: &str) -> Result<()> {
        let (lease, node_id) = {
            let mut guard = self.store.locked()?;
            let lease = guard
                .leases
                .get(lease_id)
                .cloned()
                .ok_or_else(|| AgentError::new("Unknown lease ID"))?;
            if lease.status == "stopped" {
                return Ok(());
            }
            if let Some(target) = guard.leases.get_mut(lease_id) {
                target.status = "cleanup_pending".into();
            }
            guard.save()?;
            (lease.clone(), guard.node_id.clone())
        };
        if let Err(exc) = self.engine.cleanup(&lease, &node_id) {
            let mut guard = self.store.locked()?;
            if let Some(target) = guard.leases.get_mut(lease_id) {
                target.error = Some(exc.to_string());
            }
            guard.save()?;
            return Ok(());
        }
        {
            let mut guard = self.store.locked()?;
            if let Some(target) = guard.leases.get_mut(lease_id) {
                target.status = "stopped".into();
                target.stopped_at = Some(now());
            }
            guard.save()?;
        }
        let _ = lease;
        Ok(())
    }

    /// 貸出の受付開始（Python 版 start）。
    pub fn start(&self, request: StartRequest) -> Result<Value> {
        if !self.templates.contains_key(request.template_id) {
            return Err(AgentError::new(
                "Only registered template IDs can be started",
            ));
        }
        if self.gpu(request.gpu_id).is_none() {
            return Err(AgentError::new("GPU is not registered on this node"));
        }
        let mut requested: Vec<String> = Vec::new();
        let source = request
            .gpu_ids
            .clone()
            .unwrap_or_else(|| vec![request.gpu_id.to_owned()]);
        for id in source {
            if !requested.contains(&id) {
                requested.push(id);
            }
        }
        if requested.is_empty()
            || requested[0] != request.gpu_id
            || requested.iter().any(|id| self.gpu(id).is_none())
        {
            return Err(AgentError::new("Invalid diagnostic GPU group"));
        }
        if requested.len() > 1 && !request.diagnostic {
            return Err(AgentError::new(
                "GPU groups are reserved for the local diagnostic helper",
            ));
        }
        let template = self.templates[request.template_id].clone();
        let gpus: Vec<GPU> = requested
            .iter()
            .map(|id| self.gpu(id).unwrap().clone())
            .collect();
        let vendors: Vec<&String> = gpus.iter().map(|gpu| &gpu.vendor).collect();
        let unique: std::collections::BTreeSet<&String> = vendors.iter().copied().collect();
        if unique.len() != 1
            || gpus
                .iter()
                .any(|gpu| !template.vendors.contains(&gpu.vendor))
        {
            return Err(AgentError::new(
                "Template does not declare support for this GPU vendor",
            ));
        }
        let ssh_public_key = if request.ssh_public_key.is_empty() {
            String::new()
        } else {
            validate_ssh_key(request.ssh_public_key)?
        };
        if request.connection_mode != "relay" && request.connection_mode != "direct" {
            return Err(AgentError::new("Invalid connection mode"));
        }
        let lease_id = match request.lease_id {
            Some(value) => gpupod_common::identifier::identifier(value)?.to_owned(),
            None => uuid::Uuid::new_v4().simple().to_string(),
        };

        let operation = self.store.operation(&lease_id)?;
        if !operation.acquired() {
            return Err(AgentError::new("Lease operation is already running"));
        }

        // 検証と予約。ここでの失敗はそのまま呼び出し元へ返る。
        let (lease, node_id) = {
            let mut guard = self.store.locked()?;
            if let Some(existing) = guard.leases.get(&lease_id).cloned() {
                if existing.gpu_id != request.gpu_id || existing.template_id != request.template_id
                {
                    return Err(AgentError::new("Lease ID belongs to a different request"));
                }
                for (field, value) in [
                    ("memory_bytes", request.memory_bytes),
                    ("disk_bytes", request.disk_bytes),
                ] {
                    if let Some(value) = value {
                        let expected = resources::size_bytes(value, field)?;
                        let current = existing.resources.map(|item| item.get(field)).unwrap_or(0);
                        if current != expected {
                            return Err(AgentError::new(
                                "Lease ID belongs to a different resource request",
                            ));
                        }
                    }
                }
                return Ok(serde_json::to_value(&existing).unwrap_or(Value::Null));
            }
            if request.diagnostic && guard.manual_enabled {
                return Err(AgentError::new(
                    "Turn lending OFF before running diagnostics",
                ));
            }
            if !request.diagnostic && (!guard.manual_enabled || guard.auto_busy) {
                return Err(AgentError::new("Node is not accepting leases"));
            }
            let claimed = Self::active_gpu_ids(&guard);
            if requested.iter().any(|id| claimed.contains(id)) {
                return Err(AgentError::new(
                    "GPU is already allocated or awaiting cleanup",
                ));
            }
            let reserved = resources::claim(
                guard.resource_pool.as_ref(),
                &Self::allocations(&guard),
                request.memory_bytes,
                request.disk_bytes,
            )?;
            let mut lease = Lease {
                id: lease_id.clone(),
                gpu_id: request.gpu_id.to_owned(),
                template_id: template.id.clone(),
                image: template.image.clone(),
                container: format!("gpulend-{lease_id}"),
                volume: format!("gpulend-work-{lease_id}"),
                status: "starting".into(),
                created_at: now(),
                ssh_public_key: ssh_public_key.clone(),
                network: Some(format!("gpulend-net-{lease_id}")),
                gpu_ids: requested.clone(),
                diagnostic: request.diagnostic,
                connection_mode: Some(request.connection_mode.to_owned()),
                resources: reserved,
                error: None,
                started_at: None,
                ports: None,
                stopped_at: None,
            };
            lease.status = "starting".into();
            let node_id = guard.node_id.clone();
            guard.leases.insert(lease_id.clone(), lease.clone());
            guard.save()?;
            (lease, node_id)
        };

        match self.start_engine(&lease, &template, &gpus, &node_id) {
            Ok(value) => Ok(value),
            Err(failure) => {
                let message = failure.message();
                {
                    let mut guard = self.store.locked()?;
                    if let Some(target) = guard.leases.get_mut(&lease_id) {
                        target.error = Some(message.clone());
                        target.status = "cleanup_pending".into();
                    }
                    guard.save()?;
                }
                if !failure.uncertain() {
                    self.cleanup(&lease_id)?;
                }
                Err(AgentError::new(format!(
                    "Start failed for lease {lease_id}: {message}"
                )))
            }
        }
    }

    /// コンテナの起動と running 確定（Python 版 start の try 節）。
    fn start_engine(
        &self,
        lease: &Lease,
        template: &Template,
        gpus: &[GPU],
        node_id: &str,
    ) -> std::result::Result<Value, StartFailure> {
        self.engine.create(lease, template, gpus, node_id)?;
        self.engine.start(lease)?;
        let value = match self.engine.describe(lease, node_id)? {
            Some(value) => value,
            None => return Err(DockerError::new("Container exited before becoming ready").into()),
        };
        let running = value
            .get("State")
            .and_then(|state| state.get("Running"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if !running {
            return Err(DockerError::new("Container exited before becoming ready").into());
        }
        let ports = value
            .get("NetworkSettings")
            .and_then(|settings| settings.get("Ports"))
            .cloned()
            .unwrap_or(Value::Null);
        let updated = {
            let mut guard = self.store.locked()?;
            if !guard.leases.contains_key(&lease.id) {
                return Err(AgentError::new("Unknown lease ID").into());
            }
            let current = guard.leases.get_mut(&lease.id).expect("checked above");
            if current.status == "cleanup_pending" {
                None
            } else {
                current.status = "running".into();
                current.started_at = Some(now());
                current.ports = Some(ports);
                let value = serde_json::to_value(&*current).unwrap_or(Value::Null);
                guard.save()?;
                Some(value)
            }
        };
        if let Some(value) = updated {
            return Ok(value);
        }
        self.cleanup(&lease.id)?;
        Err(AgentError::new("Lease was cancelled during startup").into())
    }

    /// 貸出の停止要求（Python 版 stop）。
    pub fn stop(&self, lease_id: &str) -> Result<Value> {
        {
            let mut guard = self.store.locked()?;
            if !guard.leases.contains_key(lease_id) {
                return Err(AgentError::new("Unknown lease ID"));
            }
            let lease = guard.leases.get_mut(lease_id).expect("checked above");
            if lease.status != "stopped" {
                lease.status = "cleanup_pending".into();
                guard.save()?;
            }
        }
        let lock = self.store.operation(lease_id)?;
        if lock.acquired() {
            self.cleanup(lease_id)?;
        }
        let status = self.status()?;
        Ok(status
            .get("leases")
            .and_then(|leases| leases.get(lease_id))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// 実態と状態の突き合わせ（Python 版 reconcile）。
    pub fn reconcile(&self) -> Result<Value> {
        let snapshot = self.status()?;
        let node_id = snapshot
            .get("node_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        let active: Vec<(String, Value)> = snapshot
            .get("leases")
            .and_then(|v| v.as_object())
            .map(|leases| {
                leases
                    .values()
                    .filter(|lease| lease.get("status").and_then(|v| v.as_str()) != Some("stopped"))
                    .filter_map(|lease| {
                        Some((lease.get("id")?.as_str()?.to_owned(), lease.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (lease_id, _) in active {
            let lock = self.store.operation(&lease_id)?;
            if !lock.acquired() {
                continue;
            }
            let lease = {
                let guard = self.store.locked()?;
                match guard.leases.get(&lease_id) {
                    Some(lease) => lease.clone(),
                    None => continue,
                }
            };
            if lease.status == "stopped" {
                continue;
            }
            if lease.status == "cleanup_pending" {
                self.cleanup(&lease_id)?;
                continue;
            }
            match self.engine.describe(&lease, &node_id) {
                Ok(Some(value)) => {
                    let running = value
                        .get("State")
                        .and_then(|state| state.get("Running"))
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false);
                    if running {
                        let ports = value
                            .get("NetworkSettings")
                            .and_then(|settings| settings.get("Ports"))
                            .cloned()
                            .unwrap_or(Value::Null);
                        let pending = {
                            let mut guard = self.store.locked()?;
                            if !guard.leases.contains_key(&lease_id) {
                                continue;
                            }
                            let current = guard.leases.get_mut(&lease_id).expect("checked above");
                            if current.status == "cleanup_pending" {
                                true
                            } else {
                                current.status = "running".into();
                                current.ports = Some(ports);
                                guard.save()?;
                                false
                            }
                        };
                        if pending {
                            self.cleanup(&lease_id)?;
                        }
                    } else {
                        self.cleanup(&lease_id)?;
                    }
                }
                Ok(None) => self.cleanup(&lease_id)?,
                Err(exc) => {
                    let mut guard = self.store.locked()?;
                    if let Some(target) = guard.leases.get_mut(&lease_id) {
                        target.error = Some(exc.to_string());
                    }
                    guard.save()?;
                }
            }
        }
        self.status()
    }

    /// コンテナごとの使用量（Python 版 statistics）。
    pub fn statistics(&self) -> Result<Value> {
        let snapshot = self.status()?;
        let node_id = snapshot
            .get("node_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        let active: Vec<Lease> = {
            let guard = self.store.locked()?;
            Self::active(&guard).into_iter().cloned().collect()
        };
        let containers = self
            .engine
            .statistics(&active, &node_id)
            .map_err(|exc| AgentError::new(exc.to_string()))?;
        Ok(json!({
            "node_id": node_id,
            "collected_at": now(),
            "containers": containers,
        }))
    }
}

/// GPU を Python 版 GPU.to_dict と同じ形にする（memory_mib は null を含む）。
pub fn gpu_to_value(gpu: &GPU) -> Value {
    json!({
        "id": gpu.id,
        "vendor": gpu.vendor,
        "name": gpu.name,
        "backend": gpu.backend,
        "selector": gpu.selector,
        "devices": gpu.devices,
        "memory_mib": gpu.memory_mib,
    })
}

/// SSH 公開鍵 1 本の検証と正規化（Python 版 validate_ssh_key）。
///
/// 単一行・4096 バイト以内・OpenSSH 形式として解釈できることを要求し、
/// 正規化した 1 行を返す。
pub fn validate_ssh_key(value: &str) -> Result<String> {
    if value.len() > 4096 || value.trim().contains('\n') {
        return Err(AgentError::new("A single SSH public key is required"));
    }
    let trimmed = value.trim();
    let key = russh::keys::ssh_key::PublicKey::from_openssh(trimmed)
        .map_err(|_| AgentError::new("Invalid SSH public key"))?;
    let exported = key
        .to_openssh()
        .map_err(|_| AgentError::new("Invalid SSH public key"))?;
    Ok(exported.trim().to_owned())
}
