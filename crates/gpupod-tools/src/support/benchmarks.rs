//! 測定結果のスキーマと保存（Python 版 benchmarks.py の移植）。

use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use gpupod_common::AgentError;

pub const MIB: i64 = 1024 * 1024;
pub const MAX_NETWORK_BYTES: i64 = 64 * MIB;
pub const MAX_RESULT_BYTES: usize = 16384;

pub fn timestamp() -> String {
    Utc::now().to_rfc3339()
}

/// 検証済みの公開フィールドだけを返す（任意 JSON をそのまま公開しない）。
pub fn public_results(value: &serde_json::Value, now: DateTime<Utc>) -> serde_json::Map<String, serde_json::Value> {
    let mut result = serde_json::Map::new();
    let Some(object) = value.as_object() else { return result };
    if object.get("version").and_then(|v| v.as_i64()) != Some(1) {
        return result;
    }
    for kind in ["disk", "network"] {
        let Some(item) = object.get(kind).and_then(|v| v.as_object()) else {
            continue;
        };
        let Some(measured) = item
            .get("measured_at")
            .and_then(|v| v.as_str())
            .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        else {
            continue;
        };
        let measured = measured.with_timezone(&Utc);
        if measured > Utc::now() + chrono::Duration::minutes(5) {
            continue;
        }
        let Some(size) = item.get("size_bytes").and_then(|v| v.as_i64()) else {
            continue;
        };
        let limit = if kind == "disk" { 4096 * MIB } else { MAX_NETWORK_BYTES };
        if size < MIB || size > limit {
            continue;
        }
        let stale = now - measured > chrono::Duration::days(7);
        let mut clean = serde_json::json!({
            "measured_at": measured.to_rfc3339(),
            "size_bytes": size,
            "size_mib": round1(size as f64 / MIB as f64),
            "stale": stale,
        });
        if kind == "disk" {
            if item.get("method").and_then(|v| v.as_str()) != Some("fio-sequential-1MiB-qd1")
                || item.get("direct_io").and_then(|v| v.as_bool()) != Some(true)
            {
                continue;
            }
            let (Some(read), Some(write)) = (
                item.get("read_mib_s").and_then(valid_number),
                item.get("write_mib_s").and_then(valid_number),
            ) else {
                continue;
            };
            clean["read_mib_s"] = serde_json::json!(read);
            clean["write_mib_s"] = serde_json::json!(write);
            clean["method"] = serde_json::json!("fio-sequential-1MiB-qd1");
            clean["direct_io"] = serde_json::json!(true);
        } else {
            if item.get("scope").and_then(|v| v.as_str()) != Some("central") {
                continue;
            }
            let (Some(download), Some(upload)) = (
                item.get("download_mbps").and_then(valid_number),
                item.get("upload_mbps").and_then(valid_number),
            ) else {
                continue;
            };
            clean["download_mbps"] = serde_json::json!(download);
            clean["upload_mbps"] = serde_json::json!(upload);
            clean["scope"] = serde_json::json!("central");
        }
        result.insert(kind.to_string(), clean);
    }
    result
}

