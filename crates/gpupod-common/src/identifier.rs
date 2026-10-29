//! 識別子とエンドポイントホストの検証（Python 版 models.identifier / endpoint_host の移植）。

use crate::error::{AgentError, Result};

/// 許可: 先頭は英数字、以降は英数字と colon / underscore / dot / hyphen、長さ 1..=128。
pub fn identifier(value: &str) -> Result<&str> {
    let bytes = value.as_bytes();
    let valid_first = bytes.first().is_some_and(|b| b.is_ascii_alphanumeric());
    let valid_rest = bytes.iter().all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'.' | b'-')
    });
    if !valid_first || !valid_rest || value.is_empty() || value.len() > 128 {
        return Err(AgentError::new("Invalid identifier"));
    }
    Ok(value)
}

/// IP アドレス、または RFC 1123 風のドメインラベル列（末尾ドット可、長さ 1..=253）。
pub fn endpoint_host(value: &str) -> Result<&str> {
    if value.is_empty() || value.len() > 253 {
        return Err(AgentError::new("Invalid endpoint hostname"));
    }
    if value.parse::<std::net::IpAddr>().is_ok() {
        return Ok(value);
    }
    let labels: Vec<&str> = value.trim_end_matches('.').split('.').collect();
    let valid = labels.iter().all(|label| {
        let b = label.as_bytes();
        let first = b.first().is_some_and(|c| c.is_ascii_alphanumeric());
        let last = b.last().is_some_and(|c| c.is_ascii_alphanumeric());
        let middle = b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-');
        first && last && middle && !label.is_empty() && label.len() <= 63
    });
    if valid {
        Ok(value)
    } else {
        Err(AgentError::new("Invalid endpoint hostname"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers() {
        assert!(identifier("gpu-0").is_ok());
        assert!(identifier("GPU:0.a_b").is_ok());
        assert!(identifier("").is_err());
        assert!(identifier("-lead").is_err());
        assert!(identifier(&"x".repeat(129)).is_err());
        assert!(identifier("a b").is_err());
    }

    #[test]
    fn hosts() {
        assert!(endpoint_host("127.0.0.1").is_ok());
        assert!(endpoint_host("::1").is_ok());
        assert!(endpoint_host("gpu.example.org").is_ok());
        assert!(endpoint_host("gpu.example.org.").is_ok());
        assert!(endpoint_host("-bad.example").is_err());
        assert!(endpoint_host("a..b").is_err());
        assert!(endpoint_host("").is_err());
    }
}
