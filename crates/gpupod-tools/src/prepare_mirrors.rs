//! Docker Hub ミラー設定の生成（Python 版 prepare_mirrors.py の移植）。

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use gpupod_common::identifier::endpoint_host;
use gpupod_common::AgentError;

use crate::support::files::atomic_json;
use crate::support::url::{scheme_allowed, urlsplit};

/// ミラー URL の検証と正規化。https のオリジンのみ（loopback の http はテスト用に許可）。
pub fn mirror_url(value: &str) -> Result<String> {
    let parts = urlsplit(value);
    let local = parts.hostname().as_deref() == Some("localhost") || parts.is_loopback_host();
    let host = parts
        .hostname()
        .ok_or_else(|| invalid_mirror())?;
    if parts.username().is_some()
        || parts.password().is_some()
        || !parts.query.is_empty()
        || !parts.fragment.is_empty()
        || !(parts.path.is_empty() || parts.path == "/")
        || value.chars().any(|c| c.is_whitespace())
        || !scheme_allowed(&parts.scheme, local)
    {
        bail!(invalid_mirror());
    }
    parts.port().map_err(|_| AgentError::new("Invalid mirror port"))?;
    endpoint_host(&host).map_err(|_| AgentError::new("Invalid mirror hostname"))?;
    Ok(format!("{}://{}", parts.scheme, parts.netloc.to_lowercase()))
}

fn invalid_mirror() -> AgentError {
    AgentError::new(
        "Mirrors must be HTTPS origin URLs without credentials, paths or query strings \
         (loopback HTTP is allowed for tests)",
    )
}

/// 既存の daemon.json にミラーをマージする。新しい指定が先頭を占める。
pub fn merge_mirrors(config: &serde_json::Value, mirrors: &[String]) -> Result<serde_json::Value> {
    let Some(object) = config.as_object() else {
        bail!(AgentError::new("Docker configuration must be a JSON object"));
    };
    let mut result = object.clone();
    let existing = match result.get("registry-mirrors") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::Array(items)) => {
            let mut values = Vec::new();
            for item in items {
                let text = item.as_str().ok_or_else(|| {
                    AgentError::new("Existing registry-mirrors must be a list of URLs")
                })?;
                values.push(text.to_string());
            }
            values
        }
        Some(_) => bail!(AgentError::new("Existing registry-mirrors must be a list of URLs")),
    };
    let mut merged: Vec<String> = Vec::new();
    for item in mirrors.iter().chain(existing.iter()) {
        let normalized = mirror_url(item)?;
        if !merged.contains(&normalized) {
            merged.push(normalized);
        }
    }
    if merged.is_empty() {
        bail!(AgentError::new("Specify at least one mirror"));
    }
    result.insert(
        "registry-mirrors".to_string(),
        serde_json::Value::Array(merged.into_iter().map(serde_json::Value::String).collect()),
    );
    Ok(serde_json::Value::Object(result))
}

/// 出力先を守りながら候補ファイルを作る。
pub fn prepare(
    output: &Path,
    mirrors: &[String],
    source_config: Option<&Path>,
) -> Result<PathBuf> {
    let output_absolute = absolute(output)?;
    if let Some(source) = source_config {
        let source_absolute = absolute(source)?;
        if same_file(&output_absolute, &source_absolute) {
            bail!(AgentError::new(
                "Choose a separate output file to review before applying it"
            ));
        }
    }
    if output_absolute.exists() {
        bail!(AgentError::new(
            "Output already exists; choose a new candidate filename"
        ));
    }
    let config = match source_config {
        Some(path) => {
            let raw = std::fs::read_to_string(path).context("Cannot read the Docker config")?;
            serde_json::from_str::<serde_json::Value>(&raw).context("Cannot parse the Docker config")?
        }
        None => serde_json::json!({}),
    };
    let merged = merge_mirrors(&config, mirrors)?;
    atomic_json(&output_absolute, &merged).context("Cannot write the candidate configuration")?;
    Ok(output_absolute)
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn same_file(left: &Path, right: &Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(a), Ok(b)) => a == b,
        _ => left == right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalizes_valid_mirrors() {
        assert_eq!(
            mirror_url("https://Mirror.Example.ORG").unwrap(),
            "https://mirror.example.org"
        );
        assert_eq!(
            mirror_url("https://mirror.example.org:443/").unwrap(),
            "https://mirror.example.org:443"
        );
        assert_eq!(
            mirror_url("http://127.0.0.1:5000").unwrap(),
            "http://127.0.0.1:5000"
        );
        assert_eq!(
            mirror_url("http://localhost:5000").unwrap(),
            "http://localhost:5000"
        );
    }

    #[test]
    fn rejects_unsafe_mirrors() {
        for bad in [
            "http://mirror.example.org",
            "https://mirror.example.org/path",
            "https://user@mirror.example.org",
            "https://mirror.example.org?q=1",
            "https://mirror.example.org#frag",
            "https://mirror.example.org/a b",
            "ftp://mirror.example.org",
            "https://-bad.example",
        ] {
            assert!(mirror_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn merges_with_new_mirrors_first() {
        let config = json!({
            "debug": true,
            "registry-mirrors": ["https://old.example.org", "https://new.example.org"]
        });
        let merged = merge_mirrors(&config, &["https://first.example.org".into()]).unwrap();
        assert_eq!(
            merged["registry-mirrors"],
            json!(["https://first.example.org", "https://old.example.org", "https://new.example.org"])
        );
        assert_eq!(merged["debug"], json!(true));
    }

    #[test]
    fn rejects_invalid_existing_mirrors() {
        assert!(merge_mirrors(&json!("not an object"), &[]).is_err());
        assert!(merge_mirrors(&json!({"registry-mirrors": "x"}), &[]).is_err());
        assert!(merge_mirrors(&json!({"registry-mirrors": [1]}), &[]).is_err());
        assert!(merge_mirrors(&json!({}), &[]).is_err());
    }

    #[test]
    fn prepare_writes_atomically_and_refuses_overwrite() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("daemon.candidate.json");
        let written = prepare(&output, &["https://mirror.example.org".into()], None).unwrap();
        assert_eq!(written, output);
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&output).unwrap()).unwrap();
        assert_eq!(saved["registry-mirrors"], json!(["https://mirror.example.org"]));
        assert!(prepare(&output, &["https://mirror.example.org".into()], None).is_err());

        let source = directory.path().join("daemon.json");
        std::fs::write(&source, "{}").unwrap();
        assert!(prepare(&source, &["https://mirror.example.org".into()], Some(&source)).is_err());
    }

    #[test]
    fn prepare_preserves_the_existing_config() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("daemon.json");
        std::fs::write(&source, "{\"log-level\": \"warn\"}").unwrap();
        let output = directory.path().join("candidate.json");
        prepare(&output, &["https://mirror.example.org".into()], Some(&source)).unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&output).unwrap()).unwrap();
        assert_eq!(saved["log-level"], json!("warn"));
        assert_eq!(saved["registry-mirrors"].as_array().unwrap().len(), 1);
    }
}
