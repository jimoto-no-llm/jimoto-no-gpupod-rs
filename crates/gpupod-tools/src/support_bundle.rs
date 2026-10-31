//! 私的なサポートアーカイブの作成（Python 版 support_bundle.py の移植）。
//!
//! 伏字処理の順序と正規表現は Python 版と一致させる。

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::Utc;
use fancy_regex::Regex;
use gpupod_common::resources::{self, Pool};
use gpupod_common::AgentError;

use crate::support::benchmarks::read_results;
use crate::support::devices;
use crate::support::files::{ensure_private_dir, fs_set_mode, link_overwrite_free};
use crate::support::repo::release_identity;

pub const LOG_LIMIT: usize = 256 * 1024;

fn sensitive() -> Regex {
    Regex::new(
        "(?i)password|passphrase|secret|token|credential|authorization|cookie|api[ _-]?key|private[ _-]?key",
    )
    .expect("valid sensitive pattern")
}

fn private_key() -> Regex {
    Regex::new(
        r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|$)",
    )
    .expect("valid private key pattern")
}

fn url_pattern() -> Regex {
    Regex::new(r#"(?i)\b(?:https?|ssh|postgres(?:ql)?|mysql|redis)://[^\s<>"']+"#)
        .expect("valid url pattern")
}

fn ssh_key() -> Regex {
    Regex::new(r"\b(?:ssh-(?:rsa|ed25519)|ecdsa-sha2-[\w-]+)\s+\S+(?:[^\n]*)")
        .expect("valid ssh key pattern")
}

fn long_value() -> Regex {
    Regex::new(r"(?<![A-Za-z0-9_+/=-])[A-Za-z0-9_+/=-]{32,}(?![A-Za-z0-9_+/=-])")
        .expect("valid long value pattern")
}

fn home_path() -> Regex {
    Regex::new(r#"/(?:home/[^/\s]+|root)(?:/[^\s"'<>]*)?"#).expect("valid home pattern")
}

/// Python 版 redact と同じ順序で伏字を入れる。
pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut result = private_key()
        .replace_all(text, "[REDACTED PRIVATE KEY]")
        .to_string();
    let mut ordered: Vec<&String> = secrets.iter().filter(|value| !value.is_empty()).collect();
    ordered.sort_by_key(|value| std::cmp::Reverse(value.len()));
    for value in ordered {
        result = result.replace(value.as_str(), "[REDACTED]");
    }
    result = ssh_key()
        .replace_all(&result, "[REDACTED SSH KEY]")
        .to_string();
    result = url_pattern()
        .replace_all(&result, "[REDACTED URL]")
        .to_string();
    result = home_path()
        .replace_all(&result, "[REDACTED HOME PATH]")
        .to_string();
    let mut lines = Vec::new();
    for line in result.split('\n') {
        if sensitive().is_match(line).unwrap_or(false) {
            lines.push("[REDACTED SENSITIVE LOG LINE]".to_string());
        } else {
            lines.push(
                long_value()
                    .replace_all(line, "[REDACTED LONG VALUE]")
                    .to_string(),
            );
        }
    }
    lines.join("\n") + "\n"
}

/// サイズ制限つき JSON オブジェクト読み取り。
pub fn json_file(path: &Path, limit: usize) -> Result<serde_json::Value> {
    let raw = std::fs::read(path).map_err(|exc| AgentError::new(format!("{exc}")))?;
    if raw.len() > limit {
        bail!(AgentError::new("Diagnostic input exceeds its size limit"));
    }
    let value: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|exc| AgentError::new(format!("{exc}")))?;
    if !value.is_object() {
        bail!(AgentError::new("Diagnostic input is not an object"));
    }
    Ok(value)
}

/// 秘密扱いのキー配下の文字列値を収集する。
pub fn secret_values(value: &serde_json::Value, sensitive_context: bool) -> Vec<String> {
    let mut result = Vec::new();
    match value {
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                let flagged = sensitive_context || sensitive().is_match(key).unwrap_or(false);
                result.extend(secret_values(item, flagged));
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                result.extend(secret_values(item, sensitive_context));
            }
        }
        serde_json::Value::String(text) if sensitive_context && !text.is_empty() => {
            result.push(text.clone());
        }
        _ => {}
    }
    result
}

