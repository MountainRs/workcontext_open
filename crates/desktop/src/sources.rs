//! 受控来源注册、白名单持久配置与手动同步/判读触发命令（详细设计 §6、§7.3、
//! §10.1、§13.1；遗留事项 B7 桌面接线）。
//!
//! 职责边界：
//! - **本模块拥有**：`source` 行生命周期（注册/授权/撤权）、`source.scope_json`
//!   白名单、`config.json`（dws profile、本人身份、出域策略——默认 `none`）、
//!   `egress_log` 审计、手动 `sync_now` / `judge_now` 触发与进度事件、
//!   restore 后的授权对账。
//! - **不拥有**：记录落库（infra `SyncEngine`）、判读事务（app `judge`，
//!   desktop 不组装 `ClassifyInput`）、`work_item` 写路径、备份/诊断。
//!
//! 四条硬规则：
//! 1. **未授权不触达**：注册 ≠ 授权。新来源 `enabled=0` + 检查点 `disabled`，
//!    须显式 `source_set_enabled(id, true)`；撤权即写 `enabled=0` + `disabled`，
//!    下一轮同步直接拒绝（不触来源）。
//! 2. **config.json 是授权上限**（restore 防护）：每来源的本机授权账本
//!    `authorized` + 白名单存于 config.json，restore 只换 SQLite、换不回它。
//!    有效授权 = DB `enabled=1 ∧ 未撤权` ∧ 账本 `authorized`；有效白名单 =
//!    账本白名单。恢复旧库（`enabled=1`、更宽的白名单）无法放宽本机已做的
//!    撤权/收窄决定；[`reconcile_after_restore`] 在恢复后把库内状态对齐到账本。
//! 3. **不执行任意程序**：CLI 只按固定 basename（`dws`/`omp`）在 PATH 与固定
//!    安装目录解析为绝对路径（[`trusted_exe`]）；IPC 与配置都不能指定别的
//!    可执行文件，调用方也不能传入任意参数（profile/白名单条目按 ID 字符集
//!    校验，不可含空白、不可以 `-` 开头）。
//! 4. **默认 `none` 不出域**：出域策略默认 `none`，判读一律 `blocked` +
//!    `egress_log`，正文不出本机；一切状态真读 SQLite，无模拟成功路径。
//!
//! 连接纪律（restore 依赖）：本模块不持有长连接；每条独立连接都在
//! [`SourceState::db_gate`] 的**读锁存活期内**打开并关闭（读锁先于连接获取、
//! 晚于连接释放）。restore 取写锁后再锁 `AppState.db`，换库期间不存在任何
//! 旁路连接。
//!
//! 集成（main.rs 已接线）：`app.manage(sources::init(app.handle())?)` 与
//! `generate_handler!` 里的十个命令。restore 流程在 `store::restore_from`
//! 之后、仍持写锁与 `AppState.db` 时调用
//! `sources::reconcile_after_restore(&sources, &mut db)`；除此之外不要在持有
//! `AppState.db` 时调用任何会取 `db_gate` 读锁的函数（锁序固定为先
//! `db_gate`、后 `AppState.db`）。
//!
//! 依赖：`workcontext-infra`、`thiserror`（见 `desktop/Cargo.toml`）。
//!
//! 前端契约：
//! - `sync_status` / `source_list` 逐字段对齐 `ui/IPC.md §1`（camelCase，
//!   UTC 毫秒，未知为 null）；错误模型对齐 §0.2 `AppError`。
//! - 尚未接线（IPC.md §5）的命令，wire 形状：
//!   - `source_register {id?, displayName, scope:{conversations[], people[]}, profile}`
//!     → `SourceView`；条目必须是稳定 ID，`profile` 形如 `corpId:userId`
//!   - `source_set_enabled {id, enabled}` → `SourceView`
//!   - `sync_now {sourceId?}` → `{jobId, sources[]}`；事件 `sync://progress`
//!     `{jobId, phase:"source"|"done", sourceId?, outcome?, error?}`
//!   - `sync_reset {sourceId}` → `null`
//!   - `judge_now {limit?}` → `{jobId}`；`judge_retry {recordIds?}` →
//!     `{jobId, requeued}`；事件 `judge://done`
//!     `{jobId, requeued?, summary, egress:{blockedBatches, allowedBatches}, error?}`
//!   - `agent_health` → `{dws:{available, version, authenticated, detail},
//!     omp:{…}, judgeMode, egressPolicy}`
//!   - `egress_get` → `{policy, approvedModel, judgeMode}`；
//!     `egress_set {policy, approvedModel?, confirmToken}` → 同上，
//!     `confirmToken` 须逐字等于目标 `policy`
//!   - `runtime_config_get` → `{me:{userId, displayName}, judge:{mode, modelId},
//!     egress:{policy, approvedModel, judgeMode}, firstPull:{lookbackHours, minHours, maxHours},
//!     trustedExecutables[]}`；`runtime_configure {args:{meUserId?, meDisplayName?,
//!     judgeMode?, modelId?, firstPullLookbackHours?}}` → 同上。这是本人身份、
//!     判读模型与模式、首次拉取回溯的**唯一**入口：只接受这五个键，可执行
//!     路径/参数等未知键整体拒绝；模型变更即关闭出域批准并审计。回溯小时数
//!     限定在获批区间（1 小时 – 7 天，默认 24），在**下一次授权**时生效。

#[path = "archive_judge.rs"]
pub(crate) mod archive_judge;
pub(crate) mod config;
mod egress;
mod judge;
mod runtime;
mod sync;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};
use std::time::{SystemTime, UNIX_EPOCH};
use workcontext_infra::sync::Scope;
use workcontext_store::rusqlite::{Connection, OptionalExtension};

use tauri::Manager;