fn valid_number(value: &serde_json::Value) -> Option<f64> {
    let number = value.as_f64()?;
    if !number.is_finite() || number <= 0.0 || number >= 1e9 {
        return None;
    }
    Some((number * 1000.0).round() / 1000.0)
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

pub fn read_results(state_dir: &Path) -> serde_json::Value {
    let Ok(raw) = std::fs::read(state_dir.join("benchmarks.json")) else {
        return serde_json::json!({});
    };
    if raw.len() > MAX_RESULT_BYTES {
        return serde_json::json!({});
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return serde_json::json!({});
    };
    serde_json::Value::Object(public_results(&value, Utc::now()))
}

pub fn save_result(state_dir: &Path, kind: &str, measurement: &serde_json::Value) -> Result<()> {
    let old = read_results(state_dir);
    let mut wrapper = serde_json::Map::new();
    wrapper.insert("version".into(), serde_json::json!(1));
    wrapper.insert(kind.to_string(), measurement.clone());
    let clean = public_results(&serde_json::Value::Object(wrapper), Utc::now());
    if !clean.contains_key(kind) {
        return Err(AgentError::new("Invalid benchmark result; previous measurement retained").into());
    }
    let mut merged = old.as_object().cloned().unwrap_or_default();
    for (key, value) in clean {
        merged.insert(key, value);
    }
    let mut document = serde_json::Map::new();
    document.insert("version".into(), serde_json::json!(1));
    document.extend(merged);
    super::files::atomic_json(&state_dir.join("benchmarks.json"), &serde_json::Value::Object(document))
        .context("Cannot store the benchmark result")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_disk() -> serde_json::Value {
        serde_json::json!({
            "measured_at": Utc::now().to_rfc3339(),
            "size_bytes": 256 * MIB,
            "read_mib_s": 900.1234,
            "write_mib_s": 800.5,
            "method": "fio-sequential-1MiB-qd1",
            "direct_io": true,
        })
    }

    #[test]
    fn accepts_valid_disk_results() {
        let value = serde_json::json!({"version": 1, "disk": sample_disk()});
        let clean = public_results(&value, Utc::now());
        let disk = clean.get("disk").unwrap();
        assert_eq!(disk["size_mib"], 256.0);
        assert_eq!(disk["read_mib_s"], 900.123);
        assert_eq!(disk["stale"], false);
    }

    #[test]
    fn rejects_invalid_and_future_results() {
        let future = serde_json::json!({
            "version": 1,
            "network": {
                "measured_at": (Utc::now() + chrono::Duration::hours(2)).to_rfc3339(),
                "size_bytes": 16 * MIB,
                "scope": "central",
                "download_mbps": 100.0,
                "upload_mbps": 50.0,
            }
        });
        assert!(public_results(&future, Utc::now()).is_empty());
        let bad_method = serde_json::json!({"version": 1, "disk": {
            "measured_at": "2026-01-01T00:00:00+00:00",
            "size_bytes": 256 * MIB,
            "read_mib_s": 1.0, "write_mib_s": 1.0,
            "method": "other", "direct_io": true
        }});
        assert!(public_results(&bad_method, Utc::now()).is_empty());
        let zero = serde_json::json!({"version": 1, "disk": {
            "measured_at": "2026-01-01T00:00:00+00:00",
            "size_bytes": 256 * MIB,
            "read_mib_s": 0.0, "write_mib_s": 1.0,
            "method": "fio-sequential-1MiB-qd1", "direct_io": true
        }});
        assert!(public_results(&zero, Utc::now()).is_empty());
    }

    #[test]
    fn merges_saved_results() {
        let directory = tempfile::tempdir().unwrap();
        save_result(directory.path(), "disk", &sample_disk()).unwrap();
        let network = serde_json::json!({
            "measured_at": "2026-01-01T00:00:00+00:00",
            "size_bytes": 16 * MIB,
            "scope": "central",
            "download_mbps": 120.0,
            "upload_mbps": 60.0,
        });
        save_result(directory.path(), "network", &network).unwrap();
        let merged = read_results(directory.path());
        assert!(merged.get("disk").is_some() && merged.get("network").is_some());
    }

    #[test]
    fn stale_results_expire_after_a_week() {
        let old = serde_json::json!({
            "version": 1,
            "disk": {
                "measured_at": "2020-01-01T00:00:00+00:00",
                "size_bytes": 256 * MIB,
                "read_mib_s": 1.0, "write_mib_s": 1.0,
                "method": "fio-sequential-1MiB-qd1", "direct_io": true
            }
        });
        let clean = public_results(&old, Utc::now());
        assert_eq!(clean["disk"]["stale"], true);
    }
}
