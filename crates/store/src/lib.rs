//! SQLite 存储层：连接 PRAGMA、只前进迁移、备份/恢复、保留清理与诊断
//! （详细设计 §4.1、§4.3、§14）。
//!
//! 入口 [`open`]：打开（不存在则创建）数据库文件，每次打开都设置 §4.1 的连接
//! PRAGMA 并验证 WAL 生效；`user_version` 低于程序支持时逐文件原子执行
//! `migrations/NNNN_name.sql`（BEGIN IMMEDIATE → 执行 → user_version=N → COMMIT，
//! 任一步失败即整体回滚、版本不前移）；高于程序支持则拒绝打开，防止旧程序写坏新库。
//! [`open_with_backup`] 在此之上按 §4.3 步骤 2 强制「迁移前备份」：
//! 有待迁移时先 `VACUUM INTO` pre-migration 备份，备份失败即中止迁移。
//!
//! - 备份/恢复（§14.1/§14.2）：[`backup_now`] 一致性快照 + manifest、
//!   [`verify_backup`] 只读预检、[`restore_from`] 安全恢复（保留失败前库、
//!   拒绝坏包、失败回滚）、[`cleanup_backups`] 保留策略。
//! - 启动检查：[`open`] / [`open_with_backup`] 先看 `<db>.restore-pending`
//!   标记——存在即说明上次恢复未被确认，先回滚到原库；原库与当前库都不存在
//!   则返回 [`OpenError::InterruptedRestore`] 拒绝打开，绝不静默建空库。
//! - [`retention_sweep`]（§14.3）：过期正文分批置空，FTS 同步清除。
//! - [`collect_diagnostics`]：诊断摘要，只有计数与状态，无正文。
//!
//! 迁移列表由 build.rs 从仓库 `migrations/` 自动生成，文件是唯一事实来源。
//! 仓储与 FTS 查询模块在后续里程碑扩展本 crate。

use std::path::Path;

use rusqlite::{Connection, TransactionBehavior};

pub use rusqlite;

pub use backup::{
    backup_list, backup_list_verified, backup_now, cleanup_backups, now_ms, restore_from,
    sha256_file, verify_backup, AppMeta, BackupEntry, BackupError, BackupKind, BackupManifest,
    ManifestCounts, RestoreError, RestoreOutcome, BACKUP_DB_FILENAME, DEFAULT_KEEP_ROUTINE,
    MANIFEST_FILENAME, RESTORE_MARKER_SUFFIX,
};
pub use diagnostics::{
    collect_diagnostics, AppInfo, BackupSummary, DbHealth, Diagnostics, JudgeStats, KeyCounts,
    LastBackup, RuleCount, StorageStats, SyncStatus,
};
pub use retention::{retention_sweep, RetentionReport};
mod backup;
pub use backup::restore_and_reconcile;
mod external;
pub use external::{open_external, ExternalConnection, ExternalOpenError};
mod diagnostics;
mod retention;

/// 一条嵌入的迁移脚本；列表版本号从 1 起连续，下标即 version-1。
struct Migration {
    version: u32,
    name: &'static str,
    sql: &'static str,
}

mod migrations {
    include!(concat!(env!("OUT_DIR"), "/migrations.rs"));
}

/// §4.1：每次打开都必须设置的连接参数（SQL 原文与设计一致，便于对照）。
const CONNECT_PRAGMAS: &[(&str, &str)] = &[
    ("journal_mode", "PRAGMA journal_mode = WAL;"),
    ("foreign_keys", "PRAGMA foreign_keys = ON;"),
    ("synchronous", "PRAGMA synchronous = NORMAL;"),
    ("busy_timeout", "PRAGMA busy_timeout = 5000;"),
    ("temp_store", "PRAGMA temp_store = MEMORY;"),
    ("trusted_schema", "PRAGMA trusted_schema = OFF;"),
];

/// 打开（不存在则创建并迁移）`path` 处的工作库。
///
/// - `path` 的父目录必须已存在（由调用方负责创建数据目录）；
/// - 重启后对同一 `path` 再次调用即可复用既有数据：版本已最新时不重复执行迁移；
/// - schema 版本高于本程序时返回 [`OpenError::FutureSchema`]，且不做任何写入。
pub fn open(path: &Path) -> Result<Connection, OpenError> {
    open_with(path, None, true)
}

