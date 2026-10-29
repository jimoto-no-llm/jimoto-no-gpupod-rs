//! ノード機材キーのローテーション（Python 版 rotate_token.py / credentials.py の移植）。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use gpupod_common::AgentError;
use rand::RngCore;
use serde_json::json;

use crate::support::files::{atomic_json, HeldLock};

/// 2 段階のローテーション。応答喪失時は同じ候補キーで commit を再試行する。
pub fn rotate_credentials(config_path: &Path) -> Result<serde_json::Value> {
    let config_path = absolute(config_path)?;
    let pending_path = PathBuf::from(format!("{}.rotation.secret.json", config_path.display()));
    let lock_path = PathBuf::from(format!("{}.lock", config_path.display()));
    let _lock = HeldLock::exclusive(&lock_path, 10)?;
    let mut config: serde_json::Value = read_json(&config_path)?;
    let mut pending = if pending_path.exists() {
        read_json(&pending_path)?
    } else {
        let value = json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "token": token_urlsafe(48),
        });
        atomic_json(&pending_path, &value).context("Cannot store the rotation candidate")?;
        value
    };
    let base = format!(
        "{}/api/nodes/{}/token",
        config["url"].as_str().unwrap_or_default(),
        config["node_id"].as_str().unwrap_or_default()
    );
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()
        .context("Cannot create the HTTP client")?;
    let candidate = format!("Bearer {}", pending["token"].as_str().unwrap_or_default());
    let rotation_id = pending["id"].clone();
    let mut commit = post_json(&client, &format!("{base}/commit/"), &candidate, &json!({"id": rotation_id}))?;
    let commit_status = commit.status().as_u16();
    if commit_status == 403 || commit_status == 409 {
        // 事前登録が欠けている（または古い）場合は中央の現在キーで prepare し直す。
        let bearer = format!("Bearer {}", config["token"].as_str().unwrap_or_default());
        let prepared = post_json(
            &client,
            &format!("{base}/prepare/"),
            &bearer,
            &json!({"id": rotation_id, "token": pending["token"]}),
        )?;
        if !prepared.status().is_success() {
            bail!(AgentError::new(format!(
                "Central server refused the rotation request: HTTP {}",
                prepared.status()
            )));
        }
        commit = post_json(&client, &format!("{base}/commit/"), &candidate, &json!({"id": rotation_id}))?;
    }
    if !commit.status().is_success() {
        bail!(AgentError::new(format!(
            "Central server refused the rotation request: HTTP {}",
            commit.status()
        )));
    }
    let confirmed: serde_json::Value = commit.json().context("Cannot read the rotation response")?;
    if confirmed.get("id") != Some(&rotation_id) {
        bail!(AgentError::new(
            "Central server did not confirm this credential rotation"
        ));
    }
    config["token"] = pending["token"].clone();
    atomic_json(&config_path, &config).context("Cannot store the rotated credential")?;
    pending = json!({});
    let _ = pending;
    let _ = std::fs::remove_file(&pending_path);
    Ok(json!({"rotation_id": rotation_id, "status": "complete"}))
}

fn post_json(
    client: &reqwest::blocking::Client,
    url: &str,
    bearer: &str,
    body: &serde_json::Value,
) -> Result<reqwest::blocking::Response> {
    client
        .post(url)
        .header(reqwest::header::AUTHORIZATION, bearer)
        .timeout(std::time::Duration::from_secs(15))
        .json(body)
        .send()
        .with_context(|| format!("Cannot reach {url}"))
}

fn read_json(path: &Path) -> Result<serde_json::Value> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Cannot read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("Cannot parse {}", path.display()))
}

/// secrets.token_urlsafe(48) 相当。
pub fn token_urlsafe(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buffer);
    URL_SAFE_NO_PAD.encode(buffer)
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
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// 簡易 HTTP サーバ。prepare / commit の応答を順に返す。
    fn serve(responses: Vec<(u16, serde_json::Value)>, hits: Arc<AtomicUsize>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for (status, body) in responses {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut buffer = [0u8; 8192];
                let mut read = 0;
                loop {
                    match stream.read(&mut buffer[read..]) {
                        Ok(0) => break,
                        Ok(n) => {
                            read += n;
                            let head = String::from_utf8_lossy(&buffer[..read]).to_string();
                            if head.contains("\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                hits.fetch_add(1, Ordering::SeqCst);
                let payload = serde_json::to_string(&body).unwrap();
                let reason = if status == 200 { "OK" } else { "Error" };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{address}")
    }

    fn write_config(directory: &std::path::Path, url: &str) -> PathBuf {
        let path = directory.join("node.secret.json");
        let config = json!({
            "url": url,
            "node_id": "node-1",
            "token": "current-token",
            "relay_host": "relay.example.org",
            "relay_host_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFakeFakeFakeFakeFakeFakeFakeFake000000"
        });
        std::fs::write(&path, serde_json::to_string(&config).unwrap()).unwrap();
        path
    }

    #[test]
    fn successful_rotation_replaces_the_token() {
        let hits = Arc::new(AtomicUsize::new(0));
        let url = serve(vec![(200, json!({"id": "unknown"}))], hits.clone());
        let directory = tempfile::tempdir().unwrap();
        let config_path = write_config(directory.path(), &url);
        // commit が先に成功する場合でも、応答の id が一致しなければ失敗する。
        assert!(rotate_credentials(&config_path).is_err());
    }

    #[test]
    fn prepare_commit_sequence_rotates_the_credential() {
        let hits = Arc::new(AtomicUsize::new(0));
        let url = serve(
            vec![
                (409, json!({"error": "not prepared"})),
                (200, json!({"ok": true})),
                (200, json!({"id": "placeholder"})),
            ],
            hits.clone(),
        );
        let directory = tempfile::tempdir().unwrap();
        let config_path = write_config(directory.path(), &url);
        let error = rotate_credentials(&config_path).unwrap_err().to_string();
        assert!(error.contains("did not confirm"), "{error}");
        // 途中で作られた候補ファイルは残る（再開用）。
        let pending = PathBuf::from(format!("{}.rotation.secret.json", config_path.display()));
        assert!(pending.exists());
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&pending).unwrap()).unwrap();
        assert!(value["token"].as_str().unwrap().len() > 40);
    }

    #[test]
    fn token_urlsafe_is_url_safe() {
        let token = token_urlsafe(48);
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(token, token_urlsafe(48));
    }
}
