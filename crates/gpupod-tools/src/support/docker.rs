//! ローカル Docker Engine アダプタ（Python 版 docker.py の診断・補助向け最小移植）。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::models::{
    ContainerCreateBody, ContainerWaitResponse, HostConfig, NetworkCreateRequest, RestartPolicy,
    RestartPolicyNameEnum, VolumeCreateRequest,
};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, InspectContainerOptions, InspectNetworkOptions,
    LogsOptions, RemoveContainerOptions, RemoveVolumeOptions, StartContainerOptions,
    WaitContainerOptions,
};
use bollard::{Docker, API_DEFAULT_VERSION};
use gpupod_common::{AgentError, Template, GPU};

pub const LEASE_LABEL: &str = "io.gpulend.lease";
pub const NODE_LABEL: &str = "io.gpulend.node";

/// 診断・自動貸出が使う最小の Docker 操作群。
#[derive(Debug, Clone)]
pub struct DockerEngine {
    pub socket: PathBuf,
    pub mock_gpu: bool,
    pub runtime: Option<String>,
    pub limits: BTreeMap<String, i64>,
    pub timeout_seconds: u64,
}

impl DockerEngine {
    pub fn new(socket: &Path) -> DockerEngine {
        DockerEngine {
            socket: socket.to_path_buf(),
            mock_gpu: false,
            runtime: None,
            limits: BTreeMap::new(),
            timeout_seconds: 60,
        }
    }

    pub fn with_runtime(mut self, runtime: Option<String>) -> Result<DockerEngine> {
        if let Some(runtime) = &runtime {
            if runtime != "runc" && runtime != "runsc" {
                bail!(AgentError::new("Runtime must be runc or runsc"));
            }
        }
        self.runtime = runtime;
        Ok(self)
    }

    pub fn mock(mut self, mock_gpu: bool) -> DockerEngine {
        self.mock_gpu = mock_gpu;
        self
    }

    fn connect(&self) -> Result<Docker> {
        let path = self.socket.to_string_lossy().to_string();
        Docker::connect_with_socket(&path, self.timeout_seconds, API_DEFAULT_VERSION)
            .context("Cannot connect to local Docker Engine")
    }

