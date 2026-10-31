//! 進捗ウォッチドッグ（Python 版 health.py の移植）。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use uuid::Uuid;

/// リリース識別子。release.json があれば読み、なければ開発版。
pub fn release_identity() -> serde_json::Value {
    let candidates = [
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("release.json"))),
        Some(std::path::PathBuf::from("release.json")),
        Some(std::path::PathBuf::from("provider_agent/release.json")),
    ];
    for candidate in candidates.into_iter().flatten() {
        if let Ok(text) = std::fs::read_to_string(&candidate) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                if value.is_object() {
                    return value;
                }
            }
        }
    }
    json!({"version": "development", "sequence": 0})
}

/// NOTIFY_SOCKET へ systemd 通知。@ で始まる abstract ソケットは先頭を NUL にする。
pub fn notify_systemd(message: &str) {
    let Ok(address) = std::env::var("NOTIFY_SOCKET") else {
        return;
    };
    if address.is_empty() {
        return;
    }
    let is_abstract = address.starts_with('@');
    // abstract 名は NUL を除いた名前そのものを渡す（送信時に先頭へ NUL を付ける）。
    let target: Vec<u8> = address
        .strip_prefix('@')
        .map(|rest| rest.as_bytes().to_vec())
        .unwrap_or_default();
    let sent: std::io::Result<()> = if is_abstract {
        send_abstract(&target, message.as_bytes()).map(|_| ())
    } else {
        std::os::unix::net::UnixDatagram::unbound()
            .and_then(|socket| socket.send_to(message.as_bytes(), address.as_str()))
            .map(|_| ())
    };
    let _ = sent;
}

/// abstract ソケット（先頭バイト NUL）へデータグラムを送る。
fn send_abstract(name: &[u8], payload: &[u8]) -> std::io::Result<usize> {
    unsafe {
        let descriptor = libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut address: libc::sockaddr_un = std::mem::zeroed();
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let capacity = std::mem::size_of::<libc::c_char>() * address.sun_path.len();
        if name.len() + 1 > capacity {
            libc::close(descriptor);
            return Err(std::io::Error::other("NOTIFY_SOCKET name is too long"));
        }
        // abstract 名は sun_path[0] が NUL、以降が名前。
        for (index, byte) in name.iter().enumerate() {
            address.sun_path[index + 1] = *byte as libc::c_char;
        }
        let length =
            (std::mem::offset_of!(libc::sockaddr_un, sun_path) + 1 + name.len()) as libc::socklen_t;
        let sent = libc::sendto(
            descriptor,
            payload.as_ptr() as *const libc::c_void,
            payload.len(),
            0,
            &address as *const libc::sockaddr_un as *const libc::sockaddr,
            length,
        );
        let result = if sent < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(sent as usize)
        };
        libc::close(descriptor);
        result
    }
}

/// 進捗を追うタスク名。Python 版 health.py と同じ順序。
pub const PROGRESS: [&str; 3] = ["poll", "maintenance", "tunnels"];

/// 単調時計（秒）。テストでは注入可能。
pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;

/// モノトニック秒を返す既定の時計。
pub fn monotonic_clock() -> Clock {
    let start = std::time::Instant::now();
    Arc::new(move || start.elapsed().as_secs_f64())
}

/// 進捗の空白（stall）を検出する監視器。
pub struct HealthMonitor {
    timeout: f64,
    clock: Clock,
    boot_id: String,
    identity: serde_json::Value,
    progress: HashMap<String, f64>,
    busy: HashMap<String, f64>,
    completed: HashSet<String>,
}

impl HealthMonitor {
    pub fn new(timeout: f64) -> Self {
        Self::with_clock(timeout, monotonic_clock())
    }

    pub fn with_clock(timeout: f64, clock: Clock) -> Self {
        let now = clock();
        let mut progress = HashMap::new();
        for name in PROGRESS {
            progress.insert(name.to_string(), now);
        }
        Self {
            timeout,
            clock,
            boot_id: Uuid::new_v4().simple().to_string(),
            identity: release_identity(),
            progress,
            busy: HashMap::new(),
            completed: HashSet::new(),
        }
    }

    /// 作業の開始を記録する。
    pub fn begin(&mut self, name: &str) {
        let now = (self.clock)();
        self.busy.insert(name.to_string(), now);
    }

    /// 作業の終了を記録し、進捗タスクなら更新日時を進める。
    pub fn beat(&mut self, name: &str) {
        let now = (self.clock)();
        self.busy.remove(name);
        if self.progress.contains_key(name) {
            self.progress.insert(name.to_string(), now);
            self.completed.insert(name.to_string());
        }
    }

    /// リリース識別子（version / sequence）。
    pub fn identity(&self) -> &serde_json::Value {
        &self.identity
    }

    /// health.json に書く現在の状態。
    pub fn snapshot(&self) -> serde_json::Value {
        let timestamp = (self.clock)();
        // 進捗タスクは固定順、作業中タスクは名前順で Python 版と同じ並びにする。
        let mut busy: Vec<&str> = self.busy.keys().map(String::as_str).collect();
        busy.sort();
        let mut names: Vec<&str> = PROGRESS.to_vec();
        names.extend(busy);
        let mut stalled: Vec<String> = Vec::new();
        for name in names {
            let updated = self
                .progress
                .get(name)
                .or_else(|| self.busy.get(name))
                .copied()
                .unwrap_or(timestamp);
            if timestamp - updated > self.timeout && !stalled.iter().any(|item| item == name) {
                stalled.push(name.to_owned());
            }
        }
        let healthy =
            stalled.is_empty() && PROGRESS.iter().all(|name| self.completed.contains(*name));
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let mut result = self.identity.clone();
        let object = result
            .as_object_mut()
            .expect("release identity is an object");
        object.insert("pid".into(), json!(std::process::id()));
        object.insert("boot_id".into(), json!(self.boot_id));
        object.insert("updated_at".into(), json!(now));
        object.insert("healthy".into(), json!(healthy));
        object.insert("stalled".into(), json!(stalled));
        result
    }
}