pub use config::{ConfigError, SettingsFile};
pub use judge::{judge_now, judge_retry};
pub use runtime::{runtime_config_get, runtime_configure};
pub use sync::{sync_now, sync_reset};
// `#[tauri::command]` 在定义处（子模块）生成的隐藏宏：`generate_handler![sources::x]`
// 按路径解析 `sources::__cmd__x` / `sources::__tauri_command_name_x`，必须一并重导出。
#[allow(unused_imports)]
pub use judge::{
    __cmd__judge_now, __cmd__judge_retry, __tauri_command_name_judge_now,
    __tauri_command_name_judge_retry,
};
#[allow(unused_imports)]
pub use runtime::{
    __cmd__runtime_config_get, __cmd__runtime_configure, __tauri_command_name_runtime_config_get,
    __tauri_command_name_runtime_configure,
};
#[allow(unused_imports)]
pub use sync::{
    __cmd__sync_now, __cmd__sync_reset, __tauri_command_name_sync_now,
    __tauri_command_name_sync_reset,
};

// ---------------------------------------------------------------------------
// 状态与连接纪律
// ---------------------------------------------------------------------------

/// 来源/同步/判读触发命令的共享状态。
pub struct SourceState {
    /// 数据库连接闸门。本模块每条独立连接都在读锁存活期内打开与关闭；
    /// restore 取写锁后再锁 `AppState.db`，保证换库期间没有旁路连接。
    pub(crate) db_gate: Arc<RwLock<()>>,
    pub(super) db_path: PathBuf,
    pub(super) config_path: PathBuf,
    /// 只串行化 config.json 的读-改-写（账本更新不可交错）。
    pub(super) config_lock: Mutex<()>,
    pub(crate) organizer_generation: Arc<std::sync::atomic::AtomicU64>,
}

impl SourceState {
    /// Call while holding config_lock, once a durable authorization boundary has changed.
    pub(crate) fn invalidate_organizer(&self) {
        self.organizer_generation.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// `sources::init` 失败。启动路径由 main.rs 决定如何呈现（fatal 或降级）。
#[derive(Debug, thiserror::Error)]
pub enum SourcesInitError {
    #[error("数据目录不可用：{0}")]
    DataDir(String),
    #[error("持久配置不可用：{0}")]
    Config(#[from] ConfigError),
}

/// 解析数据目录与数据库/配置文件路径，并预检 config.json 可读。
pub fn init(app: &tauri::AppHandle) -> Result<SourceState, SourcesInitError> {
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| SourcesInitError::DataDir(e.to_string()))?;
    std::fs::create_dir_all(&data_dir)
        .map_err(|e| SourcesInitError::DataDir(format!("{}: {e}", data_dir.display())))?;
    let config_path = data_dir.join("config.json");
    // 预检：版本过新/JSON 损坏立刻暴露，而不是等到首次同步。
    config::load(&config_path)?;
    Ok(SourceState {
        db_gate: Arc::new(RwLock::new(())),
        db_path: data_dir.join(workcontext_core::DB_FILENAME),
        config_path,
        config_lock: Mutex::new(()),
        organizer_generation: Default::default(),
    })
}

/// 取连接闸门读锁（中毒不阻断：锁内不保护数据，只保护「是否存在连接」）。
pub(super) fn lock_gate(gate: &Arc<RwLock<()>>) -> RwLockReadGuard<'_, ()> {
    gate.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 持读锁打开并用完一条独立连接。
///
/// 读锁**先于**连接获取、**晚于**连接释放（局部变量逆序 drop），因此
/// [`with_conn`] 返回时连接必已关闭；restore 的 `write()` 永远不会与本模块
/// 的任何连接并存。闭包内不得把连接泄漏出去。
pub(super) fn with_conn<T>(
    state: &SourceState,
    f: impl FnOnce(&mut Connection) -> Result<T, AppError>,
) -> Result<T, AppError> {
    let _gate = lock_gate(&state.db_gate);
    let mut conn = workcontext_store::open(&state.db_path).map_err(AppError::from_store)?;
    f(&mut conn)
}

/// 读取并校验整份配置（策略/身份取值非法按配置损坏拒绝，不猜）。
pub(super) fn load_settings(state: &SourceState) -> Result<SettingsFile, AppError> {
    let cfg = config::load(&state.config_path)?;
    validate_settings(&cfg)?;
    Ok(cfg)
}

/// 配置取值校验：出域策略合法；本人身份若已配置必须是 ID 形
/// （它会进入 CLI 参数与判读上下文，不接受任意字符串）。
fn validate_settings(cfg: &SettingsFile) -> Result<(), AppError> {
    if !cfg.egress.policy_is_valid() {
        return Err(AppError::invalid_argument(format!(
            "出域策略取值非法：{}（合法值 none/redacted/full）",
            cfg.egress.policy
        )));
    }
    if !cfg.me.user_id.trim().is_empty() && !is_id_token(cfg.me.user_id.trim()) {
        return Err(AppError::invalid_argument(
            "本人身份（config.json 的 me.userId）必须是稳定 ID：可见 ASCII、无空白、不以 - 开头。",
        ));
    }
    for (id, entry) in &cfg.sources {
        if !is_id_token(id) || !is_profile(&entry.profile) {
            return Err(AppError::invalid_argument(format!(
                "来源 {id} 的本机配置不合法（profile 须为 corpId:userId 形）。"
            )));
        }
    }
    Ok(())
}

pub(super) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 128 位随机十六进制主键（与 sync_run/run_id 同一生成方式，不依赖内存计数器）。
pub(super) fn random_id(conn: &Connection) -> Result<String, AppError> {
    conn.query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))
        .map_err(AppError::from_db)
}

// ---------------------------------------------------------------------------
// 受信 CLI 解析（不执行任意程序）
// ---------------------------------------------------------------------------

/// 允许执行的两个 CLI 的固定 basename——除此之外不接受任何可执行文件名。
pub(super) const DWS_EXE: &str = "dws";
pub(super) const OMP_EXE: &str = "omp";

/// 把固定程序名解析为受信的**绝对路径**。
///
/// 只在 PATH 与固定安装目录中查找；相对路径条目（含空串与 `.`）一律忽略，
/// 因此结果绝不相对 cwd 解析（Windows 的 `CreateProcess` 会先搜当前目录，
/// 传绝对路径可杜绝同目录同名程序被冒充）。IPC 与配置都无法替换它。
pub(super) fn trusted_exe(name: &'static str) -> Result<PathBuf, AppError> {
    debug_assert!(matches!(name, DWS_EXE | OMP_EXE), "只允许固定 basename");
    let file_name = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let search_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|dir| dir.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    search_dirs
        .into_iter()
        .chain(fixed_install_dirs(name))
        .map(|dir| dir.join(&file_name))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| AppError {
            code: "AGENT_UNREACHABLE".into(),
            message: format!(
                "未找到受信任的 {name} 可执行文件：只按固定程序名在 PATH 与固定安装目录中查找，不接受自定义路径。"
            ),
            detail: None,
            retryable: false,
            hint: Some(format!("安装官方 {name} CLI 后重试。")),
        })
}

