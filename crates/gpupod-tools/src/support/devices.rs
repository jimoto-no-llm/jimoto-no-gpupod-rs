//! GPU 品目の読み出し（Python 版 devices.py / mock.py の移植）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use gpupod_common::identifier::identifier;
use gpupod_common::{AgentError, GPU};

/// ダミー品目のみ。Docker・記録装置・ネットワーク・指標は本物のまま。
pub fn mock_gpus() -> Vec<GPU> {
    vec![
        GPU {
            id: "mock-nvidia".into(),
            vendor: "nvidia".into(),
            name: "MOCK NVIDIA GPU".into(),
            backend: "nvidia".into(),
            selector: "GPU-MOCK".into(),
            devices: Vec::new(),
            memory_mib: None,
        },
        GPU {
            id: "mock-amd".into(),
            vendor: "amd".into(),
            name: "MOCK AMD GPU".into(),
            backend: "device".into(),
            selector: String::new(),
            devices: vec!["/dev/dri/renderD128".into()],
            memory_mib: None,
        },
        GPU {
            id: "mock-intel".into(),
            vendor: "intel".into(),
            name: "MOCK Intel GPU".into(),
            backend: "device".into(),
            selector: String::new(),
            devices: vec!["/dev/dri/renderD129".into()],
            memory_mib: None,
        },
    ]
}

pub fn load_gpus(path: Option<&Path>) -> Result<Vec<GPU>> {
    let Some(path) = path else {
        return Ok(discover_gpus());
    };
    let raw = std::fs::read_to_string(path)
        .map_err(|exc| AgentError::new(format!("Cannot load GPU configuration: {exc}")))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|exc| AgentError::new(format!("Cannot load GPU configuration: {exc}")))?;
    let items = value
        .as_array()
        .ok_or_else(|| AgentError::new("Cannot load GPU configuration: not a JSON list"))?;
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for item in items {
        let gpu = GPU::from_value(item.clone())
            .map_err(|exc| AgentError::new(format!("Cannot load GPU configuration: {exc}")))?;
        if !seen.insert(gpu.id.clone()) {
            bail!(AgentError::new("Duplicate GPU IDs"));
        }
        result.push(gpu);
    }
    Ok(result)
}

/// nvidia-smi と /sys/class/drm から読み取る。失敗しても品目は空で返る。
pub fn discover_gpus() -> Vec<GPU> {
    let mut found: BTreeMap<String, GPU> = BTreeMap::new();
    if let Ok(output) = Command::new("nvidia-smi")
        .args([
            "--query-gpu=uuid,pci.bus_id,name,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
    {
        if output.status.success() {
            for row in csv_rows(&String::from_utf8_lossy(&output.stdout)) {
                if row.len() < 4 {
                    continue;
                }
                let uuid = row[0].trim().to_string();
                let Ok(bus) = normalize_pci(row[1].trim()) else { continue };
                let memory: i64 = row[3].trim().parse().unwrap_or(0);
                let gpu = GPU {
                    id: format!("pci:{bus}"),
                    vendor: "nvidia".into(),
                    name: row[2].trim().to_string(),
                    backend: "nvidia".into(),
                    selector: uuid,
                    devices: Vec::new(),
                    memory_mib: Some(memory),
                };
                found.insert(bus, gpu);
            }
        }
    }
    let vendors = |value: &str| match value.trim() {
        "0x10de" => "nvidia",
        "0x1002" => "amd",
        "0x8086" => "intel",
        _ => "other",
    };
    let mut renders: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/drm") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("renderD") {
                renders.push(entry.path());
            }
        }
    }
    renders.sort();
    for render in renders {
        let name = render.file_name().unwrap().to_string_lossy().to_string();
        let device = render.join("device");
        let bus = match std::fs::canonicalize(&device) {
            Ok(resolved) => resolved.file_name().unwrap().to_string_lossy().to_string(),
            Err(_) => continue,
        };
        if found.contains_key(&bus) {
            continue;
        }
        let vendor = vendors(&read_text(&device.join("vendor")).unwrap_or_default());
        let mut paths = vec![format!("/dev/dri/{name}")];
        if vendor == "amd" && Path::new("/dev/kfd").exists() {
            paths.push("/dev/kfd".into());
        }
        let memory_mib = read_text(&device.join("mem_info_vram_total"))
            .and_then(|value| value.trim().parse::<i64>().ok())
            .map(|bytes| bytes / (1024 * 1024));
        let gpu = GPU {
            id: format!("pci:{bus}"),
            vendor: vendor.into(),
            name: format!("{vendor} {}", read_text(&device.join("device")).unwrap_or_else(|| bus.clone())),
            backend: "device".into(),
            selector: String::new(),
            devices: paths,
            memory_mib,
        };
        found.insert(bus, gpu);
    }
    found.into_values().collect()
}

