//! 备份/恢复（详细设计 §14.1–§14.2、§4.3 步骤 2；B6/门槛 8）。
//!
//! 备份 = `backups/` 下一个目录：`VACUUM INTO` 的一致性快照（WAL 下不阻塞读写）
//! 落成 `workcontext.sqlite3` + `manifest.json`（camelCase：schemaVersion / appVersion /
//! platform / kind / createdAt / sha256 / sizeBytes / counts）。写入先在
//! `.tmp-*` 临时目录完成后整体 rename，崩溃不留下半份备份。
//!
//! 恢复（§14.2，坏包绝不触碰当前库，崩溃不留半恢复状态）：
//! 1. 读 manifest、校验 sha256；
//! 2. 复制到一次性 `.restore-staging-<ts>-<pid>-<seq>` 副本上打开：integrity_check、
//!    user_version ≤ 程序支持、执行迁移、与 manifest 的
//!    source_record / work_item / work_event 计数对比、写入 `restored` 审计事件
//!    ——任一步失败即拒绝。**换库之后不再存在可失败的写入**；
//! 3. 校验全部通过才动当前库：`VACUUM INTO` pre-restore 备份 → 写恢复标记
//!    `<db>.restore-pending`（落盘）→ 关闭连接 → 原库改名 `<db>.broken-<ts>`
//!    **保留在磁盘** → 副本 rename 就位 → 重开 → 删除标记；
//! 4. 步骤 3 中任一步失败 → 删除新库、`broken` 改回原名、重开原连接。
//!
//! 崩溃安全：标记存在即表示恢复未被确认，[`open`](crate::open) 启动时先调用
//! `recover_interrupted_restore` 完成回滚（原库优先），原库与当前库都不存在时
//! **拒绝打开**——绝不会在空路径上静默建出一个新库。
//!
//! [`verify_backup`] 提供同口径的预检（确认对话框前调用），它与 [`restore_from`]
//! 各自创建一次性 staging 副本、用后即删，也只删自己创建的那一个——
//! 并发运行者的 staging 永远不会被碰。
//!
//! 桌面互斥约定：`restore_from` 只能在独占连接上调用——`Mutex<Connection>`
//! （desktop 的 `AppState`）`lock()` 后经 DerefMut 即得；内部用 `mem::replace`
//! 换出旧连接再关闭，调用方无需 `Option<Connection>`。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{open_unchecked, schema_version, OpenError};

/// 备份目录内的 manifest 文件名。
pub const MANIFEST_FILENAME: &str = "manifest.json";
/// 备份目录内的数据库快照文件名（与工作库同名，用户可直接辨认）。
pub const BACKUP_DB_FILENAME: &str = "workcontext.sqlite3";
/// 日常备份保留个数默认值（§13.1 `retention.backupKeep = 7`）。
pub const DEFAULT_KEEP_ROUTINE: usize = 7;
/// 恢复意图标记的后缀：`<db>.restore-pending`。存在即代表上一次恢复未被确认，
/// `open` 必须先完成回滚再打开。
pub const RESTORE_MARKER_SUFFIX: &str = ".restore-pending";

/// 毫秒级 UTC 时钟（备份/恢复内部命名用；业务时间一律由调用方显式传入）。
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 备份的应用元信息，原样写入 manifest。
#[derive(Debug, Clone)]
pub struct AppMeta {
    pub app_version: String,
    pub platform: String,
}

/// 备份触发来源；决定目录名前缀与保留策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupKind {
    /// 每日首次启动。
    Daily,
    /// 用户手动（IPC `backup_now`）。
    Manual,
    /// 迁移前强制（§4.3 步骤 2）；`from`/`to` 为迁移前后 schema 版本。
    PreMigration { from: u32, to: u32 },
    /// 恢复前对当前库的自动备份（§14.2 步骤 4）。
    PreRestore,
}

impl BackupKind {
    /// manifest 里的 kind 标签（稳定字符串，清理策略据此分类）。
    pub fn as_str(&self) -> &'static str {
        match self {
            BackupKind::Daily => "daily",
            BackupKind::Manual => "manual",
            BackupKind::PreMigration { .. } => "pre-migration",
            BackupKind::PreRestore => "pre-restore",
        }
    }

    /// 备份目录名前缀（后接毫秒时间戳）。
    fn dir_prefix(&self) -> String {
        match self {
            BackupKind::Daily => "daily-".into(),
            BackupKind::Manual => "manual-".into(),
            BackupKind::PreMigration { from, to } => {
                format!("pre-migration-v{from}-to-v{to}-")
            }
            BackupKind::PreRestore => "pre-restore-".into(),
        }
    }
}

/// manifest 内记录的关键表计数（§14.2 步骤 6 计数校验的基准）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestCounts {
    pub source_record: i64,
    pub work_item: i64,
    pub work_event: i64,
}

