//! Docker エンジン（Python 版 docker.py の bollard 移植）。
//!
//! 所有権ラベル io.gpulend.lease / io.gpulend.node を検査し、
//! ネットワークは ICC 無効、ボリュームは /workspace へ、
//! RAM は mem_limit+memswap、CPU は nano_cpus、GPU は
//! device_requests（nvidia）または devices+group_add（デバイスの gid）で渡す。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::sync::Mutex;

use bollard::errors::Error as BollardError;
use bollard::models::{
    ContainerCreateBody, DeviceMapping, DeviceRequest, HostConfig, NetworkCreateRequest,
    PortBinding, RestartPolicy, RestartPolicyNameEnum, VolumeCreateRequest,
};
use bollard::query_parameters::{
    CreateContainerOptions, RemoveContainerOptions, RemoveVolumeOptions, StatsOptions,
};
use bollard::Docker;
use futures_util::StreamExt;
use gpupod_common::{AgentError, GPU, Template};
use serde_json::{json, Value};

use crate::agent::Lease;

pub const LEASE_LABEL: &str = "io.gpulend.lease";
pub const NODE_LABEL: &str = "io.gpulend.node";

/// Docker 操作の失敗。uncertain=true は予約を保持する理由になる。
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct DockerError {
    pub message: String,
    pub uncertain: bool,
}

impl DockerError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), uncertain: false }
    }

    pub fn with_uncertain(message: impl Into<String>, uncertain: bool) -> Self {
        Self { message: message.into(), uncertain }
    }
}

/// エンジンの抽象。テストはモックで差し替えられる。
pub trait DockerEngine: Send + Sync {
    fn create(
        &self,
        lease: &Lease,
        template: &Template,
        gpus: &[GPU],
        node_id: &str,
    ) -> std::result::Result<(), DockerError>;

    fn start(&self, lease: &Lease) -> std::result::Result<(), DockerError>;

    fn describe(
        &self,
        lease: &Lease,
        node_id: &str,
    ) -> std::result::Result<Option<Value>, DockerError>;

    fn cleanup(&self, lease: &Lease, node_id: &str) -> std::result::Result<(), DockerError>;

    fn statistics(
        &self,
        leases: &[Lease],
        node_id: &str,
    ) -> std::result::Result<Vec<Value>, DockerError>;

    fn container_ids(
        &self,
        leases: &[Lease],
        node_id: &str,
    ) -> std::result::Result<Vec<String>, DockerError>;
}

/// エンジン設定（Python 版 DockerEngine のキーワード引数）。
#[derive(Debug, Clone)]
pub struct EngineOptions {
    pub socket_path: String,
    pub timeout: u64,
    pub limits: BTreeMap<String, i64>,
    pub direct_bind: Option<String>,
    pub mock_gpu: bool,
    pub runtime: Option<String>,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            socket_path: "/var/run/docker.sock".into(),
            timeout: 60,
            limits: BTreeMap::new(),
            direct_bind: None,
            mock_gpu: false,
            runtime: None,
        }
    }
}

/// unix ソケットで Docker Engine に接続する本番エンジン。
pub struct BollardEngine {
    options: EngineOptions,
    runtime: tokio::runtime::Runtime,
    client: Mutex<Option<Docker>>,
}

impl BollardEngine {
    pub fn new(options: EngineOptions) -> Result<Self, DockerError> {
        if !matches!(options.runtime.as_deref(), None | Some("runc") | Some("runsc")) {
            return Err(DockerError::new("Runtime must be runc or runsc"));
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|exc| DockerError::new(format!("Cannot connect to local Docker Engine: {exc}")))?;
        Ok(Self { options, runtime, client: Mutex::new(None) })
    }

    fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    /// 初回だけ接続とバージョン自動調整を行う（version="auto" 相当）。
    fn client(&self) -> Result<Docker, DockerError> {
        {
            let cache = self.client.lock().unwrap_or_else(|exc| exc.into_inner());
            if let Some(existing) = cache.clone() {
                return Ok(existing);
            }
        }
        let path = self.options.socket_path.clone();
        let timeout = self.options.timeout;
        let docker = self.block_on(async move {
            let docker = Docker::connect_with_unix(&path, timeout, &bollard::API_DEFAULT_VERSION)
                .map_err(|exc| {
                    DockerError::new(format!("Cannot connect to local Docker Engine: {exc}"))
                })?;
            docker.negotiate_version().await.map_err(|exc| {
                DockerError::new(format!("Cannot connect to local Docker Engine: {exc}"))
            })
        })?;
        let mut cache = self.client.lock().unwrap_or_else(|exc| exc.into_inner());
        *cache = Some(docker.clone());
        Ok(docker)
    }

