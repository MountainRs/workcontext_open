//! 手动同步触发（详细设计 §6、§10.1 `sync_now`）。
//!
//! 真实接入：来源经 [`CliDingTalkSource`]（spawn `dws` CLI，白名单外不请求）
//! 进入 [`SyncEngine`]；本模块只做受控调度：
//!
//! - **有效授权 = 本机账本授权 ∧ 库内启用且未撤权**，两边缺一即不触达。
//!   有效白名单取自账本（config.json），注册进引擎时镜像回 `source.scope_json`
//!   ——恢复出的旧库即使白名单更宽也不会被读取。
//! - **撤权即停**：[`GatedSource`] 在**每一页拉取前**复核授权，撤权后当前轮
//!   在下一页前中止（引擎记失败、游标不动），不再触达来源；任务排队期间被
//!   撤权的来源直接跳过。
//! - **连接纪律**：整轮 job 持 `db_gate` 读锁，读锁先于本 job 的任何连接、
//!   晚于最后一个连接释放；restore 的写锁因此不会与同步连接并存。
//! - `dws` 可执行文件只按固定程序名在受信位置解析（[`super::trusted_exe`]），
//!   调用方不能指定别的程序或参数。
//!
//! 同步在 blocking 线程执行（CLI 子进程 + 分页事务），命令立即返回 `jobId`，
//! 进度经 `sync://progress` 事件推送（§6.2 规则 7）。引擎与连接每轮新建：
//! 账本变更下一轮即生效，无需重启。

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tauri::Emitter;
use workcontext_infra::dws::{ApprovedWindow, CliDingTalkSource, DwsCliConfig};
use workcontext_infra::sync::{
    DingTalkSource, PageParams, RecordPage, Scope, SourceError, SyncConfig, SyncEngine,
    SyncOutcome, SyncStatus as EngineStatus,
};
use workcontext_store::rusqlite::{Connection, OptionalExtension};

use super::config::{self, SettingsFile, SourceCliSettings};
use super::{lock_gate, trusted_exe, with_conn, AppError, SourceState, DWS_EXE};

/// `sync_now` 立即返回：jobId + 本轮触达的来源。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncJobStarted {
    pub job_id: String,
    pub sources: Vec<String>,
}

/// 引擎 `SyncOutcome` 的 wire 形（`sync://progress` 事件载荷的 `outcome`）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncOutcomeView {
    pub source_id: String,
    pub run_id: Option<String>,
    pub status: String,
    pub pages: usize,
    pub fetched: usize,
    pub out_of_scope: usize,
    pub inserted: usize,
    pub unchanged: usize,
    pub updated: usize,
    pub cursor_advanced: bool,
    pub error: Option<SyncErrorInfoView>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncErrorInfoView {
    pub code: String,
    pub message: String,
}

/// `sync://progress` 事件载荷。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncProgressEvent {
    job_id: String,
    /// `source`（单来源结束，看 `outcome` 或 `error`）/ `done`（整轮结束）。
    phase: &'static str,
    source_id: Option<String>,
    outcome: Option<SyncOutcomeView>,
    /// 引擎自身失败或受控跳过（未触达来源）的说明。
    error: Option<String>,
}

fn engine_status_str(status: EngineStatus) -> &'static str {
    match status {
        EngineStatus::Ok => "ok",
        EngineStatus::Partial => "partial",
        EngineStatus::Failed => "failed",
        EngineStatus::AuthRequired => "auth_required",
        EngineStatus::Degraded => "degraded",
        EngineStatus::Disabled => "disabled",
    }
}

fn outcome_view(outcome: SyncOutcome) -> SyncOutcomeView {
    SyncOutcomeView {
        source_id: outcome.source_id,
        run_id: outcome.run_id,
        status: engine_status_str(outcome.status).to_string(),
        pages: outcome.pages,
        fetched: outcome.fetched,
        out_of_scope: outcome.out_of_scope,
        inserted: outcome.inserted,
        unchanged: outcome.unchanged,
        updated: outcome.updated,
        cursor_advanced: outcome.cursor_advanced,
        error: outcome.error.map(|e| SyncErrorInfoView {
            code: e.code,
            message: e.message,
        }),
    }
}

