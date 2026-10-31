//! オフライン署名とリポジトリ管理（Python 版 release_repository.py の移植）。
//!
//! TUF 風メタデータを生成する簡易実装。Python 版の tuf ライブラリとは互換がない。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{Duration, Utc};
use gpupod_common::AgentError;
use serde_json::{json, Value};

use crate::support::files::{atomic_bytes, ensure_private_dir, HeldLock};
use crate::support::tuf;

/// パッケージ対象のディレクトリ（Rust 版リポジトリに合わせた移植）。
pub const SOURCE_DIRS: [&str; 4] = ["crates", "examples", "images", "docs"];
/// ルート直下でパッケージするファイル。
pub const TOP_FILES: [&str; 5] = [
    "Cargo.toml",
    "Cargo.lock",
    "README.md",
    "verify.yml.example",
    ".dockerignore",
];
const SUFFIXES: [&str; 9] = [
    ".rs", ".json", ".toml", ".sh", ".conf", ".env", ".md", ".html", ".yml",
];
const SKIP_DIRS: [&str; 3] = ["target", "__pycache__", ".git"];

pub fn expiry(days: i64) -> String {
    (Utc::now() + Duration::days(days)).to_rfc3339()
}

/// 信頼ルートと署名鍵を作る。既存のルートは置き換えない。
pub fn initialize(repository: &Path, keys_dir: &Path) -> Result<()> {
    let repository = absolute(repository)?;
    let keys = absolute(keys_dir)?;
    if keys.starts_with(&repository) {
        bail!(AgentError::new(
            "Signing keys must be outside the served TUF repository"
        ));
    }
    if repository.join("metadata/root.json").exists() {
        bail!(AgentError::new(
            "Refusing to replace an existing update trust root"
        ));
    }
    if keys
        .read_dir()
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
    {
        bail!(AgentError::new(
            "Refusing to replace an existing update trust root"
        ));
    }
    ensure_private_dir(&keys)?;
    std::fs::create_dir_all(repository.join("metadata"))?;
    std::fs::create_dir_all(repository.join("targets/provider"))?;
    let mut signing: BTreeMap<String, tuf::SigningKey> = BTreeMap::new();
    let mut public = serde_json::Map::new();
    let mut roles = serde_json::Map::new();
    for role in tuf::ROLES {
        let (keyid, key) = tuf::generate_keypair();
        atomic_bytes(
            &keys.join(format!("{role}.pem")),
            &tuf::signing_key_to_pem(&key)?,
            0o600,
        )?;
        public.insert(keyid.clone(), tuf::public_key_json(&key));
        roles.insert(role.to_string(), json!([keyid.clone()]));
        signing.insert(role.to_string(), key);
    }
    let root = json!({
        "spec": "gpupod-tuf-like-1",
        "_type": "root",
        "version": 1,
        "expires": expiry(365),
        "consistent_snapshot": false,
        "keys": serde_json::Value::Object(public),
        "roles": serde_json::Value::Object(roles.clone()),
    });
    let root_key = signing.get("root").context("Missing root key")?;
    let mut root_keys = BTreeMap::new();
    let root_keyid = roles
        .get("root")
        .and_then(|v| v.get(0))
        .and_then(|v| v.as_str())
        .context("Missing root key id")?
        .to_string();
    root_keys.insert(root_keyid, root_key.clone());
    let document = tuf::sign(&root, &root_keys)?;
    let bytes = tuf::document_bytes(&document)?;
    atomic_bytes(&repository.join("metadata/root.json"), &bytes, 0o644)?;
    atomic_bytes(&repository.join("metadata/1.root.json"), &bytes, 0o644)?;
    write_metadata(&repository, &keys)?;
    Ok(())
}