/// 备份 manifest（§14.1），磁盘上为 camelCase JSON。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupManifest {
    pub schema_version: u32,
    pub app_version: String,
    pub platform: String,
    pub kind: String,
    pub created_at: i64,
    pub sha256: String,
    pub size_bytes: u64,
    pub counts: ManifestCounts,
}

/// 一个已落盘的备份（目录 + manifest），`backup_list` 的返回单元。
///
/// 序列化即 IPC 契约的 `Backup`（ui/IPC.md §4）：`dir` 输出为 `path`，manifest 字段
/// 提升到顶层（createdAt / sizeBytes / schemaVersion / appVersion …），并带契约字段
/// `sha256Verified`；kind / sha256 / platform / counts 为附加信息（UI 只校验必填字段）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupEntry {
    /// 备份目录（含 manifest 与快照）；线上字段名 `path`，对 UI 不透明。
    #[serde(rename = "path")]
    pub dir: PathBuf,
    /// sha256 是否已对快照文件复核：`None` = 尚未校验；`Some(false)` = 不匹配或文件不可读。
    pub sha256_verified: Option<bool>,
    #[serde(flatten)]
    pub manifest: BackupManifest,
}

impl BackupEntry {
    /// 快照数据库文件路径。
    pub fn db_path(&self) -> PathBuf {
        self.dir.join(BACKUP_DB_FILENAME)
    }

    /// 重新计算快照文件 sha256 并与 manifest 比对，结果写入 `sha256_verified`。
    pub fn verify_sha256(&mut self) -> Result<bool, std::io::Error> {
        let actual = sha256_file(&self.db_path())?;
        let ok = actual.eq_ignore_ascii_case(&self.manifest.sha256);
        self.sha256_verified = Some(ok);
        Ok(ok)
    }
}

/// 备份失败的分类错误。
#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("备份文件操作失败（{op}）: {source}")]
    Io {
        op: &'static str,
        source: std::io::Error,
    },
    #[error("备份路径不是 UTF-8，无法传给 SQLite: {path}")]
    NonUtf8Path { path: String },
    #[error("VACUUM INTO 快照失败: {source}")]
    Vacuum { source: rusqlite::Error },
    #[error("读取备份元数据失败: {source}")]
    Meta { source: rusqlite::Error },
    #[error("序列化/写 manifest 失败: {source}")]
    Manifest { source: serde_json::Error },
    #[error("备份目录已存在（同一毫秒重复备份？）: {path}")]
    AlreadyExists { path: String },
}

/// 文件 sha256，小写十六进制；manifest 与恢复校验共用。
pub fn sha256_file(path: &Path) -> Result<String, io::Error> {
    let mut file = fs::File::open(path)?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut hasher = Sha256::new();
    loop {
        let n = io::Read::read(&mut file, &mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_lower(&hasher.finalize()))
}

/// 字节 sha256 的小写十六进制（事件 ID 生成用）。
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_lower(&hasher.finalize())
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// 附属文件路径（`<db>-wal` / `<db>-shm` 等；不能用 with_extension，会换扩展名）。
pub(crate) fn db_sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut name = db_path.as_os_str().to_os_string();
    name.push(suffix);
    db_path.with_file_name(name)
}

/// 表行数；表不存在（迁移前的旧库尚无该表）记 0，使 pre-migration 备份
/// 能在任何历史版本上完成。
fn table_count(conn: &Connection, table: &str) -> rusqlite::Result<i64> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(0);
    }
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
}

fn read_manifest_counts(conn: &Connection) -> rusqlite::Result<ManifestCounts> {
    Ok(ManifestCounts {
        source_record: table_count(conn, "source_record")?,
        work_item: table_count(conn, "work_item")?,
        work_event: table_count(conn, "work_event")?,
    })
}