/// 配置 → 引擎参数（§6.2 默认：连续失败 5 次降级、单轮页数 600）。
fn engine_config(cfg: &SettingsFile) -> SyncConfig {
    SyncConfig {
        page_size: cfg.sync.page_size.max(1),
        overlap_ms: cfg.sync.overlap_hours.max(0) * 3_600_000,
        max_pages: 600,
        max_duration: Duration::from_secs(cfg.sync.max_run_minutes.max(1) * 60),
        max_consecutive_failures: 5,
        // 正文保留期：入库时写 `retention_at`，过期由清理任务抹除正文。
        raw_body_days: cfg.retention.raw_body_days_bounded(),
    }
}

/// 按本机账本构造真实 dws 适配器配置。
///
/// 返回 `None` = 账本里没有可用的获批读取时间窗，调用方必须跳过该来源
/// （fail closed：适配器对无窗口的配置本就拒绝读取，这里提前拦下并给出说明）。
/// 有效白名单与读取窗口都取自账本；`initial_lookback` 取获批区间内的回溯值，
/// 首轮窗口再与获批窗口取交集，因此首次拉取与补拉都不会早于获批起点。
fn dws_config(
    exe: PathBuf,
    entry: &SourceCliSettings,
    cfg: &SettingsFile,
    tz_offset_ms: i64,
) -> Option<DwsCliConfig> {
    let (start_ms, end_ms) = entry.approved_window()?;
    let mut dws_cfg = DwsCliConfig::new(exe, entry.profile.clone());
    dws_cfg.me_user_id = (!cfg.me.user_id.is_empty()).then(|| cfg.me.user_id.clone());
    dws_cfg.tz_offset_ms = tz_offset_ms;
    dws_cfg.initial_lookback = Duration::from_secs(
        u64::from(cfg.sync.first_pull_lookback_hours_bounded()) * 3_600,
    );
    dws_cfg.approved_window = Some(ApprovedWindow { start_ms, end_ms });
    Some(dws_cfg)
}

/// 解析本轮可触达的来源：账本授权 ∧ 库内启用且未撤权（真读 SQLite + 账本）。
///
/// 显式 `only`：不存在 → NOT_FOUND；存在但未授权/已撤权 → INVALID_ARGUMENT。
/// 两种拒绝都不发起任何 CLI 调用。
pub(super) fn authorized_target_ids(
    conn: &Connection,
    cfg: &SettingsFile,
    only: Option<&str>,
) -> Result<Vec<String>, AppError> {
    let mut ids: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT id FROM source WHERE enabled = 1 AND revoked_at IS NULL ORDER BY created_at, id")
            .map_err(AppError::from_db)?;
        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(AppError::from_db)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(AppError::from_db)?;
        ids
    };
    ids.retain(|id| cfg.sources.get(id).is_some_and(|entry| entry.authorized));
    if let Some(id) = only {
        let exists: Option<i64> = conn
            .query_row("SELECT 1 FROM source WHERE id = ?1", [id], |row| row.get(0))
            .optional()
            .map_err(AppError::from_db)?;
        if exists.is_none() {
            return Err(AppError::not_found(format!("数据源 {id} 不存在。")));
        }
        if !ids.iter().any(|target| target == id) {
            return Err(AppError::invalid_argument(format!(
                "数据源 {id} 未授权或已撤权，未发起同步。"
            )));
        }
        ids.retain(|target| target == id);
    }
    Ok(ids)
}

