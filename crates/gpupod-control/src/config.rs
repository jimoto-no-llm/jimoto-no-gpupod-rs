//! 環境変数による設定（Python 版 control_panel/settings.py の移植）。

use std::path::{Path, PathBuf};

/// 提供PCのハートビート失効秒数（NODE_TIMEOUT_SECONDS）。
pub const NODE_TIMEOUT_SECONDS: i64 = 45;
/// 回線測定のクールダウン秒数（BENCHMARK_COOLDOWN_SECONDS）。
pub const BENCHMARK_COOLDOWN_SECONDS: i64 = 300;
/// セッション有効秒数（SESSION_COOKIE_AGE = 43200）。
pub const SESSION_COOKIE_AGE: i64 = 43200;
/// Discord サーバー ID の既定値（GPUPOD_DISCORD_GUILD_ID で上書きできる）。
pub const DEFAULT_DISCORD_GUILD_ID: &str = "1551195569429422202";

#[derive(Debug, Clone)]
pub struct Config {
    /// 状態保存先（GPUPOD_CONTROL_STATE、既定 ./.control-state）。
    pub state_dir: PathBuf,
    /// 許可される Host ヘッダー（GPUPOD_ALLOWED_HOSTS）。
    pub allowed_hosts: Vec<String>,
    /// 信頼する Origin（GPUPOD_CSRF_ORIGINS）。
    pub csrf_origins: Vec<String>,
    /// 利用者に表示する中継ホスト名（GPUPOD_RELAY_HOST、既定 localhost）。
    pub relay_host: String,
    /// 中継制御ポート（GPUPOD_RELAY_PORT、既定 19000）。
    pub relay_port: u16,
    /// 貸出割当ポート範囲（GPUPOD_RELAY_PORT_MIN / MAX、既定 19001..=19999）。
    pub relay_port_min: u16,
    pub relay_port_max: u16,
    /// 中継のリスン先ホスト（GPUPOD_RELAY_BIND_HOST、既定 0.0.0.0）。
    pub relay_bind_host: String,
    /// リバースプロキシの X-Forwarded-Proto を信頼する（GPUPOD_TRUST_PROXY=1）。
    pub trust_proxy: bool,
    /// HTTP（非 TLS）運用を許可し Secure Cookie を外す（GPUPOD_INSECURE_HTTP=1）。
    pub insecure_http: bool,
    /// Discord OAuth2 のクライアント ID（GPUPOD_DISCORD_CLIENT_ID）。
    pub discord_client_id: Option<String>,
    /// Discord OAuth2 のクライアントシークレット（GPUPOD_DISCORD_CLIENT_SECRET）。
    pub discord_client_secret: Option<String>,
    /// 公開 URL の基底（GPUPOD_PUBLIC_BASE_URL）。未設定時は Host から組み立てる。
    pub public_base_url: Option<String>,
    /// Discord サーバー ID（GPUPOD_DISCORD_GUILD_ID）。設定時は所属必須。
    pub discord_guild_id: Option<String>,
    /// 許可する Discord ロール ID（GPUPOD_DISCORD_ALLOWED_ROLE_IDS、カンマ区切り）。
    pub discord_allowed_role_ids: Vec<String>,
    /// 参加案内に表示する招待 URL（GPUPOD_DISCORD_INVITE_URL、任意）。
    pub discord_invite_url: Option<String>,
}

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_list(key: &str, default: &str) -> Vec<String> {
    env(key, default)
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

fn env_u16(key: &str, default: u16) -> u16 {
    env(key, &default.to_string()).parse().unwrap_or(default)
}

/// 中継ポート範囲の既定値（GPUPOD_RELAY_PORT_MIN / MAX）。
const DEFAULT_RELAY_PORT_MIN: u16 = 19001;
const DEFAULT_RELAY_PORT_MAX: u16 = 19999;

/// 中継ポート範囲を検証して安全な値に寄せる。
///
/// 特権ポート（1024 未満）や逆転した範囲は、中継が起動できない・
/// 予期しないポートを開く原因になるため既定値へ戻す。
fn sanitize_relay_range(min: u16, max: u16) -> (u16, u16) {
    if min < 1024 || max < min {
        tracing::warn!(
            "中継ポート範囲 {min}..={max} は使用できません。既定の {DEFAULT_RELAY_PORT_MIN}..={DEFAULT_RELAY_PORT_MAX} を使います"
        );
        (DEFAULT_RELAY_PORT_MIN, DEFAULT_RELAY_PORT_MAX)
    } else {
        (min, max)
    }
}

/// 制御ポート（中継 SSH）の検証。0 は待ち受けできないため既定へ戻す。
fn sanitize_relay_port(port: u16) -> u16 {
    if port == 0 {
        tracing::warn!("GPUPOD_RELAY_PORT=0 は使用できません。既定の 19000 を使います");
        19000
    } else {
        port
    }
}

/// OAuth のリダイレクト基底に使える「スキーム://ホスト[:ポート]」だけを通す。
///
/// パス・クエリ・フラグメント・ユーザー情報・制御文字を含む値は
/// リダイレクト先の組み立てを壊すため設定ミスとして無視する。
fn valid_public_base(value: &str) -> bool {
    let trimmed = value.trim_end_matches('/');
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return false;
    };
    if !matches!(scheme, "http" | "https") {
        return false;
    }
    if rest.is_empty()
        || rest.contains('/')
        || rest.contains('?')
        || rest.contains('#')
        || rest.contains('@')
        || rest.chars().any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return false;
    }
    true
}

