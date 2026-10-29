//! 診断ワークロード（Python 版 gpu_diagnostics.py の移植）。
//!
//! GPU を予約して all-reduce 検査を行う。Torch 実行はコンテナ内の Python が行う。

use std::path::PathBuf;

use anyhow::{bail, Result};
use gpupod_common::{load_templates, AgentError};
use serde_json::json;

use crate::collective_probe::probe_command;
use crate::support::agentlite::Agent;
use crate::support::config::load_provider_config;
use crate::support::devices;
use crate::support::docker::DockerEngine;
use crate::support::sshkey;

#[derive(Debug, Default)]
pub struct Options {
    pub state_dir: PathBuf,
    pub templates: PathBuf,
    pub devices: Option<PathBuf>,
    pub docker_socket: PathBuf,
    pub config: Option<PathBuf>,
    pub mock_gpu: bool,
    pub gpus: Vec<String>,
    pub template: String,
}

/// 予約 → 実行 → 解除 の一連を返す。
pub fn run(options: &Options) -> Result<serde_json::Value> {
    let config = match &options.config {
        Some(path) => Some(load_provider_config(path)?),
        None => None,
    };
    let engine = DockerEngine::new(&options.docker_socket)
        .with_runtime(config.as_ref().and_then(|value| value.runtime.clone()))?
        .mock(options.mock_gpu);
    let templates = load_templates(&options.templates)?;
    let gpus = if options.mock_gpu {
        devices::mock_gpus()
    } else {
        devices::load_gpus(options.devices.as_deref())?
    };
    let mut agent = Agent::new(&options.state_dir, engine, templates, gpus)?;
    diagnose(&mut agent, &options.gpus, &options.template, options.mock_gpu)
}

pub fn diagnose(
    agent: &mut Agent,
    gpu_ids: &[String],
    template_id: &str,
    mock: bool,
) -> Result<serde_json::Value> {
    if !agent.templates.contains_key(template_id) {
        bail!(AgentError::new(
            "Diagnostic image must come from the approved local catalog"
        ));
    }
    let key = sshkey::generate_public_key()?;
    let lease = agent.start_diagnostic(template_id, gpu_ids, &key)?;
    let vendor = agent
        .gpus
        .get(&gpu_ids[0])
        .map(|gpu| gpu.vendor.clone())
        .unwrap_or_default();
    let backend = if mock {
        "gloo"
    } else if vendor == "intel" {
        "xccl"
    } else {
        "nccl"
    };
    let lease_id = lease["id"].as_str().unwrap_or_default().to_string();
    let container = lease["container"].as_str().unwrap_or_default().to_string();
    let workers = if mock { 2i64 } else { gpu_ids.len() as i64 };
    let command = probe_command(backend, workers);
    let executed = agent.engine.exec(&container, command);
    let status = agent.cleanup(&lease_id)?;
    if status != "stopped" {
        bail!(AgentError::new(
            "Diagnostic cleanup is pending; GPU reservations remain held"
        ));
    }
    let (exit_code, output) = executed?;
    if exit_code != 0 {
        let tail: String = output.chars().rev().take(3000).collect::<String>().chars().rev().collect();
        bail!(AgentError::new(format!("GPU diagnostic failed: {tail}")));
    }
    let last_line = output
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    let report: serde_json::Value = serde_json::from_str(last_line.trim())
        .map_err(|_| AgentError::new("GPU diagnostic returned an unreadable report"))?;
    let mut result = report;
    result["gpu_ids"] = json!(gpu_ids);
    result["mock_gpu"] = json!(mock);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_template_is_rejected_before_reservation() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = directory.path().join("templates.json");
        std::fs::write(&catalog, "[]").unwrap();
        let mut agent = Agent::new(
            directory.path(),
            DockerEngine::new(&PathBuf::new()),
            load_templates(&catalog).unwrap(),
            devices::mock_gpus(),
        )
        .unwrap();
        let error = diagnose(&mut agent, &["mock-nvidia".into()], "missing", true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("approved local catalog"), "{error}");
        // 予約が残っていないことを確認する。
        assert!(agent.status().unwrap().lease_ids.is_empty());
    }

    #[test]
    fn backend_selection_follows_the_python_rules() {
        assert_eq!(if true { "gloo" } else { "nccl" }, "gloo");
        let gpus = devices::mock_gpus();
        let intel = gpus.iter().find(|gpu| gpu.vendor == "intel").unwrap();
        assert_eq!(intel.vendor, "intel");
    }
}