/// 此刻该来源是否仍可触达（账本 ∧ 库内）。任何读取失败一律按「否」处理
/// （fail closed）：复核点是撤权即停的唯一判据。
fn still_authorized(db_path: &Path, config_path: &Path, id: &str) -> bool {
    let Ok(cfg) = config::load(config_path) else {
        return false;
    };
    cfg.sources.get(id).is_some_and(|entry| entry.authorized)
        && match workcontext_store::open(db_path) {
            Ok(conn) => super::db_source_enabled(&conn, id).unwrap_or(false),
            Err(_) => false,
        }
}

/// 短连接读取来源行 `(display_name, enabled)`；行缺失返回 `None`。
fn read_source_row(db_path: &Path, id: &str) -> Result<Option<(String, bool)>, AppError> {
    let conn = workcontext_store::open(db_path).map_err(AppError::from_store)?;
    conn.query_row(
        "SELECT display_name, enabled FROM source WHERE id = ?1",
        [id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? == 1)),
    )
    .optional()
    .map_err(AppError::from_db)
}

/// 竞态修复：撤权与进行中的轮次竞速时，引擎的失败收尾会把检查点写回
/// `idle`；此处按授权复核结果重新压制为 `disabled`（游标与成功时间不动）。
fn reassert_disabled(db_path: &Path, id: &str) {
    let Ok(conn) = workcontext_store::open(db_path) else {
        return;
    };
    let _ = conn.execute(
        "UPDATE sync_checkpoint SET state = 'disabled', updated_at = ?2
         WHERE source_id = ?1 AND state <> 'disabled'",
        workcontext_store::rusqlite::params![id, super::now_ms()],
    );
}

/// 本机当前的 UTC 偏移（毫秒）。
///
/// `dws` 的窗口边界按本地墙钟时间下发，适配器用 `tz_offset_ms` 把毫秒时间戳
/// 换算成墙钟；留 0（UTC）会让窗口整体偏移一个时区——窗口终点落后若干小时，
/// 最近的消息被漏掉。借 SQLite 的 `localtime` 修饰符取系统时区，不引入时区库。
fn local_utc_offset_ms(db_path: &Path) -> i64 {
    let Ok(conn) = workcontext_store::open(db_path) else {
        return 0;
    };
    conn.query_row(
        "SELECT (CAST(strftime('%s', 'now', 'localtime') AS INTEGER)
                - CAST(strftime('%s', 'now') AS INTEGER)) * 1000",
        [],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
}

/// 撤权即停的执行点：包住真实来源，**每页拉取前**复核授权。
///
/// 撤权（或账本/库不可读）后返回来源错误而非继续拉取——引擎据此把本轮
/// 记为失败且游标不动，此后不再触达来源。白名单仍由内层来源与引擎两层执行。
struct GatedSource {
    inner: Arc<dyn DingTalkSource>,
    db_path: PathBuf,
    config_path: PathBuf,
}

impl DingTalkSource for GatedSource {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn scope(&self) -> &Scope {
        self.inner.scope()
    }

    fn probe(&self) -> Result<(), SourceError> {
        if !still_authorized(&self.db_path, &self.config_path, self.inner.id()) {
            return Err(SourceError::Other {
                message: "来源未授权或已撤权".into(),
            });
        }
        self.inner.probe()
    }

    fn fetch(
        &self,
        scope: &Scope,
        cursor: Option<&Value>,
        page: &PageParams,
    ) -> Result<RecordPage, SourceError> {
        if !still_authorized(&self.db_path, &self.config_path, self.inner.id()) {
            return Err(SourceError::Other {
                message: "来源已撤权：同步在下一页前中止，未再触达来源".into(),
            });
        }
        self.inner.fetch(scope, cursor, page)
    }
}