/// targets / snapshot / timestamp を再署名して書き出す。
pub fn write_metadata(repository: &Path, keys_dir: &Path) -> Result<()> {
    let directory = repository.join("metadata");
    let root = tuf::read_document(&directory.join("root.json"))?;
    let trust = tuf::root_trust(&root)?;
    let targets_path = directory.join("targets.json");
    let mut targets = if targets_path.exists() {
        tuf::read_document(&targets_path)?
    } else {
        json!({
            "signed": {
                "spec": "gpupod-tuf-like-1",
                "_type": "targets",
                "version": 1,
                "expires": expiry(90),
                "targets": {},
            },
            "signatures": [],
        })
    };
    targets["signed"]["expires"] = json!(expiry(90));
    let targets_key = load_signing_key(keys_dir, "targets")?;
    let targets_document = tuf::sign(
        &targets["signed"],
        &BTreeMap::from([("targets".to_string(), targets_key)]),
    )?;
    let targets_bytes = tuf::document_bytes(&targets_document)?;
    let version = targets_document["signed"]["version"]
        .as_u64()
        .context("Invalid targets version")?;
    let snapshot = json!({
        "spec": "gpupod-tuf-like-1",
        "_type": "snapshot",
        "version": version,
        "expires": expiry(7),
        "meta": {
            "targets.json": {
                "version": version,
                "length": targets_bytes.len(),
                "hashes": {"sha256": tuf::sha256_hex(&targets_bytes)},
            }
        },
    });
    let snapshot_key = load_signing_key(keys_dir, "snapshot")?;
    let snapshot_document = tuf::sign(
        &snapshot,
        &BTreeMap::from([("snapshot".to_string(), snapshot_key)]),
    )?;
    let snapshot_bytes = tuf::document_bytes(&snapshot_document)?;
    let timestamp = json!({
        "spec": "gpupod-tuf-like-1",
        "_type": "timestamp",
        "version": version,
        "expires": expiry(7),
        "snapshot_meta": {
            "version": version,
            "length": snapshot_bytes.len(),
            "hashes": {"sha256": tuf::sha256_hex(&snapshot_bytes)},
        },
    });
    let timestamp_key = load_signing_key(keys_dir, "timestamp")?;
    let timestamp_document = tuf::sign(
        &timestamp,
        &BTreeMap::from([("timestamp".to_string(), timestamp_key)]),
    )?;
    atomic_bytes(&targets_path, &targets_bytes, 0o644)?;
    atomic_bytes(
        &directory.join("snapshot.json"),
        &tuf::document_bytes(&snapshot_document)?,
        0o644,
    )?;
    atomic_bytes(
        &directory.join("timestamp.json"),
        &tuf::document_bytes(&timestamp_document)?,
        0o644,
    )?;
    let _ = trust;
    Ok(())
}

/// バージョン名の検証（Python 版の正規表現相当）。
pub fn valid_version(version: &str) -> bool {
    let bytes = version.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_digit() || version.len() > 64 {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// 配布用 tar.gz を作る。
pub fn package(source: &Path, destination: &Path, version: &str, sequence: i64) -> Result<()> {
    if !valid_version(version) || sequence < 1 {
        bail!(AgentError::new(
            "Release requires a positive sequence and a version beginning with a digit"
        ));
    }
    let source = absolute(source)?;
    let mut files: Vec<PathBuf> = Vec::new();
    for name in TOP_FILES {
        let path = source.join(name);
        if path.is_file() {
            files.push(path);
        }
    }
    for directory in SOURCE_DIRS {
        collect_files(&source.join(directory), &mut files)?;
    }
    files.sort();
    files.dedup();
    let parent = destination.parent().context("Invalid destination")?;
    std::fs::create_dir_all(parent)?;
    let file = std::fs::File::create(destination).context("Cannot create the release archive")?;
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for path in &files {
        let metadata = std::fs::symlink_metadata(path).context("Cannot read the release source")?;
        if metadata.file_type().is_symlink() {
            bail!(AgentError::new(
                "Release sources must not contain symbolic links"
            ));
        }
        let relative = path
            .strip_prefix(&source)
            .context("Invalid release path")?
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        if relative == "release.json" {
            continue;
        }
        let content = std::fs::read(path).context("Cannot read the release source")?;
        if contains_private_key(&content) {
            bail!(AgentError::new("Private key material cannot be packaged"));
        }
        append(&mut builder, &relative, &content, path)?;
    }
    let identity = serde_json::json!({"version": version, "sequence": sequence});
    let body = serde_json::to_vec(&identity).context("Cannot encode the release identity")?;
    let mut header = tar::Header::new_gnu();
    header.set_size(body.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append_data(&mut header, "release.json", body.as_slice())?;
    let encoder = builder
        .into_inner()
        .map_err(|exc| anyhow::anyhow!("{exc}"))?;
    encoder.finish().map_err(|exc| anyhow::anyhow!("{exc}"))?;
    Ok(())
}

fn append(
    builder: &mut tar::Builder<flate2::write::GzEncoder<std::fs::File>>,
    name: &str,
    content: &[u8],
    path: &Path,
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(content.len() as u64);
    header.set_mode(if path.extension().is_some_and(|ext| ext == "sh") {
        0o755
    } else {
        0o644
    });
    header.set_cksum();
    builder.append_data(&mut header, name, content)?;
    Ok(())
}

fn collect_files(directory: &Path, result: &mut Vec<PathBuf>) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            collect_files(&path, result)?;
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        if name.ends_with(".secret.json") {
            continue;
        }
        let allowed = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| SUFFIXES.iter().any(|suffix| *suffix == format!(".{ext}")))
            .unwrap_or(false)
            || (directory.ends_with("images") && name == "Dockerfile")
            || name == "Dockerfile";
        if allowed {
            result.push(path);
        }
    }
    Ok(())
}

