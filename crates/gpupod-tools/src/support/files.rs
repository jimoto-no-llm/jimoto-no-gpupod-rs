//! 原子的なプライベートファイル書込とファイルロック（Python 版 files.py の移植）。

use std::fs::{File, Permissions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// mkstemp + fsync + rename で置き換える。既存の symlink を辿らない。
pub fn atomic_bytes(path: &Path, value: &[u8], mode: u32) -> Result<()> {
    let parent = parent_or_dot(path);
    ensure_private_dir(&parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(&parent)
        .with_context(|| format!("Cannot create a temporary file in {}", parent.display()))?;
    temporary
        .write_all(value)
        .with_context(|| format!("Cannot write {}", temporary.path().display()))?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| "Cannot flush the temporary file".to_string())?;
    fs_set_mode(temporary.path(), mode)?;
    temporary
        .persist(path)
        .map_err(|exc| exc.error)
        .with_context(|| format!("Cannot publish {}", path.display()))?;
    sync_directory(&parent);
    Ok(())
}

pub fn atomic_json(path: &Path, value: &serde_json::Value) -> Result<()> {
    let body = serde_json::to_string_pretty(value).context("Cannot encode JSON")? + "\n";
    atomic_bytes(path, body.as_bytes(), 0o600)
}

/// 既存ファイルを上書きせず、リンクで公開する（Python 版 support bundle の os.link 相当）。
pub fn link_overwrite_free(temporary: &Path, output: &Path) -> Result<()> {
    if output.exists() || output.symlink_metadata().is_ok() {
        bail!("Output already exists; choose a new filename");
    }
    std::fs::hard_link(temporary, output)
        .with_context(|| format!("Cannot publish {}", output.display()))?;
    Ok(())
}

pub fn fs_set_mode(path: &Path, mode: u32) -> Result<()> {
    std::fs::set_permissions(path, Permissions::from_mode(mode))
        .with_context(|| format!("Cannot set permissions on {}", path.display()))
}

/// ディレクトリが未作成なら 0700 で作る（Python 版 mkdir(mode=0o700) 相当）。
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(path)
        .with_context(|| format!("Cannot create directory {}", path.display()))?;
    let _ = fs_set_mode(path, 0o700);
    Ok(())
}

fn parent_or_dot(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

fn sync_directory(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

/// 排他ファイルロック。取得できない場合のみ None を返す（Python の FileLock(timeout=0) 相当）。
pub struct HeldLock {
    file: File,
}

impl HeldLock {
    pub fn try_exclusive(path: &Path) -> Result<Option<HeldLock>> {
        let file = open_lock(path)?;
        match fs4::FileExt::try_lock(&file) {
            Ok(()) => Ok(Some(HeldLock { file })),
            Err(_) => Ok(None),
        }
    }

    pub fn exclusive(path: &Path, timeout_seconds: u64) -> Result<HeldLock> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_seconds);
        loop {
            if let Some(lock) = Self::try_exclusive(path)? {
                return Ok(lock);
            }
            if std::time::Instant::now() >= deadline {
                bail!("Another provider operation is still running");
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

impl Drop for HeldLock {
    fn drop(&mut self) {
        let _ = fs4::FileExt::unlock(&self.file);
    }
}

fn open_lock(path: &Path) -> Result<File> {
    let parent = parent_or_dot(path);
    ensure_private_dir(&parent)?;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("Cannot open the lock file {}", path.display()))
}

