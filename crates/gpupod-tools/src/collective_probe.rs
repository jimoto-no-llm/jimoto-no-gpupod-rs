//! プローブコマンドの生成（collective_probe.py はコンテナ内の Python スクリプトのまま）。
//!
//! 実 Torch 実行はコンテナ内で行う。ここでは呼び出し仕様を検証して返す。

use anyhow::{bail, Result};
use serde_json::json;

pub const BACKENDS: [&str; 3] = ["nccl", "xccl", "gloo"];

pub fn valid_backend(backend: &str) -> bool {
    BACKENDS.contains(&backend)
}

pub fn valid_workers(workers: i64) -> bool {
    (1..=32).contains(&workers)
}

/// コンテナ内で実行する probe 呼び出し（Python 版 collective_probe.py を実行する）。
pub fn probe_command(backend: &str, workers: i64) -> Vec<String> {
    vec![
        "/opt/venv/bin/python".to_string(),
        "/usr/local/bin/collective-probe.py".to_string(),
        "--backend".to_string(),
        backend.to_string(),
        "--workers".to_string(),
        workers.to_string(),
    ]
}

/// 検証済みの呼び出し仕様を JSON で返す。
pub fn run(backend: &str, workers: i64) -> Result<serde_json::Value> {
    if !valid_backend(backend) {
        bail!("backend must be nccl, xccl or gloo");
    }
    if !valid_workers(workers) {
        bail!("workers must be between 1 and 32");
    }
    Ok(json!({
        "result": "command",
        "backend": backend,
        "workers": workers,
        "command": probe_command(backend, workers),
        "note": "Torch execution happens inside the container; the host only prepares the invocation.",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_matches_the_container_script() {
        let command = probe_command("gloo", 2);
        assert_eq!(command[1], "/usr/local/bin/collective-probe.py");
        assert_eq!(command[3], "gloo");
        assert_eq!(command[5], "2");
    }

    #[test]
    fn backend_and_workers_are_validated() {
        assert!(run("nccl", 4).is_ok());
        assert!(run("gloo", 1).is_ok());
        assert!(run("bogus", 2).is_err());
        assert!(run("gloo", 0).is_err());
        assert!(run("gloo", 33).is_err());
    }
}