    async fn create_inner(
        &self,
        docker: &Docker,
        lease: &Lease,
        template: &Template,
        gpus: &[GPU],
        node_id: &str,
    ) -> std::result::Result<(), DockerError> {
        if self.options.runtime.as_deref() == Some("runsc") {
            if !self.options.mock_gpu && gpus.iter().any(|gpu| gpu.vendor != "nvidia") {
                return Err(DockerError::new(
                    "runsc GPU mode currently supports NVIDIA only; select runc for this node",
                ));
            }
            let info = docker.info().await.map_err(map_error)?;
            let registered = info
                .runtimes
                .as_ref()
                .map(|runtimes| runtimes.contains_key("runsc"))
                .unwrap_or(false);
            if !registered {
                return Err(DockerError::new(
                    "The provider has not registered the runsc Docker runtime",
                ));
            }
        }
        let direct = lease.mode() == "direct";
        if direct && self.options.direct_bind.is_none() {
            return Err(DockerError::new("Direct connections are disabled by the provider"));
        }
        let bind_host = if direct {
            self.options.direct_bind.clone().unwrap_or_else(|| "127.0.0.1".into())
        } else {
            "127.0.0.1".to_owned()
        };
        if inspect_container(docker, &lease.container).await?.is_some() {
            return Err(DockerError::new("Container name already exists"));
        }
        if let Some(network_name) = lease.network.as_deref() {
            match inspect_network(docker, network_name).await? {
                Some(network) => check_owner(
                    network.get("Labels").and_then(|v| v.as_object()),
                    lease,
                    node_id,
                )?,
                None => {
                    let request = NetworkCreateRequest {
                        name: network_name.to_owned(),
                        driver: Some("bridge".into()),
                        options: Some(HashMap::from([(
                            "com.docker.network.bridge.enable_icc".to_owned(),
                            "false".to_owned(),
                        )])),
                        labels: Some(HashMap::from([
                            (LEASE_LABEL.to_owned(), lease.id.clone()),
                            (NODE_LABEL.to_owned(), node_id.to_owned()),
                        ])),
                        ..Default::default()
                    };
                    docker.create_network(request).await.map_err(map_error)?;
                }
            }
        }
        match inspect_volume(docker, &lease.volume).await? {
            Some(volume) => {
                check_owner(volume.get("Labels").and_then(|v| v.as_object()), lease, node_id)?
            }
            None => {
                let request = VolumeCreateRequest {
                    name: Some(lease.volume.clone()),
                    labels: Some(HashMap::from([
                        (LEASE_LABEL.to_owned(), lease.id.clone()),
                        (NODE_LABEL.to_owned(), node_id.to_owned()),
                    ])),
                    ..Default::default()
                };
                docker.create_volume(request).await.map_err(map_error)?;
            }
        }

        let mut labels = HashMap::new();
        labels.insert(LEASE_LABEL.to_owned(), lease.id.clone());
        labels.insert(NODE_LABEL.to_owned(), node_id.to_owned());

        let mut environment: HashMap<String, String> = template
            .environment
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        environment.insert("GPUPOD_SSH_PUBLIC_KEY".to_owned(), lease.ssh_public_key.clone());

        let mut host = HostConfig {
            binds: Some(vec![format!("{}:/workspace:rw", lease.volume)]),
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::NO),
                maximum_retry_count: None,
            }),
            runtime: self.options.runtime.clone(),
            ..Default::default()
        };
        if let Some(network_name) = lease.network.as_deref() {
            host.network_mode = Some(network_name.to_owned());
        }
        let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
        for port in &template.ports {
            port_bindings.insert(
                port.clone(),
                Some(vec![PortBinding {
                    host_ip: Some(bind_host.clone()),
                    host_port: None,
                }]),
            );
        }
        host.port_bindings = Some(port_bindings);

        if let Some(limit) = self.options.limits.get("nano_cpus") {
            host.nano_cpus = Some(*limit);
        }
        if let Some(limit) = self.options.limits.get("mem_limit") {
            host.memory = Some(*limit);
        }
        if let Some(limit) = self.options.limits.get("pids_limit") {
            host.pids_limit = Some(*limit);
        }
        if let Some(limit) = self.options.limits.get("shm_size") {
            host.shm_size = Some(*limit);
        }
        if let Some(resources) = lease.resources {
            // これは貸出ごとの RAM 上限。遊休メモリを持ちっぱなしの常駐プロセスは要らない。
            host.memory = Some(resources.memory_bytes as i64);
            host.memory_swap = host.memory;
        }

        if !self.options.mock_gpu && !gpus.is_empty() {
            if gpus[0].backend == "nvidia" {
                host.device_requests = Some(vec![DeviceRequest {
                    driver: Some("nvidia".into()),
                    count: None,
                    device_ids: Some(gpus.iter().map(|gpu| gpu.selector.clone()).collect()),
                    capabilities: Some(vec![vec!["gpu".to_owned()]]),
                    options: None,
                }]);
            } else {
                let mut paths: HashSet<String> = HashSet::new();
                for gpu in gpus {
                    for path in &gpu.devices {
                        paths.insert(path.clone());
                    }
                }
                let mut paths: Vec<String> = paths.into_iter().collect();
                paths.sort();
                let mut groups: HashSet<u32> = HashSet::new();
                for path in &paths {
                    if let Ok(metadata) = std::fs::metadata(path) {
                        use std::os::unix::fs::MetadataExt;
                        groups.insert(metadata.gid());
                    }
                }
                let mut groups: Vec<u32> = groups.into_iter().collect();
                groups.sort_unstable();
                host.devices = Some(
                    paths
                        .iter()
                        .map(|path| DeviceMapping {
                            path_on_host: Some(path.clone()),
                            path_in_container: Some(path.clone()),
                            cgroup_permissions: Some("rw".into()),
                        })
                        .collect(),
                );
                host.group_add = Some(groups.iter().map(|group| group.to_string()).collect());
            }
        }

        let command: Vec<String> = if template.command.is_empty() {
            Vec::new()
        } else {
            template.command.clone()
        };
        let body = ContainerCreateBody {
            image: Some(template.image.clone()),
            cmd: if command.is_empty() { None } else { Some(command) },
            env: Some(
                environment
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect(),
            ),
            labels: Some(labels),
            working_dir: Some("/workspace".into()),
            volumes: Some(vec!["/workspace".to_owned()]),
            exposed_ports: Some(template.ports.clone()),
            host_config: Some(host),
            ..Default::default()
        };
        let options = CreateContainerOptions {
            name: Some(lease.container.clone()),
            ..Default::default()
        };
        docker
            .create_container(Some(options), body)
            .await
            .map_err(map_error)?;
        Ok(())
    }

    async fn cleanup_inner(
        &self,
        docker: &Docker,
        lease: &Lease,
        node_id: &str,
    ) -> std::result::Result<(), DockerError> {
        if let Some(container) = inspect_container(docker, &lease.container).await? {
            check_owner(
                container
                    .get("Config")
                    .and_then(|config| config.get("Labels"))
                    .and_then(|v| v.as_object()),
                lease,
                node_id,
            )?;
            let options = RemoveContainerOptions { force: true, v: true, link: false };
            docker
                .remove_container(&lease.container, Some(options))
                .await
                .map_err(map_error)?;
        }
        if let Some(volume) = inspect_volume(docker, &lease.volume).await? {
            check_owner(volume.get("Labels").and_then(|v| v.as_object()), lease, node_id)?;
            let options: Option<RemoveVolumeOptions> = None;
            docker
                .remove_volume(&lease.volume, options)
                .await
                .map_err(map_error)?;
        }
        if let Some(network_name) = lease.network.as_deref() {
            if let Some(network) = inspect_network(docker, network_name).await? {
                check_owner(
                    network.get("Labels").and_then(|v| v.as_object()),
                    lease,
                    node_id,
                )?;
                docker.remove_network(network_name).await.map_err(map_error)?;
            }
        }
        Ok(())
    }
}