/// 各平台的固定安装目录（均为绝对路径；GUI 进程的 PATH 常不含用户目录）。
fn fixed_install_dirs(name: &str) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if std::env::consts::OS == "windows" {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(
                PathBuf::from(local)
                    .join("Microsoft")
                    .join("WinGet")
                    .join("Links"),
            );
        }
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            let profile = Path::new(&profile);
            dirs.push(profile.join(".local").join("bin"));
            if name == DWS_EXE {
                dirs.push(profile.join("dws"));
            }
        }
    } else {
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join(".local").join("bin"));
        }
    }
    dirs.into_iter().filter(|d| d.is_absolute()).collect()
}

/// 钉钉稳定 ID 的形状：非空、可见 ASCII（无空白/控制字符）、不以 `-` 开头
/// （防选项注入）、≤128 字节。白名单条目与 `me.userId` 都必须满足。
fn is_id_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('-')
        && s.bytes().all(|b| (0x21..=0x7E).contains(&b))
}

/// dws profile：`corpId:userId`，两段都是 ID 形。
pub(super) fn is_profile(s: &str) -> bool {
    workcontext_app::external_access::valid_profile(s)
}

// ---------------------------------------------------------------------------
// 错误模型（ui/IPC.md §0.2）
// ---------------------------------------------------------------------------

/// 命令拒绝值：UI 原样展示 `message`，`code` 稳定。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppError {
    pub code: String,
    pub message: String,
    pub detail: Option<String>,
    pub retryable: bool,
    pub hint: Option<String>,
}

impl AppError {
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self {
            code: "INVALID_ARGUMENT".into(),
            message: message.into(),
            detail: None,
            retryable: false,
            hint: None,
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            code: "NOT_FOUND".into(),
            message: message.into(),
            detail: None,
            retryable: false,
            hint: None,
        }
    }

    pub fn internal(detail: impl std::fmt::Display) -> Self {
        Self {
            code: "INTERNAL".into(),
            message: "本地处理出错，未改动数据。".into(),
            detail: Some(detail.to_string()),
            retryable: true,
            hint: Some("重试一次；若持续出现请导出诊断信息。".into()),
        }
    }

    /// SQLite 错误：锁冲突映射 DB_BUSY（UI 提示稍后重试），其余 INTERNAL。
    pub fn from_db(err: workcontext_store::rusqlite::Error) -> Self {
        use workcontext_store::rusqlite::Error as RE;
        use workcontext_store::rusqlite::ErrorCode;
        let busy = matches!(
            &err,
            RE::SqliteFailure(e, _) if e.code == ErrorCode::DatabaseBusy || e.code == ErrorCode::DatabaseLocked
        );
        if busy {
            Self {
                code: "DB_BUSY".into(),
                message: "本地数据库暂时被占用，请稍后重试。".into(),
                detail: Some(err.to_string()),
                retryable: true,
                hint: Some("等待几秒后重试；不要同时启动第二个实例。".into()),
            }
        } else {
            Self::internal(err)
        }
    }

    /// 建库/迁移失败（store::open 错误）。
    pub fn from_store(err: workcontext_store::OpenError) -> Self {
        Self {
            code: "MIGRATION_FAILED".into(),
            message: "本地数据库打开或迁移失败。".into(),
            detail: Some(err.to_string()),
            retryable: false,
            hint: Some("重启应用；仍失败时用备份恢复。".into()),
        }
    }
}

impl From<ConfigError> for AppError {
    fn from(err: ConfigError) -> Self {
        Self {
            code: "INTERNAL".into(),
            message: "本地配置文件无法读取或写入。".into(),
            detail: Some(err.to_string()),
            retryable: false,
            hint: Some("检查数据目录下的 config.json；版本过新时请升级应用。".into()),
        }
    }
}

// ---------------------------------------------------------------------------
// 授权账本（config.json = 授权上限）
// ---------------------------------------------------------------------------

/// 本机账本是否授权该来源（不含 DB 侧否决）。
fn is_authorized(cfg: &SettingsFile, id: &str) -> bool {
    cfg.sources.get(id).is_some_and(|entry| entry.authorized)
}

/// DB 侧是否仍启用且未撤权。
fn db_source_enabled(conn: &Connection, id: &str) -> Result<bool, AppError> {
    let enabled: Option<i64> = conn
        .query_row(
            "SELECT enabled FROM source WHERE id = ?1 AND revoked_at IS NULL",
            [id],
            |row| row.get(0),
        )
        .optional()
        .map_err(AppError::from_db)?;
    Ok(enabled == Some(1))
}

/// `inner` 是否完全落在 `ceiling` 之内（未扩大）。
fn within(inner: &Scope, ceiling: &Scope) -> bool {
    inner.conversations.iter().all(|c| ceiling.conversations.contains(c))
        && inner.people.iter().all(|p| ceiling.people.contains(p))
}

/// 把一条来源在库内置为「已撤权」：`enabled=0`、检查点 `disabled`。
///
/// 只动授权字段与检查点状态位——游标、`last_success_at`、事项、证据、
/// 事件历史一律不碰（证据不可篡改）。
fn revoke_in_db(
    tx: &workcontext_store::rusqlite::Transaction<'_>,
    id: &str,
    now: i64,
    event_id: &str,
    actor: &str,
    note: &str,
) -> Result<(), AppError> {
    use workcontext_store::rusqlite::params;
    tx.execute(
        "UPDATE source
         SET enabled = 0, revoked_at = CASE WHEN enabled = 1 THEN ?2 ELSE COALESCE(revoked_at, ?2) END
         WHERE id = ?1",
        params![id, now],
    )
    .map_err(AppError::from_db)?;
    // 任意状态（含 syncing 残留）一律压制：撤权即停。
    tx.execute(
        "UPDATE sync_checkpoint SET state = 'disabled', updated_at = ?2
         WHERE source_id = ?1 AND state <> 'disabled'",
        params![id, now],
    )
    .map_err(AppError::from_db)?;
    tx.execute(
        "INSERT INTO work_event(id, target_type, target_id, event_type, actor, at, note)
         VALUES(?1, 'source', ?2, 'revoked', ?3, ?4, ?5)",
        params![event_id, id, actor, now, note],
    )
    .map_err(AppError::from_db)?;
    Ok(())
}

