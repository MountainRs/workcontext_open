use crate::{backup, schema_version, supported_version};
use rusqlite::{Connection, OpenFlags};
use std::{fs::{File, OpenOptions}, io, ops::{Deref, DerefMut}, path::Path, time::Duration};

#[derive(Debug, thiserror::Error)]
pub enum ExternalOpenError {
    #[error("数据库正在维护或有请求尚未结束")]
    Busy,
    #[error("数据库有未完成的恢复，请启动桌面工作台")]
    Restoring,
    #[error("工作库版本 {0} 与当前支持的版本不一致")]
    Version(u32),
    #[error("无法打开工作库或维护锁：{0}")]
    Io(#[from] io::Error),
    #[error("无法读取工作库：{0}")]
    Database(#[from] rusqlite::Error),
}
impl ExternalOpenError {
    pub fn code(&self) -> &'static str {
        match self { Self::Busy => "MAINTENANCE_BUSY", Self::Restoring => "RESTORE_PENDING", Self::Version(v) if *v > supported_version() => "SCHEMA_TOO_NEW", Self::Version(_) => "MIGRATION_REQUIRED", _ => "STORE_UNAVAILABLE" }
    }
}

pub(crate) fn lock(path: &Path, exclusive: bool, create: bool) -> Result<File, ExternalOpenError> {
    let file = OpenOptions::new().read(true).write(exclusive || create).create(create)
        .open(backup::db_sidecar_path(path, ".maintenance.lock"))?;
    let result = if exclusive { file.try_lock() } else { file.try_lock_shared() };
    result.map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => ExternalOpenError::Busy,
        std::fs::TryLockError::Error(e) => ExternalOpenError::Io(e),
    })?;
    Ok(file)
}

/// Connection drops before its OS lease, including during unwinding.
#[derive(Debug)]
pub struct ExternalConnection { db: Connection, _lease: File }
impl Deref for ExternalConnection { type Target = Connection; fn deref(&self) -> &Connection { &self.db } }
impl DerefMut for ExternalConnection { fn deref_mut(&mut self) -> &mut Connection { &mut self.db } }

pub fn open_external(path: &Path, writable: bool) -> Result<ExternalConnection, ExternalOpenError> {
    let lease = match lock(path, false, false) {
        Err(ExternalOpenError::Io(err)) if err.kind() == io::ErrorKind::NotFound && path.is_file() => lock(path, false, true)?,
        other => other?,
    };
    if backup::restore_marker_path(path).try_exists()? { return Err(ExternalOpenError::Restoring); }
    let flags = if writable { OpenFlags::SQLITE_OPEN_READ_WRITE } else { OpenFlags::SQLITE_OPEN_READ_ONLY };
    let db = Connection::open_with_flags(path, flags)?;
    db.busy_timeout(Duration::from_secs(5))?;
    let version = schema_version(&db)?;
    if version != supported_version() { return Err(ExternalOpenError::Version(version)); }
    db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF;")?;
    Ok(ExternalConnection { db, _lease: lease })
}
