//! 手动判读触发（详细设计 §7、§8；遗留事项 B7 桌面接线）。
//!
//! desktop **不组装 `ClassifyInput`**：批划分、输入规范形、判读落库事务
//! 全部在 `workcontext_app::judge::judge_pending`（调真实 `PolicyValidator`）。
//! 本模块负责受控触发：
//!
//! 1. 前置校验：本人身份已配置（V03 归属锚点不臆断）、出域策略合法、
//!    runtime 可构造——任一不满足直接拒绝，不降级为假成功；
//! 2. runtime 选择：`mock`（离线夹具，默认）或 `cli`（真实 OMP）。出域
//!    闸门在 infra 层单层执行：默认 `policy=none` 时真实 runtime 对每批
//!    返回 `Blocked`，正文不出本机，记录转 `judge_state='blocked'`；
//! 3. 审计：`blocked` 批写 `egress_log(decision='blocked')`；cli 模式下
//!    过闸批次（正文确已发往已批准模型）写 `decision='allowed'`；
//!    mock 模式内容从未离开本机，不写。
//!
//! 连接纪律：整轮 job 持 `db_gate` 读锁，读锁先于连接、晚于连接释放。
//! `omp` 可执行文件只按固定程序名在受信位置解析（[`super::trusted_exe`]），
//! 调用方不能指定别的程序或参数。
//!
//! 判读在 blocking 线程执行（CLI 子进程 + 多批事务），命令立即返回
//! `jobId`，结果经 `judge://done` 事件推送。

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tauri::Emitter;
use workcontext_app::judge::{judge_pending, ClassifyRuntime};
use workcontext_core::{MeContext, PolicyValidator};
use workcontext_infra::egress::EgressPolicy;
use workcontext_infra::mock::MockAgentRuntime;
use workcontext_infra::omp::{OmpCliConfig, OmpCliRuntime, RuntimeHealth, RuntimeHealthCode};
use workcontext_store::rusqlite::Connection;

use super::config::{self, JudgeMode, SettingsFile};
use super::egress;
use super::{lock_gate, trusted_exe, with_conn, AppError, ComponentHealth, SourceState, OMP_EXE};

/// 单轮判读的记录上限（批 ≤5 的整数倍，防止一次触发跑无限久）。
pub const DEFAULT_JUDGE_LIMIT: u16 = 25;

/// `judge_now` 立即返回。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JudgeJobStarted {
    pub job_id: String,
}

/// `judge_retry` 立即返回：jobId + 本次重新入队的记录数。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JudgeRetryStarted {
    pub job_id: String,
    pub requeued: u64,
}

/// `judge://done` 事件载荷。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct JudgeDoneEvent {
    job_id: String,
    /// 仅 `judge_retry` 触发的轮次有值。
    requeued: Option<u64>,
    /// `app::judge` 的 `JudgeRunSummary` 原样（camelCase，未变换）。
    summary: Value,
    egress: EgressCounts,
    error: Option<AppError>,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct EgressCounts {
    blocked_batches: usize,
    allowed_batches: usize,
}

/// 手动触发一轮判读：处理库内 `judge_state='pending'` 的记录。
///
/// 默认出域策略 `none` 下，cli 模式的每一批都会被闸门拦下并记入
/// `egress_log`（今日页据此显示「N 条因出域策略未判读」），不发起模型调用。
#[tauri::command]
pub async fn judge_now(
    app: tauri::AppHandle,
    state: tauri::State<'_, SourceState>,
    limit: Option<u16>,
) -> Result<JudgeJobStarted, AppError> {
    let cfg = super::load_settings(&state)?;
    validate_preconditions(&cfg)?;
    let job_id = with_conn(&state, |conn| super::random_id(conn))?;
    spawn_judge(
        &app,
        &state,
        job_id.clone(),
        None,
        limit.unwrap_or(DEFAULT_JUDGE_LIMIT).max(1),
    );
    Ok(JudgeJobStarted { job_id })
}

