//! ローカル画像ビルド（Python 版 build_images.py の移植）。

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

/// ビルドする PyTorch バリアント。 indexes は Python 版と同一。
pub const TORCH_INDEXES: [(&str, &str); 4] = [
    ("cpu", "cpu"),
    ("cuda", "cu130"),
    ("rocm", "rocm7.2"),
    ("xpu", "xpu"),
];

pub const UBUNTU_VERSIONS: [&str; 2] = ["24.04", "26.04"];

/// 指定したバリアントの画像を順にビルドする。ホストへのパッケージ導入は行わない。
pub fn run(pytorch: &[String], root: &Path) -> Result<()> {
    // 先に検証し、途中で打ち切られないようにする。
    for variant in pytorch {
        if !TORCH_INDEXES.iter().any(|(name, _)| name == variant) {
            bail!("Unknown PyTorch variant: {variant}");
        }
    }
    for version in UBUNTU_VERSIONS {
        docker_build(
            root,
            "images/ubuntu/Dockerfile",
            "UBUNTU_VERSION",
            version,
            &format!("gpupod/ubuntu:{version}"),
        )?;
    }
    for variant in pytorch {
        let Some((_, index)) = TORCH_INDEXES.iter().find(|(name, _)| name == variant) else {
            bail!("Unknown PyTorch variant: {variant}");
        };
        docker_build(
            root,
            "images/pytorch/Dockerfile",
            "TORCH_INDEX",
            index,
            &format!("gpupod/pytorch:2.14.1-{variant}"),
        )?;
    }
    Ok(())
}

fn docker_build(root: &Path, dockerfile: &str, key: &str, value: &str, tag: &str) -> Result<()> {
    let status = Command::new("docker")
        .arg("build")
        .arg("-f")
        .arg(dockerfile)
        .arg("--build-arg")
        .arg(format!("{key}={value}"))
        .arg("-t")
        .arg(tag)
        .arg(".")
        .current_dir(root)
        .status()
        .context("Cannot run docker build")?;
    if !status.success() {
        bail!("docker build failed for {tag}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_indexes_match_the_python_table() {
        assert_eq!(TORCH_INDEXES[0], ("cpu", "cpu"));
        assert_eq!(TORCH_INDEXES[1], ("cuda", "cu130"));
        assert_eq!(TORCH_INDEXES[2], ("rocm", "rocm7.2"));
        assert_eq!(TORCH_INDEXES[3], ("xpu", "xpu"));
    }

    #[test]
    fn unknown_variants_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        // docker が無くてもバリデーションが先に失敗することを確認する。
        let error = run(&["bogus".to_string()], root.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("Unknown PyTorch variant"), "{error}");
    }
}
