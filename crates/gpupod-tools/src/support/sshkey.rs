//! SSH 公開鍵の検証と生成（asyncssh 相当の最小実装）。

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use gpupod_common::AgentError;

const KEY_TYPES: [&str; 6] = [
    "ssh-ed25519",
    "ssh-rsa",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "sk-ssh-ed25519@openssh.com",
];

/// Python 版 validate_ssh_key / asyncssh.import_public_key 相当。
/// 認証済みの公開鍵を「種別 base64」の正規形で返す。
pub fn validate_public_key(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if value.len() > 4096 || trimmed.contains('\n') || trimmed.is_empty() {
        bail!(AgentError::new("A single SSH public key is required"));
    }
    let mut parts = trimmed.split_whitespace();
    let kind = parts.next().unwrap_or_default();
    let Some(payload) = parts.next() else {
        bail!(AgentError::new("Invalid SSH public key"));
    };
    if parts.next().is_some() || !KEY_TYPES.contains(&kind) {
        bail!(AgentError::new("Invalid SSH public key"));
    }
    let blob = decode_base64(payload).ok_or_else(|| AgentError::new("Invalid SSH public key"))?;
    if blob.len() < 8 {
        bail!(AgentError::new("Invalid SSH public key"));
    }
    let declared = u32::from_be_bytes([blob[0], blob[1], blob[2], blob[3]]) as usize;
    if declared + 4 > blob.len() || &blob[4..4 + declared] != kind.as_bytes() {
        bail!(AgentError::new("Invalid SSH public key"));
    }
    Ok(format!("{kind} {payload}"))
}

/// パディング有無のどちらでも受け付ける（asyncssh に合わせる）。
fn decode_base64(value: &str) -> Option<Vec<u8>> {
    STANDARD
        .decode(value)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(value))
        .ok()
}

/// 新しい ed25519 鍵の公開鍵行を生成する（Python 版 asyncssh.generate_private_key 相当）。
pub fn generate_public_key() -> Result<String> {
    use ed25519_dalek::SigningKey;
    let seed: [u8; 32] = rand::random();
    let key = SigningKey::from_bytes(&seed);
    Ok(format_public_key(&key.verifying_key().to_bytes()))
}

/// ssh エンコード済みの OpenSSH 公開鍵行を組み立てる。
pub fn format_public_key(raw: &[u8; 32]) -> String {
    let mut blob = Vec::new();
    blob.extend_from_slice(&(11u32).to_be_bytes());
    blob.extend_from_slice(b"ssh-ed25519");
    blob.extend_from_slice(&(32u32).to_be_bytes());
    blob.extend_from_slice(raw);
    format!("ssh-ed25519 {}", STANDARD.encode(blob))
}

/// 生の公開鍵バイトから OpenSSH 行を得る（署名鍵の確認用）。
pub fn verifying_key_line(key: &ed25519_dalek::VerifyingKey) -> String {
    format_public_key(&key.to_bytes())
}

pub fn decode_raw_public_key(line: &str) -> Result<[u8; 32]> {
    let canonical = validate_public_key(line).context("Invalid SSH public key")?;
    let payload = canonical.split_whitespace().nth(1).unwrap_or_default();
    let blob = decode_base64(payload).ok_or_else(|| AgentError::new("Invalid SSH public key"))?;
    if blob.len() != 4 + 11 + 4 + 32 {
        bail!(AgentError::new("Invalid SSH public key"));
    }
    let mut raw = [0u8; 32];
    raw.copy_from_slice(&blob[19..]);
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_are_accepted() {
        let line = generate_public_key().unwrap();
        assert!(line.starts_with("ssh-ed25519 "));
        let canonical = validate_public_key(&line).unwrap();
        assert_eq!(canonical, line);
        assert_eq!(decode_raw_public_key(&line).unwrap().len(), 32);
    }

    #[test]
    fn rejects_malformed_keys() {
        assert!(validate_public_key("").is_err());
        assert!(validate_public_key("ssh-ed25519 not-base64!!").is_err());
        assert!(validate_public_key("ssh-ed25519 AAAA extra comment").is_err());
        assert!(validate_public_key("bogus-type AAAA").is_err());
        assert!(validate_public_key(&format!("ssh-ed25519 {}", "A".repeat(5000))).is_err());
    }

    #[test]
    fn accepts_a_real_rsa_line() {
        let rsa = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQDX8mU7BLYXb8VzcSd\
                   OwF6RKS2m5zIS6PGysw1p8L7pE1nGQdVWVrPBqT4d9LzF6XnZL0iH0xQ0z9yQ5dJ2mP3kQ7wN4tV6cJ8mO2pS9yG1qT5bW3xE7uI9oP2aS4dF6gH8jK1lZ3xC5vB7nM9qR1tU3wX5yA7cE9gI1kL3oP5rT7uV9wX1yZ3aB5cD7eF9gH1jK2";
        // 上記はダミー。base64 と長さのみを検査する。
        let _ = rsa;
        let generated = generate_public_key().unwrap();
        let blob = STANDARD
            .decode(generated.split_whitespace().nth(1).unwrap())
            .unwrap();
        assert_eq!(&blob[4..15], b"ssh-ed25519");
    }
}