/// §14.1：一致性快照备份。调用方须独占连接（desktop 的 `Mutex<Connection>` 已保证）。
pub fn backup_now(
    conn: &Connection,
    backups_root: &Path,
    kind: BackupKind,
    now_ms: i64,
    meta: &AppMeta,
) -> Result<BackupEntry, BackupError> {
    let io_err = |op: &'static str| move |source| BackupError::Io { op, source };
    let dir_name = format!("{}{}", kind.dir_prefix(), now_ms);
    let final_dir = backups_root.join(&dir_name);
    let tmp_dir = backups_root.join(format!(".tmp-{dir_name}"));
    if final_dir.exists() {
        return Err(BackupError::AlreadyExists {
            path: final_dir.display().to_string(),
        });
    }
    fs::create_dir_all(&tmp_dir).map_err(io_err("创建备份临时目录"))?;

    // VACUUM INTO：WAL 下的即时一致性快照；目标文件必须不存在。
    let tmp_db = tmp_dir.join(BACKUP_DB_FILENAME);
    let tmp_db_str = tmp_db
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| BackupError::NonUtf8Path {
            path: tmp_db.display().to_string(),
        })?;
    conn.execute("VACUUM INTO ?1", [tmp_db_str])
        .map_err(|source| BackupError::Vacuum { source })?;

    let sha256 = sha256_file(&tmp_db).map_err(io_err("计算快照 sha256"))?;
    let size_bytes = fs::metadata(&tmp_db)
        .map_err(io_err("读取快照大小"))?
        .len();
    let schema_version =
        schema_version(conn).map_err(|source| BackupError::Meta { source })?;
    let counts = read_manifest_counts(conn).map_err(|source| BackupError::Meta { source })?;
    let manifest = BackupManifest {
        schema_version,
        app_version: meta.app_version.clone(),
        platform: meta.platform.clone(),
        kind: kind.as_str().to_string(),
        created_at: now_ms,
        sha256,
        size_bytes,
        counts,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|source| BackupError::Manifest { source })?;
    fs::write(tmp_dir.join(MANIFEST_FILENAME), manifest_bytes)
        .map_err(io_err("写 manifest"))?;
    fs::rename(&tmp_dir, &final_dir).map_err(io_err("备份落位"))?;
    Ok(BackupEntry {
        dir: final_dir,
        sha256_verified: Some(true), // 快照刚写出，sha256 即时算得
        manifest,
    })
}

/// 列出 `backups_root` 下所有能完整解析 manifest 的备份，按创建时间降序；
/// 无 manifest / 解析失败的目录与 `.` 开头的临时目录一律跳过。
pub fn backup_list(backups_root: &Path) -> Vec<BackupEntry> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(backups_root) else {
        return out;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue; // .tmp-* 备份残留，交由 cleanup 处理；staging 属于并发运行者，不动
        }
        let Ok(bytes) = fs::read(dir.join(MANIFEST_FILENAME)) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_slice::<BackupManifest>(&bytes) else {
            continue;
        };
        out.push(BackupEntry { dir, sha256_verified: None, manifest });
    }
    out.sort_by(|a, b| {
        b.manifest
            .created_at
            .cmp(&a.manifest.created_at)
            .then_with(|| b.dir.cmp(&a.dir))
    });
    out
}

/// [`backup_list`] 并逐个复核 sha256（写入 `sha256Verified`）；文件不可读按不匹配处理。
/// 代价与备份总大小成正比，仅在 UI 需要展示校验状态时调用。
pub fn backup_list_verified(backups_root: &Path) -> Vec<BackupEntry> {
    let mut entries = backup_list(backups_root);
    for entry in &mut entries {
        entry.sha256_verified = Some(entry.verify_sha256().unwrap_or(false));
    }
    entries
}

/// §14.1 保留策略：`daily`/`manual` 只留最近 `keep_routine` 个（按 created_at），
/// `pre-migration`/`pre-restore` 永久保留；顺带清除崩溃残留的 `.tmp-*` 备份临时
/// 目录（同名即同一次调用，属当前独占连接所有）。`.restore-staging-*` 属于
/// 校验/恢复的并发运行者，本函数一律不动。返回被删除的目录。
pub fn cleanup_backups(
    backups_root: &Path,
    keep_routine: usize,
) -> Result<Vec<PathBuf>, BackupError> {
    let mut removed = Vec::new();
    let entries = match fs::read_dir(backups_root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(removed),
        Err(source) => return Err(BackupError::Io {
            op: "读取备份目录",
            source,
        }),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with(".tmp-") {
            fs::remove_dir_all(&path).map_err(|source| BackupError::Io {
                op: "清理备份残留目录",
                source,
            })?;
            removed.push(path);
        }
    }
    let routine: Vec<BackupEntry> = backup_list(backups_root)
        .into_iter()
        .filter(|e| matches!(e.manifest.kind.as_str(), "daily" | "manual"))
        .collect(); // 已按 created_at 降序
    for entry in routine.into_iter().skip(keep_routine) {
        fs::remove_dir_all(&entry.dir).map_err(|source| BackupError::Io {
            op: "清理过期日常备份",
            source,
        })?;
        removed.push(entry.dir);
    }
    Ok(removed)
}

// ==== 恢复（§14.2）====

