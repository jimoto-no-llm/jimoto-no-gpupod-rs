//! 更新ワーカー（Python 版 update_agent.py / updates.py の移植・TUF 風版）。
//!
//! desired-release.json を読み、署名検証後に配置を切り替える。
//! ヘルス確認に失敗した場合は旧版へ戻す。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use gpupod_common::AgentError;
use serde_json::{json, Value};

use crate::support::config::{checked_url, UpdaterConfig};
use crate::support::files::{atomic_bytes, HeldLock};
use crate::release_repository::{SOURCE_DIRS, TOP_FILES};
use crate::support::tuf;

const MAX_RELEASE_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Request {
    pub target: String,
    pub version: String,
    pub sequence: i64,
}

/// desired-release.json を読んで返す。無ければ None。
pub fn load_request(path: &Path) -> Result<Option<Request>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path).context("Cannot read the release request")?;
    let value: Value = serde_json::from_str(&raw).context("Cannot parse the release request")?;
    if value.is_null() {
        return Ok(None);
    }
    let request: Request = serde_json::from_value(value).context("Invalid release request")?;
    Ok(Some(request))
}

/// 更新本体。restart は systemctl 相当（テスト差し替え可能）。
pub fn install<F>(config: &UpdaterConfig, request: &Request, restart: F) -> Result<Value>
where
    F: Fn(&str) -> Result<()>,
{
    if request.sequence < 1 {
        bail!(AgentError::new("Invalid release sequence"));
    }
    let root = PathBuf::from(&config.install_root);
    std::fs::create_dir_all(root.join("releases")).context("Cannot create the install root")?;
    let _lock = HeldLock::try_exclusive(&root.join("update.lock"))?
        .ok_or_else(|| AgentError::new("Another provider operation is still running"))?;
    let state_path = root.join("update-state.json");
    let mut state = read_state(&state_path);
    state = recover_pending(&state, &state_path, &root, &restart)?;
    if request.sequence <= state["highest_sequence"].as_i64().unwrap_or(0) {
        return Ok(json!({"status": "unchanged", "sequence": request.sequence}));
    }
    let current = root.join("current");
    if !current.is_symlink() {
        bail!(AgentError::new(
            "Bootstrap an existing installation with an install_root/current symlink first"
        ));
    }
    let previous = std::fs::canonicalize(&current).context("Cannot resolve the current release")?;
    let archive = fetch_release(config, request)?;
    let directory = root.join("releases").join(format!("{:08}", request.sequence));
    if directory.exists() {
        std::fs::remove_dir_all(&directory).ok();
    }
    std::fs::create_dir_all(&directory).context("Cannot create the release directory")?;
    if let Err(exc) = unpack(&archive, &directory).and_then(|_| check_identity(&directory, request)) {
        std::fs::remove_dir_all(&directory).ok();
        return Err(exc);
    }
    runtime_permissions(&directory)?;
    let old_boot = read_boot_id(&PathBuf::from(&config.provider_state));
    state["highest_sequence"] = json!(request.sequence);
    state["pending"] = json!(true);
    state["previous"] = json!(previous.display().to_string());
    state["candidate"] = json!(directory.display().to_string());
    state["status"] = json!("activating");
    atomic_json(&state_path, &state)?;
    match switch_link(&root, &directory).and_then(|_| restart(&config.service)) {
        Ok(()) => {}
        Err(exc) => {
            rollback(&root, &previous, &restart, &state_path, &mut state, exc)?;
            bail!(AgentError::new("New provider version did not become healthy"));
        }
    }
    if !health(config, request, old_boot.as_deref()) {
        rollback(
            &root,
            &previous,
            &restart,
            &state_path,
            &mut state,
            anyhow::anyhow!("New provider version did not become healthy"),
        )?;
        bail!(AgentError::new("New provider version did not become healthy"));
    }
    state["pending"] = json!(false);
    state["status"] = json!("active");
    state["version"] = json!(request.version);
    state["error"] = Value::Null;
    atomic_json(&state_path, &state)?;
    Ok(json!({"status": "active", "sequence": request.sequence, "version": request.version}))
}

fn rollback(
    root: &Path,
    previous: &Path,
    restart: &impl Fn(&str) -> Result<()>,
    state_path: &Path,
    state: &mut Value,
    error: anyhow::Error,
) -> Result<()> {
    switch_link(root, previous)?;
    let _ = restart("");
    state["pending"] = json!(false);
    state["status"] = json!("rolled_back");
    state["error"] = json!(error.to_string());
    atomic_json(state_path, state)?;
    Ok(())
}

