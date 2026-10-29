//! GPU の発見（Python 版 devices.py の移植）。ドライバー等は一切入れない。

use std::collections::BTreeMap;
use std::path::Path;

use gpupod_common::{AgentError, GPU};

fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|text| text.trim().to_owned())
}

/// nvidia-smi の PCI バス表記を 4 桁ドメイン付き小文字へ揃える。
fn pci(value: &str) -> String {
    let lower = value.to_lowercase();
    let parts: Vec<&str> = lower.split(':').collect();
    if parts.is_empty() {
        return lower;
    }
    match u32::from_str_radix(parts[0], 16) {
        Ok(domain) => {
            let rest = parts[1..].join(":");
            format!("{domain:04x}:{rest}")
        }
        Err(_) => lower,
    }
}

/// nvidia-smi と /sys/class/drm/renderD* から GPU を集める。
pub fn discover_gpus() -> Result<Vec<GPU>, AgentError> {
    let mut found: BTreeMap<String, GPU> = BTreeMap::new();
    if let Ok(output) = run_with_timeout(
        "nvidia-smi",
        &[
            "--query-gpu=uuid,pci.bus_id,name,memory.total",
            "--format=csv,noheader,nounits",
        ],
        std::time::Duration::from_secs(10),
    ) {
        for row in csv_rows(&output) {
            if row.len() != 4 {
                continue;
            }
            let uuid = row[0].trim().to_owned();
            let bus = pci(row[1].trim());
            let name = row[2].trim().to_owned();
            let memory: i64 = match row[3].trim().parse() {
                Ok(value) => value,
                Err(_) => continue,
            };
            let gpu = GPU {
                id: format!("pci:{bus}"),
                vendor: "nvidia".into(),
                name,
                backend: "nvidia".into(),
                selector: uuid,
                devices: Vec::new(),
                memory_mib: Some(memory),
            };
            gpu.validate()?;
            found.insert(gpu.id.clone(), gpu);
        }
    }

    let vendors = [
        ("0x10de", "nvidia"),
        ("0x1002", "amd"),
        ("0x8086", "intel"),
    ];
    let mut renders: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/drm") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("renderD") {
                renders.push(entry.path());
            }
        }
    }
    renders.sort();
    for render in renders {
        let device = render.join("device");
        let bus = match std::fs::canonicalize(&device) {
            Ok(resolved) => resolved
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            Err(_) => continue,
        };
        if found.contains_key(&format!("pci:{bus}")) {
            continue;
        }
        let vendor: String = read_trimmed(device.join("vendor"))
            .and_then(|value| {
                vendors
                    .iter()
                    .find(|(key, _)| *key == value.as_str())
                    .map(|(_, name)| (*name).to_string())
            })
            .unwrap_or_else(|| "other".to_owned());
        let render_name = render.file_name().unwrap().to_string_lossy().into_owned();
        let mut paths = vec![format!("/dev/dri/{render_name}")];
        if vendor == "amd" && Path::new("/dev/kfd").exists() {
            paths.push("/dev/kfd".to_owned());
        }
        let memory = read_trimmed(device.join("mem_info_vram_total"));
        let memory_mib = memory
            .as_deref()
            .and_then(|value| value.parse::<i64>().ok())
            .map(|value| value / (1024 * 1024));
        let label = read_trimmed(device.join("device"));
        let gpu = GPU {
            id: format!("pci:{bus}"),
            vendor: vendor.clone(),
            name: format!("{vendor} {}", label.unwrap_or_else(|| bus.clone())),
            backend: "device".into(),
            selector: String::new(),
            devices: paths,
            memory_mib,
        };
        gpu.validate()?;
        found.insert(gpu.id.clone(), gpu);
    }
    Ok(found.into_values().collect())
}

/// JSON の提供者所有デバイス定義を読む。未指定なら発見を使う。
pub fn load_gpus(path: Option<&str>) -> Result<Vec<GPU>, AgentError> {
    let Some(path) = path else {
        return discover_gpus();
    };
    let text = std::fs::read_to_string(path)
        .map_err(|exc| AgentError::new(format!("Cannot load GPU configuration: {exc}")))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|exc| AgentError::new(format!("Cannot load GPU configuration: {exc}")))?;
    let items = value
        .as_array()
        .ok_or_else(|| AgentError::new("Cannot load GPU configuration: expected a JSON list"))?;
    let allowed = ["id", "vendor", "name", "backend", "selector", "devices", "memory_mib"];
    let mut result = Vec::new();
    for item in items {
        let object = item
            .as_object()
            .ok_or_else(|| AgentError::new("Cannot load GPU configuration: expected a JSON object"))?;
        for key in object.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(AgentError::new(format!(
                    "Cannot load GPU configuration: GPU() got an unexpected keyword argument '{key}'"
                )));
            }
        }
        let gpu: GPU = serde_json::from_value(item.clone())
            .map_err(|exc| AgentError::new(format!("Cannot load GPU configuration: {exc}")))?;
        gpu.validate()?;
        if result.iter().any(|existing: &GPU| existing.id == gpu.id) {
            return Err(AgentError::new("Duplicate GPU IDs"));
        }
        result.push(gpu);
    }
    Ok(result)
}

/// CSV 1 行ずつに分割する（引用符対応、Python 版 csv.reader 相当）。
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

/// コマンドをタイムアウト付きで実行する（失敗や超時は Err）。
pub fn run_with_timeout(
    program: &str,
    args: &[&str],
    timeout: std::time::Duration,
) -> std::io::Result<String> {
    use std::os::unix::process::ExitStatusExt;
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(status) => {
                if !status.success() {
                    let code = status.code().unwrap_or(-1);
                    return Err(std::io::Error::other(format!(
                        "{program} exited with status {code}{}",
                        status.signal().map(|s| format!(" (signal {s})")).unwrap_or_default()
                    )));
                }
                let output = child.wait_with_output()?;
                return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
            }
            None => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(std::io::Error::other(format!("{program} timed out")));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}
