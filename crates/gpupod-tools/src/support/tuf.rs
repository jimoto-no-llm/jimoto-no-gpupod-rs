//! TUF 風メタデータの署名と検証（簡易実装）。
//!
//! Python 版の tuf ライブラリとは互換がない。公開鍵は raw 32 バイト、
//! 署名対象は "signed" 節の決定的 JSON バイト列。

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, Verifier, VerifyingKey};

pub use ed25519_dalek::SigningKey;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const ROLES: [&str; 4] = ["root", "targets", "snapshot", "timestamp"];

/// ルートメタデータに載る信頼情報。
#[derive(Debug, Clone)]
pub struct RootTrust {
    pub keys: BTreeMap<String, [u8; 32]>,
    pub roles: BTreeMap<String, Vec<String>>,
}

pub fn keyid_of(raw: &[u8; 32]) -> String {
    hex::encode(Sha256::digest(raw))
}

pub fn generate_keypair() -> (String, SigningKey) {
    let seed: [u8; 32] = rand::random();
    let key = SigningKey::from_bytes(&seed);
    let raw = key.verifying_key().to_bytes();
    (keyid_of(&raw), key)
}

/// 署名鍵を PKCS#8 DER 経由で PEM 表現にする。
pub fn signing_key_to_pem(key: &SigningKey) -> Result<Vec<u8>> {
    use ed25519_dalek::pkcs8::EncodePrivateKey;
    let document = key
        .to_pkcs8_der()
        .map_err(|exc| anyhow::anyhow!("Cannot encode the private key: {exc}"))?;
    let encoded = STANDARD.encode(document.as_bytes());
    let mut pem = String::from("-----BEGIN PRIVATE KEY-----\n");
    for chunk in encoded.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        pem.push('\n');
    }
    pem.push_str("-----END PRIVATE KEY-----\n");
    Ok(pem.into_bytes())
}

pub fn signing_key_from_pem(bytes: &[u8]) -> Result<SigningKey> {
    use ed25519_dalek::pkcs8::DecodePrivateKey;
    let text = String::from_utf8_lossy(bytes);
    let body: String = text
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("");
    let der = STANDARD
        .decode(body.trim())
        .map_err(|_| anyhow::anyhow!("Invalid private key PEM"))?;
    SigningKey::from_pkcs8_der(&der).map_err(|_| anyhow::anyhow!("Invalid private key PEM"))
}

pub fn verifying_key(key: &SigningKey) -> [u8; 32] {
    key.verifying_key().to_bytes()
}

/// "signed" 節の決定的バイト列（serde_json のキーは昇順）。
pub fn signed_bytes(signed: &Value) -> Result<Vec<u8>> {
    serde_json::to_vec(signed).context("Cannot encode metadata")
}

/// メタデータ文書全体をバイト列化する（ファイル保存・ハッシュ計算用）。
pub fn document_bytes(document: &Value) -> Result<Vec<u8>> {
    let mut text = serde_json::to_string_pretty(document).context("Cannot encode metadata")?;
    text.push('\n');
    Ok(text.into_bytes())
}

/// ドキュメントを署名する。root の場合は全ロールの公開鍵を載せる。
pub fn sign(signed: &Value, keys: &BTreeMap<String, SigningKey>) -> Result<Value> {
    let bytes = signed_bytes(signed)?;
    let mut signatures = Vec::new();
    for key in keys.values() {
        // keyid は公開鍵のダイジェスト。ロール名などを keyid にしない。
        let keyid = keyid_of(&key.verifying_key().to_bytes());
        let signature = key.sign(&bytes);
        signatures.push(json!({
            "keyid": keyid,
            "sig": STANDARD.encode(signature.to_bytes()),
        }));
    }
    Ok(json!({"signed": signed, "signatures": signatures}))
}

