//! Docker なしで動くモックエンジン。

use std::sync::Mutex;

use gpupod_agent::docker::{DockerEngine, DockerError};
use gpupod_agent::Lease;
use gpupod_common::{Template, GPU};
use serde_json::{json, Value};

/// 成功経路を既定とし、失敗・停止を注入できるモック。
pub struct MockEngine {
    /// describe が Running を返すか。
    pub running: bool,
    /// create を失敗させるメッセージ。
    pub fail_create: Option<String>,
    /// create の不確実失敗にするか。
    pub fail_uncertain: bool,
    /// 作成した貸出の記録。
    pub created: Mutex<Vec<String>>,
    /// cleanup の呼び出し回数。
    pub cleaned: Mutex<usize>,
}

impl Default for MockEngine {
    fn default() -> Self {
        Self {
            running: true,
            fail_create: None,
            fail_uncertain: false,
            created: Mutex::new(Vec::new()),
            cleaned: Mutex::new(0),
        }
    }
}

impl MockEngine {
    #[allow(dead_code)]
    pub fn failing(message: &str, uncertain: bool) -> Self {
        Self {
            fail_create: Some(message.to_owned()),
            fail_uncertain: uncertain,
            ..Default::default()
        }
    }

    #[allow(dead_code)]
    pub fn created_count(&self) -> usize {
        self.created.lock().unwrap().len()
    }

    #[allow(dead_code)]
    pub fn cleaned_count(&self) -> usize {
        *self.cleaned.lock().unwrap()
    }
}

impl DockerEngine for MockEngine {
    fn create(
        &self,
        lease: &Lease,
        _template: &Template,
        _gpus: &[GPU],
        _node_id: &str,
    ) -> Result<(), DockerError> {
        if let Some(message) = &self.fail_create {
            return Err(DockerError::with_uncertain(
                message.clone(),
                self.fail_uncertain,
            ));
        }
        self.created.lock().unwrap().push(lease.id.clone());
        Ok(())
    }

    fn start(&self, _lease: &Lease) -> Result<(), DockerError> {
        Ok(())
    }

    fn describe(&self, _lease: &Lease, _node_id: &str) -> Result<Option<Value>, DockerError> {
        if self.running {
            Ok(Some(json!({
                "State": { "Running": true },
                "NetworkSettings": {
                    "Ports": { "22/tcp": [{ "HostIp": "127.0.0.1", "HostPort": "32768" }] }
                }
            })))
        } else {
            Ok(Some(json!({ "State": { "Running": false } })))
        }
    }

    fn cleanup(&self, _lease: &Lease, _node_id: &str) -> Result<(), DockerError> {
        *self.cleaned.lock().unwrap() += 1;
        Ok(())
    }

    fn statistics(&self, leases: &[Lease], _node_id: &str) -> Result<Vec<Value>, DockerError> {
        Ok(leases
            .iter()
            .filter(|lease| lease.status == "running")
            .map(|lease| json!({ "lease_id": lease.id, "docker": { "stats": "mock" } }))
            .collect())
    }

    fn container_ids(&self, leases: &[Lease], _node_id: &str) -> Result<Vec<String>, DockerError> {
        Ok(leases
            .iter()
            .filter(|lease| lease.status == "running")
            .map(|lease| format!("container-{}", lease.id))
            .collect())
    }
}