fn recover_pending(
    state: &Value,
    state_path: &Path,
    root: &Path,
    restart: &impl Fn(&str) -> Result<()>,
) -> Result<Value> {
    let mut state = state.clone();
    if state.get("pending").and_then(|v| v.as_bool()) != Some(true) {
        return Ok(state);
    }
    let previous = PathBuf::from(state["previous"].as_str().unwrap_or_default());
    switch_link(root, &previous)?;
    let _ = restart("");
    state["pending"] = json!(false);
    state["status"] = json!("rolled_back");
    state["error"] = json!("Interrupted activation was rolled back");
    atomic_json(state_path, &state)?;
    Ok(state)
}

fn read_state(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or(json!({"highest_sequence": 0}))
}

fn atomic_json(path: &Path, value: &Value) -> Result<()> {
    crate::support::files::atomic_json(path, value)
}

/// リポジトリから署名検証済みアーカイブを取得する。
pub fn fetch_release(config: &UpdaterConfig, request: &Request) -> Result<PathBuf> {
    let base = checked_url(&config.repository_url)?;
    let root = PathBuf::from(&config.install_root);
    let metadata = root.join("tuf-metadata");
    let downloads = root.join("downloads");
    std::fs::create_dir_all(&metadata).context("Cannot create the metadata cache")?;
    std::fs::create_dir_all(&downloads).context("Cannot create the download cache")?;
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .context("Cannot create the HTTP client")?;
    let trusted = tuf::read_document(Path::new(&config.trusted_root))?;
    let trust = tuf::root_trust(&trusted)?;
    let timestamp = fetch_metadata(&client, &base, "timestamp.json", &metadata)?;
    let timestamp_signed = tuf::verify(&timestamp, "timestamp", &trust)?;
    let snapshot_bytes = fetch_bytes(
        &client,
        &base,
        "snapshot.json",
        &metadata,
        &timestamp_signed["snapshot_meta"],
    )?;
    let snapshot: Value = serde_json::from_slice(&snapshot_bytes).context("Invalid snapshot metadata")?;
    let snapshot_signed = tuf::verify(&snapshot, "snapshot", &trust)?;
    let targets_bytes = fetch_bytes(
        &client,
        &base,
        "targets.json",
        &metadata,
        &snapshot_signed["meta"]["targets.json"],
    )?;
    let targets: Value = serde_json::from_slice(&targets_bytes).context("Invalid targets metadata")?;
    let targets_signed = tuf::verify(&targets, "targets", &trust)?;
    let Some(entry) = targets_signed["targets"].get(&request.target) else {
        bail!(AgentError::new("Assigned update does not match signed release metadata"));
    };
    let custom_matches = entry.get("custom").and_then(|c| c.get("version")).and_then(|v| v.as_str())
        == Some(request.version.as_str())
        && entry.get("custom").and_then(|c| c.get("sequence")).and_then(|v| v.as_i64())
            == Some(request.sequence);
    if !custom_matches {
        bail!(AgentError::new(
            "Assigned update does not match signed release metadata"
        ));
    }
    let body = fetch_target(&client, &base, &request.target)?;
    let expected_length = entry.get("length").and_then(|v| v.as_u64());
    let expected_hash = entry
        .get("hashes")
        .and_then(|h| h.get("sha256"))
        .and_then(|v| v.as_str());
    if expected_length != Some(body.len() as u64)
        || expected_hash != Some(tuf::sha256_hex(&body).as_str())
    {
        bail!(AgentError::new("Downloaded release differs from signed metadata"));
    }
    let destination = downloads.join(request.target.replace('/', "_"));
    atomic_bytes(&destination, &body, 0o600)?;
    Ok(destination)
}

fn fetch_metadata(
    client: &reqwest::blocking::Client,
    base: &str,
    name: &str,
    cache: &Path,
) -> Result<Value> {
    let bytes = raw_fetch(client, base, &format!("metadata/{name}"))?;
    atomic_bytes(&cache.join(name), &bytes, 0o600)?;
    serde_json::from_slice(&bytes).with_context(|| format!("Invalid {name} metadata"))
}

#[allow(clippy::too_many_arguments)]
fn fetch_bytes(
    client: &reqwest::blocking::Client,
    base: &str,
    name: &str,
    cache: &Path,
    expected: &Value,
) -> Result<Vec<u8>> {
    let bytes = raw_fetch(client, base, &format!("metadata/{name}"))?;
    let length = expected.get("length").and_then(|v| v.as_u64());
    let hash = expected.get("hashes").and_then(|h| h.get("sha256")).and_then(|v| v.as_str());
    if length != Some(bytes.len() as u64) || hash != Some(tuf::sha256_hex(&bytes).as_str()) {
        bail!(AgentError::new("Metadata differs from its signed description"));
    }
    atomic_bytes(&cache.join(name), &bytes, 0o600)?;
    Ok(bytes)
}

fn fetch_target(client: &reqwest::blocking::Client, base: &str, name: &str) -> Result<Vec<u8>> {
    raw_fetch(client, base, &format!("targets/{name}"))
}

