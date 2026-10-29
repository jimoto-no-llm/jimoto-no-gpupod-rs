//! 機材キー（ノードクレデンシャル）のダイジェストと照合。

use sha2::{Digest, Sha256};

/// SHA-256 の小文字16進。Python 版 token_digest と同一。
pub fn token_digest(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// 常時比較によるダイジェスト照合（タイミング攻撃を避ける）。
/// 長さが異なる場合も比較を続け、結果を XOR で合成する。
pub fn digest_equals(a: &str, b: &str) -> bool {
    let mut result = a.len() ^ b.len();
    for (x, y) in a.bytes().zip(b.bytes()) {
        result |= (x ^ y) as usize;
    }
    result == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_sha256_hex() {
        // printf 'token' | sha256sum
        assert_eq!(
            token_digest("token"),
            "3c469e9d6c5875d37a43f353d4f88e61fcf812c66eee3457465a40b0da4153e0"
        );
        assert_eq!(token_digest("token").len(), 64);
        assert_ne!(token_digest("token"), token_digest("token2"));
    }

    #[test]
    fn digest_equals_handles_length() {
        assert!(digest_equals("abc", "abc"));
        assert!(!digest_equals("abc", "abd"));
        assert!(!digest_equals("abc", "abcd"));
        assert!(!digest_equals("", "a"));
    }
}