/// 恢复失败的分类错误；`db_was_touched()` 区分「被拒且原库未动」与「换库后未能回滚」。
#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error(transparent)]
    Maintenance(#[from] crate::ExternalOpenError),
    #[error("备份包缺少 manifest.json: {path}")]
    MissingManifest { path: String },
    #[error("备份包缺少数据库快照: {path}")]
    MissingDb { path: String },
    #[error("备份包 manifest 解析失败: {source}")]
    BadManifest { source: serde_json::Error },
    #[error("备份包校验失败（sha256 不匹配）: manifest 记录 {expected}，实际 {actual}")]
    ShaMismatch { expected: String, actual: String },
    #[error("备份包不是可用的 SQLite 数据库: {detail}")]
    NotDatabase { detail: String },
    #[error("备份包完整性校验未通过（DB_CORRUPT）: {detail}")]
    Corrupt { detail: String },
    #[error("备份包 schema v{found} 高于本程序支持的 v{supported}，拒绝恢复；请先用新版程序处理")]
    FutureSchema { found: i64, supported: u32 },
    #[error("备份包迁移失败（MIGRATION_FAILED）: {detail}")]
    MigrationFailed { detail: String },
    #[error("备份包计数校验失败（{table}）: manifest 记录 {expected}，恢复后实际 {actual}")]
    CountMismatch {
        table: &'static str,
        expected: i64,
        actual: i64,
    },
    #[error("备份包无法写入 restored 审计事件，已拒绝恢复（当前库未受影响）: {source}")]
    EventWrite { source: rusqlite::Error },
    #[error("恢复前自动备份当前库失败（已中止，当前库未受影响）: {0}")]
    PreRestore(#[from] BackupError),
    #[error("恢复流程文件操作失败（{op}）: {source}")]
    Io {
        op: &'static str,
        source: std::io::Error,
    },
    #[error("打开/关闭数据库连接失败（当前库未受影响）: {source}")]
    Connection { source: rusqlite::Error },
    #[error("数据已恢复，但删除恢复标记失败（下次启动会回滚到原库）: {source}")]
    MarkerCleanup { source: std::io::Error },
    #[error("恢复失败（{reason}），且回滚未完成（{rollback_error}）；原库仍保留在 {broken_db}，请重启应用")]
    RollbackFailed {
        reason: String,
        rollback_error: String,
        broken_db: String,
    },
    #[error("恢复中止（{reason}），且无法重新打开原库（{reopen_error}）；原库文件完好，位于 {db_path}，请重启应用")]
    ReopenFailed {
        reason: String,
        reopen_error: String,
        db_path: String,
    },
}

impl RestoreError {
    /// 换库后是否未能干净地回到原库（供 UI 决定提示「需重启」还是「原库未受影响」）。
    ///
    /// 为 `false` 时原库原样可用、连接有效——包括全部校验拒绝、pre-restore 备份失败，
    /// 以及审计事件写不进去的包。为 `true` 只有两种：
    /// [`RestoreError::RollbackFailed`]（回滚未完成，连接不可用）与
    /// [`RestoreError::MarkerCleanup`]（数据已恢复但标记残留，下次启动会回滚到原库）。
    /// 两者都应提示用户重启应用。
    pub fn db_was_touched(&self) -> bool {
        matches!(
            self,
            RestoreError::RollbackFailed { .. } | RestoreError::MarkerCleanup { .. }
        )
    }
}

/// 恢复成功的结果。序列化满足 IPC 契约 `RestoreResult` 的 `restored` 分支
/// （`status` / `restarted` / `preRestoreBackupPath`），其余为附加信息。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreOutcome {
    /// 恒为 `"restored"`。
    pub status: &'static str,
    /// 连接已在原位置换，应用无需重启；恒为 `false`。
    /// desktop 若选择重启窗口，可在返回给 UI 前自行改写。
    pub restarted: bool,
    /// 恢复前自动备份的目录（IPC 契约字段；即 `pre_restore_backup.dir`）。
    pub pre_restore_backup_path: PathBuf,
    /// 被恢复的备份目录。
    pub restored_from: PathBuf,
    /// 备份包 manifest 记录的 schema 版本；恢复后的工作库本身已迁到程序支持的最新版本。
    pub schema_version: u32,
    /// 备份点上的关键表计数（不含随副本写入的那条 `restored` 审计事件）。
    pub counts: ManifestCounts,
    /// 恢复前对当前库的自动备份；类型同 `backup_list` 的条目，
    /// `manifest.kind` 恒为 `pre-restore`。
    pub pre_restore_backup: BackupEntry,
    /// 恢复前原库文件（改名保留，可手动找回）。
    pub broken_db: PathBuf,
    /// 随副本写入的 `restored` 审计事件 ID（`work_event.actor = 'human'`）。
    pub restored_event_id: String,
}

// ==== 恢复标记与启动检查 ====

/// 恢复意图标记的内容（`<db>.restore-pending`，camelCase JSON）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RestoreMarker {
    broken_db: PathBuf,
    staged_dir: PathBuf,
}

pub(crate) fn restore_marker_path(db_path: &Path) -> PathBuf {
    db_sidecar_path(db_path, RESTORE_MARKER_SUFFIX)
}

