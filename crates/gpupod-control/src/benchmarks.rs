//! 回線・ディスク測定（portal/benchmarks.py と provider_agent.benchmarks.public_results の移植）。

use chrono::{DateTime, Duration, Utc};
use rusqlite::params;

use crate::config::BENCHMARK_COOLDOWN_SECONDS;
use crate::db::{fmt_dt, now, parse_dt};

pub const MIB: u64 = 1024 * 1024;
pub const MAX_NETWORK_BYTES: u64 = 64 * MIB;

/// 公開してよい測定結果 1 件（disk）。
#[derive(Debug, Clone)]
pub struct PublicDisk {
    pub measured_at: String,
    pub size_bytes: u64,
    pub size_mib: f64,
    pub stale: bool,
    pub read_mib_s: f64,
    pub write_mib_s: f64,
    pub method: String,
    pub direct_io: bool,
}

/// 公開してよい測定結果 1 件（network）。
#[derive(Debug, Clone)]
pub struct PublicNetwork {
    pub measured_at: String,
    pub size_bytes: u64,
    pub size_mib: f64,
    pub stale: bool,
    pub download_mbps: f64,
    pub upload_mbps: f64,
    pub scope: String,
}

/// public_results の結果。
#[derive(Debug, Clone, Default)]
pub struct PublicResults {
    pub disk: Option<PublicDisk>,
    pub network: Option<PublicNetwork>,
}

/// _number: 有限で 0 < v < 1e9 の値を小数 3 桁へ。
fn number(value: Option<&serde_json::Value>) -> Option<f64> {
    let raw = match value {
        Some(serde_json::Value::Number(number)) => number.as_f64()?,
        _ => return None,
    };
    if raw.is_finite() && raw > 0.0 && raw < 1e9 {
        Some((raw * 1000.0).round() / 1000.0)
    } else {
        None
    }
}

/// タイムゾーン付きのみ受理（Python 版は naive な測定日時を捨てる）。
fn parse_iso(value: &str) -> Option<DateTime<Utc>> {
    let normalized = if value.ends_with('Z') {
        format!("{}+00:00", &value[..value.len() - 1])
    } else {
        value.to_string()
    };
    DateTime::parse_from_rfc3339(&normalized)
        .ok()
        .map(|parsed| parsed.with_timezone(&Utc))
}

/// Python の datetime.isoformat() 相当（秒なら小数なし、+00:00 付き）。
fn isoformat(value: DateTime<Utc>) -> String {
    let micro = value.timestamp_subsec_micros();
    let base = value.format("%Y-%m-%dT%H:%M:%S").to_string();
    if micro == 0 {
        format!("{base}+00:00")
    } else {
        format!("{}.{:06}+00:00", base, micro)
    }
}

/// public_results: 知られた公開フィールドだけを検証して返す。
pub fn public_results(statistics: &serde_json::Value, now_value: DateTime<Utc>) -> PublicResults {
    let mut results = PublicResults::default();
    let root = match statistics.as_object() {
        Some(root) => root,
        None => return results,
    };
    match root.get("version").and_then(|value| value.as_i64()) {
        Some(1) => {}
        _ => return results,
    }
    if let Some(item) = root.get("disk") {
        results.disk = clean_disk(item, now_value);
    }
    if let Some(item) = root.get("network") {
        results.network = clean_network(item, now_value);
    }
    results
}

fn common_fields(
    item: &serde_json::Value,
    kind_max: u64,
    now_value: DateTime<Utc>,
) -> Option<(String, u64, f64, bool)> {
    let map = item.as_object()?;
    let measured = parse_iso(map.get("measured_at")?.as_str()?)?;
    if measured > now_value + Duration::minutes(5) {
        return None;
    }
    let size = match map.get("size_bytes")? {
        serde_json::Value::Number(number) if number.is_i64() || number.is_u64() => {
            number.as_u64()?
        }
        _ => return None,
    };
    if !(MIB..=kind_max).contains(&size) {
        return None;
    }
    let stale = now_value - measured > Duration::days(7);
    Some((isoformat(measured), size, size as f64 / MIB as f64, stale))
}

fn clean_disk(item: &serde_json::Value, now_value: DateTime<Utc>) -> Option<PublicDisk> {
    let map = item.as_object()?;
    if map.get("method").and_then(|value| value.as_str()) != Some("fio-sequential-1MiB-qd1")
        || map.get("direct_io").and_then(|value| value.as_bool()) != Some(true)
    {
        return None;
    }
    let (measured_at, size_bytes, size_mib, stale) = common_fields(item, 4096 * MIB, now_value)?;
    Some(PublicDisk {
        measured_at,
        size_bytes,
        size_mib: (size_mib * 10.0).round() / 10.0,
        stale,
        read_mib_s: number(map.get("read_mib_s"))?,
        write_mib_s: number(map.get("write_mib_s"))?,
        method: map.get("method")?.as_str()?.to_string(),
        direct_io: true,
    })
}

fn clean_network(item: &serde_json::Value, now_value: DateTime<Utc>) -> Option<PublicNetwork> {
    let map = item.as_object()?;
    if map.get("scope").and_then(|value| value.as_str()) != Some("central") {
        return None;
    }
    let (measured_at, size_bytes, size_mib, stale) =
        common_fields(item, MAX_NETWORK_BYTES, now_value)?;
    Some(PublicNetwork {
        measured_at,
        size_bytes,
        size_mib: (size_mib * 10.0).round() / 10.0,
        stale,
        download_mbps: number(map.get("download_mbps"))?,
        upload_mbps: number(map.get("upload_mbps"))?,
        scope: "central".to_string(),
    })
}

/// _claim: 条件付き UPDATE でクールダウンを確保する（SQLite でもアトミック）。
///
/// 未知の方向は失敗扱いにする（列名を外部入力から決めない）。
pub fn claim(
    conn: &rusqlite::Connection,
    node_hex: &str,
    direction: &str,
) -> Result<bool, rusqlite::Error> {
    let field = match direction {
        "download" => "benchmark_download_after",
        "upload" => "benchmark_upload_after",
        _ => return Ok(false),
    };
    let current_value = now();
    let current = fmt_dt(current_value);
    let next = fmt_dt(current_value + Duration::seconds(BENCHMARK_COOLDOWN_SECONDS));
    let updated = conn.execute(
        &format!(
            "UPDATE portal_node SET {field} = ?2
             WHERE id = ?1 AND enabled AND ({field} IS NULL OR {field} <= ?3)"
        ),
        params![node_hex, next, current],
    )?;
    Ok(updated > 0)
}

/// 統計 statistics から測定結果を読む。
pub fn results_from_statistics(statistics: &serde_json::Value) -> PublicResults {
    public_results(statistics, now())
}

/// クールダウン時刻の残り秒。
pub fn cooldown_remaining(value: Option<&str>) -> Option<i64> {
    let parsed = parse_dt(value?)?;
    let remaining = (parsed - now()).num_seconds();
    (remaining > 0).then_some(remaining)
}