impl DockerEngine for BollardEngine {
    fn create(
        &self,
        lease: &Lease,
        template: &Template,
        gpus: &[GPU],
        node_id: &str,
    ) -> std::result::Result<(), DockerError> {
        let docker = self.client()?;
        self.block_on(self.create_inner(&docker, lease, template, gpus, node_id))
    }

    fn start(&self, lease: &Lease) -> std::result::Result<(), DockerError> {
        let docker = self.client()?;
        self.block_on(async move {
            if inspect_container(&docker, &lease.container).await?.is_none() {
                return Err(DockerError::new("Container disappeared before start"));
            }
            docker
                .start_container(&lease.container, None)
                .await
                .map_err(map_error)
        })
    }

    fn describe(
        &self,
        lease: &Lease,
        node_id: &str,
    ) -> std::result::Result<Option<Value>, DockerError> {
        let docker = self.client()?;
        self.block_on(async move {
            let Some(container) = inspect_container(&docker, &lease.container).await? else {
                return Ok(None);
            };
            check_owner(
                container
                    .get("Config")
                    .and_then(|config| config.get("Labels"))
                    .and_then(|v| v.as_object()),
                lease,
                node_id,
            )?;
            Ok(Some(container))
        })
    }

    fn cleanup(&self, lease: &Lease, node_id: &str) -> std::result::Result<(), DockerError> {
        let docker = self.client()?;
        self.block_on(self.cleanup_inner(&docker, lease, node_id))
    }