pub fn contains_private_key(content: &[u8]) -> bool {
    private_key_line(content)
}

fn private_key_line(content: &[u8]) -> bool {
    for line in content.split(|byte| *byte == b'\n') {
        let mut rest = line;
        if rest.starts_with(b"-----BEGIN ") {
            rest = &rest[b"-----BEGIN ".len()..];
            let Some(end) = rest
                .windows(b"PRIVATE KEY-----".len())
                .position(|w| w == b"PRIVATE KEY-----")
            else {
                continue;
            };
            let prefix = &rest[..end];
            if prefix.is_empty() || prefix.iter().all(|b| b.is_ascii_uppercase() || *b == b' ') {
                return true;
            }
        }
    }
    false
}

/// リリースを公開する。シーケンスは必ず増加させる。
pub fn publish(
    repository: &Path,
    keys_dir: &Path,
    source: &Path,
    version: &str,
    sequence: i64,
) -> Result<Value> {
    let repository = absolute(repository)?;
    let _lock = HeldLock::exclusive(&repository.join("publish.lock"), 10)?;
    let targets_path = repository.join("metadata/targets.json");
    let mut targets = tuf::read_document(&targets_path)?;
    let highest = targets["signed"]["targets"]
        .as_object()
        .map(|map| {
            map.values()
                .filter_map(|item| {
                    item.get("custom")
                        .and_then(|c| c.get("sequence"))
                        .and_then(|v| v.as_i64())
                })
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0);
    if sequence <= highest {
        bail!(AgentError::new("Release sequences must increase"));
    }
    let target_name = format!("provider/{sequence:08}-{version}.tar.gz");
    let target_path = repository.join("targets").join(&target_name);
    package(source, &target_path, version, sequence)?;
    let body = std::fs::read(&target_path).context("Cannot read the release archive")?;
    targets["signed"]["targets"][&target_name] = json!({
        "length": body.len(),
        "hashes": {"sha256": tuf::sha256_hex(&body)},
        "custom": {"version": version, "sequence": sequence},
    });
    targets["signed"]["version"] = json!(targets["signed"]["version"].as_u64().unwrap_or(0) + 1);
    atomic_bytes(&targets_path, &tuf::document_bytes(&targets)?, 0o644)?;
    write_metadata(&repository, keys_dir)?;
    Ok(json!({"version": version, "sequence": sequence, "target": target_name}))
}

/// 有効期限だけ更新する。
pub fn refresh(repository: &Path, keys_dir: &Path) -> Result<()> {
    let repository = absolute(repository)?;
    let _lock = HeldLock::exclusive(&repository.join("publish.lock"), 10)?;
    let targets_path = repository.join("metadata/targets.json");
    let mut targets = tuf::read_document(&targets_path)?;
    targets["signed"]["version"] = json!(targets["signed"]["version"].as_u64().unwrap_or(0) + 1);
    atomic_bytes(&targets_path, &tuf::document_bytes(&targets)?, 0o644)?;
    write_metadata(&repository, keys_dir)
}

fn load_signing_key(keys_dir: &Path, role: &str) -> Result<tuf::SigningKey> {
    let path = keys_dir.join(format!("{role}.pem"));
    let raw = std::fs::read(&path).with_context(|| format!("Cannot read {}", path.display()))?;
    tuf::signing_key_from_pem(&raw)
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::tuf::{root_trust, verify};

    fn fixture_source() -> PathBuf {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("source");
        std::fs::create_dir_all(root.join("crates/gpupod-agent/src")).unwrap();
        std::fs::create_dir_all(root.join("examples")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(
            root.join("crates/gpupod-agent/src/main.rs"),
            "fn main() {}\n",
        )
        .unwrap();
        std::fs::write(root.join("examples/templates.json"), "[]\n").unwrap();
        std::fs::write(root.join("README.md"), "# gpupod\n").unwrap();
        std::mem::forget(directory);
        root
    }

    #[test]
    fn version_validation_matches_the_python_rule() {
        assert!(valid_version("1.0.0"));
        assert!(valid_version("2026.10.19"));
        assert!(!valid_version("v1.0.0"));
        assert!(!valid_version(""));
        assert!(!valid_version(&"1".repeat(65)));
        assert!(!valid_version("1.0/evil"));
    }

    #[test]
    fn private_key_material_is_detected() {
        assert!(contains_private_key(b"-----BEGIN PRIVATE KEY-----\nabc\n"));
        assert!(contains_private_key(
            b"-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n"
        ));
        assert!(!contains_private_key(b"public info\n"));
        assert!(!contains_private_key(b"-----BEGIN CERTIFICATE-----\nabc\n"));
    }

    #[test]
    fn init_creates_a_verifiable_trust_root() {
        let repository = tempfile::tempdir().unwrap();
        let keys = tempfile::tempdir().unwrap();
        initialize(repository.path(), keys.path()).unwrap();
        let root = tuf::read_document(&repository.path().join("metadata/root.json")).unwrap();
        let trust = root_trust(&root).unwrap();
        assert_eq!(trust.roles["root"].len(), 1);
        for name in ["targets.json", "snapshot.json", "timestamp.json"] {
            let document =
                tuf::read_document(&repository.path().join("metadata").join(name)).unwrap();
            let role = name.trim_end_matches(".json");
            let outcome = verify(&document, role, &trust);
            assert!(outcome.is_ok(), "{name}: {:#}", outcome.unwrap_err());
        }
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(keys.path().join("root.pem"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert!(initialize(repository.path(), keys.path()).is_err());
    }

    #[test]
    fn publish_records_the_target_and_requires_increasing_sequences() {
        let repository = tempfile::tempdir().unwrap();
        let keys = tempfile::tempdir().unwrap();
        let source = fixture_source();
        initialize(repository.path(), keys.path()).unwrap();
        let result = publish(repository.path(), keys.path(), &source, "1.0.0", 1).unwrap();
        assert_eq!(result["target"], "provider/00000001-1.0.0.tar.gz");
        assert!(publish(repository.path(), keys.path(), &source, "1.0.0", 1).is_err());
        let second = publish(repository.path(), keys.path(), &source, "1.0.1", 2).unwrap();
        assert_eq!(second["sequence"], 2);
        let root = tuf::read_document(&repository.path().join("metadata/root.json")).unwrap();
        let trust = root_trust(&root).unwrap();
        let targets = tuf::read_document(&repository.path().join("metadata/targets.json")).unwrap();
        let signed = verify(&targets, "targets", &trust).unwrap();
        assert_eq!(signed["targets"].as_object().unwrap().len(), 2);
        let entry = &signed["targets"]["provider/00000001-1.0.0.tar.gz"];
        assert_eq!(entry["custom"]["version"], "1.0.0");
        assert!(entry["hashes"]["sha256"].as_str().unwrap().len() == 64);
    }

    #[test]
    fn refresh_only_bumps_the_version() {
        let repository = tempfile::tempdir().unwrap();
        let keys = tempfile::tempdir().unwrap();
        let source = fixture_source();
        initialize(repository.path(), keys.path()).unwrap();
        publish(repository.path(), keys.path(), &source, "1.0.0", 1).unwrap();
        refresh(repository.path(), keys.path()).unwrap();
        let root = tuf::read_document(&repository.path().join("metadata/root.json")).unwrap();
        let trust = root_trust(&root).unwrap();
        let targets = tuf::read_document(&repository.path().join("metadata/targets.json")).unwrap();
        let signed = verify(&targets, "targets", &trust).unwrap();
        assert_eq!(signed["version"], 3);
        assert_eq!(signed["targets"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn package_rejects_private_keys_and_symbols() {
        let repository = tempfile::tempdir().unwrap();
        let keys = tempfile::tempdir().unwrap();
        initialize(repository.path(), keys.path()).unwrap();
        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(source.path().join("crates")).unwrap();
        std::fs::write(
            source.path().join("crates/leak.rs"),
            "-----BEGIN PRIVATE KEY-----\nabc\n",
        )
        .unwrap();
        assert!(
            publish(repository.path(), keys.path(), source.path(), "1.0.0", 1)
                .unwrap_err()
                .to_string()
                .contains("Private key")
        );
    }
}