/// 重新入队被出域策略拦截（或被校验拒绝）的记录并立即触发判读。
///
/// - `recordIds` 省略 → 重新入队**全部** `judge_state='blocked'`（典型场景：
///   出域策略获批后补判读）；
/// - 指定 → 仅这些记录，且只接受 `blocked`/`rejected` 状态（`judged` 的
///   结果不回滚——人工已确认的历史不被覆盖）。
#[tauri::command]
pub async fn judge_retry(
    app: tauri::AppHandle,
    state: tauri::State<'_, SourceState>,
    record_ids: Option<Vec<String>>,
) -> Result<JudgeRetryStarted, AppError> {
    let cfg = super::load_settings(&state)?;
    validate_preconditions(&cfg)?;
    let (job_id, requeued) = with_conn(&state, |conn| {
        let requeued = requeue_records(conn, record_ids)?;
        let job_id = super::random_id(conn)?;
        Ok((job_id, requeued))
    })?;
    spawn_judge(&app, &state, job_id.clone(), Some(requeued), DEFAULT_JUDGE_LIMIT);
    Ok(JudgeRetryStarted { job_id, requeued })
}

fn spawn_judge(
    app: &tauri::AppHandle,
    state: &SourceState,
    job_id: String,
    requeued: Option<u64>,
    limit: u16,
) {
    let db_gate = Arc::clone(&state.db_gate);
    let db_path = state.db_path.clone();
    let config_path = state.config_path.clone();
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        run_judge_job(app, db_gate, db_path, config_path, job_id, requeued, limit);
    });
}

/// 读锁是本函数的第一个局部变量：先于连接获取、晚于连接释放。
fn run_judge_job(
    app: tauri::AppHandle,
    db_gate: Arc<RwLock<()>>,
    db_path: PathBuf,
    config_path: PathBuf,
    job_id: String,
    requeued: Option<u64>,
    limit: u16,
) {
    let _gate = lock_gate(&db_gate);
    let (summary, egress_counts, error) = match judge_round(&db_path, &config_path, limit) {
        Ok((summary, counts)) => (summary, counts, None),
        Err(err) => (Value::Null, EgressCounts::default(), Some(err)),
    };
    let _ = app.emit(
        "judge://done",
        JudgeDoneEvent {
            job_id,
            requeued,
            summary,
            egress: egress_counts,
            error,
        },
    );
}

/// 一轮判读 + 出域审计。所有失败原样上抛（AppError），不静默吞。
fn judge_round(
    db_path: &Path,
    config_path: &Path,
    limit: u16,
) -> Result<(Value, EgressCounts), AppError> {
    let cfg = config::load(config_path)?;
    super::validate_settings(&cfg)?;
    validate_preconditions(&cfg)?;
    let mut conn = workcontext_store::open(db_path).map_err(AppError::from_store)?;

    let me = MeContext {
        user_id: cfg.me.user_id.trim().to_string(),
        display_name: cfg.me.display_name.clone(),
    };
    let validator = PolicyValidator::new();
    let runtime: Box<dyn ClassifyRuntime> = match cfg.judge.mode {
        JudgeMode::Mock => Box::new(mock_runtime(&cfg)?),
        JudgeMode::Cli => Box::new(omp_runtime(&cfg)?),
    };

    let now = super::now_ms();
    let summary = judge_pending(&mut conn, runtime.as_ref(), &validator, &me, now, limit)
        .map_err(AppError::internal)?;
    let summary_value = serde_json::to_value(&summary).map_err(AppError::internal)?;

    // 出域审计：按批落 egress_log。字段经序列化视图读取（与 wire 契约同形），
    // 不依赖 app::judge 的 Rust 字段命名。
    let mut counts = EgressCounts::default();
    if let Some(batches) = summary_value.get("batches").and_then(Value::as_array) {
        for batch in batches {
            let status = batch.get("status").and_then(Value::as_str).unwrap_or("");
            let record_ids: Vec<String> = batch
                .get("recordIds")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let run_id = batch.get("runId").and_then(Value::as_str);
            if status == "blocked" {
                egress::log_blocked(
                    &conn,
                    now,
                    &cfg.egress.policy,
                    cfg.egress.approved_model.as_deref(),
                    &record_ids,
                    run_id,
                )
                .map_err(AppError::from_db)?;
                counts.blocked_batches += 1;
            } else if matches!(cfg.judge.mode, JudgeMode::Cli)
                && matches!(status, "ok" | "rejected")
            {
                if let Some(run_id) = run_id {
                    egress::log_allowed(
                        &conn,
                        now,
                        &cfg.egress.policy,
                        cfg.egress.approved_model.as_deref(),
                        &record_ids,
                        run_id,
                    )
                    .map_err(AppError::from_db)?;
                    counts.allowed_batches += 1;
                }
            }
        }
    }
    Ok((summary_value, counts))
}