/// restore 之后由主流程调用：把恢复出来的库对齐到本机授权账本。
///
/// **调用方式**：在持有 `SourceState::db_gate` 写锁与 `AppState.db` 锁期间，
/// 直接传入 `AppState.db` 的连接——本函数不打开任何额外连接，也不取读锁
/// （否则与调用方持有的写锁自锁）。
///
/// 也不取 `config_lock`：来源命令的取锁顺序是「`config_lock` → `db_gate` 读锁」，
/// 对账若在持写锁时再等 `config_lock`，会与一个正持 `config_lock` 等读锁的
/// 命令成环。账本写入是原子 rename，不加锁读到的要么是旧要么是新的完整文件；
/// 被写锁挡住的命令随后各自完成「库 + 账本」的自洽更新。
///
/// 规则（只收窄、不放宽）：
/// - 账本未授权 / 账本里没有这个来源 → 库内置为已撤权（`enabled=0`、检查点
///   `disabled`）。恢复出来的旧 `enabled=1` 不能复活本机已撤销的授权。
/// - 账本已授权但库内白名单比账本宽 → 收窄到账本白名单；其余原样保留。
/// - **绝不**自动启用：账本已授权而库内 `enabled=0` 的来源保持停用，须用户
///   在设置页显式重新授权。
///
/// 不触碰 `work_item` / `source_record` / `link_evidence` / `field_override`，
/// 也不改任何检查点的游标与 `last_success_at`。幂等，可重复调用。
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileReport {
    /// 被置为已撤权的来源。
    pub revoked: Vec<String>,
    /// 白名单被收窄到账本上限的来源。
    pub narrowed: Vec<String>,
}

pub fn reconcile_after_restore(
    state: &SourceState,
    conn: &mut Connection,
) -> Result<ReconcileReport, AppError> {
    let cfg = config::load(&state.config_path)?;
    let cfg = match validate_settings(&cfg) {
        Ok(()) => cfg,
        // 配置本身不可信（损坏/非法）时：账本视为「全部未授权」，只收窄。
        Err(_) => SettingsFile::default(),
    };
    let now = now_ms();

    let rows: Vec<(String, i64, String)> = {
        let mut stmt = conn
            .prepare("SELECT id, enabled, scope_json FROM source ORDER BY id")
            .map_err(AppError::from_db)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(AppError::from_db)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(AppError::from_db)?;
        rows
    };

    let mut report = ReconcileReport::default();
    let tx = conn.transaction().map_err(AppError::from_db)?;
    for (id, enabled, scope_json) in rows {
        let entry = cfg.sources.get(&id).filter(|entry| entry.authorized);
        let Some(entry) = entry else {
            let checkpoint_disabled: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM sync_checkpoint
                     WHERE source_id = ?1 AND state = 'disabled'",
                    [&id],
                    |row| row.get(0),
                )
                .map_err(AppError::from_db)?;
            if enabled == 1 || checkpoint_disabled == 0 {
                let event_id: String = tx
                    .query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))
                    .map_err(AppError::from_db)?;
                revoke_in_db(
                    &tx,
                    &id,
                    now,
                    &event_id,
                    "system",
                    "恢复备份后按本机授权账本撤权：该来源未获本机授权或已被撤销",
                )?;
                report.revoked.push(id);
            }
            continue;
        };
        // 账本已授权：只把白名单收窄到账本上限，其余（含 enabled）原样保留。
        let db_scope: Option<Scope> = serde_json::from_str(&scope_json).ok();
        if db_scope.as_ref() != Some(&entry.scope) {
            tx.execute(
                "UPDATE source SET scope_json = ?2 WHERE id = ?1",
                workcontext_store::rusqlite::params![id, entry.scope.to_json()],
            )
            .map_err(AppError::from_db)?;
            report.narrowed.push(id);
        }
    }
    tx.commit().map_err(AppError::from_db)?;
    Ok(report)
}

// ---------------------------------------------------------------------------
// wire DTO（ui/IPC.md §1 逐字段对齐）
// ---------------------------------------------------------------------------

/// `source_list` 返回行（IPC.md `SourceView`）。
///
/// `enabled` 是**有效授权**（账本授权 ∧ 库内启用且未撤权）：恢复旧库后库内
/// 可能残留 `enabled=1`，此处如实反映「现在能否同步」。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceView {
    pub id: String,
    pub kind: String,
    pub display_name: String,
    pub scope_summary: String,
    pub enabled: bool,
    pub authorized_at: Option<i64>,
    pub revoked_at: Option<i64>,
    pub created_at: i64,
}

/// `sync_status` / `diagnostics.sources` 返回行（IPC.md `SyncStatus`）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatusView {
    pub source_id: String,
    pub display_name: String,
    pub state: String,
    pub last_attempt_at: Option<i64>,
    pub last_success_at: Option<i64>,
    pub consecutive_failures: i64,
    pub last_error_code: Option<String>,
    pub last_error_msg: Option<String>,
    pub window_start: Option<i64>,
    pub window_end: Option<i64>,
    pub cursor_at: Option<i64>,
}

/// 注册来源的白名单入参（落 `scope_json` 与本机账本）。
///
/// 条目必须是稳定 ID（openConversationId / staffId 等）：不接受群名，
/// 不接受空白与 `-` 开头（它们会成为 CLI 参数值）。
#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScopeDto {
    #[serde(default)]
    pub conversations: Vec<String>,
    #[serde(default)]
    pub people: Vec<String>,
}

