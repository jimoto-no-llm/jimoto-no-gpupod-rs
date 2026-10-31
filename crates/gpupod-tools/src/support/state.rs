//! 提供 PC 状態の保存（Python 版 state.py の移植・最小実装）。
//!
//! スキーマは Python 版と同一（agent_state テーブルの payload JSON）。
//! gpupod-agent の lib 実装が揃い次第、こちらの StateStore は置き換えられる。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use gpupod_common::AgentError;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct State {
    pub version: u32,
    pub node_id: String,
    pub manual_enabled: bool,
    pub auto_busy: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_pool: Option<serde_json::Value>,
    #[serde(default)]
    pub leases: BTreeMap<String, serde_json::Value>,
}

impl State {
    fn initial() -> State {
        State {
            version: 1,
            node_id: uuid::Uuid::new_v4().simple().to_string(),
            manual_enabled: false,
            auto_busy: false,
            resource_pool: None,
            leases: BTreeMap::new(),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1
            || self.node_id.is_empty()
            || self
                .leases
                .keys()
                .any(|key| key != self.node_id.as_str() && false)
        {
            bail!(AgentError::new(
                "Unsupported or invalid state; existing reservations retained"
            ));
        }
        Ok(())
    }

    /// 未停止の貸出一覧。
    pub fn active(&self) -> Vec<serde_json::Value> {
        self.leases
            .values()
            .filter(|lease| lease.get("status").and_then(|s| s.as_str()) != Some("stopped"))
            .cloned()
            .collect()
    }
}

#[derive(Debug)]
pub struct StateStore {
    pub directory: PathBuf,
    path: PathBuf,
}

impl StateStore {
    pub fn new(directory: &Path) -> Result<StateStore> {
        super::files::ensure_private_dir(directory)?;
        let path = directory.join("state.sqlite3");
        Ok(StateStore {
            directory: directory.to_path_buf(),
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 排他ロック中で state を渡し、成功した場合のみ保存する。
    pub fn with_locked<F, R>(&self, action: F) -> Result<R>
    where
        F: FnOnce(&mut State) -> Result<R>,
    {
        let _lock = super::files::HeldLock::exclusive(&self.directory.join("state.lock"), 65)?;
        if self.directory.join("state.json").exists() {
            bail!(AgentError::new(
                "Legacy state.json exists; migrate it before starting the SQLite agent"
            ));
        }
        let mut state = self.load()?.unwrap_or_else(State::initial);
        let result = action(&mut state)?;
        self.save(&state)?;
        Ok(result)
    }

    fn load(&self) -> Result<Option<State>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = open(&self.path)?;
        let row: Option<String> =
            match connection.query_row("SELECT payload FROM agent_state WHERE id=1", [], |row| {
                row.get(0)
            }) {
                Ok(value) => Some(value),
                Err(rusqlite::Error::QueryReturnedNoRows) => None,
                Err(exc) => bail!(AgentError::new(format!("Cannot read state: {exc}"))),
            };
        let Some(payload) = row else { return Ok(None) };
        let state: State = serde_json::from_str(&payload).map_err(|_| {
            AgentError::new("Unsupported or invalid state; existing reservations retained")
        })?;
        state.validate()?;
        Ok(Some(state))
    }

    fn save(&self, state: &State) -> Result<()> {
        let connection = open(&self.path)?;
        let payload = serde_json::to_string(state).context("Cannot encode provider state")?;
        connection
            .execute(
                "INSERT INTO agent_state(id, payload) VALUES(1, ?1) \
                 ON CONFLICT(id) DO UPDATE SET payload=excluded.payload",
                [&payload],
            )
            .map_err(|exc| AgentError::new(format!("Cannot save state: {exc}")))?;
        Ok(())
    }
}

fn open(path: &Path) -> Result<rusqlite::Connection> {
    let connection = rusqlite::Connection::open(path).context("Cannot open the provider state")?;
    let _ = super::files::fs_set_mode(path, 0o600);
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS agent_state (id INTEGER PRIMARY KEY CHECK(id=1), payload TEXT NOT NULL)",
        )
        .map_err(|exc| AgentError::new(format!("Cannot prepare the provider state: {exc}")))?;
    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_state() {
        let directory = tempfile::tempdir().unwrap();
        let store = StateStore::new(directory.path()).unwrap();
        let node_id = store
            .with_locked(|state| {
                state.manual_enabled = true;
                Ok(state.node_id.clone())
            })
            .unwrap();
        let restored = store
            .with_locked(|state| {
                assert!(state.manual_enabled);
                assert_eq!(state.version, 1);
                Ok(state.node_id.clone())
            })
            .unwrap();
        assert_eq!(node_id, restored);
    }

    #[test]
    fn rejects_legacy_state_file() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("state.json"), "{}").unwrap();
        let store = StateStore::new(directory.path()).unwrap();
        assert!(store.with_locked(|_| Ok(())).is_err());
    }

    #[test]
    fn active_excludes_stopped_leases() {
        let mut state = State::initial();
        state.leases.insert(
            "a".into(),
            serde_json::json!({"id": "a", "status": "running"}),
        );
        state.leases.insert(
            "b".into(),
            serde_json::json!({"id": "b", "status": "stopped"}),
        );
        assert_eq!(state.active().len(), 1);
    }
}