// ---------------------------------------------------------------------------
// runtime 构造（真实 CLI / 离线夹具）
// ---------------------------------------------------------------------------

fn egress_policy_of(cfg: &SettingsFile) -> Result<EgressPolicy, AppError> {
    match cfg.egress.policy.as_str() {
        "none" => Ok(EgressPolicy::None),
        "redacted" => Ok(EgressPolicy::Redacted),
        "full" => Ok(EgressPolicy::Full),
        other => Err(AppError::invalid_argument(format!(
            "出域策略取值非法：{other}"
        ))),
    }
}

/// 离线夹具 runtime：无夹具目录即拒绝（返回 unknown 而非编造答案）。
fn mock_runtime(cfg: &SettingsFile) -> Result<MockAgentRuntime, AppError> {
    let dir = cfg
        .judge
        .fixtures_dir
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .ok_or_else(|| {
            AppError::invalid_argument("mock 判读需要配置夹具目录（judge.fixturesDir）。")
        })?;
    MockAgentRuntime::open(Path::new(dir))
        .map_err(|e| AppError::invalid_argument(format!("判读夹具加载失败：{e}")))
}

/// 真实 OMP CLI runtime：`omp` 只按固定程序名解析为受信绝对路径；闸门参数
/// 随构造传入（infra 层单层执行）。
pub(super) fn omp_runtime(cfg: &SettingsFile) -> Result<OmpCliRuntime, AppError> {
    let model_id = cfg
        .judge
        .model_id
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .ok_or_else(|| {
            AppError::invalid_argument("cli 判读需要配置 judge.modelId（精确 provider/model id）。")
        })?
        .to_string();
    let policy = egress_policy_of(cfg)?;
    if cfg.egress.policy != "none" {
        // 双层不同源的第二层：egress_set 已校验过一次，构造 runtime 再核一遍。
        match cfg
            .egress
            .approved_model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            Some(approved) if approved == model_id => {}
            Some(approved) => {
                return Err(AppError::invalid_argument(format!(
                    "已批准模型（{approved}）与判读模型（{model_id}）不一致，正文不会发送。"
                )))
            }
            None => {
                return Err(AppError::invalid_argument(
                    "策略非 none 时必须配置已批准模型（egress.approvedModel）。",
                ))
            }
        }
    }
    // `omp` 只按固定程序名解析为受信绝对路径；其余参数取 runtime 默认。
    let mut omp_cfg = OmpCliConfig::new(trusted_exe(OMP_EXE)?, model_id);
    omp_cfg.timeout = Duration::from_secs(cfg.judge.timeout_seconds.clamp(5, 600));
    omp_cfg.egress_policy = policy;
    omp_cfg.approved_model = cfg.egress.approved_model.clone();
    Ok(OmpCliRuntime::new(omp_cfg))
}

