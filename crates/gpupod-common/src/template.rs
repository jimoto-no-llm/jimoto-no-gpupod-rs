//! 承認済みテンプレートカタログ（Python 版 models.Template の移植）。
//!
//! 指紋（fingerprint）は Python の json.dumps(d, sort_keys=True) とバイト一致する
//! 正規化 JSON で計算する。中央と提供PCが同一カタログを指すことを確認する値。

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{AgentError, Result};
use crate::identifier::identifier;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Template {
    pub id: String,
    pub image: String,
    pub vendors: Vec<String>,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub ports: Vec<String>,
}

impl Template {
    /// Python 版 from_dict と同じ規則で検証して構築する。
    pub fn from_value(raw: serde_json::Value) -> Result<Self> {
        let map = raw
            .as_object()
            .ok_or_else(|| AgentError::new("Invalid template"))?;
        let allowed = ["id", "image", "vendors", "command", "environment", "ports"];
        if map.keys().any(|key| !allowed.contains(&key.as_str())) {
            return Err(AgentError::new("Unknown template fields"));
        }
        let string_of = |key: &str| -> Result<String> {
            map.get(key)
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .ok_or_else(|| AgentError::new("Invalid template fields"))
        };
        let template_id = identifier(&string_of("id")?)?.to_owned();
        let image = string_of("image")?;
        let image_ok = !image.is_empty()
            && image.as_bytes()[0].is_ascii_alphanumeric()
            && image.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b':' | b'@' | b'-')
            });
        if !image_ok {
            return Err(AgentError::new("Invalid template image reference"));
        }
        let list_of_strings = |key: &str| -> Result<Vec<String>> {
            match map.get(key) {
                None => Ok(Vec::new()),
                Some(serde_json::Value::Array(items)) => items
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| {
                                AgentError::new("Template vendors must be a nonempty list")
                            })
                    })
                    .collect(),
                _ => Err(AgentError::new("Template vendors must be a nonempty list")),
            }
        };
        let vendors = list_of_strings("vendors")?;
        if vendors.is_empty() {
            return Err(AgentError::new("Template vendors must be a nonempty list"));
        }
        let command = list_of_strings("command")?;
        if command.iter().any(|item| item.contains('\0')) {
            return Err(AgentError::new("Template command must be a string list"));
        }
        let mut environment = BTreeMap::new();
        if let Some(value) = map.get("environment") {
            let obj = value
                .as_object()
                .ok_or_else(|| AgentError::new("Invalid template environment"))?;
            for (key, item) in obj {
                let first = key.as_bytes().first().copied();
                let key_ok = first.is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                    && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
                let value_ok = item.as_str().is_some_and(|s| !s.contains('\0'));
                if !key_ok || !value_ok {
                    return Err(AgentError::new("Invalid template environment"));
                }
                environment.insert(key.clone(), item.as_str().unwrap().to_owned());
            }
        }
        let ports = list_of_strings("ports")?;
        let mut seen = HashSet::new();
        for port in &ports {
            let (number, proto) = port.split_once('/').unwrap_or(("", ""));
            let ok = !number.is_empty()
                && number.len() <= 5
                && number.bytes().all(|b| b.is_ascii_digit())
                && (proto == "tcp" || proto == "udp");
            if !ok {
                return Err(AgentError::new("Ports must use PORT/tcp or PORT/udp"));
            }
            let n: u32 = number
                .parse()
                .map_err(|_| AgentError::new("Port out of range"))?;
            if !(1..=65535).contains(&n) {
                return Err(AgentError::new("Port out of range"));
            }
            if !seen.insert(port.clone()) {
                return Err(AgentError::new(
                    "Template ports must be a list without duplicates",
                ));
            }
        }
        Ok(Template { id: template_id, image, vendors, command, environment, ports })
    }

    /// Python 版 to_dict と同じ形（環境はマップ）。
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "image": self.image,
            "vendors": self.vendors,
            "command": self.command,
            "environment": self.environment,
            "ports": self.ports,
        })
    }

    /// Python の json.dumps(sort_keys=True) と一致する指紋。
    pub fn fingerprint(&self) -> String {
        let canonical = canonical_json(&self.to_value());
        hex::encode(Sha256::digest(canonical.as_bytes()))
    }
}

/// Python の json.dumps(value, sort_keys=True) 相当（区切りは ", " / ": "、ensure_ascii）。
fn canonical_json(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_python(value, &mut out);
    out
}

fn write_python(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        serde_json::Value::Number(n) => out.push_str(&n.to_string()),
        serde_json::Value::String(s) => write_python_string(s, out),
        serde_json::Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_python(item, out);
            }
            out.push(']');
        }
        serde_json::Value::Object(map) => {
            out.push('{');
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_python_string(key, out);
                out.push_str(": ");
                write_python(&map[*key], out);
            }
            out.push('}');
        }
    }
}

fn write_python_string(value: &str, out: &mut String) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// カタログファイルを読み込み、ID の重複を拒否して返す。
pub fn load_templates(path: impl AsRef<Path>) -> Result<BTreeMap<String, Template>> {
    let raw = std::fs::read_to_string(path.as_ref())
        .map_err(|exc| AgentError::new(format!("Cannot load templates: {exc}")))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|exc| AgentError::new(format!("Cannot load templates: {exc}")))?;
    let items = value
        .as_array()
        .ok_or_else(|| AgentError::new("Templates must be a JSON list"))?;
    let mut result = BTreeMap::new();
    for item in items {
        let template = Template::from_value(item.clone())?;
        if result.insert(template.id.clone(), template).is_some() {
            return Err(AgentError::new("Duplicate template IDs"));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fingerprint_matches_python_json_dumps() {
        // Python: hashlib.sha256(json.dumps(d, sort_keys=True).encode()).hexdigest()
        let template = Template::from_value(json!({
            "id": "ubuntu-24.04", "image": "gpupod/ubuntu:24.04",
            "vendors": ["nvidia", "amd"], "ports": ["22/tcp"]
        }))
        .unwrap();
        let expected_input = "{\"command\": [], \"environment\": {}, \"id\": \"ubuntu-24.04\", \
                              \"image\": \"gpupod/ubuntu:24.04\", \"ports\": [\"22/tcp\"], \
                              \"vendors\": [\"nvidia\", \"amd\"]}";
        let digest = Sha256::digest(expected_input.as_bytes());
        assert_eq!(template.fingerprint(), hex::encode(digest));
    }

    #[test]
    fn rejects_bad_ports_and_environment() {
        let bad_port = json!({
            "id": "a", "image": "img:1", "vendors": ["nvidia"], "ports": ["0/tcp"]
        });
        assert!(Template::from_value(bad_port).is_err());
        let bad_env = json!({
            "id": "a", "image": "img:1", "vendors": ["nvidia"],
            "environment": {"1BAD": "x"}
        });
        assert!(Template::from_value(bad_env).is_err());
        let unknown = json!({
            "id": "a", "image": "img:1", "vendors": ["nvidia"], "extra": 1
        });
        assert!(Template::from_value(unknown).is_err());
    }
}
