//! リポジトリルートとリリース識別の解決。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// 現在の作業ディレクトリから上方向へ Cargo ワークスペースのルートを探す。
pub fn repo_root() -> Result<PathBuf> {
    if let Ok(root) = std::env::var("GPUPOD_REPO_ROOT") {
        return Ok(PathBuf::from(root));
    }
    let start = std::env::current_dir().context("Cannot determine the working directory")?;
    for candidate in start.ancestors() {
        if candidate.join("Cargo.toml").is_file() && candidate.join("images").is_dir() {
            return Ok(candidate.to_path_buf());
        }
    }
    bail!(
        "Cannot find the repository root; run from the gpupod checkout or set GPUPOD_REPO_ROOT"
    )
}

/// リリース識別。GPUPOD_RELEASE_JSON か release.json を優先し、なければ development。
pub fn release_identity() -> serde_json::Value {
    if let Ok(raw) = std::env::var("GPUPOD_RELEASE_JSON") {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
            return value;
        }
    }
    if let Ok(raw) = std::fs::read_to_string("release.json") {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
            return value;
        }
    }
    serde_json::json!({"version": "development", "sequence": 0})
}

/// systemd ユニット名の検証（Python 版の re.fullmatch 相当）。
pub fn valid_service_name(name: &str) -> bool {
    let Some(rest) = name.strip_suffix(".service") else {
        return false;
    };
    let Some(role) = rest.strip_prefix("gpupod-") else {
        return false;
    };
    !role.is_empty() && role.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub fn ensure_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).with_context(|| format!("Cannot create {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_names_follow_the_pattern() {
        assert!(valid_service_name("gpupod-provider.service"));
        assert!(valid_service_name("gpupod-web.service"));
        assert!(!valid_service_name("gpupod-.service"));
        assert!(!valid_service_name("docker.service"));
        assert!(!valid_service_name("gpupod-UPPER.service"));
        assert!(!valid_service_name("gpupod-provider.timer"));
    }

    #[test]
    fn release_identity_defaults_to_development() {
        std::env::remove_var("GPUPOD_RELEASE_JSON");
        let identity = release_identity();
        assert_eq!(identity["version"], "development");
        assert_eq!(identity["sequence"], 0);
    }
}