/// 役割ごとの署名を検証し "signed" 節を返す。
pub fn verify(document: &Value, role: &str, trust: &RootTrust) -> Result<Value> {
    let Some(signed) = document.get("signed") else {
        bail!("Signed metadata is missing its payload");
    };
    let bytes = signed_bytes(signed)?;
    let allowed = trust
        .roles
        .get(role)
        .ok_or_else(|| anyhow::anyhow!("Unknown metadata role"))?;
    let mut valid = 0usize;
    let signatures = document
        .get("signatures")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("Metadata has no signatures"))?;
    for entry in signatures {
        let Some(keyid) = entry.get("keyid").and_then(|v| v.as_str()) else {
            continue;
        };
        if !allowed.iter().any(|item| item == keyid) {
            continue;
        }
        let Some(raw) = trust.keys.get(keyid) else {
            continue;
        };
        let Some(signature) = entry.get("sig").and_then(|v| v.as_str()) else {
            continue;
        };
        let Ok(decoded) = STANDARD.decode(signature) else {
            continue;
        };
        let Ok(bytes_sig) = <[u8; 64]>::try_from(decoded.as_slice()) else {
            continue;
        };
        let Ok(key) = VerifyingKey::from_bytes(raw) else {
            continue;
        };
        if key
            .verify(&bytes, &Signature::from_bytes(&bytes_sig))
            .is_ok()
        {
            valid += 1;
        }
    }
    if valid == 0 {
        bail!("Metadata signature verification failed");
    }
    Ok(signed.clone())
}

/// root.json から信頼情報を読み込む。
pub fn root_trust(document: &Value) -> Result<RootTrust> {
    let signed = verify_root(document)?;
    let mut keys = BTreeMap::new();
    if let Some(map) = signed.get("keys").and_then(|v| v.as_object()) {
        for (keyid, item) in map {
            let raw = item
                .get("public")
                .and_then(|v| v.as_str())
                .and_then(|value| STANDARD.decode(value).ok())
                .and_then(|bytes| <[u8; 32]>::try_from(bytes.as_slice()).ok());
            let Some(raw) = raw else {
                bail!("Root metadata carries an invalid public key");
            };
            keys.insert(keyid.clone(), raw);
        }
    }
    let mut roles = BTreeMap::new();
    if let Some(map) = signed.get("roles").and_then(|v| v.as_object()) {
        for (role, item) in map {
            let list = item
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            roles.insert(role.clone(), list);
        }
    }
    for role in ROLES {
        if !roles.contains_key(role) {
            bail!("Root metadata does not list the {role} role");
        }
    }
    Ok(RootTrust { keys, roles })
}

/// root は自己署名（root 鍵のみ）を要求する。
pub fn verify_root(document: &Value) -> Result<Value> {
    let Some(signed) = document.get("signed") else {
        bail!("Signed metadata is missing its payload");
    };
    let bytes = signed_bytes(signed)?;
    let Some(allowed) = signed
        .get("roles")
        .and_then(|v| v.get("root"))
        .and_then(|v| v.as_array())
    else {
        bail!("Root metadata does not list the root role");
    };
    let Some(keys) = signed.get("keys").and_then(|v| v.as_object()) else {
        bail!("Root metadata carries no keys");
    };
    let signatures = document
        .get("signatures")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("Metadata has no signatures"))?;
    for entry in signatures {
        let Some(keyid) = entry.get("keyid").and_then(|v| v.as_str()) else {
            continue;
        };
        if !allowed.iter().any(|item| item.as_str() == Some(keyid)) {
            continue;
        }
        let Some(raw) = keys
            .get(keyid)
            .and_then(|v| v.get("public"))
            .and_then(|v| v.as_str())
        else {
            continue;
        };
        let Ok(decoded) = STANDARD.decode(raw) else {
            continue;
        };
        let Ok(raw_bytes) = <[u8; 32]>::try_from(decoded.as_slice()) else {
            continue;
        };
        let Some(signature) = entry
            .get("sig")
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            continue;
        };
        let Ok(decoded) = STANDARD.decode(signature) else {
            continue;
        };
        let Ok(bytes_sig) = <[u8; 64]>::try_from(decoded.as_slice()) else {
            continue;
        };
        let Ok(key) = VerifyingKey::from_bytes(&raw_bytes) else {
            continue;
        };
        if key
            .verify(&bytes, &Signature::from_bytes(&bytes_sig))
            .is_ok()
        {
            return Ok(signed.clone());
        }
    }
    bail!("Root metadata signature verification failed")
}

