//! 環境変数による設定（Python 版 control_panel/settings.py の移植）。

use std::path::{Path, PathBuf};

/// 提供PCのハートビート失効秒数（NODE_TIMEOUT_SECONDS）。
pub const NODE_TIMEOUT_SECONDS: i64 = 45;
/// 回線測定のクールダウン秒数（BENCHMARK_COOLDOWN_SECONDS）。
pub const BENCHMARK_COOLDOWN_SECONDS: i64 = 300;
/// セッション有効秒数（SESSION_COOKIE_AGE = 43200）。
pub const SESSION_COOKIE_AGE: i64 = 43200;

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

impl Config {
    pub fn from_env() -> Config {
        let state_dir = std::env::var_os("GPUPOD_CONTROL_STATE")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default().join(".control-state"));
        Config {
            state_dir,
            allowed_hosts: env_list("GPUPOD_ALLOWED_HOSTS", "localhost,127.0.0.1,[::1]"),
            csrf_origins: env_list("GPUPOD_CSRF_ORIGINS", ""),
            relay_host: env("GPUPOD_RELAY_HOST", "localhost"),
            relay_port: env_u16("GPUPOD_RELAY_PORT", 19000),
            relay_port_min: env_u16("GPUPOD_RELAY_PORT_MIN", 19001),
            relay_port_max: env_u16("GPUPOD_RELAY_PORT_MAX", 19999),
            relay_bind_host: env("GPUPOD_RELAY_BIND_HOST", "0.0.0.0"),
            trust_proxy: env("GPUPOD_TRUST_PROXY", "0") == "1",
            insecure_http: env("GPUPOD_INSECURE_HTTP", "0") == "1",
        }
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

/// URL Safe なランダムトークン（secrets.token_urlsafe 相当）。
pub fn random_token(bytes: usize) -> String {
    use base64::Engine;
    let mut buffer = vec![0u8; bytes];
    rand::Rng::fill(&mut rand::thread_rng(), &mut buffer[..]);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buffer)
}
