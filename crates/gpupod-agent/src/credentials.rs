//! 機材キー（ノードクレデンシャル）の二段階交換（Python 版 credentials.py の移植）。

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpupod_common::AgentError;
use serde_json::{json, Value};

use crate::files::atomic_json;

pub type Result<T> = std::result::Result<T, AgentError>;

/// 予約ロックを最大 10 秒待つ（Python 版 FileLock timeout=10 相当）。
fn lock_config(path: &Path) -> Result<std::fs::File> {
    let lock_path = PathBuf::from(format!("{}.lock", path.display()));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|exc| AgentError::new(format!("{exc}")))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match fs4::FileExt::try_lock(&file) {
            Ok(()) => return Ok(file),
            Err(fs4::TryLockError::WouldBlock) => {
                if std::time::Instant::now() >= deadline {
                    return Err(AgentError::new(
                        "Another provider operation is still running",
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(fs4::TryLockError::Error(exc)) => {
                return Err(AgentError::new(format!("{exc}")));
            }
        }
    }
}

fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|exc| AgentError::new(format!("{exc}")))
}

fn read_json(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).map_err(|exc| AgentError::new(format!("{exc}")))?;
    serde_json::from_str(&text).map_err(|exc| AgentError::new(format!("{exc}")))
}

/// prepare → commit の二段階で機材キーを交換する。
///
/// 失敗しても pending ファイルに残った候補を先に再送し、
/// 応答が失われた交換は同じ候補で回復する。
pub fn rotate_credentials(config_path: impl AsRef<Path>) -> Result<Value> {
    let config_path = config_path
        .as_ref()
        .canonicalize()
        .unwrap_or_else(|_| config_path.as_ref().to_path_buf());
    let pending_path = PathBuf::from(format!("{}.rotation.secret.json", config_path.display()));
    let _lock = lock_config(&config_path)?;
    let mut config = read_json(&config_path)?;
    let pending = if pending_path.exists() {
        read_json(&pending_path)?
    } else {
        let pending = json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "token": random_token(48),
        });
        atomic_json(&pending_path, &pending)?;
        pending
    };
    let pending_id = pending
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::new("Invalid pending credential rotation"))?
        .to_owned();
    let pending_token = pending
        .get("token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::new("Invalid pending credential rotation"))?
        .to_owned();
    let base = format!(
        "{}/api/nodes/{}/token",
        config
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
        config
            .get("node_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
    );
    let client = http_client()?;
    let candidate = json!({ "id": pending_id });
    let commit = client
        .post(format!("{base}/commit/"))
        .header("Authorization", format!("Bearer {pending_token}"))
        .json(&candidate)
        .send()
        .map_err(|exc| AgentError::new(format!("{exc}")))?;
    let mut status = commit.status().as_u16();
    let mut commit_body: Option<Value> = if commit.status().is_success() {
        Some(
            commit
                .json()
                .map_err(|exc| AgentError::new(format!("{exc}")))?,
        )
    } else {
        None
    };
    if status == 403 || status == 409 {
        let config_token = config
            .get("token")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        let prepared = client
            .post(format!("{base}/prepare/"))
            .header("Authorization", format!("Bearer {config_token}"))
            .json(&json!({
                "id": pending_id,
                "token": pending_token,
            }))
            .send()
            .map_err(|exc| AgentError::new(format!("{exc}")))?;
        if !prepared.status().is_success() {
            return Err(AgentError::new(format!(
                "Central server rejected the credential rotation: {}",
                prepared.status()
            )));
        }
        let commit = client
            .post(format!("{base}/commit/"))
            .header("Authorization", format!("Bearer {pending_token}"))
            .json(&candidate)
            .send()
            .map_err(|exc| AgentError::new(format!("{exc}")))?;
        status = commit.status().as_u16();
        commit_body = if commit.status().is_success() {
            Some(
                commit
                    .json()
                    .map_err(|exc| AgentError::new(format!("{exc}")))?,
            )
        } else {
            None
        };
    }
    if !(200..300).contains(&status) {
        return Err(AgentError::new(format!(
            "Central server rejected the credential rotation: {status}"
        )));
    }
    let body = commit_body.unwrap_or(Value::Null);
    if body.get("id").and_then(|v| v.as_str()) != Some(pending_id.as_str()) {
        return Err(AgentError::new(
            "Central server did not confirm this credential rotation",
        ));
    }
    config["token"] = json!(pending_token);
    atomic_json(&config_path, &config)?;
    let _ = std::fs::remove_file(&pending_path);
    Ok(json!({ "rotation_id": pending_id, "status": "complete" }))
}

/// secrets.token_urlsafe(48) 相当（64 文字の URL 安全な文字列）。
///
/// 機材キーの候補なので OS の乱数源から取る。
fn random_token(bytes: usize) -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut buffer = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut buffer);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buffer)
}