/// 手动同步：立即返回 jobId，真实同步在后台执行。
///
/// - `sourceId` 省略 → 同步全部**有效授权**来源（未授权/已撤权的不触达）；
/// - 显式指定 → 不存在报 NOT_FOUND；未授权或已撤权报 INVALID_ARGUMENT。
///   两种情况都不发起任何 CLI 调用。
#[tauri::command]
pub async fn sync_now(
    app: tauri::AppHandle,
    state: tauri::State<'_, SourceState>,
    source_id: Option<String>,
) -> Result<SyncJobStarted, AppError> {
    let cfg = super::load_settings(&state)?;
    // 目标解析与 jobId 在读锁内的短连接上完成；连接随 with_conn 关闭。
    let (job_id, targets) = with_conn(&state, |conn| {
        let targets = authorized_target_ids(conn, &cfg, source_id.as_deref())?;
        if targets.is_empty() {
            return Err(AppError::invalid_argument(
                "没有已授权的数据源可同步：先在设置中注册来源并完成授权。",
            ));
        }
        let job_id = super::random_id(conn)?;
        Ok((job_id, targets))
    })?;

    let db_gate = Arc::clone(&state.db_gate);
    let db_path = state.db_path.clone();
    let config_path = state.config_path.clone();
    let job_targets = targets.clone();
    let job_id_in_job = job_id.clone();
    tauri::async_runtime::spawn_blocking(move || {
        run_sync_job(
            app,
            db_gate,
            db_path,
            config_path,
            job_id_in_job,
            job_targets,
        );
    });

    Ok(SyncJobStarted {
        job_id,
        sources: targets,
    })
}

/// 手动恢复（§6.2 规则 3/4 的恢复入口）：`degraded` / `auth_required`
/// → `idle` 并清零失败计数；游标与最后成功时间原样保留。
///
/// 直接复用 `SyncEngine::reset`（不复制 SQL）；连接在读锁存活期内开关。
/// 已撤权（检查点 `disabled`）的来源不受影响——恢复授权只能走
/// `source_set_enabled`。
#[tauri::command]
pub fn sync_reset(
    state: tauri::State<'_, SourceState>,
    source_id: String,
) -> Result<(), AppError> {
    let cfg = super::load_settings(&state)?;
    let _gate = lock_gate(&state.db_gate);
    let conn = workcontext_store::open(&state.db_path).map_err(AppError::from_store)?;
    let exists: Option<i64> = conn
        .query_row("SELECT 1 FROM source WHERE id = ?1", [&source_id], |row| {
            row.get(0)
        })
        .optional()
        .map_err(AppError::from_db)?;
    if exists.is_none() {
        return Err(AppError::not_found(format!("数据源 {source_id} 不存在。")));
    }
    let engine = SyncEngine::new(conn, engine_config(&cfg));
    engine
        .reset(&source_id, super::now_ms())
        .map_err(AppError::internal)
}

