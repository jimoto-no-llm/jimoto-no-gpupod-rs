//! 原子書き込みとパーミッション制御（Python 版 files.py の移植）。

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use gpupod_common::AgentError;

/// JSON をインデント 2 で改行付きにして原子的に書き込む。
pub fn atomic_json(path: impl AsRef<Path>, value: &serde_json::Value) -> Result<(), AgentError> {
    let mut text =
        serde_json::to_string_pretty(value).map_err(|exc| AgentError::new(format!("{exc}")))?;
    text.push('\n');
    atomic_bytes(path, text.as_bytes(), 0o600)
}

/// 一時ファイルを作って fsync → chmod → rename → 親ディレクトリ fsync。
pub fn atomic_bytes(path: impl AsRef<Path>, value: &[u8], mode: u32) -> Result<(), AgentError> {
    let path = path.as_ref();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    // 既存ディレクトリ（/etc など）の権限は変えない。新規に作った時だけ 0700 にする。
    let parent_existed = parent.exists();
    fs::create_dir_all(parent).map_err(|exc| AgentError::new(format!("{exc}")))?;
    #[cfg(unix)]
    {
        if !parent_existed {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    let temporary = temporary_path(parent, path)?;
    let result = (|| -> std::io::Result<()> {
        let mut stream = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temporary)?;
        stream.write_all(value)?;
        stream.flush()?;
        stream.sync_all()?;
        fs::rename(&temporary, path)?;
        let directory = fs::File::open(parent)?;
        directory.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|exc| AgentError::new(format!("{exc}")))
}

/// tempfile.mkstemp 相当。既存ファイルを壊さない一意な名前を作る。
fn temporary_path(parent: &Path, path: &Path) -> Result<std::path::PathBuf, AgentError> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let pid = std::process::id();
    for attempt in 0..128u32 {
        let candidate = parent.join(format!(".{name}.{pid}.{attempt}"));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => {
                let _ = fs::remove_file(&candidate);
                return Ok(candidate);
            }
            Err(exc) if exc.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(exc) => return Err(AgentError::new(format!("{exc}"))),
        }
    }
    Err(AgentError::new(
        "Cannot create a temporary file for the atomic write",
    ))
}