impl ScopeDto {
    /// 校验并收敛为规范白名单；空集拒绝（白名单是显式授权边界）。
    fn into_scope(self) -> Result<Scope, AppError> {
        let collect = |items: Vec<String>, what: &str| -> Result<BTreeSet<String>, AppError> {
            let mut set = BTreeSet::new();
            for item in items {
                if !is_id_token(item.trim()) {
                    return Err(AppError::invalid_argument(format!(
                        "白名单{what}条目必须是稳定 ID：可见 ASCII、无空白、不以 - 开头，且不超过 128 字符。"
                    )));
                }
                set.insert(item.trim().to_string());
            }
            if set.len() > 200 {
                return Err(AppError::invalid_argument(format!(
                    "白名单{what}条目过多（≤200）。"
                )));
            }
            Ok(set)
        };
        let scope = Scope {
            conversations: collect(self.conversations, "会话")?,
            people: collect(self.people, "联系人")?,
        };
        if scope.conversations.is_empty() && scope.people.is_empty() {
            return Err(AppError::invalid_argument(
                "白名单不能为空：至少授权一个会话或联系人，未授权范围不会被读取。",
            ));
        }
        Ok(scope)
    }
}

/// 从 `scope_json` 生成人读摘要（IPC.md `scopeSummary`）。
fn scope_summary(scope_json: &str) -> String {
    match serde_json::from_str::<Scope>(scope_json) {
        Ok(scope) => {
            let (c, p) = (scope.conversations.len(), scope.people.len());
            if c == 0 && p == 0 {
                "未设置白名单（不读取任何记录）".to_string()
            } else {
                format!("{c} 个会话 · {p} 个联系人")
            }
        }
        Err(_) => "白名单数据损坏".to_string(),
    }
}

/// `sync_status` 的查询实现；`diagnostics` 命令直接复用（同一 DTO）。
/// 调用方负责其连接的纪律（通常是持 `AppState.db` 的主流程）。
pub fn sync_status_rows(conn: &Connection) -> Result<Vec<SyncStatusView>, AppError> {
    let mut stmt = conn
        .prepare(
            "SELECT s.id, s.display_name,
                    COALESCE(c.state, CASE s.enabled WHEN 1 THEN 'idle' ELSE 'disabled' END),
                    c.last_attempt_at, c.last_success_at,
                    COALESCE(c.consecutive_failures, 0),
                    c.last_error_code, c.last_error_msg,
                    c.window_start, c.window_end, c.cursor_at
             FROM source s
             LEFT JOIN sync_checkpoint c ON c.source_id = s.id
             ORDER BY s.created_at, s.id",
        )
        .map_err(AppError::from_db)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(SyncStatusView {
                source_id: row.get(0)?,
                display_name: row.get(1)?,
                state: row.get(2)?,
                last_attempt_at: row.get(3)?,
                last_success_at: row.get(4)?,
                consecutive_failures: row.get(5)?,
                last_error_code: row.get(6)?,
                last_error_msg: row.get(7)?,
                window_start: row.get(8)?,
                window_end: row.get(9)?,
                cursor_at: row.get(10)?,
            })
        })
        .map_err(AppError::from_db)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(AppError::from_db)?;
    Ok(rows)
}

const SOURCE_COLUMNS: &str =
    "id, kind, display_name, scope_json, enabled, authorized_at, revoked_at, created_at";

/// 读一条来源行并合成 `SourceView`（`enabled` = 有效授权，账本与库两边都看）。
fn source_view(conn: &Connection, cfg: &SettingsFile, id: &str) -> Result<SourceView, AppError> {
    conn.query_row(
        &format!("SELECT {SOURCE_COLUMNS} FROM source WHERE id = ?1"),
        [id],
        |row| {
            let id: String = row.get(0)?;
            let db_enabled = row.get::<_, i64>(4)? != 0;
            Ok(SourceView {
                enabled: db_enabled && is_authorized(cfg, &id),
                id,
                kind: row.get(1)?,
                display_name: row.get(2)?,
                scope_summary: scope_summary(&row.get::<_, String>(3)?),
                authorized_at: row.get(5)?,
                revoked_at: row.get(6)?,
                created_at: row.get(7)?,
            })
        },
    )
    .optional()
    .map_err(AppError::from_db)?
    .ok_or_else(|| AppError::not_found(format!("数据源 {id} 不存在。")))
}

// ---------------------------------------------------------------------------
// 命令：来源注册 / 白名单 / 授权
// ---------------------------------------------------------------------------

/// 设置页数据源列表（IPC.md §1 `source_list`）。
#[tauri::command]
pub fn source_list(state: tauri::State<'_, SourceState>) -> Result<Vec<SourceView>, AppError> {
    list_sources(&state)
}

fn list_sources(state: &SourceState) -> Result<Vec<SourceView>, AppError> {
    let _cfg_guard = state.config_lock.lock().unwrap_or_else(|p| p.into_inner());
    let cfg = load_settings(state)?;
    with_conn(state, |conn| {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {SOURCE_COLUMNS} FROM source ORDER BY created_at, id"
            ))
            .map_err(AppError::from_db)?;
        let rows = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let db_enabled = row.get::<_, i64>(4)? != 0;
                Ok(SourceView {
                    enabled: db_enabled && is_authorized(&cfg, &id),
                    id,
                    kind: row.get(1)?,
                    display_name: row.get(2)?,
                    scope_summary: scope_summary(&row.get::<_, String>(3)?),
                    authorized_at: row.get(5)?,
                    revoked_at: row.get(6)?,
                    created_at: row.get(7)?,
                })
            })
            .map_err(AppError::from_db)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(AppError::from_db)?;
        Ok(rows)
    })
}

/// 注册（或更新）一个受控来源：写 `source` 行 + 白名单 + 本机授权账本。
///
/// 受控语义：
/// - **注册 ≠ 授权**：新来源 `enabled=0`、检查点 `disabled`、账本
///   `authorized=false`，须再 `source_set_enabled(id, true)` 显式授权。
/// - **白名单扩大或更换 profile 不继承旧授权**：已授权来源的白名单超出
///   账本上限（或换了账号）时，本次更新立即撤权（库内置 `enabled=0`、检查点
///   `disabled`、账本 `authorized=false`），必须重新显式授权；仅收窄或改名
///   保留授权。
/// - 空白名单拒绝（引擎规则 7：白名单是显式授权边界）。
///
/// 可执行文件不由调用方指定：`dws` 按固定程序名在受信位置解析。
#[tauri::command]
pub fn source_register(
    state: tauri::State<'_, SourceState>,
    id: Option<String>,
    display_name: String,
    scope: ScopeDto,
    profile: String,
) -> Result<SourceView, AppError> {
    register_source(&state, id, display_name, scope, profile)
}