/// 后台同步主体：每轮新建引擎与连接，来源按本机账本构造真实 CLI 适配器。
///
/// 读锁是本函数的**第一个**局部变量，其余（连接、引擎）都晚于它声明，
/// 因而全部先于它释放——整轮期间 restore 的写锁拿不到，换库不会有旁路连接。
fn run_sync_job(
    app: tauri::AppHandle,
    db_gate: Arc<RwLock<()>>,
    db_path: PathBuf,
    config_path: PathBuf,
    job_id: String,
    targets: Vec<String>,
) {
    let _gate = lock_gate(&db_gate);
    let emit = |phase: &'static str,
                source_id: Option<String>,
                outcome: Option<SyncOutcomeView>,
                error: Option<String>| {
        let _ = app.emit(
            "sync://progress",
            SyncProgressEvent {
                job_id: job_id.clone(),
                phase,
                source_id,
                outcome,
                error,
            },
        );
    };

    // 受信 CLI：固定程序名解析为绝对路径；找不到就如实结束，不回落到别的程序。
    let dws_exe = match trusted_exe(DWS_EXE) {
        Ok(exe) => exe,
        Err(err) => {
            emit("done", None, None, Some(err.message));
            return;
        }
    };
    let conn = match workcontext_store::open(&db_path) {
        Ok(conn) => conn,
        Err(err) => {
            emit("done", None, None, Some(AppError::from_store(err).message));
            return;
        }
    };
    let cfg = match config::load(&config_path) {
        Ok(cfg) => cfg,
        Err(err) => {
            emit("done", None, None, Some(err.to_string()));
            return;
        }
    };
    let engine = SyncEngine::new(conn, engine_config(&cfg));
    let tz_offset_ms = local_utc_offset_ms(&db_path);
    let now = super::now_ms();

    for id in &targets {
        // 账本每源重读：任务排队期间发生的撤权在这里生效（不触达来源）。
        let entry = match config::load(&config_path) {
            Ok(latest) => latest.sources.get(id).filter(|entry| entry.authorized).cloned(),
            Err(_) => None,
        };
        let Some(entry) = entry else {
            emit(
                "source",
                Some(id.clone()),
                None,
                Some("来源未授权或已撤权，已跳过（未触达来源）".into()),
            );
            continue;
        };
        // 库内行（display_name、是否仍启用）真读自 SQLite。
        let (display_name, db_enabled) = match read_source_row(&db_path, id) {
            Ok(Some((display_name, db_enabled))) => (display_name, db_enabled),
            // 行缺失（例如恢复到注册之前的备份）：账本已授权即按账本重建来源行。
            Ok(None) => (id.clone(), true),
            Err(err) => {
                emit("source", Some(id.clone()), None, Some(err.message));
                continue;
            }
        };
        if !db_enabled {
            emit(
                "source",
                Some(id.clone()),
                None,
                Some("来源已在库内停用或撤权，已跳过（未触达来源）".into()),
            );
            continue;
        }
        let Some(dws_cfg) = dws_config(dws_exe.clone(), &entry, &cfg, tz_offset_ms) else {
            emit(
                "source",
                Some(id.clone()),
                None,
                Some("来源缺少可用的获批读取时间窗，已跳过（未触达来源）；请重新授权".into()),
            );
            continue;
        };
        // 有效白名单 = 账本白名单；注册进引擎时镜像回 source.scope_json。
        let real = Arc::new(CliDingTalkSource::new(id, entry.scope, dws_cfg));
        let gated: Arc<dyn DingTalkSource> = Arc::new(GatedSource {
            inner: real,
            db_path: db_path.clone(),
            config_path: config_path.clone(),
        });
        if let Err(err) = engine.register(gated, &display_name, now) {
            emit("source", Some(id.clone()), None, Some(err.to_string()));
            continue;
        }
        match engine.sync_source(id, now) {
            Ok(outcome) => emit("source", Some(id.clone()), Some(outcome_view(outcome)), None),
            Err(err) => emit("source", Some(id.clone()), None, Some(err.to_string())),
        }
        // 撤权与本轮竞速：引擎失败收尾可能把检查点写回 idle，按授权复核后压制。
        if !still_authorized(&db_path, &config_path, id) {
            reassert_disabled(&db_path, id);
        }
    }
    emit("done", None, None, None);
}

#[cfg(test)]
mod tests {
    use workcontext_infra::sync::{MockSource, PageParams, RecordPage};

    use super::super::config::{
        DEFAULT_RAW_BODY_DAYS, MAX_FIRST_PULL_LOOKBACK_HOURS, MAX_RAW_BODY_DAYS,
    };
    use super::super::tests::Fixture;
    use super::*;

    fn page_params() -> PageParams {
        PageParams {
            size: 100,
            overlap_ms: 0,
        }
    }