/// レンズキー（公開フィールド）のみを残した設定サマリ。
pub fn safe_config(config: &serde_json::Value) -> serde_json::Value {
    let empty = serde_json::Map::new();
    let object = config.as_object().unwrap_or(&empty);
    let runtime = object.get("runtime").and_then(|v| v.as_str()).unwrap_or("");
    let mut result = serde_json::json!({
        "runtime": if runtime == "runc" || runtime == "runsc" { serde_json::json!(runtime) } else { serde_json::json!("default") },
        "central_configured": object.get("url").is_some_and(|v| !v.is_null()),
        "direct_configured": object.get("direct_host").is_some_and(|v| !v.is_null()),
    });
    for key in ["watchdog_timeout", "min_disk_free_bytes"] {
        if let Some(value) = object.get(key) {
            if value.is_number() && value.as_f64().is_some_and(|v| v.is_finite() && v >= 0.0) {
                result[key] = value.clone();
            }
        }
    }
    if let Some(limits) = object.get("limits").and_then(|v| v.as_object()) {
        let mut clean = serde_json::Map::new();
        for (key, value) in limits {
            if matches!(
                key.as_str(),
                "nano_cpus" | "mem_limit" | "pids_limit" | "shm_size"
            ) && value.as_i64().is_some_and(|v| v > 0)
            {
                clean.insert(key.clone(), value.clone());
            }
        }
        result["limits"] = serde_json::Value::Object(clean);
    }
    result
}

fn state_summary(directory: &Path) -> Result<serde_json::Value> {
    let path = directory.join("state.sqlite3");
    if !path.exists() {
        return Ok(serde_json::json!({"available": false}));
    }
    let connection =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|exc| AgentError::new(format!("{exc}")))?;
    let row: Option<String> = match connection.query_row(
        "SELECT payload FROM agent_state WHERE id=1 AND length(payload)<=8388608",
        [],
        |row| row.get(0),
    ) {
        Ok(value) => Some(value),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(exc) => bail!(AgentError::new(format!("{exc}"))),
    };
    let payload = row.ok_or_else(|| AgentError::new("Provider state is missing or too large"))?;
    let state: serde_json::Value =
        serde_json::from_str(&payload).map_err(|_| AgentError::new("Provider state is invalid"))?;
    let leases = state
        .get("leases")
        .and_then(|v| v.as_object())
        .ok_or_else(|| AgentError::new("Provider state is invalid"))?;
    let mut statuses: BTreeMap<&str, i64> = BTreeMap::new();
    for key in ["starting", "running", "cleanup_pending", "stopped"] {
        statuses.insert(key, 0);
    }
    for lease in leases.values() {
        if let Some(status) = lease.get("status").and_then(|v| v.as_str()) {
            if let Some(slot) = statuses.get_mut(status) {
                *slot += 1;
            }
        }
    }
    let mut summary = serde_json::json!({
        "available": true,
        "manual_enabled": state.get("manual_enabled") == Some(&serde_json::json!(true)),
        "auto_busy": state.get("auto_busy") == Some(&serde_json::json!(true)),
        "resources": resource_summary(&state)?,
        "leases": serde_json::Map::new(),
    });
    let lease_map = summary["leases"].as_object_mut().unwrap();
    for (key, count) in statuses {
        lease_map.insert(key.to_string(), serde_json::json!(count));
    }
    Ok(summary)
}

/// 提供枠の要約（Python 版 resources.summary 相当）。
pub fn resource_summary(state: &serde_json::Value) -> Result<serde_json::Value> {
    let raw = state.get("resource_pool");
    let Some(raw) = raw.filter(|value| !value.is_null()) else {
        return Ok(serde_json::Value::Null);
    };
    let pool: Pool = Pool::from_value(raw.clone())?;
    let accepting = state.get("manual_enabled") == Some(&serde_json::json!(true))
        && state.get("auto_busy") != Some(&serde_json::json!(true));
    let mut allocations = Vec::new();
    if let Some(leases) = state.get("leases").and_then(|v| v.as_object()) {
        for lease in leases.values() {
            if lease.get("status").and_then(|v| v.as_str()) == Some("stopped") {
                continue;
            }
            match lease.get("resources") {
                None | Some(serde_json::Value::Null) => allocations.push(None),
                Some(value) => {
                    let allocation: resources::Allocation =
                        serde_json::from_value(value.clone())
                            .map_err(|_| AgentError::new("Invalid lease resource allocation"))?;
                    allocations.push(Some(allocation));
                }
            }
        }
    }
    let summary = resources::summary(Some(&pool), &allocations, accepting)?;
    Ok(match summary {
        Some(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, value))
                .collect::<serde_json::Map<String, serde_json::Value>>(),
        ),
        None => serde_json::Value::Null,
    })
}

