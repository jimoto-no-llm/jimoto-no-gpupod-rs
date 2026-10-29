//! 提供 PC 設定の読み出しと検証（Python 版 daemon.load_config の移植）。

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use gpupod_common::identifier::endpoint_host;
use gpupod_common::AgentError;

use super::url::{scheme_allowed, urlsplit};

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct ProviderConfig {
    pub url: String,
    pub node_id: String,
    pub token: String,
    pub relay_host: String,
    pub relay_host_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_bind: Option<String>,
    #[serde(default)]
    pub limits: BTreeMap<String, i64>,
    #[serde(default)]
    pub min_disk_free_bytes: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(default = "default_watchdog")]
    pub watchdog_timeout: f64,
}

fn default_watchdog() -> f64 {
    180.0
}

/// Python 版 load_config と同じ規則で設定を検証する。
pub fn load_provider_config(path: &Path) -> Result<ProviderConfig> {
    let raw = std::fs::read_to_string(path).map_err(|exc| {
        AgentError::new(format!("Invalid provider configuration: {exc}"))
    })?;
    let value: serde_json::Value = serde_json::from_str(&raw).map_err(|exc| {
        AgentError::new(format!("Invalid provider configuration: {exc}"))
    })?;
    parse_provider_config(&value)
}

pub fn parse_provider_config(value: &serde_json::Value) -> Result<ProviderConfig> {
    let object = value
        .as_object()
        .ok_or_else(|| AgentError::new("Invalid provider configuration: not a JSON object"))?;
    let url = required(object, "url")?;
    let parts = urlsplit(&url);
    let local = parts.is_loopback_host();
    if !scheme_allowed(&parts.scheme, local) || parts.username().is_some() || parts.password().is_some()
        || !parts.query.is_empty() || !parts.fragment.is_empty()
    {
        return Err(AgentError::new(
            "Central URL must use HTTPS (HTTP is allowed only for loopback demos)",
        ).into());
    }
    let relay_host = required(object, "relay_host")?;
    let relay_host_key = required(object, "relay_host_key")?;
    endpoint_host(&relay_host)?;
    if let Some(direct_host) = optional(object, "direct_host") {
        endpoint_host(&direct_host)?;
    }
    if let Some(direct_bind) = optional(object, "direct_bind") {
        direct_bind
            .parse::<std::net::IpAddr>()
            .map_err(|_| AgentError::new("Invalid provider configuration: invalid direct_bind"))?;
    }
    let mut limits = BTreeMap::new();
    if let Some(raw) = object.get("limits") {
        let map = raw.as_object().ok_or_else(|| AgentError::new("Unknown resource budget"))?;
        for (key, item) in map {
            if !matches!(
                key.as_str(),
                "nano_cpus" | "mem_limit" | "pids_limit" | "shm_size"
            ) {
                return Err(AgentError::new("Unknown resource budget").into());
            }
            let number = item.as_i64().ok_or_else(|| {
                AgentError::new("Resource budgets must be positive integers in bytes/nanocpus/count")
            })?;
            if number <= 0 {
                return Err(AgentError::new(
                    "Resource budgets must be positive integers in bytes/nanocpus/count",
                ).into());
            }
            limits.insert(key.clone(), number);
        }
    }
    let min_disk_free_bytes = match object.get("min_disk_free_bytes") {
        None => 0,
        Some(serde_json::Value::Null) => 0,
        Some(item) => item.as_i64().filter(|value| *value >= 0).ok_or_else(|| {
            AgentError::new("Invalid provider configuration: invalid disk reserve")
        })?,
    };
    let runtime = match object.get("runtime") {
        None | Some(serde_json::Value::Null) => None,
        Some(item) => {
            let text = item.as_str().ok_or_else(|| {
                AgentError::new("Invalid provider configuration: runtime must be runc or runsc")
            })?;
            if text != "runc" && text != "runsc" {
                return Err(AgentError::new(
                    "Invalid provider configuration: runtime must be runc or runsc",
                ).into());
            }
            Some(text.to_string())
        }
    };
    let watchdog_timeout = match object.get("watchdog_timeout") {
        None => 180.0,
        Some(item) => {
            let number = item.as_f64().ok_or_else(|| {
                AgentError::new("Invalid provider configuration: watchdog timeout must be at least 30 seconds")
            })?;
            if number < 30.0 {
                return Err(AgentError::new(
                    "Invalid provider configuration: watchdog timeout must be at least 30 seconds",
                ).into());
            }
            number
        }
    };
    let direct_host = optional(object, "direct_host");
    let direct_bind = optional(object, "direct_bind");
    if direct_host.is_some() && direct_bind.is_none() {
        return Err(AgentError::new(
            "Invalid provider configuration: direct connections require both direct_host and direct_bind",
        ).into());
    }
    super::sshkey::validate_public_key(&relay_host_key)?;
    Ok(ProviderConfig {
        url: url.trim_end_matches('/').to_string(),
        node_id: required(object, "node_id")?,
        token: required(object, "token")?,
        relay_host,
        relay_host_key,
        direct_host,
        direct_bind,
        limits,
        min_disk_free_bytes,
        runtime,
        watchdog_timeout,
    })
}

