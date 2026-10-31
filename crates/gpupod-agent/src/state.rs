//! SQLite 単一行ペイロードとファイルロックによる直列化（Python 版 state.py の移植）。
//!
//! グローバルロックは state.lock（最大 65 秒）、貸出ごとの操作ロックは
//! lease-<id>.lock（試行のみ、非ブロッキング）。

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs4::{FileExt, TryLockError};
use gpupod_common::resources::Pool;
use gpupod_common::AgentError;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::agent::Lease;

pub type Result<T> = std::result::Result<T, AgentError>;

/// グローバルロックの最大待ち時間（Python 版 filelock timeout=65 秒相当）。
const LOCK_TIMEOUT_SECONDS: u64 = 65;

/// agent_state テーブルの単一行ペイロード（v1 スキーマ）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentState {
    pub version: u32,
    pub node_id: String,
    pub manual_enabled: bool,
    pub auto_busy: bool,
    pub leases: BTreeMap<String, Lease>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_pool: Option<Pool>,
    /// 中央コマンドの結果。最大 100 件は daemon 側で保つ。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub command_results: BTreeMap<String, serde_json::Value>,
    /// 未知のキー（将来のスキーマ）は保存時に落とさない。
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl AgentState {
    /// Python 版 locked() と同じ妥当性検査。v1 のみ受け付ける。
    pub fn validate(&self) -> std::result::Result<(), ()> {
        if self.version != 1 {
            return Err(());
        }
        Ok(())
    }

    /// 新規ノードの既定状態。
    pub fn fresh() -> Self {
        Self {
            version: 1,
            node_id: uuid::Uuid::new_v4().simple().to_string(),
            manual_enabled: false,
            auto_busy: false,
            leases: BTreeMap::new(),
            resource_pool: None,
            command_results: BTreeMap::new(),
            extra: BTreeMap::new(),
        }
    }
}

const INVALID_STATE: &str = "Unsupported or invalid state; existing reservations retained";

fn wrap_state(exc: impl std::fmt::Display) -> AgentError {
    AgentError::new(format!(
        "Cannot read or save state; existing reservations retained: {exc}"
    ))
}

/// 提供ノード 1 台分の状態保存庫。
#[derive(Debug, Clone)]
pub struct StateStore {
    pub directory: PathBuf,
    pub path: PathBuf,
    pub lock_path: PathBuf,
}

