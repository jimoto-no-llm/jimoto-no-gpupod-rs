//! GPU 情報の型と検証（Python 版 models.GPU の移植）。

use serde::{Deserialize, Serialize};

use crate::error::{AgentError, Result};
use crate::identifier::identifier;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GPU {
    pub id: String,
    pub vendor: String,
    pub name: String,
    /// "nvidia"（NVIDIA コンテナツールキット）または "device"（/dev ファイル直接割当）。
    pub backend: String,
    #[serde(default)]
    pub selector: String,
    #[serde(default)]
    pub devices: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<i64>,
}

impl GPU {
    pub fn validate(&self) -> Result<()> {
        identifier(&self.id)?;
        if self.backend != "nvidia" && self.backend != "device" {
            return Err(AgentError::new("GPU backend must be nvidia or device"));
        }
        if self.backend == "nvidia" {
            let ok = !self.selector.is_empty()
                && self
                    .selector
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'));
            if !ok {
                return Err(AgentError::new("NVIDIA GPU selector is required"));
            }
        }
        if self.backend == "device" && self.devices.is_empty() {
            return Err(AgentError::new("Device backend requires device paths"));
        }
        for device in &self.devices {
            if !device.starts_with("/dev/")
                || device.split('/').any(|part| part == "..")
                || device.contains(':')
            {
                return Err(AgentError::new("GPU device paths must be under /dev"));
            }
        }
        Ok(())
    }

    pub fn from_value(value: serde_json::Value) -> Result<Self> {
        let gpu: GPU = serde_json::from_value(value)
            .map_err(|_| AgentError::new("Invalid GPU configuration"))?;
        gpu.validate()?;
        Ok(gpu)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_nvidia() {
        let gpu = GPU::from_value(json!({
            "id": "pci:0000:01:00.0", "vendor": "nvidia", "name": "RTX",
            "backend": "nvidia", "selector": "GPU-uuid-1"
        }))
        .unwrap();
        assert_eq!(gpu.vendor, "nvidia");
    }

    #[test]
    fn device_paths_are_confined() {
        let bad = json!({
            "id": "pci:1", "vendor": "amd", "name": "amd", "backend": "device",
            "devices": ["/dev/../etc/passwd"]
        });
        assert!(GPU::from_value(bad).is_err());
        let colon = json!({
            "id": "pci:1", "vendor": "amd", "name": "amd", "backend": "device",
            "devices": ["/dev/dri/renderD128:0"]
        });
        assert!(GPU::from_value(colon).is_err());
    }

    #[test]
    fn nvidia_requires_selector() {
        let bad = json!({
            "id": "pci:1", "vendor": "nvidia", "name": "n", "backend": "nvidia"
        });
        assert!(GPU::from_value(bad).is_err());
    }
}