    #[test]
    fn gated_source_stops_before_touching_the_source_once_revoked() {
        let fx = Fixture::new();
        fx.register("s1", &["cidA"], &[], "corp1:user1").expect("注册");
        fx.set_enabled("s1", true).expect("授权");
        let scope = fx.config().sources["s1"].scope.clone();

        let inner = Arc::new(MockSource::new("s1", scope.clone()));
        inner.push_page(RecordPage {
            records: Vec::new(),
            next_cursor: None,
            has_more: false,
            snapshot_at: 1,
        });
        let gated = GatedSource {
            inner: inner.clone(),
            db_path: fx.state.db_path.clone(),
            config_path: fx.state.config_path.clone(),
        };

        // 已授权：放行到内层来源。
        assert!(gated.fetch(&scope, None, &page_params()).is_ok());
        assert!(gated.probe().is_ok());
        let fetched_while_authorized = inner.fetch_log().len();
        assert_eq!(fetched_while_authorized, 1);

        // 撤权后：下一页之前即中止，内层来源不再被调用。
        fx.set_enabled("s1", false).expect("撤权");
        let err = gated.fetch(&scope, None, &page_params()).expect_err("撤权后必须中止");
        assert!(matches!(err, SourceError::Other { .. }));
        assert!(gated.probe().is_err());
        assert_eq!(inner.fetch_log().len(), fetched_while_authorized);
    }

    #[test]
    fn authorization_is_rechecked_on_both_sides() {
        let fx = Fixture::new();
        fx.register("s1", &["cidA"], &[], "corp1:user1").expect("注册");
        let (db, cfg_path) = (fx.state.db_path.clone(), fx.state.config_path.clone());
        assert!(!still_authorized(&db, &cfg_path, "s1"), "未授权");
        assert!(!still_authorized(&db, &cfg_path, "nope"), "不存在的来源");

        fx.set_enabled("s1", true).expect("授权");
        assert!(still_authorized(&db, &cfg_path, "s1"));

        // 仅库内停用（例如直接改库）：否。
        fx.with_conn(|conn| {
            conn.execute("UPDATE source SET enabled = 0 WHERE id = 's1'", [])
                .expect("停用");
        });
        assert!(!still_authorized(&db, &cfg_path, "s1"));
        fx.with_conn(|conn| {
            conn.execute("UPDATE source SET enabled = 1 WHERE id = 's1'", [])
                .expect("启用");
        });

        // 仅账本撤销（例如恢复出旧库的 enabled=1）：同样否。
        let mut cfg = fx.config();
        cfg.sources.get_mut("s1").expect("账本").authorized = false;
        config::save(&fx.state.config_path, &cfg).expect("写配置");
        assert!(fx.db_enabled("s1"));
        assert!(!still_authorized(&db, &cfg_path, "s1"));

        // 账本读不到（文件损坏）：按未授权处理。
        std::fs::write(&cfg_path, "{not json").expect("写坏配置");
        assert!(!still_authorized(&db, &cfg_path, "s1"));
    }

    #[test]
    fn source_rows_are_read_from_the_database() {
        let fx = Fixture::new();
        assert_eq!(read_source_row(&fx.state.db_path, "s1").expect("查询"), None);
        fx.register("s1", &["cidA"], &[], "corp1:user1").expect("注册");
        assert_eq!(
            read_source_row(&fx.state.db_path, "s1").expect("查询"),
            Some(("测试来源".to_string(), false))
        );
        fx.set_enabled("s1", true).expect("授权");
        assert_eq!(
            read_source_row(&fx.state.db_path, "s1").expect("查询"),
            Some(("测试来源".to_string(), true))
        );
    }

    #[test]
    fn local_utc_offset_is_a_valid_zone_offset() {
        let fx = Fixture::new();
        let offset = local_utc_offset_ms(&fx.state.db_path);
        // 合法的时区偏移：±14h 以内、15 分钟的整数倍。
        assert!(offset.abs() <= 14 * 3_600_000, "{offset}");
        assert_eq!(offset % (15 * 60_000), 0, "{offset}");
    }