fn register_source(
    state: &SourceState,
    id: Option<String>,
    display_name: String,
    scope: ScopeDto,
    profile: String,
) -> Result<SourceView, AppError> {
    // ---- 校验（fail closed：宁可不注册，不注册一个边界不清的来源）----
    let display_name = display_name.trim().to_string();
    if display_name.is_empty() || display_name.chars().count() > 60 {
        return Err(AppError::invalid_argument(
            "数据源名称必填且不超过 60 字。",
        ));
    }
    let profile = profile.trim().to_string();
    if !is_profile(&profile) {
        return Err(AppError::invalid_argument(
            "dws profile 格式须为 corpId:userId（两段均为稳定 ID，不含空白）。",
        ));
    }
    let new_scope = scope.into_scope()?;
    let scope_json = new_scope.to_json();

    let id = match id {
        Some(given) => {
            let id = given.trim().to_lowercase();
            let valid = !id.is_empty()
                && id.len() <= 64
                && id.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                && id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'));
            if !valid {
                return Err(AppError::invalid_argument(
                    "来源 id 只允许小写字母、数字、- _ .，且以字母或数字开头（≤64 字符）。",
                ));
            }
            id
        }
        // 默认从 profile 派生（corpId:userId → corpid-userid），与账号一一对应。
        None => {
            let slug: String = profile
                .to_lowercase()
                .chars()
                .map(|c| {
                    if c.is_ascii_lowercase() || c.is_ascii_digit() {
                        c
                    } else {
                        '-'
                    }
                })
                .collect();
            let slug = slug.trim_matches('-').to_string();
            if slug.is_empty() || slug.len() > 64 {
                return Err(AppError::invalid_argument(
                    "无法从 profile 派生来源 id，请显式传入 id。",
                ));
            }
            slug
        }
    };

    let _cfg_guard = state.config_lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut cfg = load_settings(state)?;

    // 授权是否保留：以本机账本的上限为准（而不是可能已被恢复回滚的库）。
    let previous = cfg.sources.get(&id);
    let was_authorized = previous.is_some_and(|entry| entry.authorized);
    let profile_changed = previous.is_some_and(|entry| entry.profile != profile);
    let scope_changed = previous.is_some_and(|entry| entry.scope != new_scope);
    // 未扩大白名单且未换账号才继承授权；任一发生即撤权（库与账本同时降级）。
    let keep = previous.is_some_and(|entry| {
        entry.profile == profile && within(&new_scope, &entry.scope)
    });
    let authorized = was_authorized && keep;
    let authorization_dropped = was_authorized && !authorized;
    let drop_reason = if profile_changed {
        "profile 变更（更换账号）：旧授权不继承，须重新授权"
    } else {
        "白名单扩大：旧授权不自动生效，须重新授权"
    };

    let now = now_ms();
    // 获批读取时间窗：授权保留则沿用；授权作废则即刻封闭（终点 = 现在）。
    let ledger_window: (Option<i64>, Option<i64>) = match (previous, authorization_dropped) {
        (Some(entry), true) => (entry.window_start_ms, Some(now)),
        (Some(entry), false) => (entry.window_start_ms, entry.window_end_ms),
        (None, _) => (None, None),
    };
    with_conn(state, |conn| {
        let db_enabled: Option<i64> = conn
            .query_row("SELECT enabled FROM source WHERE id = ?1", [&id], |row| {
                row.get(0)
            })
            .optional()
            .map_err(AppError::from_db)?;
        let event_id = random_id(conn)?;

        let tx = conn.transaction().map_err(AppError::from_db)?;
        // 库内：只同步名称与白名单；授权字段在下方按账本处理，不随更新放宽。
        tx.execute(
            "INSERT INTO source(id, kind, display_name, scope_json, enabled, created_at)
             VALUES(?1, 'dws_im', ?2, ?3, 0, ?4)
             ON CONFLICT(id) DO UPDATE SET display_name = excluded.display_name,
                                           scope_json = excluded.scope_json",
            workcontext_store::rusqlite::params![id, display_name, scope_json, now],
        )
        .map_err(AppError::from_db)?;
        tx.execute(
            "INSERT OR IGNORE INTO sync_checkpoint(source_id, state, consecutive_failures, updated_at)
             VALUES(?1, 'disabled', 0, ?2)",
            workcontext_store::rusqlite::params![id, now],
        )
        .map_err(AppError::from_db)?;
        let (event_type, note) = match db_enabled {
            None => ("created", format!("注册来源；profile={profile}")),
            Some(_) => ("updated", format!("更新来源；profile={profile}")),
        };
        tx.execute(
            "INSERT INTO work_event(id, target_type, target_id, event_type, actor, at, note)
             VALUES(?1, 'source', ?2, ?3, 'human', ?4, ?5)",
            workcontext_store::rusqlite::params![event_id, id, event_type, now, note],
        )
        .map_err(AppError::from_db)?;
        if authorization_dropped {
            let revoke_event = random_id(&tx)?;
            revoke_in_db(&tx, &id, now, &revoke_event, "human", drop_reason)?;
        }
        tx.commit().map_err(AppError::from_db)?;
        if profile_changed || (was_authorized && scope_changed) { state.invalidate_organizer(); }

        // 账本（config.json = 授权上限）：先落库成功再写账本；账本未写则仍视为未授权。
        if profile_changed && cfg.extra.get("workspaceAccount").and_then(|v| v.get("sourceId")).and_then(|v| v.as_str()) == Some(id.as_str()) {
            cfg.extra.remove("externalAccess");
            cfg.extra.remove("workspaceAccount");
        }
        cfg.sources.insert(
            id.clone(),
            config::SourceCliSettings {
                profile,
                authorized,
                scope: new_scope.clone(),
                window_start_ms: ledger_window.0,
                window_end_ms: ledger_window.1,
            },
        );
        config::save(&state.config_path, &cfg)?;
        source_view(conn, &cfg, &id)
    })
}

