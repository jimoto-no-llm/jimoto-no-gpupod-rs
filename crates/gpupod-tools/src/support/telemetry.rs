//! ホスト指標と GPU 活動の観測（Python 版 telemetry.py の移植）。読取専用。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use gpupod_common::GPU;

/// 外部活動の判定結果。None は「判定不能」。
pub type Activity = Option<bool>;

#[derive(Debug)]
pub struct IdlePolicy {
    pub idle_seconds: f64,
    idle_since: Option<f64>,
}

impl IdlePolicy {
    pub fn new(idle_seconds: f64) -> Self {
        IdlePolicy {
            idle_seconds,
            idle_since: None,
        }
    }

    /// 判定不能は idle の証拠にしない（Python 版と同一の規則）。
    pub fn update(&mut self, activity: Activity, timestamp: f64) -> bool {
        if activity != Some(false) {
            self.idle_since = None;
            return true;
        }
        match self.idle_since {
            None => {
                self.idle_since = Some(timestamp);
                true
            }
            Some(since) => timestamp - since < self.idle_seconds,
        }
    }
}

pub fn collect(gpus: &[GPU], disk_path: &Path) -> serde_json::Value {
    let mut warnings: Vec<String> = Vec::new();
    let mut sensors: BTreeMap<String, BTreeMap<String, serde_json::Value>> = BTreeMap::new();
    if gpus
        .iter()
        .any(|gpu| gpu.vendor == "nvidia" && !gpu.id.starts_with("mock-"))
    {
        match nvidia_sensors() {
            Some(values) => sensors = values,
            None => warnings.push("NVIDIA metrics unavailable".into()),
        }
    }
    let mut values = Vec::new();
    for gpu in gpus {
        let mut value = serde_json::json!({
            "id": gpu.id,
            "vendor": gpu.vendor,
            "mock": gpu.id.starts_with("mock-"),
        });
        if let Some(map) = sensors.get(&gpu.selector) {
            for (key, item) in map {
                value[key] = item.clone();
            }
        }
        for device in &gpu.devices {
            if !device.starts_with("/dev/dri/render") {
                continue;
            }
            let name = Path::new(device)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let root = PathBuf::from("/sys/class/drm").join(name).join("device");
            value["utilization_percent"] = read_number(&root.join("gpu_busy_percent"));
            let used = read_number(&root.join("mem_info_vram_used"));
            value["memory_used_mib"] = match used.as_f64() {
                Some(bytes) => serde_json::json!(bytes / 1_048_576.0),
                None => serde_json::Value::Null,
            };
        }
        values.push(value);
    }
    let memory = host_memory();
    let disk = disk_usage(disk_path).unwrap_or((0, 0));
    serde_json::json!({
        "host": {
            "cpu_percent": cpu_percent(),
            "cpu_count": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
            "memory_total": memory.0,
            "memory_available": memory.1,
            "disk_total": disk.0,
            "disk_free": disk.1,
        },
        "gpus": values,
        "warnings": warnings,
    })
}

fn nvidia_sensors() -> Option<BTreeMap<String, BTreeMap<String, serde_json::Value>>> {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=uuid,utilization.gpu,memory.used,temperature.gpu,power.draw",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut result = BTreeMap::new();
    for row in crate::support::devices::csv_rows(&String::from_utf8_lossy(&output.stdout)) {
        if row.len() < 2 {
            continue;
        }
        let mut map = BTreeMap::new();
        for (index, key) in [
            "utilization_percent",
            "memory_used_mib",
            "temperature_c",
            "power_w",
        ]
        .iter()
        .enumerate()
        {
            map.insert(
                (*key).to_string(),
                row.get(index + 1)
                    .map(|value| read_number_text(value))
                    .unwrap_or(serde_json::Value::Null),
            );
        }
        result.insert(row[0].trim().to_string(), map);
    }
    Some(result)
}

fn read_number_text(value: &str) -> serde_json::Value {
    match value.trim().parse::<f64>() {
        Ok(number) => serde_json::json!(number),
        Err(_) => serde_json::Value::Null,
    }
}

fn read_number(path: &Path) -> serde_json::Value {
    match std::fs::read_to_string(path) {
        Ok(text) => read_number_text(&text),
        Err(_) => serde_json::Value::Null,
    }
}

/// /proc/meminfo から (total, available) バイトを返す。
fn host_memory() -> (u64, u64) {
    let mut total = 0;
    let mut available = 0;
    if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            let key = parts.next().unwrap_or_default();
            let value: u64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            match key {
                "MemTotal:" => total = value * 1024,
                "MemAvailable:" => available = value * 1024,
                _ => {}
            }
        }
    }
    (total, available)
}

/// 150ms のサンプルで /proc/stat を比較し、システム全体の CPU 使用率を返す。
fn cpu_percent() -> f64 {
    let before = read_cpu_times();
    std::thread::sleep(Duration::from_millis(150));
    let after = read_cpu_times();
    let (idle_a, total_a) = before;
    let (idle_b, total_b) = after;
    if total_b <= total_a {
        return 0.0;
    }
    let busy = (total_b - total_a) - (idle_b - idle_a);
    busy as f64 / (total_b - total_a) as f64 * 100.0
}

