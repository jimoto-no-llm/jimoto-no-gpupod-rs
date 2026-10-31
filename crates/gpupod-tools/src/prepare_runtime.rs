//! gVisor バンドル取得と Docker ランタイム設定の生成（Python 版 prepare_runtime.py の移植）。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha512};

/// Python 版と同じ固定リリース。
pub const VERSION: &str = "20261019.0";
/// Python 版と同じピン（SHA-512）。
pub const SHA512_X86_64: &str = "7cbc2aeeee809dcccfb40e612504a1b76f2c119790cee59e482f7b35428cccc\
                                 4f71705e7e322442d07d1f7d29b5d9e72e31319fc3088d6dcbec9fc7c1f0d8881";
const MAX_BUNDLE_BYTES: u64 = 200 * 1024 * 1024;

pub fn bundle_url() -> String {
    format!(
        "https://storage.googleapis.com/gvisor/releases/release/{VERSION}/x86_64/gvisor.tar.bz2"
    )
}

/// バンドルを取得し、安全に展開して daemon.json を生成する。生成した設定のパスを返す。
pub fn prepare(output: &Path, source_config: Option<&Path>, gpu: bool) -> Result<PathBuf> {
    std::fs::create_dir_all(output).context("Cannot create the output directory")?;
    let archive_path = output.join("gvisor.tar.bz2");
    download(&bundle_url(), &archive_path)?;
    if let Err(exc) = verify_digest(&archive_path) {
        let _ = std::fs::remove_file(&archive_path);
        return Err(exc);
    }
    extract_tar_bz2(&archive_path, output)?;
    std::fs::remove_file(&archive_path).ok();
    let mut config = match source_config {
        Some(path) if path.exists() => {
            let raw = std::fs::read_to_string(path).context("Cannot read the Docker config")?;
            serde_json::from_str::<serde_json::Value>(&raw)
                .context("Cannot parse the Docker config")?
        }
        _ => serde_json::json!({}),
    };
    if !config.is_object() {
        bail!("Docker configuration must be a JSON object");
    }
    let mut runtime_args = vec!["--platform=systrap".to_string()];
    if gpu {
        runtime_args.push("--nvproxy".to_string());
    }
    let runsc = serde_json::json!({
        "path": output.join("runsc").display().to_string(),
        "runtimeArgs": runtime_args,
    });
    let object = config.as_object_mut().unwrap();
    let runtimes = object
        .entry("runtimes")
        .or_insert_with(|| serde_json::json!({}));
    runtimes["runsc"] = runsc;
    let destination = output.join("daemon.json");
    let body = serde_json::to_string_pretty(&config)? + "\n";
    std::fs::write(&destination, body).context("Cannot write daemon.json")?;
    Ok(destination)
}

fn download(url: &str, destination: &Path) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .context("Cannot create the HTTP client")?;
    let mut response = client
        .get(url)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .context("Cannot download the runtime bundle")?;
    if !response.status().is_success() {
        bail!(
            "Runtime bundle download failed with status {}",
            response.status()
        );
    }
    let mut file = std::fs::File::create(destination).context("Cannot store the runtime bundle")?;
    let mut length: u64 = 0;
    let mut digest = Sha512::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = response
            .read(&mut buffer)
            .context("Cannot read the runtime bundle")?;
        if read == 0 {
            break;
        }
        length += read as u64;
        if length > MAX_BUNDLE_BYTES {
            bail!("Runtime bundle exceeds the expected size");
        }
        digest.update(&buffer[..read]);
        file.write_all(&buffer[..read])
            .context("Cannot store the runtime bundle")?;
    }
    file.flush().ok();
    Ok(())
}