fn health_summary(directory: &Path) -> Result<serde_json::Value> {
    let health = json_file(&directory.join("health.json"), LOG_LIMIT)?;
    let stalled: Vec<&str> = health
        .get("stalled")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .filter(|name| matches!(*name, "poll" | "maintenance" | "tunnels" | "worker"))
                .collect()
        })
        .unwrap_or_default();
    Ok(serde_json::json!({
        "healthy": health.get("healthy") == Some(&serde_json::json!(true)),
        "stalled": stalled,
    }))
}

/// アーカイブを作る。出力は既存なら拒否し、権限 600 で公開する。
pub fn create_bundle(
    state_dir: &Path,
    output: &Path,
    config_path: Option<&Path>,
    log_path: Option<&Path>,
    _docker_socket: &Path,
    mock_gpu: bool,
) -> Result<PathBuf> {
    if output.symlink_metadata().is_ok() {
        bail!(AgentError::new(
            "Output already exists; choose a new support archive filename"
        ));
    }
    let mut config = serde_json::json!({});
    let mut secrets: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    if let Some(path) = config_path {
        config = json_file(path, LOG_LIMIT)?;
        secrets.extend(secret_values(&config, false));
        let mut pending_name = path.as_os_str().to_owned();
        pending_name.push(".rotation.secret.json");
        let pending = PathBuf::from(pending_name);
        if pending.exists() {
            // 伏字入力が読めない場合は失敗させる（弱いアーカイブを作らない）。
            let value = json_file(&pending, LOG_LIMIT)?;
            secrets.extend(secret_values(&value, false));
        }
    }
    let release = release_identity();
    let mut report = serde_json::json!({
        "schema": 1,
        "created_at": Utc::now().to_rfc3339(),
        "tools_version": env!("CARGO_PKG_VERSION"),
        "system": std::env::consts::OS,
        "architecture": std::env::consts::ARCH,
        "release": {
            "version": release.get("version").cloned().unwrap_or(serde_json::json!("development")),
            "sequence": release.get("sequence").cloned().unwrap_or(serde_json::json!(0)),
        },
        "mock_gpu": mock_gpu,
        "configuration": safe_config(&config),
        "benchmarks": read_results(state_dir),
    });
    let gpus = if mock_gpu {
        Ok(devices::mock_gpus())
    } else {
        devices::load_gpus(None)
    };
    match gpus {
        Ok(gpus) => {
            report["gpus"] = serde_json::Value::Array(
                gpus.iter()
                    .map(|gpu| {
                        serde_json::json!({
                            "vendor": gpu.vendor,
                            "name": gpu.name,
                            "memory_mib": gpu.memory_mib,
                        })
                    })
                    .collect(),
            );
        }
        Err(_) => errors.push("GPU inventory unavailable".to_string()),
    }
    let mut dependencies = serde_json::Map::new();
    dependencies.insert(
        "gpupod-tools".to_string(),
        serde_json::json!(env!("CARGO_PKG_VERSION")),
    );
    dependencies.insert(
        "docker-cli".to_string(),
        serde_json::json!(docker_cli_version()),
    );
    report["dependencies"] = serde_json::Value::Object(dependencies);
    for (name, collector) in [
        (
            "state",
            state_summary as fn(&Path) -> Result<serde_json::Value>,
        ),
        ("health", health_summary),
    ] {
        match collector(state_dir) {
            Ok(value) => report[name] = value,
            Err(_) => errors.push(format!("{name} unavailable")),
        }
    }
    match docker_info_subset(_docker_socket) {
        Ok(value) => report["docker"] = value,
        Err(_) => errors.push("Docker unavailable".to_string()),
    }
    report["collection_warnings"] =
        serde_json::Value::Array(errors.into_iter().map(serde_json::Value::String).collect());

    // フィールドは上で明示的に選んでいる。文字列だけ伏字にし、JSON の構造は保つ。
    let cleaned = clean_value(&report, &secrets);
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    files.push((
        "report.json".to_string(),
        (serde_json::to_string_pretty(&cleaned)? + "\n").into_bytes(),
    ));
    if let Some(path) = log_path {
        let raw = std::fs::read(path).context("Cannot read the provider log")?;
        let offset = raw.len().saturating_sub(LOG_LIMIT);
        let mut slice = &raw[offset..];
        if offset > 0 {
            // 切り該め先頭の認証片を残さない。
            if let Some(position) = slice.iter().position(|byte| *byte == b'\n') {
                slice = &slice[position + 1..];
            }
        }
        let text = String::from_utf8_lossy(slice).to_string();
        files.push((
            "provider.log".to_string(),
            redact(&text, &secrets).into_bytes(),
        ));
    }
    files.push((
        "README.txt".to_string(),
        "Local support archive. No data has been uploaded.\n         Configuration/state are summarized using an allowlist; machine/lease IDs and endpoints are omitted from the report. \
         Environment, raw config, keys, container logs and workspace contents are not collected.\n         An explicitly supplied provider log is bounded and redacted for known credentials, private/public keys, \
         URLs, home paths and credential-bearing lines. Review it before sharing: arbitrary unlabelled secrets \
         in free-form logs cannot be identified reliably.\n"
            .to_string()
            .into_bytes(),
    ));

    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    ensure_private_dir(parent)?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).context("Cannot create a temporary file")?;
    {
        let encoder =
            flate2::write::GzEncoder::new(temporary.as_file_mut(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (name, body) in &files {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o600);
            header.set_cksum();
            builder.append_data(&mut header, name, body.as_slice())?;
        }
        let mut encoder = builder
            .into_inner()
            .map_err(|exc| anyhow::anyhow!("{exc}"))?;
        encoder.flush().ok();
        encoder.finish().map_err(|exc| anyhow::anyhow!("{exc}"))?;
        temporary.as_file().sync_all().ok();
    }
    link_overwrite_free(temporary.path(), output)?;
    let _ = fs_set_mode(output, 0o600);
    Ok(output.to_path_buf())
}