    pub fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        block_on(future)
    }

    pub fn info(&self) -> Result<serde_json::Value> {
        let engine = self.connect()?;
        block_on(async {
            let info = engine.info().await.context("Docker info failed")?;
            serde_json::to_value(info).context("Cannot encode Docker info")
        })
    }

    pub fn pull_image(&self, image: &str) -> Result<String> {
        let engine = self.connect()?;
        block_on(async move {
            let options = CreateImageOptions {
                from_image: Some(image.to_string()),
                ..Default::default()
            };
            let mut stream = engine.create_image(Some(options), None, None);
            use futures_util::StreamExt;
            while let Some(chunk) = stream.next().await {
                chunk.context("Cannot pull the image")?;
            }
            let inspect = engine
                .inspect_image(image)
                .await
                .context("Cannot inspect the pulled image")?;
            Ok(inspect.id.unwrap_or_else(|| image.to_string()))
        })
    }

    /// ラベル所有を確認しつつコンテナを返す。
    pub fn describe(
        &self,
        name: &str,
        lease_id: &str,
        node_id: &str,
    ) -> Result<Option<serde_json::Value>> {
        let engine = self.connect()?;
        block_on(async move {
            let container = match engine
                .inspect_container(name, None::<InspectContainerOptions>)
                .await
            {
                Ok(container) => container,
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => return Ok(None),
                Err(exc) => return Err(exc).context("Cannot inspect the container"),
            };
            check_owner(
                container.config.as_ref().and_then(|c| c.labels.as_ref()),
                lease_id,
                node_id,
            )?;
            serde_json::to_value(container)
                .map(Some)
                .context("Cannot encode the container description")
        })
    }

    pub fn container_ids(&self, names: &[(String, String)], node_id: &str) -> Result<Vec<String>> {
        let engine = self.connect()?;
        block_on(async move {
            let mut result = Vec::new();
            for (name, lease_id) in names {
                let container = match engine
                    .inspect_container(name, None::<InspectContainerOptions>)
                    .await
                {
                    Ok(container) => container,
                    Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    }) => continue,
                    Err(exc) => return Err(exc).context("Cannot inspect the container"),
                };
                check_owner(
                    container.config.as_ref().and_then(|c| c.labels.as_ref()),
                    lease_id,
                    node_id,
                )?;
                if let Some(id) = container.id {
                    result.push(id);
                }
            }
            Ok(result)
        })
    }

    /// 診断用コンテナを作る（Python 版 DockerEngine.create の移植）。
    #[allow(clippy::too_many_arguments)]
    pub fn create_diagnostic(
        &self,
        lease_id: &str,
        node_id: &str,
        container_name: &str,
        volume_name: &str,
        network_name: &str,
        template: &Template,
        gpus: &[GPU],
        ssh_public_key: &str,
        memory_bytes: Option<i64>,
    ) -> Result<()> {
        let engine = self.connect()?;
        let image = template.image.clone();
        let command = if template.command.is_empty() {
            None
        } else {
            Some(template.command.clone())
        };
        let mut environment: Vec<String> = template
            .environment
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        environment.push(format!("GPUPOD_SSH_PUBLIC_KEY={ssh_public_key}"));
        let labels = HashMap::from([
            (LEASE_LABEL.to_string(), lease_id.to_string()),
            (NODE_LABEL.to_string(), node_id.to_string()),
        ]);
        let network_name = if network_name.is_empty() {
            None
        } else {
            Some(network_name.to_string())
        };
        let (devices, device_requests, group_add) = self.gpu_attachment(gpus)?;
        let exposed: Vec<String> = template.ports.clone();
        let port_bindings: HashMap<String, Option<Vec<bollard::models::PortBinding>>> = template
            .ports
            .iter()
            .map(|port| {
                (
                    port.clone(),
                    Some(vec![bollard::models::PortBinding {
                        host_ip: Some("127.0.0.1".into()),
                        host_port: None,
                    }]),
                )
            })
            .collect();
        let runtime = self.runtime.clone();
        block_on(async move {
            if let Some(network) = network_name.as_deref() {
                let existing = match engine
                    .inspect_network(network, None::<InspectNetworkOptions>)
                    .await
                {
                    Ok(_) => true,
                    Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    }) => false,
                    Err(exc) => return Err(exc).context("Cannot inspect the network"),
                };
                if !existing {
                    let request = NetworkCreateRequest {
                        name: network.to_string(),
                        driver: Some("bridge".to_string()),
                        options: Some(HashMap::from([(
                            "com.docker.network.bridge.enable_icc".to_string(),
                            "false".to_string(),
                        )])),
                        labels: Some(labels.clone()),
                        ..Default::default()
                    };
                    engine
                        .create_network(request)
                        .await
                        .context("Cannot create the network")?;
                }
            }
            let volume = VolumeCreateRequest {
                name: Some(volume_name.to_string()),
                labels: Some(labels.clone()),
                ..Default::default()
            };
            engine
                .create_volume(volume)
                .await
                .context("Cannot create the workspace volume")?;
            let host_config = HostConfig {
                binds: Some(vec![format!("{volume_name}:/workspace:rw")]),
                network_mode: network_name,
                devices,
                device_requests,
                group_add,
                memory: memory_bytes,
                memory_swap: memory_bytes,
                port_bindings: Some(port_bindings),
                runtime,
                restart_policy: Some(RestartPolicy {
                    name: Some(RestartPolicyNameEnum::NO),
                    maximum_retry_count: None,
                }),
                ..Default::default()
            };
            let config: ContainerCreateBody = ContainerCreateBody {
                image: Some(image),
                cmd: command,
                env: Some(environment),
                labels: Some(labels),
                working_dir: Some("/workspace".to_string()),
                exposed_ports: if exposed.is_empty() {
                    None
                } else {
                    Some(exposed)
                },

                host_config: Some(host_config),
                ..Default::default()
            };
            let options = CreateContainerOptions {
                name: Some(container_name.to_string()),
                ..Default::default()
            };
            engine
                .create_container(Some(options), config)
                .await
                .context("Cannot create the diagnostic container")?;
            Ok(())
        })
    }

    fn gpu_attachment(
        &self,
        gpus: &[GPU],
    ) -> Result<(
        Option<Vec<bollard::models::DeviceMapping>>,
        Option<Vec<bollard::models::DeviceRequest>>,
        Option<Vec<String>>,
    )> {
        if self.mock_gpu || gpus.is_empty() {
            return Ok((None, None, None));
        }
        if gpus[0].backend == "nvidia" {
            let request = bollard::models::DeviceRequest {
                driver: Some("nvidia".to_string()),
                count: None,
                device_ids: Some(gpus.iter().map(|gpu| gpu.selector.clone()).collect()),
                capabilities: Some(vec![vec!["gpu".to_string()]]),
                options: None,
            };
            return Ok((None, Some(vec![request]), None));
        }
        let paths: Vec<String> = gpus
            .iter()
            .flat_map(|gpu| gpu.devices.iter().cloned())
            .collect::<std::collections::BTreeSet<String>>()
            .into_iter()
            .collect();
        let devices = paths
            .iter()
            .map(|path| bollard::models::DeviceMapping {
                path_on_host: Some(path.clone()),
                path_in_container: Some(path.clone()),
                cgroup_permissions: Some("rw".to_string()),
            })
            .collect();
        let mut groups: Vec<String> = gpus
            .iter()
            .flat_map(|gpu| gpu.devices.iter())
            .filter_map(|path| {
                let meta = std::fs::metadata(path).ok()?;
                use std::os::unix::fs::MetadataExt;
                Some(meta.gid().to_string())
            })
            .collect::<std::collections::BTreeSet<String>>()
            .into_iter()
            .collect();
        groups.sort();
        let group_add = if groups.is_empty() {
            None
        } else {
            Some(groups)
        };
        Ok((Some(devices), None, group_add))
    }

    pub fn start(&self, container_name: &str) -> Result<()> {
        let engine = self.connect()?;
        block_on(async move {
            engine
                .start_container(container_name, None::<StartContainerOptions>)
                .await
                .context("Cannot start the container")?;
            Ok(())
        })
    }

    /// コンテナ内でコマンドを実行し (exit code, 出力) を返す。
    pub fn exec(&self, container_name: &str, command: Vec<String>) -> Result<(i64, String)> {
        let engine = self.connect()?;
        block_on(async move {
            use futures_util::StreamExt;
            let exec = engine
                .create_exec(
                    container_name,
                    CreateExecOptions {
                        attach_stdout: Some(true),
                        attach_stderr: Some(true),
                        cmd: Some(command),
                        ..Default::default()
                    },
                )
                .await
                .context("Cannot create the diagnostic command")?;
            let results = engine
                .start_exec(&exec.id, None::<StartExecOptions>)
                .await
                .context("Cannot run the diagnostic command")?;
            let mut output = Vec::new();
            match results {
                StartExecResults::Attached {
                    output: mut stream, ..
                } => {
                    while let Some(chunk) = stream.next().await {
                        use bollard::container::LogOutput;
                        match chunk.context("Cannot read the diagnostic output")? {
                            LogOutput::StdOut { message }
                            | LogOutput::StdErr { message }
                            | LogOutput::Console { message } => output.extend_from_slice(&message),
                            _ => {}
                        }
                    }
                }
                StartExecResults::Detached => {}
            }
            let exit = engine
                .inspect_exec(&exec.id)
                .await
                .context("Cannot inspect the diagnostic command")?;
            Ok((
                exit.exit_code.unwrap_or(-1),
                String::from_utf8_lossy(&output).to_string(),
            ))
        })
    }

    pub fn logs_tail(&self, container_name: &str, tail: usize) -> Result<String> {
        let engine = self.connect()?;
        block_on(async move {
            use futures_util::StreamExt;
            let options = LogsOptions {
                stdout: true,
                stderr: true,
                tail: tail.to_string(),
                ..Default::default()
            };
            let mut stream = engine.logs(container_name, Some(options));
            let mut text = String::new();
            while let Some(chunk) = stream.next().await {
                use bollard::container::LogOutput;
                match chunk.context("Cannot read logs")? {
                    LogOutput::StdOut { message }
                    | LogOutput::StdErr { message }
                    | LogOutput::Console { message } => {
                        text.push_str(&String::from_utf8_lossy(&message));
                    }
                    _ => {}
                }
            }
            Ok(text)
        })
    }

    pub fn wait_running(&self, container_name: &str, timeout_seconds: u64) -> Result<bool> {
        let engine = self.connect()?;
        block_on(async move {
            use futures_util::StreamExt;
            let mut stream = engine.wait_container(
                container_name,
                Some(WaitContainerOptions {
                    condition: "not-running".to_string(),
                }),
            );
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(timeout_seconds),
                stream.next(),
            )
            .await;
            match result {
                Ok(Some(Ok(ContainerWaitResponse { status_code, error }))) => {
                    if let Some(error) = error {
                        let message = error.message.unwrap_or_default();
                        bail!(AgentError::new(format!("Container failed: {message}")));
                    }
                    Ok(status_code == 0)
                }
                Ok(Some(Err(exc))) => Err(exc).context("Cannot wait for the container"),
                Ok(None) => Ok(false),
                Err(_) => bail!(AgentError::new("Diagnostic container timed out")),
            }
        })
    }

    pub fn cleanup(
        &self,
        container_name: &str,
        volume_name: &str,
        network_name: &str,
        lease_id: &str,
        node_id: &str,
    ) -> Result<()> {
        let engine = self.connect()?;
        block_on(async move {
            let container = match engine
                .inspect_container(container_name, None::<InspectContainerOptions>)
                .await
            {
                Ok(container) => Some(container),
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => None,
                Err(exc) => return Err(exc).context("Cannot inspect the container"),
            };
            if let Some(container) = container {
                check_owner(
                    container.config.as_ref().and_then(|c| c.labels.as_ref()),
                    lease_id,
                    node_id,
                )?;
                let options = RemoveContainerOptions {
                    force: true,
                    v: true,
                    ..Default::default()
                };
                engine
                    .remove_container(container_name, Some(options))
                    .await
                    .context("Cannot remove the container")?;
            }
            if engine.inspect_volume(volume_name).await.is_ok() {
                engine
                    .remove_volume(volume_name, None::<RemoveVolumeOptions>)
                    .await
                    .context("Cannot remove the volume")?;
            }
            if !network_name.is_empty() {
                let existing = match engine
                    .inspect_network(network_name, None::<InspectNetworkOptions>)
                    .await
                {
                    Ok(_) => true,
                    Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    }) => false,
                    Err(exc) => return Err(exc).context("Cannot inspect the network"),
                };
                if existing {
                    engine
                        .remove_network(network_name)
                        .await
                        .context("Cannot remove the network")?;
                }
            }
            Ok(())
        })
    }
}

pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Cannot start the async runtime")
        .block_on(future)
}

fn check_owner(
    labels: Option<&HashMap<String, String>>,
    lease_id: &str,
    node_id: &str,
) -> Result<()> {
    let labels = labels.cloned().unwrap_or_default();
    if labels.get(LEASE_LABEL) != Some(&lease_id.to_string())
        || labels.get(NODE_LABEL) != Some(&node_id.to_string())
    {
        bail!(AgentError::new(
            "Refusing to operate on a resource owned by another lease or node"
        ));
    }
    Ok(())
}