/// 空文字を未設定として扱う任意の環境変数。
fn env_opt(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

impl Config {
    pub fn from_env() -> Config {
        let state_dir = std::env::var_os("GPUPOD_CONTROL_STATE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_default()
                    .join(".control-state")
            });
        let discord_client_id = env_opt("GPUPOD_DISCORD_CLIENT_ID");
        let discord_client_secret = env_opt("GPUPOD_DISCORD_CLIENT_SECRET");
        if discord_client_id.is_some() != discord_client_secret.is_some() {
            tracing::warn!(
                "Discord ログインの設定が不完全です（GPUPOD_DISCORD_CLIENT_ID と GPUPOD_DISCORD_CLIENT_SECRET の両方を設定してください）"
            );
        }
        // 参加要件は既定で公式 Discord サーバー。環境変数で上書きできる。
        let discord_guild_id = env_opt("GPUPOD_DISCORD_GUILD_ID")
            .or_else(|| Some(DEFAULT_DISCORD_GUILD_ID.to_string()));
        let discord_allowed_role_ids = env_list("GPUPOD_DISCORD_ALLOWED_ROLE_IDS", "");
        let discord_invite_url = env_opt("GPUPOD_DISCORD_INVITE_URL");
        let public_base_url = env_opt("GPUPOD_PUBLIC_BASE_URL");
        if let Some(value) = &public_base_url {
            if !valid_public_base(value) {
                tracing::warn!(
                    "GPUPOD_PUBLIC_BASE_URL が http(s)://ホスト[:ポート] の形式ではありません。Host ヘッダーから組み立てます"
                );
            }
        }
        let (relay_port_min, relay_port_max) = sanitize_relay_range(
            env_u16("GPUPOD_RELAY_PORT_MIN", DEFAULT_RELAY_PORT_MIN),
            env_u16("GPUPOD_RELAY_PORT_MAX", DEFAULT_RELAY_PORT_MAX),
        );
        let public_base_url = public_base_url.filter(|value| valid_public_base(value));
        Config {
            state_dir,
            allowed_hosts: env_list("GPUPOD_ALLOWED_HOSTS", "localhost,127.0.0.1,[::1]"),
            csrf_origins: env_list("GPUPOD_CSRF_ORIGINS", ""),
            relay_host: env("GPUPOD_RELAY_HOST", "localhost"),
            relay_port: sanitize_relay_port(env_u16("GPUPOD_RELAY_PORT", 19000)),
            relay_port_min,
            relay_port_max,
            relay_bind_host: env("GPUPOD_RELAY_BIND_HOST", "0.0.0.0"),
            trust_proxy: env("GPUPOD_TRUST_PROXY", "0") == "1",
            insecure_http: env("GPUPOD_INSECURE_HTTP", "0") == "1",
            discord_client_id,
            discord_client_secret,
            public_base_url,
            discord_guild_id,
            discord_allowed_role_ids,
            discord_invite_url,
        }
    }

    /// Discord ログインが使えるか（client_id と secret の両方が必要）。
    pub fn discord_enabled(&self) -> bool {
        self.discord_client_id.is_some() && self.discord_client_secret.is_some()
    }

    /// リダイレクト URI の基底 URL（PUBLIC_BASE_URL 優先、未設定は Host から）。
    ///
    /// Host が空なら既定値 `http://localhost:8000` を返す。
    pub fn public_base(&self, headers: &axum::http::HeaderMap) -> String {
        if let Some(base) = &self.public_base_url {
            let trimmed = base.trim_end_matches('/');
            // 直接 Config を組み立てた場合も、壊れた基底でリダイレクトしない。
            if !trimmed.is_empty() && valid_public_base(trimmed) {
                return trimmed.to_string();
            }
        }
        let host = headers
            .get(axum::http::header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .trim();
        if host.is_empty() {
            return "http://localhost:8000".to_string();
        }
        format!("{}://{}", crate::auth::request_scheme(headers, self), host)
    }

    pub fn db_path(&self) -> PathBuf {
        self.state_dir.join("control.sqlite3")
    }

    pub fn host_key_path(&self) -> PathBuf {
        self.state_dir.join("relay-host-key")
    }

    pub fn secret_key_path(&self) -> PathBuf {
        self.state_dir.join("secret-key")
    }

    /// STATE_DIR の作成とセッション署名鍵の用意（settings.py 相当）。
    pub fn ensure_state_dir(&self) -> std::io::Result<()> {
        create_dir_private(&self.state_dir)?;
        let secret = self.secret_key_path();
        if !secret.exists() {
            write_private(&secret, format!("{}\n", random_token(64)).as_bytes())?;
        }
        // 既存の署名鍵が緩い権限で残っていても、他ユーザーから読めないよう直す。
        restrict_private(&secret);
        Ok(())
    }
}

/// 親ディレクトリを権限 0700 で作る。
pub fn create_dir_private(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// 権限 0600 で新規作成する（既存は上書きしない）。
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    #[cfg(unix)]
    let mut options = {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        options
    };
    #[cfg(not(unix))]
    let mut options = {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        options
    };
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

/// 中継のリスン先ホスト（settings.RELAY_BIND_HOST）。
pub fn relay_bind_host() -> String {
    std::env::var("GPUPOD_RELAY_BIND_HOST").unwrap_or_else(|_| "0.0.0.0".to_string())
}

/// 既存ファイルの権限を 0600 に寄せる（失敗は無視。呼び出し側は秘密の保護だけが目的）。
pub fn restrict_private(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            if metadata.permissions().mode() & 0o777 != 0o600 {
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
}

/// URL Safe なランダムトークン（secrets.token_urlsafe 相当）。
///
/// セッション署名鍵や機材キーに使うため OS の乱数源から取る。
pub fn random_token(bytes: usize) -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut buffer = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut buffer);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_range_rejects_privileged_or_inverted_values() {
        assert_eq!(sanitize_relay_range(19001, 19999), (19001, 19999));
        assert_eq!(sanitize_relay_range(1024, 1024), (1024, 1024));
        // 特権ポートを含む範囲は既定へ戻す。
        assert_eq!(
            sanitize_relay_range(22, 19999),
            (DEFAULT_RELAY_PORT_MIN, DEFAULT_RELAY_PORT_MAX)
        );
        // 逆転した範囲も既定へ戻す。
        assert_eq!(
            sanitize_relay_range(20000, 19001),
            (DEFAULT_RELAY_PORT_MIN, DEFAULT_RELAY_PORT_MAX)
        );
    }

    #[test]
    fn relay_port_zero_falls_back_to_default() {
        assert_eq!(sanitize_relay_port(0), 19000);
        assert_eq!(sanitize_relay_port(19000), 19000);
        assert_eq!(sanitize_relay_port(20000), 20000);
    }

    #[test]
    fn public_base_accepts_only_origins() {
        assert!(valid_public_base("https://gpupod.example.org"));
        assert!(valid_public_base("http://127.0.0.1:8000/"));
        assert!(valid_public_base("https://gpupod.example.org:8443"));
        assert!(!valid_public_base("javascript:alert(1)"));
        assert!(!valid_public_base("https://user@evil.example.org"));
        assert!(!valid_public_base("https://gpupod.example.org/path"));
        assert!(!valid_public_base("https://gpupod.example.org?x=1"));
        assert!(!valid_public_base("https://gpupod.example.org#frag"));
        assert!(!valid_public_base("ftp://gpupod.example.org"));
        assert!(!valid_public_base(""));
        assert!(!valid_public_base("https://exa mple.org"));
    }

    #[test]
    fn random_tokens_are_url_safe_and_unique() {
        let first = random_token(32);
        let second = random_token(32);
        assert_ne!(first, second);
        assert!(first
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')));
    }
}
