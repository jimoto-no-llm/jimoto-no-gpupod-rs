//! RAM / ディスクの提供枠・予約計算（Python 版 provider_agent.resources の移植）。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{AgentError, Result};

pub const MIB: u64 = 1024 * 1024;
pub const GIB: u64 = 1024 * 1024 * 1024;
pub const MAXIMUM: u64 = 1024u64.pow(6);
pub const FIELDS: [&str; 2] = ["memory_bytes", "disk_bytes"];

/// Python 版 MINIMUM: RAM 64 MiB / ディスク 16 MiB。
pub fn minimum(field: &str) -> u64 {
    match field {
        "memory_bytes" => 64 * MIB,
        _ => 16 * MIB,
    }
}

/// 整数バイト数の検証（size_bytes）。
pub fn size_bytes(value: u64, field: &str) -> Result<u64> {
    if value < minimum(field) || value > MAXIMUM || value % MIB != 0 {
        return Err(AgentError::new(
            "RAM must be at least 64 MiB and disk at least 16 MiB, in whole MiB",
        ));
    }
    Ok(value)
}

/// GiB 表記文字列を整数バイト数へ（gib_bytes）。Python 版は Decimal で厳密に扱う。
pub fn gib_bytes(value: &str, field: &str) -> Result<u64> {
    if value.len() > 64 || value.is_empty() {
        return Err(AgentError::new(
            "Enter a positive GiB capacity in whole MiB increments",
        ));
    }
    // "8" / "0.0625" / "8.5" のような十進表記を分数で厳密に評価する。
    let (int_part, frac_part) = match value.split_once('.') {
        Some((a, b)) => (a, b),
        None => (value, ""),
    };
    if !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
        || (int_part.is_empty() && frac_part.is_empty())
    {
        return Err(AgentError::new(
            "Enter a positive GiB capacity in whole MiB increments",
        ));
    }
    let digits = format!("{int_part}{frac_part}");
    let numerator: u128 = digits
        .parse()
        .map_err(|_| AgentError::new("Enter a positive GiB capacity in whole MiB increments"))?;
    let scale = 10u128.pow(frac_part.len() as u32);
    let amount = numerator * GIB as u128;
    if amount % scale != 0 {
        return Err(AgentError::new(
            "Enter a positive GiB capacity in whole MiB increments",
        ));
    }
    let bytes = amount / scale;
    let bytes = u64::try_from(bytes)
        .map_err(|_| AgentError::new("Enter a positive GiB capacity in whole MiB increments"))?;
    size_bytes(bytes, field)
}

/// 提供者の提供枠設定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pool {
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub default_memory_bytes: u64,
    pub default_disk_bytes: u64,
}

impl Pool {
    pub fn validate(&self) -> Result<Pool> {
        let memory = size_bytes(self.memory_bytes, "memory_bytes")?;
        let disk = size_bytes(self.disk_bytes, "disk_bytes")?;
        let default_memory = size_bytes(self.default_memory_bytes, "memory_bytes")?;
        let default_disk = size_bytes(self.default_disk_bytes, "disk_bytes")?;
        if default_memory > memory || default_disk > disk {
            return Err(AgentError::new(
                "Default lease capacity exceeds the configured pool",
            ));
        }
        Ok(Pool {
            memory_bytes: memory,
            disk_bytes: disk,
            default_memory_bytes: default_memory,
            default_disk_bytes: default_disk,
        })
    }

    pub fn from_value(value: serde_json::Value) -> Result<Pool> {
        let pool: Pool = serde_json::from_value(value)
            .map_err(|_| AgentError::new("Invalid resource pool configuration"))?;
        pool.validate()
    }
}

/// 貸出ごとの予約量。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Allocation {
    pub memory_bytes: u64,
    pub disk_bytes: u64,
}

impl Allocation {
    pub fn validate(&self) -> Result<Allocation> {
        Ok(Allocation {
            memory_bytes: size_bytes(self.memory_bytes, "memory_bytes")?,
            disk_bytes: size_bytes(self.disk_bytes, "disk_bytes")?,
        })
    }

    pub fn get(&self, field: &str) -> u64 {
        match field {
            "memory_bytes" => self.memory_bytes,
            _ => self.disk_bytes,
        }
    }
}

/// 未停止の貸出の予約合計。require_all では欠落をエラーにする。
pub fn used_resources(
    leases: &[Option<Allocation>],
    require_all: bool,
) -> Result<Allocation> {
    let mut used = Allocation { memory_bytes: 0, disk_bytes: 0 };
    for lease in leases {
        match lease {
            None => {
                if require_all {
                    return Err(AgentError::new(
                        "An active lease has no resource allocation; return it before configuring a pool",
                    ));
                }
            }
            Some(value) => {
                let value = value.validate()?;
                used.memory_bytes += value.memory_bytes;
                used.disk_bytes += value.disk_bytes;
            }
        }
    }
    Ok(used)
}