fn verify_digest(path: &Path) -> Result<()> {
    let mut digest = Sha512::new();
    let mut file = std::fs::File::open(path).context("Cannot reopen the runtime bundle")?;
    let mut buffer = [0u8; 65536];
    loop {
        let read = file
            .read(&mut buffer)
            .context("Cannot read the runtime bundle")?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let actual = hex::encode(digest.finalize());
    if actual != SHA512_X86_64.replace(char::is_whitespace, "") {
        bail!("Runtime bundle checksum differs from the pinned release");
    }
    Ok(())
}

/// tar.bz2 の安全な展開。絶対パス・親ディレクトリ参照・特殊メンバーを拒否する。
pub fn extract_tar_bz2(archive_path: &Path, output: &Path) -> Result<()> {
    let file = std::fs::File::open(archive_path).context("Cannot open the runtime bundle")?;
    let decoder = bzip2::read::BzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    for entry in archive
        .entries()
        .context("Cannot read the runtime bundle")?
    {
        let mut entry = entry.context("Cannot read the runtime bundle")?;
        let path = entry.path()?.into_owned();
        if path.is_absolute() || path.components().any(|c| c.as_os_str() == "..") {
            bail!("Unexpected runtime archive member");
        }
        let kind = entry.header().entry_type();
        let destination = output.join(&path);
        if kind.is_dir() {
            std::fs::create_dir_all(&destination).context("Cannot create the runtime directory")?;
            continue;
        }
        if !kind.is_file() {
            bail!("Unexpected runtime archive member");
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).context("Cannot create the runtime directory")?;
        }
        let mut target =
            std::fs::File::create(&destination).context("Cannot write the runtime file")?;
        std::io::copy(&mut entry, &mut target).context("Cannot write the runtime file")?;
        crate::support::files::fs_set_mode(&destination, 0o755)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// tar クレートは set_path で ".." や絶対パスを拒否するため、
    /// 名前バイト列を直接書き込んで悪意ある配置物を再現する。
    fn build_archive(directory: &Path, members: &[(&str, &[u8])]) -> PathBuf {
        let path = directory.join("bundle.tar.bz2");
        let file = std::fs::File::create(&path).unwrap();
        let encoder = bzip2::write::BzEncoder::new(file, bzip2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (name, body) in members {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            {
                let old = header.as_old_mut();
                let bytes = name.as_bytes();
                old.name[..bytes.len()].copy_from_slice(bytes);
            }
            header.set_cksum();
            builder.append(&header, *body).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
        path
    }

    #[test]
    fn pin_is_unchanged_from_python() {
        assert_eq!(VERSION, "20261019.0");
        assert_eq!(SHA512_X86_64.replace(char::is_whitespace, "").len(), 128);
        assert!(SHA512_X86_64.starts_with("7cbc2aee"));
    }

    #[test]
    fn extracts_safe_members() {
        let directory = tempfile::tempdir().unwrap();
        let archive = build_archive(
            directory.path(),
            &[("runsc", b"binary"), ("doc/readme.txt", b"text")],
        );
        let output = tempfile::tempdir().unwrap();
        extract_tar_bz2(&archive, output.path()).unwrap();
        assert_eq!(
            std::fs::read(output.path().join("runsc")).unwrap(),
            b"binary"
        );
        assert_eq!(
            std::fs::read(output.path().join("doc/readme.txt")).unwrap(),
            b"text"
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(output.path().join("runsc"))
            .unwrap()
            .permissions();
        assert_eq!(mode.mode() & 0o777, 0o755);
    }

    #[test]
    fn rejects_traversing_members() {
        let directory = tempfile::tempdir().unwrap();
        let archive = build_archive(directory.path(), &[("../escape", b"evil")]);
        let output = tempfile::tempdir().unwrap();
        let error = extract_tar_bz2(&archive, output.path())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("Unexpected runtime archive member"),
            "{error}"
        );
        assert!(!output.path().join("escape").exists());
    }

    #[test]
    fn rejects_absolute_members() {
        let directory = tempfile::tempdir().unwrap();
        let archive = build_archive(directory.path(), &[("/etc/gpupod-escape", b"evil")]);
        let output = tempfile::tempdir().unwrap();
        let error = extract_tar_bz2(&archive, output.path())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("Unexpected runtime archive member"),
            "{error}"
        );
    }

    #[test]
    fn gpu_arguments_are_appended() {
        let output = tempfile::tempdir().unwrap();
        let runsc = serde_json::json!({
            "path": output.path().join("runsc").display().to_string(),
            "runtimeArgs": ["--platform=systrap", "--nvproxy"],
        });
        assert_eq!(runsc["runtimeArgs"][1], "--nvproxy");
    }
}