/// 授权（`enabled=true`）或撤权（`enabled=false`）一个来源——撤销即停。
///
/// - 授权：把当前白名单记入本机账本（成为上限）并置 `authorized=true`；
///   库内 `enabled=1`、`authorized_at=now`、清除 `revoked_at`，检查点
///   `disabled → idle`。两边都成立才是有效授权。
/// - 撤权：库内 `enabled=0`、`revoked_at=now`、检查点 `disabled`，账本
///   `authorized=false`。进行中的调度立即停止，下一轮 `sync_now` 被拒绝
///   （不触来源）；已落库记录与游标原样保留（证据不可篡改）。账本里的撤权
///   决定不随 restore 回滚：恢复出旧的 `enabled=1` 也同步不了。
#[tauri::command]
pub fn source_set_enabled(
    state: tauri::State<'_, SourceState>,
    id: String,
    enabled: bool,
) -> Result<SourceView, AppError> {
    set_source_enabled(&state, id, enabled)
}

fn set_source_enabled(
    state: &SourceState,
    id: String,
    enabled: bool,
) -> Result<SourceView, AppError> {
    let _cfg_guard = state.config_lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut cfg = load_settings(state)?;
    let now = now_ms();
    with_conn(state, |conn| {
        let row: Option<(i64, Option<i64>, Option<i64>, String)> = conn
            .query_row(
                "SELECT enabled, authorized_at, revoked_at, scope_json FROM source WHERE id = ?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(AppError::from_db)?;
        let Some((was_enabled, _authorized_at, revoked_at, scope_json)) = row else {
            return Err(AppError::not_found(format!("数据源 {id} 不存在。")));
        };
        let ledger_authorized = is_authorized(&cfg, &id);
        // 授权：库内启用且账本授权 = 无需动作；撤权：库内已停用且账本未授权 = 无需动作。
        let already = if enabled {
            was_enabled == 1 && revoked_at.is_none() && ledger_authorized
        } else {
            was_enabled == 0 && !ledger_authorized
        };
        if !already {
            let event_id = random_id(conn)?;
            let tx = conn.transaction().map_err(AppError::from_db)?;
            let (event_type, note) = if enabled {
                let scope: Scope = serde_json::from_str(&scope_json).map_err(|_| {
                    AppError::invalid_argument("该来源的白名单数据损坏，无法授权；请重新注册。")
                })?;
                if scope.conversations.is_empty() && scope.people.is_empty() {
                    return Err(AppError::invalid_argument(
                        "白名单为空的来源不能授权：未授权范围不会被读取。",
                    ));
                }
                // 获批读取时间窗的起点：授权时刻 − 首次拉取回溯（获批区间内的值）。
                // 适配器对每个请求窗口与 `[起点, 终点)` 取交集，首次拉取与补拉
                // 都不会早于它——读取范围不可无限回扫。
                let lookback_ms = i64::from(cfg.sync.first_pull_lookback_hours_bounded()) * 3_600_000;
                let Some(entry) = cfg.sources.get_mut(&id) else {
                    return Err(AppError::invalid_argument(
                        "该来源缺少本机 profile 配置，请重新注册后再授权。",
                    ));
                };
                // 当前白名单与读取时间窗记入账本，成为此后不可被旧库放宽的上限。
                entry.scope = scope;
                entry.authorized = true;
                entry.window_start_ms = Some(now - lookback_ms);
                entry.window_end_ms = None;
                tx.execute(
                    "UPDATE source SET enabled = 1, authorized_at = ?2, revoked_at = NULL WHERE id = ?1",
                    workcontext_store::rusqlite::params![id, now],
                )
                .map_err(AppError::from_db)?;
                tx.execute(
                    "UPDATE sync_checkpoint SET state = 'idle', updated_at = ?2
                     WHERE source_id = ?1 AND state = 'disabled'",
                    workcontext_store::rusqlite::params![id, now],
                )
                .map_err(AppError::from_db)?;
                (
                    "authorized",
                    "用户授权来源：白名单与读取时间窗（起点 = 授权时刻 − 获批回溯）记入本机账本",
                )
            } else {
                if let Some(entry) = cfg.sources.get_mut(&id) {
                    entry.authorized = false;
                    // 封闭读取时间窗：撤权时刻之后的消息不在这次授权范围内。
                    if entry.window_start_ms.is_some() {
                        entry.window_end_ms = Some(now);
                    }
                }
                tx.execute(
                    "UPDATE source SET enabled = 0, revoked_at = ?2 WHERE id = ?1",
                    workcontext_store::rusqlite::params![id, now],
                )
                .map_err(AppError::from_db)?;
                ("revoked", "用户撤销授权：停止同步，不再触达来源")
            };
            tx.execute(
                "INSERT INTO work_event(id, target_type, target_id, event_type, actor, at, note)
                 VALUES(?1, 'source', ?2, ?3, 'human', ?4, ?5)",
                workcontext_store::rusqlite::params![event_id, id, event_type, now, note],
            )
            .map_err(AppError::from_db)?;
            if !enabled {
                // 任意状态（含 syncing 残留）一律压制：撤权即停。
                tx.execute(
                    "UPDATE sync_checkpoint SET state = 'disabled', updated_at = ?2 WHERE source_id = ?1",
                    workcontext_store::rusqlite::params![id, now],
                )
                .map_err(AppError::from_db)?;
            }
            tx.commit().map_err(AppError::from_db)?;
            if !enabled { state.invalidate_organizer(); }
            config::save(&state.config_path, &cfg)?;
        }
        source_view(conn, &cfg, &id)
    })
}

/// 状态栏/设置页同步状态（IPC.md §1 `sync_status`；无记录返回 `[]`）。
#[tauri::command]
pub fn sync_status(state: tauri::State<'_, SourceState>) -> Result<Vec<SyncStatusView>, AppError> {
    sync_status_of(&state)
}

fn sync_status_of(state: &SourceState) -> Result<Vec<SyncStatusView>, AppError> {
    with_conn(state, |conn| sync_status_rows(conn))
}

// ---------------------------------------------------------------------------
// 命令：出域策略（默认 none）
// ---------------------------------------------------------------------------

/// `egress_get` / `egress_set` 返回的出域视图。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EgressView {
    pub policy: String,
    pub approved_model: Option<String>,
    pub judge_mode: String,
}