impl StateStore {
    /// 状態ディレクトリを 0700 で用意する。
    pub fn new(directory: impl AsRef<Path>) -> Result<Self> {
        let directory = directory.as_ref().to_path_buf();
        std::fs::create_dir_all(&directory).map_err(|exc| AgentError::new(format!("{exc}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700));
        }
        let path = directory.join("state.sqlite3");
        let lock_path = directory.join("state.lock");
        Ok(Self {
            directory,
            path,
            lock_path,
        })
    }

    /// Python 版 StateStore.operation：貸出ごとのロックを試行する。
    ///
    /// 既に取得済みなら acquired=false を返し、呼び出し側が判断する。
    pub fn operation(&self, lease_id: &str) -> Result<OperationLock> {
        gpupod_common::identifier::identifier(lease_id)?;
        let path = self.directory.join(format!("lease-{lease_id}.lock"));
        let file = open_lock(&path)?;
        let acquired = match FileExt::try_lock(&file) {
            Ok(()) => true,
            Err(TryLockError::WouldBlock) => false,
            Err(TryLockError::Error(exc)) => {
                return Err(AgentError::new(format!("{exc}")));
            }
        };
        Ok(OperationLock { file, acquired })
    }

    /// Python 版 StateStore.locked：グローバルロックを取得して状態を返す。
    pub fn locked(&self) -> Result<StateGuard<'_>> {
        let file = open_lock(&self.lock_path)?;
        let deadline = Instant::now() + Duration::from_secs(LOCK_TIMEOUT_SECONDS);
        loop {
            match FileExt::try_lock(&file) {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Err(AgentError::new(
                            "Another provider operation is still running",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(TryLockError::Error(exc)) => {
                    return Err(AgentError::new(format!("{exc}")));
                }
            }
        }
        if self.directory.join("state.json").exists() {
            return Err(AgentError::new(
                "Legacy state.json exists; migrate it before starting the SQLite agent",
            ));
        }
        let state = self.read()?;
        Ok(StateGuard {
            store: self,
            _file: file,
            state,
        })
    }

    /// ペイロードを読み、v1 妥当性を確かめる。行がなければ既定値を作る。
    fn read(&self) -> Result<AgentState> {
        let payload = match self.load_payload()? {
            Some(payload) => payload,
            None => {
                let state = AgentState::fresh();
                self.save(&state)?;
                return Ok(state);
            }
        };
        let value: serde_json::Value =
            serde_json::from_str(&payload).map_err(|_| AgentError::new(INVALID_STATE))?;
        let object = value
            .as_object()
            .ok_or_else(|| AgentError::new(INVALID_STATE))?;
        let version_ok = object.get("version").and_then(|v| v.as_u64()) == Some(1);
        let leases_ok = object.get("leases").map(|v| v.is_object()).unwrap_or(false);
        let node_ok = object
            .get("node_id")
            .map(|v| v.is_string())
            .unwrap_or(false);
        let manual_ok = object
            .get("manual_enabled")
            .map(|v| v.is_boolean())
            .unwrap_or(false);
        let busy_ok = object
            .get("auto_busy")
            .map(|v| v.is_boolean())
            .unwrap_or(false);
        if !(version_ok && leases_ok && node_ok && manual_ok && busy_ok) {
            return Err(AgentError::new(INVALID_STATE));
        }
        let state: AgentState =
            serde_json::from_value(value).map_err(|_| AgentError::new(INVALID_STATE))?;
        state
            .validate()
            .map_err(|_| AgentError::new(INVALID_STATE))?;
        Ok(state)
    }

    /// agent_state の 1 行を返す。
    fn load_payload(&self) -> Result<Option<String>> {
        let connection = self.connection().map_err(wrap_state)?;
        let mut statement = connection
            .prepare("SELECT payload FROM agent_state WHERE id=1")
            .map_err(wrap_state)?;
        let mut rows = statement.query([]).map_err(wrap_state)?;
        match rows.next().map_err(wrap_state)? {
            Some(row) => {
                let payload: String = row.get(0).map_err(wrap_state)?;
                Ok(Some(payload))
            }
            None => Ok(None),
        }
    }

    /// 状態を 1 行に保存する。
    pub fn save(&self, state: &AgentState) -> Result<()> {
        let payload = serde_json::to_string(state).map_err(|exc| wrap_state(exc))?;
        let connection = self.connection().map_err(wrap_state)?;
        connection
            .execute(
                "INSERT INTO agent_state(id, payload) VALUES(1, ?1) \
                 ON CONFLICT(id) DO UPDATE SET payload=excluded.payload",
                rusqlite::params![payload],
            )
            .map_err(wrap_state)?;
        Ok(())
    }

    /// 接続を作ってスキーマを用意する（Python 版 _connection 相当）。
    fn connection(&self) -> std::result::Result<Connection, AgentError> {
        let connection =
            Connection::open(&self.path).map_err(|exc| AgentError::new(format!("{exc}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
        let _ = connection.busy_timeout(Duration::from_secs(30));
        match connection.execute(
            "CREATE TABLE IF NOT EXISTS agent_state (id INTEGER PRIMARY KEY CHECK(id=1), payload TEXT NOT NULL)",
            [],
        ) {
            Ok(_) => Ok(connection),
            Err(exc) => Err(AgentError::new(format!("{exc}"))),
        }
    }
}

fn open_lock(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|exc| AgentError::new(format!("{exc}")))
}

/// 貸出ごとの操作ロック。解放は Drop（flock 解放）。
#[derive(Debug)]
pub struct OperationLock {
    #[allow(dead_code)]
    file: File,
    acquired: bool,
}

impl OperationLock {
    /// ロックを取得できたか。Python 版の yield 値に相当する。
    pub fn acquired(&self) -> bool {
        self.acquired
    }
}

/// グローバルロックを保持した状態。save() で書き戻す。
#[derive(Debug)]
pub struct StateGuard<'a> {
    store: &'a StateStore,
    _file: File,
    state: AgentState,
}

impl StateGuard<'_> {
    /// Python 版 store.save(state) 相当。
    pub fn save(&mut self) -> Result<()> {
        self.store.save(&self.state)
    }

    /// 状態の参照。
    pub fn state(&self) -> &AgentState {
        &self.state
    }
}

impl Deref for StateGuard<'_> {
    type Target = AgentState;
    fn deref(&self) -> &AgentState {
        &self.state
    }
}

impl DerefMut for StateGuard<'_> {
    fn deref_mut(&mut self) -> &mut AgentState {
        &mut self.state
    }
}