/// 仅供恢复流程内部使用：跳过「未完成恢复」启动检查。
///
/// 恢复进行期间标记 `<db>.restore-pending` 必然存在，且由 [`restore_from`]
/// 自己写入和删除；此时若再做启动检查，会把进行中的恢复回滚掉。
pub(crate) fn open_unchecked(path: &Path) -> Result<Connection, OpenError> {
    open_with(path, None, false)
}

/// [`open`] + §4.3 步骤 2：若有待执行的迁移，先在 `backups_root` 下强制做一份
/// pre-migration 备份（`from`/`to` 记入目录名与 manifest），备份失败则中止、
/// 不执行任何迁移（库停留在原版本，可用该备份回退）。desktop 启动请用本入口。
pub fn open_with_backup(
    path: &Path,
    backups_root: &Path,
    meta: &AppMeta,
) -> Result<Connection, OpenError> {
    let root = backups_root.to_path_buf();
    let hook_meta = meta.clone();
    let mut pre_migration = |conn: &Connection, from: u32, to: u32| {
        backup_now(
            conn,
            &root,
            BackupKind::PreMigration { from, to },
            now_ms(),
            &hook_meta,
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
    };
    open_with(path, Some(&mut pre_migration), true)
}

/// 迁移前备份钩子：在首个迁移执行前以当前连接、当前版本与目标版本被调用；
/// 返回 `Err` 即中止迁移（库停留在原版本，尚未执行任何迁移）。
type PreMigrationHook<'a> = Option<&'a mut dyn FnMut(&Connection, u32, u32) -> Result<(), String>>;

/// `open` 的内部形态。`recover = true` 时先做启动检查：存在恢复标记即完成
/// 回滚，原库与当前库都不存在则拒绝打开——否则 `Connection::open` 会在
/// 空路径上静默建出一个新库，用户数据「凭空消失」。
fn open_with(
    path: &Path,
    pre_migration: PreMigrationHook<'_>,
    recover: bool,
) -> Result<Connection, OpenError> {
    let _lease = if recover {
        let shared = match external::lock(path, false, true) {
            Err(ExternalOpenError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(OpenError::Open { source: rusqlite::Error::InvalidPath(path.to_owned()) });
            }
            other => other?,
        };
        if !backup::restore_marker_path(path).try_exists().map_err(ExternalOpenError::Io)? && path.is_file() {
            let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
                .map_err(|source| OpenError::Open { source })?;
            if schema_version(&db).ok() == Some(supported_version()) {
                apply_pragmas(&db)?;
                return Ok(db);
            }
        }
        drop(shared);
        Some(external::lock(path, true, true)?)
    } else { None };
    if recover {
        backup::recover_interrupted_restore(path)?;
    }
    let mut conn =
        Connection::open(path).map_err(|source| OpenError::Open { source })?;
    apply_pragmas(&conn)?;
    let current =
        schema_version(&conn).map_err(|source| OpenError::Pragma { name: "user_version", source })?;
    let supported = supported_version();
    if i64::from(current) > i64::from(supported) {
        return Err(OpenError::FutureSchema {
            found: i64::from(current),
            supported,
        });
    }
    let pending = &migrations::MIGRATIONS[current as usize..];
    let migrated = !pending.is_empty();
    if migrated {
        if let Some(hook) = pre_migration {
            hook(&conn, current, supported)
                .map_err(|detail| OpenError::PreMigrationBackup { detail })?;
        }
    }
    for m in pending {
        run_migration(&mut conn, m)?;
    }
    if migrated {
        verify_integrity(&conn)?;
    }
    Ok(conn)
}

/// 本程序支持的最高 schema 版本（= `migrations/` 内最大编号）。
pub fn supported_version() -> u32 {
    migrations::MIGRATIONS.last().map_or(0, |m| m.version)
}

/// 数据库当前的 schema 版本（`PRAGMA user_version`），用于诊断展示。
pub fn schema_version(conn: &Connection) -> rusqlite::Result<u32> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
}