fn egress_view(cfg: &SettingsFile) -> EgressView {
    EgressView {
        policy: cfg.egress.policy.clone(),
        approved_model: cfg.egress.approved_model.clone(),
        judge_mode: match cfg.judge.mode {
            config::JudgeMode::Mock => "mock",
            config::JudgeMode::Cli => "cli",
        }
        .to_string(),
    }
}

#[tauri::command]
pub fn egress_get(state: tauri::State<'_, SourceState>) -> Result<EgressView, AppError> {
    current_egress(&state)
}

fn current_egress(state: &SourceState) -> Result<EgressView, AppError> {
    let _cfg_guard = state.config_lock.lock().unwrap_or_else(|p| p.into_inner());
    let cfg = load_settings(state)?;
    Ok(egress_view(&cfg))
}

/// 变更出域策略（§13.1：改它需要确认词并写 `work_event(actor=human)`）。
///
/// 确认词约定：`confirmToken` 必须逐字等于目标策略值（`redacted`/`full`/
/// `none`）——UI 让用户亲手输入要开启的策略名，后端校验同一词。策略非
/// `none` 时已批准模型必须与 `judge.modelId` 一致（真实 runtime 会再校验
/// 一次，双层不同源）。收紧为 `none` 永远允许，无需额外条件。
#[tauri::command]
pub fn egress_set(
    state: tauri::State<'_, SourceState>,
    policy: String,
    approved_model: Option<String>,
    confirm_token: String,
) -> Result<EgressView, AppError> {
    set_egress(&state, policy, approved_model, confirm_token)
}

fn set_egress(
    state: &SourceState,
    policy: String,
    approved_model: Option<String>,
    confirm_token: String,
) -> Result<EgressView, AppError> {
    let policy = policy.trim().to_string();
    if !matches!(policy.as_str(), "none" | "redacted" | "full") {
        return Err(AppError::invalid_argument(
            "出域策略只允许 none / redacted / full。",
        ));
    }
    if confirm_token != policy {
        return Err(AppError::invalid_argument(format!(
            "确认词不匹配：修改出域策略需逐字输入「{policy}」。"
        )));
    }
    let approved_model = approved_model
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());

    let _cfg_guard = state.config_lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut cfg = load_settings(state)?;
    let previous_model = cfg.egress.approved_model.clone();
    if policy != "none" {
        let judge_model = cfg.judge.model_id.clone().filter(|m| !m.is_empty());
        let Some(model) = approved_model.clone().or(judge_model.clone()) else {
            return Err(AppError::invalid_argument(
                "策略非 none 时必须指明已批准模型（approvedModel 或 judge.modelId）。",
            ));
        };
        if let Some(jm) = judge_model {
            if jm != model {
                return Err(AppError::invalid_argument(format!(
                    "已批准模型（{model}）与判读模型（{jm}）不一致，正文不会发送。"
                )));
            }
        }
        cfg.egress.approved_model = Some(model);
    } else {
        cfg.egress.approved_model = None;
    }
    let previous = cfg.egress.policy.clone();
    cfg.egress.policy = policy;
    config::save(&state.config_path, &cfg)?;
    if previous != cfg.egress.policy || previous_model != cfg.egress.approved_model { state.invalidate_organizer(); }

    // work_event：出域策略属全局安全决策，actor=human 必须留痕。
    let view = egress_view(&cfg);
    with_conn(state, |conn| {
        let now = now_ms();
        let event_id = random_id(conn)?;
        conn.execute(
            "INSERT INTO work_event(id, target_type, target_id, event_type, actor, at, note)
             VALUES(?1, 'source', 'egress', 'updated', 'human', ?2, ?3)",
            workcontext_store::rusqlite::params![
                event_id,
                now,
                format!(
                    "egress policy {previous} -> {}; approved_model={:?}",
                    view.policy, view.approved_model
                )
            ],
        )
        .map_err(AppError::from_db)?;
        Ok(view.clone())
    })
}

// ---------------------------------------------------------------------------
// 命令：runtime 健康（设置页）
// ---------------------------------------------------------------------------

/// `agent_health` 返回：两个真实 CLI 的可用性（探测均真实执行，不模拟）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentHealthView {
    pub dws: ComponentHealth,
    pub omp: ComponentHealth,
    pub judge_mode: String,
    pub egress_policy: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentHealth {
    pub available: bool,
    pub version: Option<String>,
    /// dws 登录态（无 profile 可探时为 null）。
    pub authenticated: Option<bool>,
    pub detail: Option<String>,
}

/// 探测 `dws`/`omp` 是否可用（真实子进程，放 blocking 线程，不冻结 UI）。
/// 不访问数据库，因此不涉及连接闸门；可执行文件只按固定程序名解析。
#[tauri::command]
pub async fn agent_health(state: tauri::State<'_, SourceState>) -> Result<AgentHealthView, AppError> {
    let cfg = load_settings(&state)?;
    tauri::async_runtime::spawn_blocking(move || agent_health_view(cfg))
        .await
        .map_err(AppError::internal)
}

fn agent_health_view(cfg: SettingsFile) -> AgentHealthView {
    let dws = match trusted_exe(DWS_EXE) {
        Ok(exe) => {
            let opts = workcontext_infra::dws::DwsTransportOptions::default();
            match workcontext_infra::dws::cli_version(&exe, &opts) {
                Ok(version) => {
                    let authenticated = cfg.sources.values().next().map(|entry| {
                        matches!(
                            workcontext_infra::dws::auth_status(&exe, &entry.profile, &opts),
                            Ok(status) if status.authenticated
                        )
                    });
                    ComponentHealth {
                        available: true,
                        version: Some(version),
                        authenticated,
                        detail: None,
                    }
                }
                Err(err) => ComponentHealth {
                    available: false,
                    version: None,
                    authenticated: None,
                    detail: Some(err.to_string()),
                },
            }
        }
        Err(err) => ComponentHealth {
            available: false,
            version: None,
            authenticated: None,
            detail: Some(err.message),
        },
    };
    AgentHealthView {
        dws,
        omp: judge::omp_health(&cfg),
        judge_mode: egress_view(&cfg).judge_mode,
        egress_policy: cfg.egress.policy,
    }
}

#[cfg(test)]
mod tests;