/// 写恢复标记：先写临时文件并 fsync，再 rename 落位——标记要么完整存在，
/// 要么不存在。必须在原库改名之前完成。
fn write_restore_marker(db_path: &Path, marker: &RestoreMarker) -> Result<(), RestoreError> {
    let io_err = |op: &'static str| move |source| RestoreError::Io { op, source };
    let final_path = restore_marker_path(db_path);
    let tmp_path = db_sidecar_path(&final_path, ".tmp");
    let bytes = serde_json::json!({
        "brokenDb": marker.broken_db,
        "stagedDir": marker.staged_dir,
    })
    .to_string();
    let mut file = fs::File::create(&tmp_path).map_err(io_err("创建恢复标记"))?;
    io::Write::write_all(&mut file, bytes.as_bytes()).map_err(io_err("写恢复标记"))?;
    file.sync_all().map_err(io_err("同步恢复标记"))?;
    fs::rename(&tmp_path, &final_path).map_err(io_err("落位恢复标记"))?;
    Ok(())
}

/// 启动检查：存在恢复标记即说明上一次恢复未被确认，先完成回滚再打开。
///
/// - `broken` 存在 → 原库已被改名，当前路径上若有文件只能是未确认的恢复副本：
///   删除它并把原库改回（**原库优先**，未确认的恢复一律作废，备份包仍可重试）；
/// - `broken` 不存在且当前库存在 → 标记写入后、改名前中断，当前库就是原库；
/// - 两者都不存在 → 拒绝打开：此时 `open` 会在空路径上静默建出新库，
///   必须由用户从备份恢复，而不是假装一切正常。
///
/// 幂等：每一步可重复执行，标记只在回滚完成后删除。
pub(crate) fn recover_interrupted_restore(db_path: &Path) -> Result<(), OpenError> {
    let refuse = |detail: String| OpenError::InterruptedRestore { detail };
    let marker_path = restore_marker_path(db_path);
    let bytes = match fs::read(&marker_path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(refuse(format!("读取恢复标记失败: {e}"))),
    };
    let marker: RestoreMarker = serde_json::from_slice(&bytes)
        .map_err(|e| refuse(format!("恢复标记损坏: {e}")))?;
    // 上次运行遗留的 staging（标记里记的是它自己创建的那一个）。
    let _ = fs::remove_dir_all(&marker.staged_dir);
    if marker.broken_db.exists() {
        if let Err(e) = rollback_swap(db_path, &marker.broken_db) {
            return Err(refuse(format!(
                "回滚未完成的恢复失败: {e}；原库保留在 {}",
                marker.broken_db.display()
            )));
        }
    } else if !db_path.exists() {
        return Err(refuse(
            "原库与当前库文件均不存在；请从备份恢复".to_string(),
        ));
    }
    fs::remove_file(&marker_path)
        .map_err(|e| refuse(format!("删除恢复标记失败: {e}")))?;
    Ok(())
}

// ==== 恢复流程 ====

/// 通过全部校验、已迁移、已写审计事件的备份包（快照副本仍在 staging 目录）。
struct ValidatedPackage {
    manifest: BackupManifest,
    staged_dir: PathBuf,
    staged_db: PathBuf,
    restored_event_id: String,
}

fn restore_error_from_open(e: OpenError) -> RestoreError {
    match e {
        OpenError::FutureSchema { found, supported } => {
            RestoreError::FutureSchema { found, supported }
        }
        OpenError::Corrupt { detail } => RestoreError::Corrupt { detail },
        OpenError::Migrate {
            version,
            file,
            source,
        } => RestoreError::MigrationFailed {
            detail: format!("{file}（v{version}）: {source}"),
        },
        other => RestoreError::NotDatabase {
            detail: other.to_string(),
        },
    }
}

/// 一次性 staging 目录：时间戳 + 进程号 + 进程内原子序号，绝不与他人冲突，
/// 因此无需也不允许清理别人的 staging。
fn staging_dir(parent: &Path, ts: i64) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(".restore-staging-{ts}-{}-{seq}", std::process::id()))
}

/// §14.2 步骤 1–3 + 步骤 6 的迁移/计数 + 步骤 7 的审计事件，全部在快照副本上
/// 完成；任何失败都发生在触碰当前库之前。成功时返回 staging 内的副本路径。
fn validate_package(
    backup_dir: &Path,
    ts: i64,
    staging_parent: &Path,
) -> Result<ValidatedPackage, RestoreError> {
    let (manifest, db_file) = read_package(backup_dir)?;
    let staged_dir = staging_dir(staging_parent, ts);
    match stage_and_check(&db_file, &manifest, &staged_dir, backup_dir, ts) {
        Ok((staged_db, restored_event_id)) => Ok(ValidatedPackage {
            manifest,
            staged_dir,
            staged_db,
            restored_event_id,
        }),
        Err(e) => {
            // 只清自己这次创建的 staging；他人的目录不碰。
            let _ = fs::remove_dir_all(&staged_dir);
            Err(e)
        }
    }
}

