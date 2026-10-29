//! 読み取り専用の提供 PC 診断（Python 版 doctor.py の移植）。

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::json;

use crate::support::{devices, docker::DockerEngine, telemetry};

pub struct Options {
    pub devices: Option<PathBuf>,
    pub docker_socket: PathBuf,
    pub disk_path: PathBuf,
    pub mock_gpu: bool,
}

/// 診断結果を返す。終了コードは Docker エラーがあれば 1。
pub fn run(options: &Options) -> Result<(serde_json::Value, i32)> {
    let gpus = if options.mock_gpu {
        devices::mock_gpus()
    } else {
        devices::load_gpus(options.devices.as_deref())?
    };
    let mut result = json!({
        "mock_gpu": options.mock_gpu,
        "gpus": gpus.iter().map(serde_json::to_value).collect::<Result<Vec<_>, _>>()?,
        "metrics": telemetry::collect(&gpus, &options.disk_path),
        "devices": [],
    });
    let mut device_checks = Vec::new();
    if !options.mock_gpu {
        for gpu in &gpus {
            for path in &gpu.devices {
                device_checks.push(json!({
                    "path": path,
                    "exists": Path::new(path).exists(),
                    "read_write": read_write(Path::new(path)),
                }));
            }
        }
    }
    result["devices"] = serde_json::Value::Array(device_checks);
    let engine = DockerEngine::new(&options.docker_socket);
    let code = match engine.info() {
        Ok(info) => {
            result["docker"] = info;
            0
        }
        Err(exc) => {
            result["docker_error"] = json!(format!("{exc:#}"));
            1
        }
    };
    Ok((result, code))
}

fn read_write(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.permissions().mode() & 0o600 == 0o600,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_inventory_reports_without_docker() {
        let options = Options {
            devices: None,
            docker_socket: PathBuf::from("/nonexistent/docker.sock"),
            disk_path: PathBuf::from("/"),
            mock_gpu: true,
        };
        let (report, code) = run(&options).unwrap();
        assert_eq!(report["mock_gpu"], true);
        assert_eq!(report["gpus"].as_array().unwrap().len(), 3);
        assert_eq!(report["devices"].as_array().unwrap().len(), 0);
        assert!(report.get("docker_error").is_some());
        assert_eq!(code, 1);
        assert!(report["metrics"]["host"]["cpu_count"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn real_inventory_lists_device_paths() {
        let options = Options {
            devices: None,
            docker_socket: PathBuf::from("/nonexistent/docker.sock"),
            disk_path: PathBuf::from("/"),
            mock_gpu: false,
        };
        let (report, _) = run(&options).unwrap();
        assert_eq!(report["mock_gpu"], false);
        assert!(report["devices"].is_array());
    }
}