    #[test]
    fn reasserting_disabled_keeps_cursor_untouched() {
        let fx = Fixture::new();
        fx.register("s1", &["cidA"], &[], "corp1:user1").expect("注册");
        fx.with_conn(|conn| {
            conn.execute(
                "UPDATE sync_checkpoint
                 SET state = 'idle', cursor_json = '{\"w\":9}', last_success_at = 5
                 WHERE source_id = 's1'",
                [],
            )
            .expect("写检查点");
        });
        reassert_disabled(&fx.state.db_path, "s1");
        assert_eq!(fx.checkpoint_state("s1"), "disabled");
        let (cursor, last_success): (Option<String>, Option<i64>) = fx.with_conn(|conn| {
            conn.query_row(
                "SELECT cursor_json, last_success_at FROM sync_checkpoint WHERE source_id = 's1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("读检查点")
        });
        assert_eq!(cursor.as_deref(), Some("{\"w\":9}"));
        assert_eq!(last_success, Some(5));
    }

    #[test]
    fn adapter_config_comes_from_the_ledger_and_is_bounded() {
        let fx = Fixture::new();
        let exe = PathBuf::from("/trusted/bin/dws");
        fx.register("s1", &["cidA"], &[], "corp1:user1").expect("注册");

        // 未授权：账本没有获批读取时间窗 → 不构造适配器（fail closed）。
        let cfg = fx.config();
        assert!(dws_config(exe.clone(), &cfg.sources["s1"], &cfg, 0).is_none());

        // 授权后：窗口取自账本，回溯为获批区间内的默认值。
        fx.set_enabled("s1", true).expect("授权");
        let cfg = fx.config();
        let entry = &cfg.sources["s1"];
        let start_ms = entry.window_start_ms.expect("窗口起点");
        let dws_cfg = dws_config(exe.clone(), entry, &cfg, 8 * 3_600_000).expect("已授权");
        assert_eq!(dws_cfg.profile, "corp1:user1");
        assert_eq!(dws_cfg.exe, exe);
        assert_eq!(dws_cfg.me_user_id, None);
        assert_eq!(dws_cfg.tz_offset_ms, 8 * 3_600_000);
        assert_eq!(dws_cfg.initial_lookback, Duration::from_secs(24 * 3_600));
        assert_eq!(
            dws_cfg.approved_window,
            Some(ApprovedWindow {
                start_ms,
                end_ms: None,
            })
        );

        // 手改的越界回溯被钳制到 7 天上界。
        let mut tampered = cfg.clone();
        tampered.sync.first_pull_lookback_hours = 100_000;
        let dws_cfg = dws_config(exe.clone(), entry, &tampered, 0).expect("已授权");
        assert_eq!(
            dws_cfg.initial_lookback,
            Duration::from_secs(u64::from(MAX_FIRST_PULL_LOOKBACK_HOURS) * 3_600)
        );

        // 撤权后窗口封闭：适配器拿到的是带终点的窗口。
        fx.set_enabled("s1", false).expect("撤权");
        let cfg = fx.config();
        let entry = &cfg.sources["s1"];
        let dws_cfg = dws_config(exe, entry, &cfg, 0).expect("封闭窗口仍可表达");
        let window = dws_cfg.approved_window.expect("窗口");
        assert_eq!(window.start_ms, start_ms);
        assert!(window.end_ms.is_some_and(|end_ms| end_ms >= start_ms));
    }

    #[test]
    fn engine_config_carries_the_bounded_retention() {
        let fx = Fixture::new();
        let cfg = fx.config();
        assert_eq!(engine_config(&cfg).raw_body_days, DEFAULT_RAW_BODY_DAYS);
        let mut tampered = cfg.clone();
        tampered.retention.raw_body_days = 400;
        assert_eq!(engine_config(&tampered).raw_body_days, MAX_RAW_BODY_DAYS);
        tampered.retention.raw_body_days = 0;
        assert_eq!(engine_config(&tampered).raw_body_days, DEFAULT_RAW_BODY_DAYS);
        tampered.retention.raw_body_days = 7;
        assert_eq!(engine_config(&tampered).raw_body_days, 7);
    }
}