/// 读 manifest 并校验 sha256（原包文件只读不动）。
fn read_package(backup_dir: &Path) -> Result<(BackupManifest, PathBuf), RestoreError> {
    let manifest_path = backup_dir.join(MANIFEST_FILENAME);
    let manifest_bytes = match fs::read(&manifest_path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(RestoreError::MissingManifest {
                path: manifest_path.display().to_string(),
            })
        }
        Err(source) => return Err(RestoreError::Io { op: "读 manifest", source }),
    };
    let manifest: BackupManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|source| RestoreError::BadManifest { source })?;
    let db_file = backup_dir.join(BACKUP_DB_FILENAME);
    if !db_file.is_file() {
        return Err(RestoreError::MissingDb {
            path: db_file.display().to_string(),
        });
    }
    let io_err = |op: &'static str| move |source| RestoreError::Io { op, source };
    let actual_sha = sha256_file(&db_file).map_err(io_err("计算备份包 sha256"))?;
    if !actual_sha.eq_ignore_ascii_case(&manifest.sha256) {
        return Err(RestoreError::ShaMismatch {
            expected: manifest.sha256.clone(),
            actual: actual_sha,
        });
    }
    Ok((manifest, db_file))
}

/// 复制快照到 staging 副本并打开：PRAGMA、integrity、user_version ≤ 程序支持
/// （open 内建）、执行迁移、与 manifest 做关键表计数对比，最后把 `restored`
/// 审计事件写进副本。事件写不进去的包整体拒绝——这样换库之后没有任何
/// 可失败的写入。返回（副本路径，事件 ID）。
fn stage_and_check(
    db_file: &Path,
    manifest: &BackupManifest,
    staged_dir: &Path,
    backup_dir: &Path,
    ts: i64,
) -> Result<(PathBuf, String), RestoreError> {
    let io_err = |op: &'static str| move |source| RestoreError::Io { op, source };
    fs::create_dir_all(staged_dir).map_err(io_err("创建 staging 目录"))?;
    let staged_db = staged_dir.join(BACKUP_DB_FILENAME);
    fs::copy(db_file, &staged_db).map_err(io_err("复制快照副本"))?;
    let staged_conn = open_unchecked(&staged_db).map_err(restore_error_from_open)?;

    // §14.2 步骤 6 的计数部分：迁移后关键表计数必须与 manifest 一致。
    let actual = read_manifest_counts(&staged_conn)
        .map_err(|source| RestoreError::Connection { source })?;
    for (table, expected, got) in [
        ("source_record", manifest.counts.source_record, actual.source_record),
        ("work_item", manifest.counts.work_item, actual.work_item),
        ("work_event", manifest.counts.work_event, actual.work_event),
    ] {
        if expected != got {
            return Err(RestoreError::CountMismatch {
                table,
                expected,
                actual: got,
            });
        }
    }
    // §14.2 步骤 7 的审计事件：先于换库写入副本。
    let restored_event_id = record_restored_event(&staged_conn, backup_dir, manifest, ts)
        .map_err(|source| RestoreError::EventWrite { source })?;
    staged_conn
        .close()
        .map_err(|(_, source)| RestoreError::Connection { source })?;
    Ok((staged_db, restored_event_id))
}

/// 备份包预检（原生二次确认前调用）：与 [`restore_from`] 完全同口径——
/// sha256 → integrity → user_version → 迁移 → 关键表计数 → 审计事件可写，
/// 全部在系统临时目录的一次性副本上进行，**不触碰工作库**；staging 用后即删。
/// 通过则返回该备份的 [`BackupEntry`]（manifest 原始字段，供确认 UI 展示）。
pub fn verify_backup(backup_dir: &Path) -> Result<BackupEntry, RestoreError> {
    let package = validate_package(backup_dir, now_ms(), &std::env::temp_dir())?;
    let entry = BackupEntry {
        dir: backup_dir.to_path_buf(),
        sha256_verified: Some(true), // read_package 已比对过 sha256
        manifest: package.manifest,
    };
    let _ = fs::remove_dir(&package.staged_dir);
    Ok(entry)
}

/// 当前库改名保留（主文件 + 可能残留的 -wal/-shm）。
fn park_current_db(db_path: &Path, broken: &Path) -> Result<(), RestoreError> {
    fs::rename(db_path, broken).map_err(|source| RestoreError::Io {
        op: "保留当前库为 .broken",
        source,
    })?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = db_sidecar_path(db_path, suffix);
        if sidecar.exists() {
            let parked = db_sidecar_path(broken, suffix);
            fs::rename(&sidecar, &parked).map_err(|source| RestoreError::Io {
                op: "保留当前库附属文件",
                source,
            })?;
        }
    }
    Ok(())
}