fn read_cpu_times() -> (u64, u64) {
    let Ok(text) = std::fs::read_to_string("/proc/stat") else {
        return (0, 0);
    };
    let Some(line) = text.lines().next() else {
        return (0, 0);
    };
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .map(|value| value.parse().unwrap_or(0))
        .collect();
    if values.len() < 5 {
        return (0, 0);
    }
    let idle = values[3] + values[4];
    (idle, values.iter().sum())
}

/// statvfs による (total, available) バイト。
pub fn disk_usage(path: &Path) -> std::io::Result<(u64, u64)> {
    use std::ffi::CString;
    let text = path.to_string_lossy().to_string();
    let c_text =
        CString::new(text).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let code = unsafe { libc::statvfs(c_text.as_ptr(), &mut stat) };
    if code != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let size = stat.f_frsize as u64;
    Ok((stat.f_blocks as u64 * size, stat.f_bavail as u64 * size))
}

/// PID が借手コンテナに属するか（cgroup から判定）。
pub fn process_is_borrower(pid: i32, container_ids: &[String], proc_root: &Path) -> Option<bool> {
    let text = std::fs::read_to_string(proc_root.join(pid.to_string()).join("cgroup")).ok()?;
    Some(container_ids.iter().any(|id| text.contains(id.as_str())))
}

/// 借手以外の GPU 利用を True、所有者が分からない場合を None で返す。
pub fn external_gpu_activity(gpus: &[GPU], container_ids: &[String], proc_root: &Path) -> Activity {
    if gpus.is_empty() {
        return None;
    }
    let mut pids: std::collections::BTreeSet<i32> = std::collections::BTreeSet::new();
    if gpus.iter().any(|gpu| gpu.vendor == "nvidia") {
        if let Ok(output) = Command::new("nvidia-smi")
            .args(["--query-compute-apps=pid", "--format=csv,noheader,nounits"])
            .output()
        {
            if output.status.success() {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    if let Ok(pid) = line.trim().parse::<i32>() {
                        pids.insert(pid);
                    }
                }
            }
        }
    }
    let wanted: Vec<String> = gpus
        .iter()
        .flat_map(|gpu| gpu.devices.iter().cloned())
        .filter(|path| path.starts_with("/dev/dri/"))
        .collect();
    if !wanted.is_empty() {
        let Ok(entries) = std::fs::read_dir(proc_root) else {
            return None;
        };
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let fd_dir = entry.path().join("fd");
            let Ok(fds) = std::fs::read_dir(&fd_dir) else {
                continue;
            };
            for fd in fds.flatten() {
                match std::fs::canonicalize(fd.path()) {
                    Ok(target) if wanted.contains(&target.to_string_lossy().to_string()) => {
                        pids.insert(pid);
                        break;
                    }
                    Ok(_) => continue,
                    Err(_) => continue,
                }
            }
        }
    }
    let mut unknown = false;
    for pid in pids {
        match process_is_borrower(pid, container_ids, proc_root) {
            Some(false) => return Some(true),
            None => unknown = true,
            Some(true) => {}
        }
    }
    if unknown {
        None
    } else {
        Some(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_policy_treats_unknown_as_busy() {
        let mut policy = IdlePolicy::new(60.0);
        assert!(policy.update(Some(true), 0.0));
        assert!(policy.update(None, 10.0));
        assert!(policy.update(Some(false), 20.0));
        assert!(policy.update(Some(false), 79.0));
        assert!(!policy.update(Some(false), 81.0));
        assert!(!policy.update(Some(false), 82.0));
    }

    #[test]
    fn idle_policy_restarts_after_activity() {
        let mut policy = IdlePolicy::new(60.0);
        assert!(policy.update(Some(false), 0.0));
        assert!(policy.update(Some(true), 30.0));
        assert!(policy.update(Some(false), 40.0));
        assert!(policy.update(Some(false), 99.0));
        assert!(!policy.update(Some(false), 101.0));
    }

    #[test]
    fn borrower_detection_uses_cgroup() {
        let dir = tempfile::tempdir().unwrap();
        let pid_dir = dir.path().join("1234");
        std::fs::create_dir(&pid_dir).unwrap();
        std::fs::write(pid_dir.join("cgroup"), "0::/docker/abcdef1234567890\n").unwrap();
        assert_eq!(
            process_is_borrower(1234, &["abcdef1234567890".into()], dir.path()),
            Some(true)
        );
        assert_eq!(
            process_is_borrower(1234, &["0000ffff0000ffff".into()], dir.path()),
            Some(false)
        );
        assert_eq!(process_is_borrower(9999, &[], dir.path()), None);
    }

    #[test]
    fn disk_usage_reports_filesystem() {
        let (total, free) = disk_usage(Path::new("/")).unwrap();
        assert!(total > 0);
        assert!(free <= total);
    }
}