/// `agent_health` 的 OMP 侧（真实执行 `omp --version`，不模拟）。
pub(super) fn omp_health(cfg: &SettingsFile) -> ComponentHealth {
    let unavailable = |detail: String| ComponentHealth {
        available: false,
        version: None,
        authenticated: None,
        detail: Some(detail),
    };
    let runtime = match omp_runtime(cfg) {
        Ok(runtime) => runtime,
        Err(err) => return unavailable(err.message),
    };
    match runtime.health() {
        RuntimeHealth::Ok { version } => ComponentHealth {
            available: true,
            version: Some(version),
            authenticated: None,
            detail: None,
        },
        RuntimeHealth::Unavailable { code } => unavailable(describe_health_code(&code)),
    }
}

fn describe_health_code(code: &RuntimeHealthCode) -> String {
    match code {
        RuntimeHealthCode::Spawn => "无法启动 OMP CLI（检查是否已安装 omp）。".into(),
        RuntimeHealthCode::Timeout => "OMP CLI 版本探测超时。".into(),
        RuntimeHealthCode::BadOutput => "OMP CLI 版本输出无法解析（可能版本漂移）。".into(),
        RuntimeHealthCode::VersionMismatch {
            found,
            expected_major,
        } => format!("OMP 版本不匹配：实测 {found}，期望主版本 {expected_major}。"),
    }
}

// ---------------------------------------------------------------------------
// 前置校验 / 重入队
// ---------------------------------------------------------------------------

pub(super) fn validate_preconditions(cfg: &SettingsFile) -> Result<(), AppError> {
    if cfg.me.user_id.trim().is_empty() {
        return Err(AppError::invalid_argument(
            "未配置本人身份（config.json 的 me.userId）：归属锚点无法判定，判读拒绝执行。",
        ));
    }
    if matches!(cfg.judge.mode, JudgeMode::Cli)
        && cfg
            .judge
            .model_id
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .is_none()
    {
        return Err(AppError::invalid_argument(
            "cli 判读需要配置 judge.modelId（精确 provider/model id）。",
        ));
    }
    Ok(())
}