fn read_text(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|value| value.trim().to_string())
}

/// nvidia-smi の 8 桁 PCI ドメインを 4 桁に正規化する。
fn normalize_pci(value: &str) -> Result<String> {
    let lowered = value.to_lowercase();
    let parts: Vec<&str> = lowered.split(':').collect();
    if parts.len() < 3 {
        bail!("Invalid PCI bus id");
    }
    let domain: u32 = u32::from_str_radix(parts[0], 16).context("Invalid PCI domain")?;
    Ok(format!("{domain:04x}:{}", parts[1..].join(":")))
}

/// 引用符を扱う最小 CSV 読み取り（Python の csv.reader 相当）。
pub(crate) fn csv_rows(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut fields: Vec<String> = Vec::new();
        let mut field = String::new();
        let mut quoted = false;
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '"' if quoted && chars.peek() == Some(&'"') => {
                    field.push('"');
                    chars.next();
                }
                '"' if !quoted && field.trim().is_empty() => {
                    field.clear();
                    quoted = true;
                }
                '"' => quoted = !quoted,
                ',' if !quoted => fields.push(std::mem::take(&mut field)),
                ' ' if !quoted && field.trim().is_empty() => {}
                _ => field.push(ch),
            }
        }
        fields.push(field);
        rows.push(fields);
    }
    rows
}

/// 診断 GPU 群の検証（Python 版 Agent.start の該当部分）。
pub fn validate_gpu_group(gpus: &[GPU], template_vendors: &[String], requested: &[String]) -> Result<()> {
    if requested.is_empty() {
        bail!(AgentError::new("Invalid diagnostic GPU group"));
    }
    let mut chosen = Vec::new();
    for id in requested {
        identifier(id)?;
        let gpu = gpus
            .iter()
            .find(|item| &item.id == id)
            .ok_or_else(|| AgentError::new("GPU is not registered on this node"))?;
        chosen.push(gpu);
    }
    if chosen.iter().any(|gpu| gpu.vendor != chosen[0].vendor)
        || !chosen.iter().all(|gpu| template_vendors.contains(&gpu.vendor))
    {
        bail!(AgentError::new(
            "Template does not declare support for this GPU vendor"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_inventory_is_stable() {
        let gpus = mock_gpus();
        assert_eq!(gpus.len(), 3);
        assert_eq!(gpus[0].vendor, "nvidia");
        assert!(gpus.iter().all(|gpu| gpu.validate().is_ok()));
    }

    #[test]
    fn pci_domain_is_normalized() {
        assert_eq!(normalize_pci("00000000:01:00.0").unwrap(), "0000:01:00.0");
        assert_eq!(normalize_pci("00000009:AF:00.1").unwrap(), "0009:af:00.1");
        assert!(normalize_pci("bad").is_err());
    }

    #[test]
    fn csv_handles_quoted_names() {
        let rows = csv_rows("abc-1, 00000000:01:00.0, \"GPU, T4\", 15360\n");
        assert_eq!(rows[0][2], "GPU, T4");
    }

    #[test]
    fn gpu_group_requires_matching_vendor() {
        let gpus = mock_gpus();
        let vendors = vec!["nvidia".to_string()];
        assert!(validate_gpu_group(&gpus, &vendors, &["mock-nvidia".into()]).is_ok());
        assert!(validate_gpu_group(&gpus, &vendors, &["mock-amd".into()]).is_err());
        assert!(validate_gpu_group(&gpus, &vendors, &["missing".into()]).is_err());
    }
}
