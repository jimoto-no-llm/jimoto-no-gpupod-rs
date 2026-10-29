//! ホストと GPU の計測（Python 版 telemetry.py の移植）。
//!
//! 不在のセンサーは unknown のまま。外部利用の判定は False / False / None。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};

use gpupod_common::GPU;

use crate::devices::run_with_timeout;

fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|text| text.trim().to_owned())
}

fn number(value: Option<String>) -> Option<f64> {
    value.and_then(|text| text.parse::<f64>().ok()).filter(|value| value.is_finite())
}

/// /proc/stat の差分から CPU 使用率（%）。初回呼び出しは 0.0（Python 版と同じ）。
fn cpu_percent() -> f64 {
    static PREVIOUS: Mutex<Option<(u64, u64)>> = Mutex::new(None);
    let Ok(text) = std::fs::read_to_string("/proc/stat") else {
        return 0.0;
    };
    let Some(line) = text.lines().find(|line| line.starts_with("cpu ")) else {
        return 0.0;
    };
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|value| value.parse::<u64>().ok())
        .collect();
    if fields.len() < 4 {
        return 0.0;
    }
    let total: u64 = fields.iter().sum();
    let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
    let mut previous = PREVIOUS.lock().unwrap_or_else(|exc| exc.into_inner());
    let result = match *previous {
        None => 0.0,
        Some((old_idle, old_total)) => {
            let total_delta = total.saturating_sub(old_total);
            let idle_delta = idle.saturating_sub(old_idle);
            if total_delta == 0 {
                0.0
            } else {
                100.0 * (total_delta - idle_delta) as f64 / total_delta as f64
            }
        }
    };
    *previous = Some((idle, total));
    result
}

/// メモリとディスクの残量（psutil 相当の statvfs）。
fn statvfs(path: &Path) -> std::io::Result<(u64, u64)> {
    use std::ffi::CString;
    let raw = CString::new(path.to_string_lossy().as_bytes())
        .map_err(|_| std::io::Error::other("invalid path"))?;
    unsafe {
        let mut info: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(raw.as_ptr(), &mut info) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let size = info.f_frsize as u64;
        Ok((info.f_blocks as u64 * size, info.f_bavail as u64 * size))
    }
}

/// 指定パスの残容量（バイト）。Python 版 psutil.disk_usage(...).free 相当。
pub fn disk_free(path: impl AsRef<Path>) -> std::io::Result<u64> {
    statvfs(path.as_ref()).map(|(_, free)| free)
}

/// 指定パスの総容量と残量。
pub fn disk_usage(path: impl AsRef<Path>) -> std::io::Result<(u64, u64)> {
    statvfs(path.as_ref())
}

/// ホストと GPU の現在値を集める。
pub fn collect(gpus: &[GPU], disk_path: &Path) -> std::io::Result<Value> {
    let (disk_total, disk_free) = disk_usage(disk_path)?;
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    let cpu_count = std::thread::available_parallelism()
        .map(|value| value.get() as u64)
        .unwrap_or(1);

    let mut warnings: Vec<String> = Vec::new();
    let mut sensors: BTreeMap<String, Value> = BTreeMap::new();
    let has_nvidia = gpus
        .iter()
        .any(|gpu| gpu.vendor == "nvidia" && !gpu.id.starts_with("mock-"));
    if has_nvidia {
        match run_with_timeout(
            "nvidia-smi",
            &[
                "--query-gpu=uuid,utilization.gpu,memory.used,temperature.gpu,power.draw",
                "--format=csv,noheader,nounits",
            ],
            std::time::Duration::from_secs(5),
        ) {
            Ok(output) => {
                for row in csv_rows(&output) {
                    if row.is_empty() {
                        continue;
                    }
                    let uuid = row[0].trim().to_owned();
                    let mut value = serde_json::Map::new();
                    let keys = [
                        "utilization_percent",
                        "memory_used_mib",
                        "temperature_c",
                        "power_w",
                    ];
                    for (index, key) in keys.iter().enumerate() {
                        let raw = row.get(index + 1).map(|item| item.trim().to_owned());
                        match number(raw) {
                            Some(value_number) => {
                                value.insert((*key).into(), json!(value_number));
                            }
                            None => {
                                value.insert((*key).into(), Value::Null);
                            }
                        }
                    }
                    sensors.insert(uuid, Value::Object(value));
                }
            }
            Err(_) => warnings.push("NVIDIA metrics unavailable".to_owned()),
        }
    }

    let mut values = Vec::new();
    for gpu in gpus {
        let mut value = serde_json::Map::new();
        value.insert("id".into(), json!(gpu.id));
        value.insert("vendor".into(), json!(gpu.vendor));
        value.insert("mock".into(), json!(gpu.id.starts_with("mock-")));
        if let Some(sensor) = sensors.get(&gpu.selector) {
            if let Some(object) = sensor.as_object() {
                for (key, item) in object {
                    value.insert(key.clone(), item.clone());
                }
            }
        }
        for device in &gpu.devices {
            if let Some(name) = device.strip_prefix("/dev/dri/") {
                let root = Path::new("/sys/class/drm").join(name).join("device");
                if let Some(busy) = number(read_trimmed(root.join("gpu_busy_percent"))) {
                    value.insert("utilization_percent".into(), json!(busy));
                }
                match number(read_trimmed(root.join("mem_info_vram_used"))) {
                    Some(used) => value
                        .insert("memory_used_mib".into(), json!(used / 1_048_576.0)),
                    None => value.insert("memory_used_mib".into(), Value::Null),
                };
            }
        }
        values.push(Value::Object(value));
    }

    Ok(json!({
        "host": {
            "cpu_percent": cpu_percent(),
            "cpu_count": cpu_count,
            "memory_total": system.total_memory(),
            "memory_available": system.available_memory(),
            "disk_total": disk_total,
            "disk_free": disk_free,
        },
        "gpus": values,
        "warnings": warnings,
    }))
}