fn raw_fetch(client: &reqwest::blocking::Client, base: &str, path: &str) -> Result<Vec<u8>> {
    let url = format!("{base}/{path}");
    let response = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .with_context(|| format!("Cannot download {url}"))?;
    if !response.status().is_success() {
        bail!(AgentError::new(format!("Cannot download {url}: HTTP {}", response.status())));
    }
    Ok(response.bytes().with_context(|| format!("Cannot read {url}"))?.to_vec())
}

/// アーカイブの安全な展開（許可リスト・サイズ上限つき）。
pub fn unpack(archive_path: &Path, destination: &Path) -> Result<()> {
    let file = std::fs::File::open(archive_path).context("Cannot open the release archive")?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let mut total: u64 = 0;
    for entry in archive.entries().context("Cannot read the release archive")? {
        let mut entry = entry.context("Cannot read the release archive")?;
        let path = entry.path()?.into_owned();
        let name = path.to_string_lossy().to_string();
        let head = path
            .components()
            .next()
            .map(|component| component.as_os_str().to_string_lossy().to_string())
            .unwrap_or_default();
        let allowed = SOURCE_DIRS.contains(&head.as_str())
            || TOP_FILES.contains(&name.as_str())
            || name == "release.json";
        if !entry.header().entry_type().is_file()
            || path.is_absolute()
            || name.contains('\\')
            || path.components().any(|c| c.as_os_str() == "..")
            || !allowed
        {
            bail!(AgentError::new("Release contains an unsafe archive member"));
        }
        total += entry.header().size().unwrap_or(0);
        if total > MAX_RELEASE_BYTES {
            bail!(AgentError::new("Release source exceeds the allowed size"));
        }
        let target = destination.join(&path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).context("Cannot create the release directory")?;
        }
        let mut output = std::fs::File::create(&target).context("Cannot write the release file")?;
        std::io::copy(&mut entry, &mut output).context("Cannot write the release file")?;
        let mode = if path.extension().is_some_and(|ext| ext == "sh") { 0o755 } else { 0o644 };
        crate::support::files::fs_set_mode(&target, mode)?;
    }
    Ok(())
}

fn check_identity(directory: &Path, request: &Request) -> Result<()> {
    let raw = std::fs::read_to_string(directory.join("release.json"))
        .context("Release is missing its identity")?;
    let identity: Value = serde_json::from_str(&raw).context("Release identity is invalid")?;
    if identity != json!({"version": request.version, "sequence": request.sequence}) {
        bail!(AgentError::new("Release identity differs from the signed assignment"));
    }
    Ok(())
}

fn runtime_permissions(destination: &Path) -> Result<()> {
    crate::support::files::fs_set_mode(&destination.parent().unwrap_or(destination).join("releases"), 0o755)?;
    let mut stack = vec![destination.to_path_buf()];
    while let Some(directory) = stack.pop() {
        crate::support::files::fs_set_mode(&directory, 0o755)?;
        let Ok(entries) = std::fs::read_dir(&directory) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(path);
                continue;
            }
            if let Ok(metadata) = std::fs::metadata(&path) {
                use std::os::unix::fs::PermissionsExt;
                let executable = metadata.permissions().mode() & 0o111 != 0;
                crate::support::files::fs_set_mode(&path, if executable { 0o755 } else { 0o644 })?;
            }
        }
    }
    Ok(())
}

/// 現在のリンクを原子的に入り替える。
pub fn switch_link(root: &Path, target: &Path) -> Result<()> {
    let next = root.join(".current-next");
    let _ = std::fs::remove_file(&next);
    symlink(target, &next)?;
    std::fs::rename(&next, root.join("current")).context("Cannot activate the new release")?;
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link).context("Cannot create the release link")
}

fn read_boot_id(state_dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(state_dir.join("health.json")).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value.get("boot_id").and_then(|v| v.as_str()).map(str::to_string)
}

