//! 承認済みテンプレート画像の取得（Python 版 prepare_template.py の移植）。

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use gpupod_common::{load_templates, AgentError};

use crate::support::docker::DockerEngine;

/// テンプレート ID の画像を Docker ホストへ pull し、画像 ID を返す。
pub fn run(template_id: &str, templates_path: &Path, docker_socket: &Path) -> Result<String> {
    let templates = load_templates(templates_path)?;
    let Some(template) = templates.get(template_id) else {
        bail!(AgentError::new("Unknown template ID"));
    };
    let engine = DockerEngine::new(docker_socket);
    engine.pull_image(&template.image)
}

/// 既定のカタログパス（リポジトリの examples/templates.json）。
pub fn default_catalog(root: Option<&Path>) -> PathBuf {
    match root {
        Some(root) => root.join("examples/templates.json"),
        None => crate::support::repo::repo_root()
            .map(|root| root.join("examples/templates.json"))
            .unwrap_or_else(|_| PathBuf::from("examples/templates.json")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_template_is_rejected_before_docker() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = directory.path().join("templates.json");
        std::fs::write(
            &catalog,
            "[{\"id\": \"ubuntu-24.04\", \"image\": \"gpupod/ubuntu:24.04\", \"vendors\": [\"nvidia\"]}]",
        )
        .unwrap();
        let error = run("missing", &catalog, Path::new("/nonexistent/docker.sock"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("Unknown template ID"), "{error}");
    }

    #[test]
    fn known_template_reaches_the_docker_step() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = directory.path().join("templates.json");
        std::fs::write(
            &catalog,
            "[{\"id\": \"ubuntu-24.04\", \"image\": \"gpupod/ubuntu:24.04\", \"vendors\": [\"nvidia\"]}]",
        )
        .unwrap();
        // ソケットが無ければ Docker 接続エラーになる（テンプレート検証は通る）。
        let error = run("ubuntu-24.04", &catalog, Path::new("/nonexistent/docker.sock"))
            .unwrap_err()
            .to_string();
        assert!(!error.contains("Unknown template ID"), "{error}");
    }
}