/// cgroup からそのプロセスが貸出コンテナのものかを見る。
pub fn process_is_borrower(
    pid: i32,
    container_ids: &[String],
    proc_root: impl AsRef<Path>,
) -> Option<bool> {
    let path = proc_root.as_ref().join(pid.to_string()).join("cgroup");
    let text = read_trimmed(path)?;
    Some(container_ids.iter().any(|id| text.contains(id.as_str())))
}

/// GPU を使っている外部プロセスの有無。False / True / None（不明）。
pub fn external_gpu_activity(
    gpus: &[GPU],
    container_ids: &[String],
    proc_root: impl AsRef<Path>,
) -> Option<bool> {
    if gpus.is_empty() {
        return None;
    }
    let proc_root = proc_root.as_ref().to_path_buf();
    let mut pids: std::collections::BTreeSet<i32> = std::collections::BTreeSet::new();
    let mut nvidia_ok = true;
    if gpus.iter().any(|gpu| gpu.vendor == "nvidia") {
        match run_with_timeout(
            "nvidia-smi",
            &["--query-compute-apps=pid", "--format=csv,noheader,nounits"],
            std::time::Duration::from_secs(5),
        ) {
            Ok(output) => {
                for line in output.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    if let Ok(pid) = line.parse::<i32>() {
                        pids.insert(pid);
                    }
                }
            }
            Err(_) => nvidia_ok = false,
        }
        if !nvidia_ok {
            return None;
        }
    }
    let wanted: Vec<PathBuf> = gpus
        .iter()
        .flat_map(|gpu| gpu.devices.iter().cloned())
        .filter(|path| path.starts_with("/dev/dri/"))
        .map(PathBuf::from)
        .collect();
    if !wanted.is_empty() {
        let Ok(entries) = std::fs::read_dir(&proc_root) else {
            return None;
        };
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let directory = entry.path().join("fd");
            let Ok(fds) = std::fs::read_dir(&directory) else {
                continue;
            };
            for fd in fds.flatten() {
                match std::fs::read_link(fd.path()) {
                    Ok(target) => {
                        if wanted.iter().any(|path| *path == target) {
                            pids.insert(pid);
                            break;
                        }
                    }
                    Err(exc) if exc.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(_) => return None,
                }
            }
        }
    }
    let mut unknown = false;
    for pid in pids {
        match process_is_borrower(pid, container_ids, &proc_root) {
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

/// 遊休判定。unknown は遊休の証拠にしない。
pub struct IdlePolicy {
    idle_seconds: f64,
    idle_since: Option<f64>,
}

impl IdlePolicy {
    pub fn new(idle_seconds: f64) -> Self {
        Self { idle_seconds, idle_since: None }
    }

    /// activity が False の連続時間で遊休を判断し、遊休なら true。
    pub fn update(&mut self, activity: Option<bool>, timestamp: f64) -> bool {
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

    /// 現在の遊休開始時刻。
    pub fn idle_since(&self) -> Option<f64> {
        self.idle_since
    }
}

/// CSV 1 行ずつに分割する（引用符対応）。
fn csv_rows(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut fields = Vec::new();
        let mut current = String::new();
        let mut quoted = false;
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '"' => {
                    if quoted && chars.peek() == Some(&'"') {
                        current.push('"');
                        chars.next();
                    } else {
                        quoted = !quoted;
                    }
                }
                ',' if !quoted => fields.push(std::mem::take(&mut current)),
                other => current.push(other),
            }
        }
        fields.push(current);
        rows.push(fields);
    }
    rows
}