/// update-agent 用の updater 設定。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct UpdaterConfig {
    pub repository_url: String,
    pub trusted_root: String,
    pub install_root: String,
    pub provider_state: String,
    #[serde(default = "default_service")]
    pub service: String,
    #[serde(default = "default_health_timeout")]
    pub health_timeout: u64,
}

fn default_service() -> String {
    "gpupod-provider.service".into()
}

fn default_health_timeout() -> u64 {
    60
}

pub fn load_updater_config(path: &Path) -> Result<UpdaterConfig> {
    let raw = std::fs::read_to_string(path)
        .map_err(|exc| AgentError::new(format!("Invalid updater configuration: {exc}")))?;
    let config: UpdaterConfig = serde_json::from_str(&raw)
        .map_err(|exc| AgentError::new(format!("Invalid updater configuration: {exc}")))?;
    if !super::repo::valid_service_name(&config.service) {
        return Err(AgentError::new("Invalid provider service name").into());
    }
    Ok(config)
}

/// https のみ（loopback の http はテスト用に許可）。
pub fn checked_url(value: &str) -> Result<String> {
    let parts = urlsplit(value);
    if parts.username().is_some()
        || parts.password().is_some()
        || !parts.query.is_empty()
        || !parts.fragment.is_empty()
        || !scheme_allowed(&parts.scheme, parts.is_loopback_host())
    {
        return Err(AgentError::new(
            "Update repository must use HTTPS; loopback HTTP is allowed for tests",
        )
        .into());
    }
    Ok(value.trim_end_matches('/').to_string())
}

fn required(map: &serde_json::Map<String, serde_json::Value>, key: &str) -> Result<String> {
    match map.get(key).and_then(|v| v.as_str()) {
        Some(text) if !text.is_empty() => Ok(text.to_string()),
        _ => Err(AgentError::new(format!("Missing {key}")).into()),
    }
}

fn optional(map: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    map.get(key)
        .and_then(|v| v.as_str())
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid() -> serde_json::Value {
        json!({
            "url": "https://central.example.org/",
            "node_id": "node-1",
            "token": "secret-token",
            "relay_host": "relay.example.org",
            "relay_host_key": "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFakeFakeFakeFakeFakeFakeFakeFake000000"
        })
    }

    #[test]
    fn accepts_a_valid_configuration() {
        let config = parse_provider_config(&valid()).unwrap();
        assert_eq!(config.url, "https://central.example.org");
        assert_eq!(config.watchdog_timeout, 180.0);
        assert_eq!(config.runtime, None);
    }

    #[test]
    fn allows_loopback_http_only() {
        let mut value = valid();
        value["url"] = json!("http://127.0.0.1:8000");
        assert!(parse_provider_config(&value).is_ok());
        let mut value = valid();
        value["url"] = json!("http://central.example.org");
        assert!(parse_provider_config(&value).is_err());
    }

    #[test]
    fn rejects_missing_and_unknown_limits() {
        let mut value = valid();
        value.as_object_mut().unwrap().remove("token");
        let error = parse_provider_config(&value).unwrap_err().to_string();
        assert!(error.contains("Missing token"), "{error}");
        let mut value = valid();
        value["limits"] = json!({"bogus": 1});
        assert!(parse_provider_config(&value).is_err());
        let mut value = valid();
        value["limits"] = json!({"mem_limit": -1});
        assert!(parse_provider_config(&value).is_err());
    }

    #[test]
    fn direct_connections_need_both_fields() {
        let mut value = valid();
        value["direct_host"] = json!("gpu.example.org");
        assert!(parse_provider_config(&value).is_err());
        value["direct_bind"] = json!("0.0.0.0");
        assert!(parse_provider_config(&value).is_ok());
    }

    #[test]
    fn runtime_and_watchdog_are_constrained() {
        let mut value = valid();
        value["runtime"] = json!("runc");
        assert!(parse_provider_config(&value).is_ok());
        value["runtime"] = json!("bogus");
        assert!(parse_provider_config(&value).is_err());
        let mut value = valid();
        value["watchdog_timeout"] = json!(10);
        assert!(parse_provider_config(&value).is_err());
    }

    #[test]
    fn checked_url_requires_https() {
        assert!(checked_url("https://repo.example.org/").is_ok());
        assert!(checked_url("http://repo.example.org").is_err());
        assert!(checked_url("http://localhost:8000/releases").is_ok());
    }
}