pub fn public_key_json(key: &SigningKey) -> Value {
    json!({
        "type": "ed25519",
        "public": STANDARD.encode(verifying_key(key)),
    })
}

pub fn read_document(path: &Path) -> Result<Value> {
    let raw = std::fs::read(path).with_context(|| format!("Cannot read {}", path.display()))?;
    serde_json::from_slice(&raw).with_context(|| format!("Cannot parse {}", path.display()))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_verify_for_the_named_role() {
        let (root_id, root_key) = generate_keypair();
        let (targets_id, targets_key) = generate_keypair();
        let targets_public = verifying_key(&targets_key);
        let mut keys = BTreeMap::new();
        keys.insert(targets_id.clone(), targets_key);
        let signed = json!({"_type": "targets", "version": 1});
        let document = sign(&signed, &keys).unwrap();
        let mut roles = BTreeMap::new();
        roles.insert("root".to_string(), vec![root_id.clone()]);
        roles.insert("targets".to_string(), vec![targets_id.clone()]);
        roles.insert("snapshot".to_string(), vec![]);
        roles.insert("timestamp".to_string(), vec![]);
        let mut public = BTreeMap::new();
        public.insert(root_id, verifying_key(&root_key));
        public.insert(targets_id, targets_public);
        let trust = RootTrust {
            keys: public,
            roles,
        };
        assert_eq!(verify(&document, "targets", &trust).unwrap()["version"], 1);
        assert!(verify(&document, "snapshot", &trust).is_err());
    }

    #[test]
    fn tampered_metadata_is_rejected() {
        let (root_id, root_key) = generate_keypair();
        let signed = json!({"_type": "targets", "version": 1});
        let root_public = verifying_key(&root_key);
        let mut keys = BTreeMap::new();
        keys.insert(root_id.clone(), root_key);
        let document = sign(&signed, &keys).unwrap();
        let mut roles = BTreeMap::new();
        roles.insert("root".to_string(), vec![root_id.clone()]);
        roles.insert("targets".to_string(), vec![root_id.clone()]);
        roles.insert("snapshot".to_string(), vec![]);
        roles.insert("timestamp".to_string(), vec![]);
        let mut public = BTreeMap::new();
        public.insert(root_id, root_public);
        let trust = RootTrust {
            keys: public,
            roles,
        };
        let mut tampered = document.clone();
        tampered["signed"]["version"] = json!(2);
        assert!(verify(&tampered, "targets", &trust).is_err());
        assert!(verify(&document, "targets", &trust).is_ok());
    }

    #[test]
    fn root_self_signature_is_required() {
        let (root_id, root_key) = generate_keypair();
        let mut key_map = serde_json::Map::new();
        key_map.insert(root_id.clone(), public_key_json(&root_key));
        let signed = json!({
            "_type": "root",
            "keys": serde_json::Value::Object(key_map),
            "roles": {"root": [root_id.clone()]}
        });
        let mut keys = BTreeMap::new();
        keys.insert(root_id.clone(), root_key);
        let document = sign(&signed, &keys).unwrap();
        assert!(verify_root(&document).is_ok());
        let mut tampered = document.clone();
        tampered["signed"]["_type"] = json!("targets");
        assert!(verify_root(&tampered).is_err());
    }

    #[test]
    fn private_keys_round_trip_through_pem() {
        let (_, key) = generate_keypair();
        let pem = signing_key_to_pem(&key).unwrap();
        let text = String::from_utf8(pem).unwrap();
        assert!(text.starts_with("-----BEGIN PRIVATE KEY-----"));
        let restored = signing_key_from_pem(text.as_bytes()).unwrap();
        assert_eq!(verifying_key(&key), verifying_key(&restored));
    }
}