fn clean_value(value: &serde_json::Value, secrets: &[String]) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), clean_value(item, secrets)))
                .collect(),
        ),
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|item| clean_value(item, secrets))
                .collect(),
        ),
        serde_json::Value::String(text) => {
            serde_json::Value::String(redact(text, secrets).trim().to_string())
        }
        other => other.clone(),
    }
}

fn docker_cli_version() -> String {
    std::process::Command::new("docker")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|| "unavailable".to_string())
}

fn docker_info_subset(socket: &Path) -> Result<serde_json::Value> {
    let engine = crate::support::docker::DockerEngine::new(socket);
    let info = engine.info()?;
    let mut subset = serde_json::Map::new();
    let wanted = [
        "ServerVersion",
        "Driver",
        "CgroupVersion",
        "CgroupDriver",
        "KernelVersion",
        "OSType",
        "Architecture",
        "NCPU",
        "MemTotal",
    ];
    for key in wanted {
        if let Some(value) = info.get(key) {
            if value.is_string() || value.is_number() {
                subset.insert(key.to_string(), value.clone());
            }
        }
    }
    Ok(serde_json::Value::Object(subset))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_private_keys_first() {
        let text = "before\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXkta2V5\n-----END OPENSSH PRIVATE KEY-----\nafter\n";
        let result = redact(text, &[]);
        // 秘密鍵ブロックは消える。Python 版と同様、その後の行単位の
        // sensitive 判定でその行自体も伏字行に置き換わる。
        assert!(!result.contains("b3BlbnNzaC1rZXkta2V5"));
        assert!(!result.contains("BEGIN OPENSSH PRIVATE KEY"));
        assert!(result.contains("[REDACTED"));
        assert!(result.contains("before"));
        assert!(result.contains("after"));
    }

    #[test]
    fn private_key_marker_is_replaced_before_line_filtering() {
        // 行単位の sensitive 判定が走らない文脈でも秘密鍵は先に消える。
        let text = "key=-----BEGIN PRIVATE KEY-----\nabc\n";
        let result = redact(text, &[]);
        assert!(!result.contains("abc"));
        assert!(!result.contains("BEGIN"));
    }

    #[test]
    fn redacts_known_secrets_longest_first() {
        let secrets = vec!["short".to_string(), "longer-secret-value".to_string()];
        let result = redact("value=longer-secret-value and short\n", &secrets);
        assert!(!result.contains("longer-secret-value"));
        assert!(result.contains("[REDACTED]"));
    }

    #[test]
    fn redacts_ssh_keys_and_urls() {
        let result = redact(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIexample comment\nsee https://example.org/path?x=1\n",
            &[],
        );
        assert!(result.contains("[REDACTED SSH KEY]"));
        assert!(result.contains("[REDACTED URL]"));
        assert!(!result.contains("example.org/path"));
    }

    #[test]
    fn drops_credential_lines_and_home_paths() {
        let result = redact("API_KEY = abc\nconnect /home/alice/secret\n", &[]);
        assert!(result.contains("[REDACTED SENSITIVE LOG LINE]"));
        assert!(result.contains("[REDACTED HOME PATH]"));
        assert!(!result.contains("/home/alice"));
    }

    #[test]
    fn long_token_like_values_are_removed() {
        let result = redact(
            "value=aGVsbG8tdGhpcy1pcy1hLWxvbmctdG9rZW4tbGlrZQ== tail\n",
            &[],
        );
        assert!(result.contains("[REDACTED LONG VALUE]"));
        assert!(!result.contains("aGVsbG8tdGhpcy1pcy1hLWxvbmctdG9rZW4tbGlrZQ=="));
        assert!(result.ends_with('\n'));
    }

    #[test]
    fn sensitive_search_is_case_insensitive() {
        assert!(sensitive().is_match("PassWord").unwrap());
        assert!(sensitive().is_match("api-key").unwrap());
        assert!(sensitive().is_match("X-Api_Key").unwrap());
    }

    #[test]
    fn secret_values_follow_flagged_keys() {
        let config = serde_json::json!({
            "url": "https://central.example.org",
            "token": "super-secret-token",
            "nested": {"authorization": "Bearer abc", "plain": "visible"}
        });
        let secrets = secret_values(&config, false);
        assert!(secrets.contains(&"super-secret-token".to_string()));
        assert!(secrets.contains(&"Bearer abc".to_string()));
        assert!(!secrets.contains(&"visible".to_string()));
        assert!(!secrets.contains(&"https://central.example.org".to_string()));
    }

    #[test]
    fn safe_config_keeps_only_allowlisted_fields() {
        let config = serde_json::json!({
            "runtime": "runsc",
            "url": "https://central.example.org",
            "direct_host": "gpu.example.org",
            "watchdog_timeout": 180,
            "min_disk_free_bytes": -5,
            "limits": {"mem_limit": 123, "bogus": 1, "pids_limit": -1},
            "token": "secret"
        });
        let safe = safe_config(&config);
        assert_eq!(safe["runtime"], "runsc");
        assert_eq!(safe["central_configured"], true);
        assert_eq!(safe["direct_configured"], true);
        assert_eq!(safe["watchdog_timeout"], 180);
        assert!(safe.get("min_disk_free_bytes").is_none());
        assert_eq!(safe["limits"], serde_json::json!({"mem_limit": 123}));
        assert!(safe.get("token").is_none());
    }

    #[test]
    fn safe_config_defaults_runtime_to_default() {
        let safe = safe_config(&serde_json::json!({"runtime": "bogus"}));
        assert_eq!(safe["runtime"], "default");
        assert_eq!(safe_config(&serde_json::json!({}))["runtime"], "default");
    }

    #[test]
    fn json_file_rejects_oversize_and_non_objects() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("input.json");
        std::fs::write(&path, "{\"a\": 1}").unwrap();
        assert!(json_file(&path, 4).is_err());
        std::fs::write(&path, "[1, 2]").unwrap();
        assert!(json_file(&path, 1024).is_err());
        std::fs::write(&path, "{\"a\": 1}").unwrap();
        assert!(json_file(&path, 1024).is_ok());
    }

    #[test]
    fn resource_summary_respects_accepting_flag() {
        let state = serde_json::json!({
            "manual_enabled": true,
            "auto_busy": false,
            "resource_pool": {
                "memory_bytes": 8 * 1024 * 1024 * 1024u64,
                "disk_bytes": 100 * 1024 * 1024 * 1024u64,
                "default_memory_bytes": 2 * 1024 * 1024 * 1024u64,
                "default_disk_bytes": 25 * 1024 * 1024 * 1024u64
            },
            "leases": {
                "a": {"status": "running", "resources": {
                    "memory_bytes": 2 * 1024 * 1024 * 1024u64,
                    "disk_bytes": 25 * 1024 * 1024 * 1024u64
                }}
            }
        });
        let summary = resource_summary(&state).unwrap();
        assert_eq!(
            summary["available"]["memory_bytes"],
            6 * 1024 * 1024 * 1024u64
        );
        assert!(resource_summary(&serde_json::json!({"leases": {}}))
            .unwrap()
            .is_null());
    }

    #[test]
    fn bundle_is_created_with_private_permissions() {
        let state = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();
        let output = output_dir.path().join("bundle.tar.gz");
        let log = state.path().join("provider.log");
        std::fs::write(&log, "line without secrets\n").unwrap();
        let created = create_bundle(
            state.path(),
            &output,
            None,
            Some(&log),
            Path::new("/var/run/docker.sock"),
            true,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&created).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let file = std::fs::File::open(&created).unwrap();
        let decoder = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        let names: Vec<String> = archive
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap().path().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"report.json".to_string()));
        assert!(names.contains(&"provider.log".to_string()));
        assert!(names.contains(&"README.txt".to_string()));
        assert!(create_bundle(
            state.path(),
            &output,
            None,
            None,
            Path::new("/var/run/docker.sock"),
            true
        )
        .is_err());
    }
}