    fn statistics(
        &self,
        leases: &[Lease],
        node_id: &str,
    ) -> std::result::Result<Vec<Value>, DockerError> {
        let docker = self.client()?;
        self.block_on(async move {
            let mut results = Vec::new();
            for lease in leases {
                if lease.status != "running" {
                    continue;
                }
                let Some(container) = inspect_container(&docker, &lease.container).await? else {
                    continue;
                };
                check_owner(
                    container
                        .get("Config")
                        .and_then(|config| config.get("Labels"))
                        .and_then(|v| v.as_object()),
                    lease,
                    node_id,
                )?;
                let options = StatsOptions { stream: false, one_shot: true };
                let mut stream = docker.stats(&lease.container, Some(options));
                if let Some(item) = stream.next().await {
                    let stats = item.map_err(map_error)?;
                    results.push(json!({
                        "lease_id": lease.id,
                        "docker": serde_json::to_value(stats).unwrap_or(Value::Null),
                    }));
                }
            }
            Ok(results)
        })
    }

    fn container_ids(
        &self,
        leases: &[Lease],
        node_id: &str,
    ) -> std::result::Result<Vec<String>, DockerError> {
        let docker = self.client()?;
        self.block_on(async move {
            let mut result = Vec::new();
            for lease in leases {
                if lease.status != "running" {
                    continue;
                }
                let Some(container) = inspect_container(&docker, &lease.container).await? else {
                    continue;
                };
                check_owner(
                    container
                        .get("Config")
                        .and_then(|config| config.get("Labels"))
                        .and_then(|v| v.as_object()),
                    lease,
                    node_id,
                )?;
                if let Some(id) = container.get("Id").and_then(|v| v.as_str()) {
                    result.push(id.to_owned());
                }
            }
            Ok(result)
        })
    }
}

/// ラベルがこの貸出とこのノードのものかを確かめる。
fn check_owner(
    labels: Option<&serde_json::Map<String, Value>>,
    lease: &Lease,
    node_id: &str,
) -> std::result::Result<(), DockerError> {
    let read = |key: &str| {
        labels
            .and_then(|labels| labels.get(key))
            .and_then(|value| value.as_str())
            .unwrap_or("")
    };
    let lease_label = read(LEASE_LABEL);
    let node_label = read(NODE_LABEL);
    if lease_label != lease.id || node_label != node_id {
        return Err(DockerError::new(
            "Refusing to operate on a resource owned by another lease or node",
        ));
    }
    Ok(())
}

/// コンテナを JSON で引き、無ければ None。
async fn inspect_container(
    docker: &Docker,
    name: &str,
) -> std::result::Result<Option<Value>, DockerError> {
    match docker.inspect_container(name, None).await {
        Ok(container) => serde_json::to_value(&container)
            .map(Some)
            .map_err(|exc| DockerError::with_uncertain(format!("{exc}"), true)),
        Err(exc) if is_missing(&exc) => Ok(None),
        Err(exc) => Err(map_error(exc)),
    }
}

/// ボリュームを JSON で引き、無ければ None。
async fn inspect_volume(docker: &Docker, name: &str) -> std::result::Result<Option<Value>, DockerError> {
    match docker.inspect_volume(name).await {
        Ok(volume) => serde_json::to_value(&volume)
            .map(Some)
            .map_err(|exc| DockerError::with_uncertain(format!("{exc}"), true)),
        Err(exc) if is_missing(&exc) => Ok(None),
        Err(exc) => Err(map_error(exc)),
    }
}

/// ネットワークを JSON で引き、無ければ None。
async fn inspect_network(
    docker: &Docker,
    name: &str,
) -> std::result::Result<Option<Value>, DockerError> {
    match docker.inspect_network(name, None).await {
        Ok(network) => serde_json::to_value(&network)
            .map(Some)
            .map_err(|exc| DockerError::with_uncertain(format!("{exc}"), true)),
        Err(exc) if is_missing(&exc) => Ok(None),
        Err(exc) => Err(map_error(exc)),
    }
}

fn is_missing(exc: &BollardError) -> bool {
    matches!(exc, BollardError::DockerResponseServerError { status_code: 404, .. })
}

/// Python 版 _call の例外分類をなぞる。
fn map_error(exc: BollardError) -> DockerError {
    match exc {
        BollardError::DockerResponseServerError { status_code, ref message } => {
            DockerError::with_uncertain(message.clone(), status_code >= 500)
        }
        BollardError::RequestTimeoutError => DockerError::with_uncertain(
            "Docker timed out; reservation retained until reconciliation",
            true,
        ),
        other => DockerError::with_uncertain(other.to_string(), true),
    }
}

/// エンジン起因の失敗を台帳用のエラーへ写す。
pub fn as_agent_error(exc: DockerError) -> AgentError {
    AgentError::new(exc.to_string())
}