/// 新規貸出の予約判定。枠が未設定なら None（従来動作を維持）。
pub fn claim(
    pool: Option<&Pool>,
    leases: &[Option<Allocation>],
    memory_bytes: Option<u64>,
    disk_bytes: Option<u64>,
) -> Result<Option<Allocation>> {
    let Some(pool) = pool else {
        if memory_bytes.is_some() || disk_bytes.is_some() {
            return Err(AgentError::new(
                "Configure the provider resource pool before requesting capacity",
            ));
        }
        return Ok(None);
    };
    let pool = pool.validate()?;
    let requested = Allocation {
        memory_bytes: memory_bytes.unwrap_or(pool.default_memory_bytes),
        disk_bytes: disk_bytes.unwrap_or(pool.default_disk_bytes),
    }
    .validate()?;
    let used = used_resources(leases, true)?;
    for field in FIELDS {
        if used.get(field) + requested.get(field) > pool.get_field(field) {
            return Err(AgentError::new(
                "The provider's reserved RAM or disk pool is exhausted",
            ));
        }
    }
    Ok(Some(requested))
}

impl Pool {
    pub fn get_field(&self, field: &str) -> u64 {
        match field {
            "memory_bytes" => self.memory_bytes,
            _ => self.disk_bytes,
        }
    }
}

/// パネル表示用の要約。
pub fn summary(
    pool: Option<&Pool>,
    leases: &[Option<Allocation>],
    accepting: bool,
) -> Result<Option<BTreeMap<String, serde_json::Value>>> {
    let Some(pool) = pool else {
        return Ok(None);
    };
    let pool = pool.validate()?;
    let used = used_resources(leases, true)?;
    for field in FIELDS {
        if used.get(field) > pool.get_field(field) {
            return Err(AgentError::new(
                "Existing allocations exceed the resource pool; reservations retained",
            ));
        }
    }
    let mut map = BTreeMap::new();
    map.insert("memory_bytes".to_string(), serde_json::json!(pool.memory_bytes));
    map.insert("disk_bytes".to_string(), serde_json::json!(pool.disk_bytes));
    map.insert("default_memory_bytes".to_string(), serde_json::json!(pool.default_memory_bytes));
    map.insert("default_disk_bytes".to_string(), serde_json::json!(pool.default_disk_bytes));
    map.insert(
        "allocated".to_string(),
        serde_json::json!({"memory_bytes": used.memory_bytes, "disk_bytes": used.disk_bytes}),
    );
    let mut available = serde_json::Map::new();
    for field in FIELDS {
        let value = if accepting { pool.get_field(field) - used.get(field) } else { 0 };
        available.insert(field.to_string(), serde_json::json!(value));
    }
    map.insert("available".to_string(), serde_json::Value::Object(available));
    Ok(Some(map))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gib_conversion_is_exact() {
        assert_eq!(gib_bytes("8", "memory_bytes").unwrap(), 8 * GIB);
        assert_eq!(gib_bytes("0.0625", "memory_bytes").unwrap(), 64 * MIB);
        assert_eq!(gib_bytes("1.5", "disk_bytes").unwrap(), GIB + GIB / 2);
        assert!(gib_bytes("0.0001", "memory_bytes").is_err());
        assert!(gib_bytes("-1", "memory_bytes").is_err());
        assert!(gib_bytes("abc", "memory_bytes").is_err());
        assert!(gib_bytes(&"9".repeat(70), "memory_bytes").is_err());
    }

    #[test]
    fn claim_respects_pool() {
        let pool = Pool {
            memory_bytes: 8 * GIB,
            disk_bytes: 100 * GIB,
            default_memory_bytes: 2 * GIB,
            default_disk_bytes: 25 * GIB,
        }
        .validate()
        .unwrap();
        let none: Vec<Option<Allocation>> = vec![];
        let first = claim(Some(&pool), &none, None, None).unwrap().unwrap();
        assert_eq!(first.memory_bytes, 2 * GIB);
        let second = claim(Some(&pool), &[Some(first)], None, None).unwrap().unwrap();
        let third = claim(Some(&pool), &[Some(first), Some(second)], None, None).unwrap().unwrap();
        // 4 x 2 GiB = 8 GiB で枠いっぱい。5つ目は失敗する。
        let fourth = claim(Some(&pool), &[Some(first), Some(second), Some(third)], None, None)
            .unwrap()
            .unwrap();
        assert!(claim(
            Some(&pool),
            &[Some(first), Some(second), Some(third), Some(fourth)],
            None,
            None
        )
        .is_err());
        assert!(claim(None, &[], Some(GIB), None).is_err());
        assert!(claim(None, &[], None, None).unwrap().is_none());
    }

    #[test]
    fn summary_reports_availability() {
        let pool = Pool {
            memory_bytes: 8 * GIB,
            disk_bytes: 100 * GIB,
            default_memory_bytes: 2 * GIB,
            default_disk_bytes: 25 * GIB,
        };
        let leases = vec![Some(Allocation { memory_bytes: 2 * GIB, disk_bytes: 25 * GIB })];
        let map = summary(Some(&pool), &leases, true).unwrap().unwrap();
        assert_eq!(map["available"]["memory_bytes"], 6 * GIB);
        let stopped = summary(Some(&pool), &leases, false).unwrap().unwrap();
        assert_eq!(stopped["available"]["memory_bytes"], 0);
    }
}