/// 把 `blocked`/`rejected` 记录放回 `pending`（返回受影响行数，真实计数）。
fn requeue_records(
    conn: &mut Connection,
    record_ids: Option<Vec<String>>,
) -> Result<u64, AppError> {
    use workcontext_store::rusqlite::params_from_iter;
    let changed = match record_ids {
        Some(ids) if !ids.is_empty() => {
            let placeholders = vec!["?"; ids.len()].join(",");
            let sql = format!(
                "UPDATE source_record SET judge_state = 'pending'
                 WHERE judge_state IN ('blocked','rejected') AND id IN ({placeholders})"
            );
            conn.execute(&sql, params_from_iter(ids.iter().map(String::as_str)))
                .map_err(AppError::from_db)? as u64
        }
        _ => conn
            .execute(
                "UPDATE source_record SET judge_state = 'pending' WHERE judge_state = 'blocked'",
                [],
            )
            .map_err(AppError::from_db)? as u64,
    };
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::super::tests::Fixture;
    use super::*;

    /// 插入一条可判读的入库记录（带 0002 元数据、`judge_state='pending'`）。
    fn insert_record(fx: &Fixture, id: &str, judge_state: &str) {
        fx.with_conn(|conn| {
            conn.execute(
                "INSERT INTO source_record(id, source_id, external_id, kind, occurred_at, ingested_at,
                        conversation_id, sender_id, sender_name, direction, summary, body, content_hash,
                        raw_state, judge_state, mentions_json, conversation_type)
                 VALUES(?1, 's1', ?2, 'im_message', 1000, 1000, 'cidA', 'other01', '对方', 'in',
                        '摘要', '正文', ?3, 'available', ?4, '[]', 'dm')",
                workcontext_store::rusqlite::params![
                    id,
                    id,
                    format!("{id}-hash"),
                    judge_state
                ],
            )
            .expect("插入记录");
        });
    }

    fn judge_state(fx: &Fixture, id: &str) -> String {
        fx.with_conn(|conn| {
            conn.query_row(
                "SELECT judge_state FROM source_record WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .expect("记录")
        })
    }

    fn count(fx: &Fixture, what: &str) -> i64 {
        fx.with_conn(|conn| {
            conn.query_row(&format!("SELECT COUNT(*) FROM {what}"), [], |row| row.get(0))
                .expect("计数")
        })
    }

    /// 写入「真实 CLI 判读」所需的配置：本人身份 + 精确模型 id。
    fn configure_cli_judging(fx: &Fixture, model: &str) {
        let mut cfg = fx.config();
        cfg.me.user_id = "me01".into();
        cfg.judge.mode = JudgeMode::Cli;
        cfg.judge.model_id = Some(model.into());
        config::save(&fx.state.config_path, &cfg).expect("写配置");
    }

#[test]
    fn cli_judging_under_the_default_policy_is_blocked_before_anything_leaves() {
        let fx = Fixture::new();
        fx.register("s1", &["cidA"], &[], "corp1:user1").expect("注册");
        insert_record(&fx, "sr_1", "pending");
        configure_cli_judging(&fx, "vendor/model-x");
        assert_eq!(fx.config().egress.policy, "none", "默认策略必须是 none");
        let (db, cfg_path) = (fx.state.db_path.clone(), fx.state.config_path.clone());
        let omp_available = trusted_exe(OMP_EXE).is_ok();

        match judge_round(&db, &cfg_path, 25) {
            Ok((summary, counts)) => {
                // 真实 runtime 已构造（闸门在 runtime 内）：每一批都被拦下。
                let batches = summary["batches"].as_array().expect("batches");
                assert_eq!(batches.len(), 1);
                assert_eq!(batches[0]["status"], "blocked");
                assert_eq!(batches[0]["recordIds"], serde_json::json!(["sr_1"]));
                assert_eq!(counts.blocked_batches, 1);
                assert_eq!(counts.allowed_batches, 0);
                assert_eq!(judge_state(&fx, "sr_1"), "blocked");
                // 审计：一条 blocked、零条 allowed；没有成功的模型调用；事项不动。
                assert_eq!(count(&fx, "egress_log WHERE decision = 'blocked'"), 1);
                assert_eq!(count(&fx, "egress_log WHERE decision = 'allowed'"), 0);
                assert_eq!(count(&fx, "agent_run WHERE status = 'ok'"), 0);
                assert_eq!(count(&fx, "work_item"), 0);
                // 重试入口：重新入队后回到 pending（尚未再次判读）。
                let requeued = fx.with_conn(|conn| requeue_records(conn, None).expect("重入队"));
                assert_eq!(requeued, 1);
                assert_eq!(judge_state(&fx, "sr_1"), "pending");
            }
            Err(err) => {
                // 本机没有受信 omp：如实报不可用，不构造 runtime，记录原样保留。
                assert!(!omp_available, "装有 omp 时不应失败：{err:?}");
                assert_eq!(err.code, "AGENT_UNREACHABLE");
                assert_eq!(judge_state(&fx, "sr_1"), "pending");
                assert_eq!(count(&fx, "egress_log"), 0);
            }
        }
    }

    #[test]
    fn judging_refuses_to_guess_without_identity_or_model() {
        let fx = Fixture::new();
        fx.register("s1", &["cidA"], &[], "corp1:user1").expect("注册");
        insert_record(&fx, "sr_1", "pending");
        let (db, cfg_path) = (fx.state.db_path.clone(), fx.state.config_path.clone());

        // 未配置本人身份：归属锚点无法判定，拒绝（默认 mock 模式）。
        let no_identity = judge_round(&db, &cfg_path, 25).expect_err("缺身份");
        assert_eq!(no_identity.code, "INVALID_ARGUMENT");

        // 配了身份，但 cli 模式没有模型 id。
        let mut cfg = fx.config();
        cfg.me.user_id = "me01".into();
        cfg.judge.mode = JudgeMode::Cli;
        config::save(&fx.state.config_path, &cfg).expect("写配置");
        let no_model = judge_round(&db, &cfg_path, 25).expect_err("缺模型");
        assert_eq!(no_model.code, "INVALID_ARGUMENT");

        // 身份与模型齐备但仍是默认 none：结论只可能是「拦截」或「omp 不可用」。
        cfg.judge.model_id = Some("vendor/model-x".into());
        config::save(&fx.state.config_path, &cfg).expect("写配置");
        match judge_round(&db, &cfg_path, 25) {
            Ok((_, counts)) => {
                assert_eq!(counts.allowed_batches, 0, "none 策略下不得有放行批次");
                assert_eq!(count(&fx, "egress_log WHERE decision = 'allowed'"), 0);
            }
            Err(err) => assert_eq!(err.code, "AGENT_UNREACHABLE"),
        }
        assert_eq!(count(&fx, "agent_run WHERE status = 'ok'"), 0);
        assert_eq!(count(&fx, "work_item"), 0);
    }

    #[test]
    fn runtime_construction_requires_an_approved_matching_model() {
        let fx = Fixture::new();
        let mut cfg = fx.config();
        cfg.me.user_id = "me01".into();
        cfg.judge.mode = JudgeMode::Cli;
        cfg.judge.model_id = Some("vendor/model-x".into());

        // none：不需要已批准模型；结果只取决于 omp 是否安装。
        let outcome = omp_runtime(&cfg);
        assert_eq!(outcome.is_ok(), trusted_exe(OMP_EXE).is_ok());

        // 放行策略下：未指明已批准模型、或与判读模型不一致，一律拒绝构造。
        cfg.egress.policy = "full".into();
        cfg.egress.approved_model = None;
        assert_eq!(omp_runtime(&cfg).expect_err("缺已批准模型").code, "INVALID_ARGUMENT");
        cfg.egress.approved_model = Some("vendor/other-model".into());
        assert_eq!(omp_runtime(&cfg).expect_err("模型不一致").code, "INVALID_ARGUMENT");

        // 一致时才允许构造；策略取值非法仍然拒绝。
        cfg.egress.approved_model = Some("vendor/model-x".into());
        assert_eq!(omp_runtime(&cfg).is_ok(), trusted_exe(OMP_EXE).is_ok());
        cfg.egress.policy = "sometimes".into();
        assert_eq!(omp_runtime(&cfg).expect_err("非法策略").code, "INVALID_ARGUMENT");
    }

    #[test]
    fn requeue_only_touches_blocked_and_rejected_records() {
        let fx = Fixture::new();
        fx.register("s1", &["cidA"], &[], "corp1:user1").expect("注册");
        for (id, state) in [
            ("sr_blocked", "blocked"),
            ("sr_rejected", "rejected"),
            ("sr_judged", "judged"),
            ("sr_skipped", "skipped"),
        ] {
            insert_record(&fx, id, state);
        }
        // 不指定：只重入队全部 blocked。
        assert_eq!(
            fx.with_conn(|conn| requeue_records(conn, None).expect("重入队")),
            1
        );
        assert_eq!(judge_state(&fx, "sr_blocked"), "pending");
        assert_eq!(judge_state(&fx, "sr_rejected"), "rejected");
        // 指定：rejected 可重入队；judged/skipped 不受影响。
        assert_eq!(
            fx.with_conn(|conn| {
                requeue_records(
                    conn,
                    Some(vec!["sr_rejected".into(), "sr_judged".into(), "sr_skipped".into()]),
                )
                .expect("重入队")
            }),
            1
        );
        assert_eq!(judge_state(&fx, "sr_rejected"), "pending");
        assert_eq!(judge_state(&fx, "sr_judged"), "judged");
        assert_eq!(judge_state(&fx, "sr_skipped"), "skipped");
    }
}