/// §4.1 连接参数：全部设置，并显式验证 WAL 确实生效
/// （journal_mode 记在库文件头，个别文件系统会静默退回 delete）。
fn apply_pragmas(conn: &Connection) -> Result<(), OpenError> {
    for &(name, sql) in CONNECT_PRAGMAS {
        conn.execute_batch(sql)
            .map_err(|source| OpenError::Pragma { name, source })?;
    }
    let mode: String = conn
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .map_err(|source| OpenError::Pragma { name: "journal_mode", source })?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(OpenError::NotWal { actual: mode });
    }
    Ok(())
}

/// §4.3：单文件单事务，失败即停并整体回滚，库停留在迁移前版本。
fn run_migration(conn: &mut Connection, m: &Migration) -> Result<(), OpenError> {
    let fail =
        |source: rusqlite::Error| OpenError::Migrate {
            version: m.version,
            file: m.name,
            source,
        };
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(fail)?;
    tx.execute_batch(m.sql).map_err(fail)?;
    tx.pragma_update(None, "user_version", m.version as i64)
        .map_err(fail)?;
    tx.commit().map_err(fail)?;
    Ok(())
}

/// §4.3 第 4 步：迁移后 `integrity_check` 必须为 ok（M1 仅结构迁移，
/// 数据迁移的计数对比由恢复流程的 manifest 承担，见 backup 模块）。
fn verify_integrity(conn: &Connection) -> Result<(), OpenError> {
    let result: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|e| OpenError::Corrupt {
            detail: format!("无法执行完整性校验: {e}"),
        })?;
    if result != "ok" {
        return Err(OpenError::Corrupt { detail: result });
    }
    Ok(())
}

/// 打开/迁移数据库失败的分类错误。
///
/// 与详细设计 §18 错误码对应：`Migrate` → MIGRATION_FAILED（事务已回滚，
/// 可用备份回退），`Corrupt` → DB_CORRUPT；`FutureSchema` 拒绝旧程序打开新库；
/// `PreMigrationBackup` 是 §4.3 步骤 2 的预防性中止（尚未迁移，库完好）；
/// `InterruptedRestore` 表示检测到未完成的恢复且无法自动回滚，拒绝打开
/// 以免把空库当新库（原库文件未被改动，按 `detail` 提示用户处理）。
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error(transparent)]
    Maintenance(#[from] ExternalOpenError),
    #[error("打开数据库文件失败（目录不存在或不可写）: {source}")]
    Open { source: rusqlite::Error },
    #[error("设置连接参数失败（PRAGMA {name}）: {source}")]
    Pragma { name: &'static str, source: rusqlite::Error },
    #[error("数据库未能进入 WAL 日志模式（当前 {actual}）；WAL 需要本地磁盘文件系统")]
    NotWal { actual: String },
    #[error("数据库 schema 版本 v{found} 高于本程序支持的 v{supported}，拒绝打开；请升级应用后再试")]
    FutureSchema { found: i64, supported: u32 },
    #[error("迁移前强制备份失败，已中止迁移（库保持原版本）: {detail}")]
    PreMigrationBackup { detail: String },
    #[error("检测到未完成的恢复，已拒绝打开以免把空库当成新库: {detail}")]
    InterruptedRestore { detail: String },
    #[error("迁移 {file}（v{version}）执行失败，事务已回滚（MIGRATION_FAILED）: {source}")]
    Migrate {
        version: u32,
        file: &'static str,
        source: rusqlite::Error,
    },
    #[error("迁移后完整性校验未通过（DB_CORRUPT）: {detail}")]
    Corrupt { detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_list_is_contiguous_from_one() {
        // build.rs 已保证；这里守住“下标即 version-1”这一切片前提。
        for (idx, m) in migrations::MIGRATIONS.iter().enumerate() {
            assert_eq!(m.version, idx as u32 + 1);
            assert!(!m.name.is_empty());
            assert!(!m.sql.trim().is_empty());
        }
        assert_eq!(supported_version(), migrations::MIGRATIONS.len() as u32);
    }
}