/// ヘルス確認（Python 版 _health 相当）。
pub fn health(config: &UpdaterConfig, request: &Request, old_boot: Option<&str>) -> bool {
    let path = PathBuf::from(&config.provider_state).join("health.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(config.health_timeout);
    while std::time::Instant::now() < deadline {
        if health_matches(&path, request, old_boot, chrono::Utc::now().timestamp()) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    false
}

/// 1 回分のヘルス判定（テスト対象）。
pub fn health_matches(path: &Path, request: &Request, old_boot: Option<&str>, now: i64) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else { return false };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else { return false };
    if value.get("healthy").and_then(|v| v.as_bool()) != Some(true) {
        return false;
    }
    let boot = value.get("boot_id").and_then(|v| v.as_str());
    if boot == old_boot || boot.is_none() {
        return false;
    }
    if value.get("sequence").and_then(|v| v.as_i64()) != Some(request.sequence)
        || value.get("version").and_then(|v| v.as_str()) != Some(request.version.as_str())
    {
        return false;
    }
    match value.get("updated_at").and_then(|v| v.as_i64()) {
        Some(updated) => now - updated < 15,
        None => false,
    }
}

/// CLI エントリ。desired-release.json が無ければ何もしない。
pub fn run(config_path: &Path, request_path: Option<&Path>) -> Result<Option<Value>> {
    let config = crate::support::config::load_updater_config(config_path)?;
    let request_path = match request_path {
        Some(path) => path.to_path_buf(),
        None => PathBuf::from(&config.provider_state).join("desired-release.json"),
    };
    let Some(request) = load_request(&request_path)? else {
        return Ok(None);
    };
    let service = config.service.clone();
    let result = install(&config, &request, |name| {
        let name = if name.is_empty() { service.as_str() } else { name };
        if !crate::support::repo::valid_service_name(name) {
            bail!(AgentError::new("Invalid provider service name"));
        }
        let status = std::process::Command::new("systemctl")
            .arg("restart")
            .arg(name)
            .status()
            .context("Cannot restart the provider service")?;
        if !status.success() {
            bail!(AgentError::new("Provider service restart failed"));
        }
        Ok(())
    })?;
    Ok(Some(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Request {
        Request { target: "provider/00000001-1.0.0.tar.gz".into(), version: "1.0.0".into(), sequence: 1 }
    }

    /// 悪意ある配置物を再現するため、名前バイト列を直接書き込む。
    fn build_release_archive(directory: &Path, name: &str, body: &[u8]) -> PathBuf {
        let archive = directory.join("release.tar.gz");
        let file = std::fs::File::create(&archive).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
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
        builder.append(&header, body).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        archive
    }

    #[test]
    fn unpack_accepts_allowlisted_paths_only() {
        let scratch = tempfile::tempdir().unwrap();
        let archive = build_release_archive(
            scratch.path(),
            "crates/demo/src/main.rs",
            b"fn main() {}\n",
        );
        let destination = tempfile::tempdir().unwrap();
        unpack(&archive, destination.path()).unwrap();
        assert!(destination.path().join("crates/demo/src/main.rs").exists());
    }

    #[test]
    fn unpack_rejects_unlisted_roots() {
        let scratch = tempfile::tempdir().unwrap();
        let archive = build_release_archive(scratch.path(), "secrets/key.pem", b"abc");
        let destination = tempfile::tempdir().unwrap();
        assert!(unpack(&archive, destination.path()).is_err());
        assert!(!destination.path().join("secrets").exists());
    }

    #[test]
    fn unpack_rejects_parent_traversal() {
        let scratch = tempfile::tempdir().unwrap();
        let archive = build_release_archive(scratch.path(), "crates/../../escape", b"abc");
        let destination = tempfile::tempdir().unwrap();
        assert!(unpack(&archive, destination.path()).is_err());
    }

    #[test]
    fn switch_link_replaces_the_current_symlink() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("releases/1");
        let second = root.path().join("releases/2");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        switch_link(root.path(), &first).unwrap();
        assert_eq!(std::fs::canonicalize(root.path().join("current")).unwrap(), first);
        switch_link(root.path(), &second).unwrap();
        assert_eq!(std::fs::canonicalize(root.path().join("current")).unwrap(), second);
    }

    #[test]
    fn health_requires_a_fresh_boot_and_matching_release() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("health.json");
        let now = chrono::Utc::now().timestamp();
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "healthy": true, "boot_id": "new-boot", "sequence": 1, "version": "1.0.0",
                "updated_at": now - 5
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(health_matches(&path, &request(), Some("old-boot"), now));
        assert!(!health_matches(&path, &request(), Some("new-boot"), now));
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "healthy": true, "boot_id": "new-boot", "sequence": 2, "version": "1.0.0",
                "updated_at": now
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(!health_matches(&path, &request(), Some("old-boot"), now));
        std::fs::write(
            &path,
            serde_json::to_string(&json!({
                "healthy": true, "boot_id": "new-boot", "sequence": 1, "version": "1.0.0",
                "updated_at": now - 60
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(!health_matches(&path, &request(), Some("old-boot"), now));
    }

    #[test]
    fn missing_request_file_is_a_no_op() {
        let directory = tempfile::tempdir().unwrap();
        assert!(load_request(&directory.path().join("desired-release.json")).unwrap().is_none());
        let path = directory.path().join("desired-release.json");
        std::fs::write(&path, "null").unwrap();
        assert!(load_request(&path).unwrap().is_none());
        std::fs::write(&path, serde_json::to_string(&request()).unwrap()).unwrap();
        assert_eq!(load_request(&path).unwrap().unwrap().sequence, 1);
    }
}