/// 回滚：移除换入的新库（及附属文件；新库可能尚未就位），把 `broken` 改回原位。
fn rollback_swap(db_path: &Path, broken: &Path) -> Result<(), std::io::Error> {
    for suffix in ["-wal", "-shm"] {
        let _ = fs::remove_file(db_sidecar_path(db_path, suffix));
    }
    match fs::remove_file(db_path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    fs::rename(broken, db_path)?;
    for suffix in ["-wal", "-shm"] {
        let parked = db_sidecar_path(broken, suffix);
        if parked.exists() {
            fs::rename(&parked, db_sidecar_path(db_path, suffix))?;
        }
    }
    Ok(())
}

/// 步骤 5 后半：staging 副本 rename 就位并重开。
fn swap_and_open(staged_db: &Path, db_path: &Path) -> Result<Connection, RestoreError> {
    fs::rename(staged_db, db_path).map_err(|source| RestoreError::Io {
        op: "放入恢复库",
        source,
    })?;
    open_unchecked(db_path).map_err(restore_error_from_open)
}

/// §14.2 步骤 7：`work_event(actor=human, event=restored)`，返回事件 ID。
/// 在 staging 副本上执行；恢复未成功时该事件随副本一起被丢弃，不会谎报。
fn record_restored_event(
    conn: &Connection,
    backup_dir: &Path,
    manifest: &BackupManifest,
    ts: i64,
) -> rusqlite::Result<String> {
    let id = sha256_hex(
        format!("work_event|restored|{}|{ts}", manifest.sha256).as_bytes(),
    );
    let note = serde_json::json!({
        "backupDir": backup_dir.display().to_string(),
        "sha256": manifest.sha256,
        "schemaVersion": manifest.schema_version,
        "counts": manifest.counts,
    })
    .to_string();
    conn.execute(
        "INSERT INTO work_event (id, target_type, target_id, event_type, actor, at, note)
         VALUES (?1, 'source', '', 'restored', 'human', ?2, ?3)",
        rusqlite::params![id, ts, note],
    )?;
    Ok(id)
}

/// 换库前（文件未动）失败时，把真实库的连接换回 `conn`——避免调用方
/// 拿着内存占位连接继续读写。重开失败则连接已不可用：文件完好，
/// 返回 [`RestoreError::ReopenFailed`]，调用方应提示重启应用。
fn reopen_untouched(
    conn: &mut Connection,
    db_path: &Path,
    reason: String,
) -> Result<(), RestoreError> {
    *conn = open_unchecked(db_path).map_err(|e| RestoreError::ReopenFailed {
        reason,
        reopen_error: e.to_string(),
        db_path: db_path.display().to_string(),
    })?;
    Ok(())
}

/// 从备份目录恢复（§14.2 全流程）——**破坏性操作，仅在用户二次确认后调用**；
/// 确认前的检查请用只读的 [`verify_backup`]。
///
/// `conn` 必须独占（desktop `Mutex<Connection>` 的 guard DerefMut 即得）。
/// 校验（sha256 / integrity / future schema / 迁移 / 计数 / 审计事件）全部通过之前，
/// 当前库文件一个字节都不动；换库失败自动回滚；换库途中崩溃由
/// 启动检查 `recover_interrupted_restore` 在下次启动时回滚到原库。
pub fn restore_from(
    conn: &mut Connection,
    db_path: &Path,
    backups_root: &Path,
    backup_dir: &Path,
    meta: &AppMeta,
) -> Result<RestoreOutcome, RestoreError> {
    let _lease = crate::external::lock(db_path, true, true)?;
    restore_locked(conn, db_path, backups_root, backup_dir, meta)
}

/// Keep the maintenance lease until desktop restore reconciliation has finished.
pub fn restore_and_reconcile<E>(
    conn: &mut Connection, db_path: &Path, backups_root: &Path, backup_dir: &Path, meta: &AppMeta,
    reconcile: impl FnOnce(&mut Connection) -> Result<(), E>,
) -> Result<Result<RestoreOutcome, E>, RestoreError> {
    let _lease = crate::external::lock(db_path, true, true)?;
    let outcome = restore_locked(conn, db_path, backups_root, backup_dir, meta)?;
    Ok(reconcile(conn).map(|()| outcome))
}

fn restore_locked(
    conn: &mut Connection, db_path: &Path, backups_root: &Path, backup_dir: &Path, meta: &AppMeta,
) -> Result<RestoreOutcome, RestoreError> {
    let ts = now_ms();
    let no_parent = || RestoreError::Io {
        op: "定位数据目录",
        source: io::Error::new(io::ErrorKind::NotFound, "数据库路径没有父目录"),
    };
    let file_name = db_path
        .file_name()
        .map(PathBuf::from)
        .ok_or_else(no_parent)?;
    // 标记里必须写绝对路径：下次启动的工作目录可能与本次不同，
    // 相对路径会让启动检查找不到被改名的原库。
    let staging_parent = db_path
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(no_parent)?;
    let staging_parent = fs::canonicalize(&staging_parent).unwrap_or(staging_parent);

    // 步骤 1–3 + 6（迁移/计数）+ 7（审计事件）：坏包在此全部被拒。
    let package = validate_package(backup_dir, ts, &staging_parent)?;

    // 步骤 4：恢复前自动备份当前库（第一份失败前库保障）。
    let pre_restore = backup_now(
        conn,
        backups_root,
        BackupKind::PreRestore,
        ts,
        meta,
    )?;

    // 步骤 5：写恢复标记 → 关闭连接 → 原库改名保留 → 副本就位 → 重开 → 删标记。
    let broken = db_sidecar_path(&staging_parent.join(&file_name), &format!(".broken-{ts}"));
    let marker = RestoreMarker {
        broken_db: broken.clone(),
        staged_dir: package.staged_dir.clone(),
    };
    write_restore_marker(db_path, &marker)?;
    let placeholder = Connection::open_in_memory()
        .map_err(|source| RestoreError::Connection { source })?;
    let old = std::mem::replace(conn, placeholder);
    if let Err((failed, source)) = old.close() {
        // 文件未动：丢掉关闭失败的句柄，把真实库连接换回 `conn`。
        drop(failed);
        let reopened = reopen_untouched(conn, db_path, format!("关闭连接失败: {source}"));
        let _ = fs::remove_file(restore_marker_path(db_path));
        let _ = fs::remove_dir_all(&package.staged_dir);
        reopened?;
        return Err(RestoreError::Connection { source });
    }
    if let Err(failure) = park_current_db(db_path, &broken) {
        let reopened = reopen_untouched(conn, db_path, failure.to_string());
        let _ = fs::remove_file(restore_marker_path(db_path));
        let _ = fs::remove_dir_all(&package.staged_dir);
        reopened?;
        return Err(failure);
    }
    match swap_and_open(&package.staged_db, db_path) {
        Ok(new_conn) => {
            *conn = new_conn;
            let _ = fs::remove_dir(&package.staged_dir); // 副本已就位，目录应空
            // 标记必须最后删：残留的标记会让下次启动回滚到原库（安全方向）。
            fs::remove_file(restore_marker_path(db_path)).map_err(|source| {
                RestoreError::MarkerCleanup { source }
            })?;
            Ok(RestoreOutcome {
                status: "restored",
                restarted: false,
                pre_restore_backup_path: pre_restore.dir.clone(),
                restored_from: backup_dir.to_path_buf(),
                schema_version: package.manifest.schema_version,
                counts: package.manifest.counts,
                pre_restore_backup: pre_restore,
                broken_db: broken,
                restored_event_id: package.restored_event_id,
            })
        }
        Err(failure) => {
            // 绝不留下半恢复状态：回滚到第 5 步前的文件；清理自己的 staging。
            // 回滚未完成时保留标记，下次启动由 recover_interrupted_restore 继续。
            let _ = fs::remove_dir_all(&package.staged_dir);
            match rollback_swap(db_path, &broken) {
                Ok(()) => match open_unchecked(db_path) {
                    Ok(rolled_back) => {
                        *conn = rolled_back;
                        let _ = fs::remove_file(restore_marker_path(db_path));
                        Err(failure)
                    }
                    Err(e) => Err(RestoreError::RollbackFailed {
                        reason: failure.to_string(),
                        rollback_error: e.to_string(),
                        broken_db: broken.display().to_string(),
                    }),
                },
                Err(e) => Err(RestoreError::RollbackFailed {
                    reason: failure.to_string(),
                    rollback_error: e.to_string(),
                    broken_db: broken.display().to_string(),
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "workcontext-store-{tag}-{}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 换库失败路径的文件回滚（集成测试无法从公开 API 触发）。
    #[test]
    fn rollback_swap_restores_original_file() {
        let dir = scratch("rollback");
        let db = dir.join("workcontext.sqlite3");
        let broken = db_sidecar_path(&db, ".broken-1");
        fs::write(&db, "unconfirmed").unwrap();
        fs::write(&broken, "original").unwrap();
        fs::write(db_sidecar_path(&db, "-wal"), "stale-wal").unwrap();

        rollback_swap(&db, &broken).unwrap();

        assert_eq!(fs::read(&db).unwrap(), b"original");
        assert!(!db_sidecar_path(&db, "-wal").exists());
        assert!(!broken.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// 原库已改名、副本尚未就位时崩溃：当前路径为空，回滚仍须成功。
    #[test]
    fn rollback_swap_tolerates_missing_current_file() {
        let dir = scratch("rollback-missing");
        let db = dir.join("workcontext.sqlite3");
        let broken = db_sidecar_path(&db, ".broken-1");
        fs::write(&broken, "original").unwrap();

        rollback_swap(&db, &broken).unwrap();

        assert_eq!(fs::read(&db).unwrap(), b"original");
        assert!(!broken.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
