//! 遊休時は使わない、範囲を限った性能測定（Python 版 benchmarks.py の移植）。

use std::path::Path;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use gpupod_common::AgentError;

use crate::files::atomic_json;

pub const MIB: u64 = 1024 * 1024;
pub const MAX_NETWORK_BYTES: u64 = 64 * MIB;
pub const MAX_RESULT_BYTES: u64 = 16384;

/// 現在時刻（UTC）。
pub fn timestamp() -> String {
    Utc::now().to_rfc3339()
}

/// 丸め。Python 版 round(value, digits) と同じ（小数第 n 位で四捨五入）。
fn rounded(value: f64, digits: usize) -> f64 {
    format!("{value:.digits$}").parse::<f64>().unwrap_or(value)
}

/// 測定値の検証。既知の範囲だけを通す。
fn measured_number(value: Option<&serde_json::Value>) -> Option<f64> {
    let number = match value? {
        serde_json::Value::Number(number) => number.as_f64(),
        _ => None,
    }?;
    if !number.is_finite() || !(0.0 < number) || number >= 1e9 {
        return None;
    }
    Some(number)
}

/// 日時文字列を UTC に読む。タイムゾーン無しは Python 版と同じく無効扱い。
fn parse_measured(value: &serde_json::Value) -> Option<DateTime<Utc>> {
    let text = value.as_str()?;
    let parsed = DateTime::parse_from_rfc3339(text).ok();
    parsed.map(|value| value.with_timezone(&Utc))
}

/// 検証済みの公開結果だけを返す（Python 版 public_results）。
pub fn public_results(value: &serde_json::Value, now: Option<DateTime<Utc>>) -> serde_json::Value {
    let now = now.unwrap_or_else(Utc::now);
    if value.get("version").and_then(|v| v.as_i64()) != Some(1) {
        return serde_json::json!({});
    }
    let mut result = serde_json::Map::new();
    for kind in ["disk", "network"] {
        let Some(item) = value.get(kind).and_then(|v| v.as_object()) else {
            continue;
        };
        let process = || -> Option<serde_json::Map<String, serde_json::Value>> {
            let measured = parse_measured(item.get("measured_at")?)?;
            if measured > now + ChronoDuration::minutes(5) {
                return None;
            }
            let size = match item.get("size_bytes") {
                Some(serde_json::Value::Number(number)) => number.as_u64()?,
                _ => return None,
            };
            let limit = if kind == "disk" {
                4096 * MIB
            } else {
                MAX_NETWORK_BYTES
            };
            if !(MIB..=limit).contains(&size) {
                return None;
            }
            let mut clean = serde_json::Map::new();
            clean.insert(
                "measured_at".into(),
                serde_json::Value::String(measured.to_rfc3339()),
            );
            clean.insert("size_bytes".into(), serde_json::json!(size));
            clean.insert(
                "size_mib".into(),
                serde_json::json!(rounded(size as f64 / MIB as f64, 1)),
            );
            clean.insert(
                "stale".into(),
                serde_json::json!(now - measured > ChronoDuration::days(7)),
            );
            if kind == "disk" {
                if item.get("method").and_then(|v| v.as_str()) != Some("fio-sequential-1MiB-qd1")
                    || item.get("direct_io").and_then(|v| v.as_bool()) != Some(true)
                {
                    return None;
                }
                clean.insert(
                    "read_mib_s".into(),
                    serde_json::json!(rounded(measured_number(item.get("read_mib_s"))?, 3)),
                );
                clean.insert(
                    "write_mib_s".into(),
                    serde_json::json!(rounded(measured_number(item.get("write_mib_s"))?, 3)),
                );
                clean.insert(
                    "method".into(),
                    serde_json::json!("fio-sequential-1MiB-qd1"),
                );
                clean.insert("direct_io".into(), serde_json::json!(true));
            } else {
                if item.get("scope").and_then(|v| v.as_str()) != Some("central") {
                    return None;
                }
                clean.insert(
                    "download_mbps".into(),
                    serde_json::json!(rounded(measured_number(item.get("download_mbps"))?, 3)),
                );
                clean.insert(
                    "upload_mbps".into(),
                    serde_json::json!(rounded(measured_number(item.get("upload_mbps"))?, 3)),
                );
                clean.insert("scope".into(), serde_json::json!("central"));
            }
            Some(clean)
        };
        if let Some(clean) = process() {
            result.insert(kind.to_owned(), serde_json::Value::Object(clean));
        }
    }
    serde_json::Value::Object(result)
}

/// benchmarks.json を上限付きで読む。
pub fn read_results(state_dir: impl AsRef<Path>) -> serde_json::Value {
    let path = state_dir.as_ref().join("benchmarks.json");
    let Ok(file) = std::fs::File::open(&path) else {
        return serde_json::json!({});
    };
    use std::io::Read;
    let mut raw = Vec::new();
    let Ok(_) = file.take(MAX_RESULT_BYTES + 1).read_to_end(&mut raw) else {
        return serde_json::json!({});
    };
    if raw.len() as u64 > MAX_RESULT_BYTES {
        return serde_json::json!({});
    }
    match serde_json::from_slice::<serde_json::Value>(&raw) {
        Ok(value) => public_results(&value, None),
        Err(_) => serde_json::json!({}),
    }
}

/// 1 件の測定を検証して保存する。不正なら以前の結果を保つ。
pub fn save_result(
    state_dir: impl AsRef<Path>,
    kind: &str,
    measurement: serde_json::Value,
) -> Result<(), AgentError> {
    let state_dir = state_dir.as_ref();
    let mut old = read_results(state_dir);
    let mut wrapper = serde_json::Map::new();
    wrapper.insert("version".into(), serde_json::json!(1));
    wrapper.insert(kind.to_owned(), measurement);
    let clean = public_results(&serde_json::Value::Object(wrapper), None);
    let Some(entry) = clean.get(kind).cloned() else {
        return Err(AgentError::new(
            "Invalid benchmark result; previous measurement retained",
        ));
    };
    if let Some(object) = old.as_object_mut() {
        object.insert(kind.to_owned(), entry);
        object.insert("version".into(), serde_json::json!(1));
    } else {
        old = serde_json::json!({ "version": 1, kind: entry });
    }
    atomic_json(state_dir.join("benchmarks.json"), &old)?;
    Ok(())
}
