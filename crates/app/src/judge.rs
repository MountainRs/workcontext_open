//! SQLite 判读事务（详细设计 §5.2、§9 `judge_pending`）。
//!
//! 链路：待判读 `source_record`（`judge_state='pending'`）→ 按
//! `(source_id, conversation_id)` 连续分组（批 ≤5，§7.1）→ 组装规范
//! [`ClassifyInput`] → runtime（当前 [`MockAgentRuntime`] 可回放）→ core
//! [`PolicyValidator`]（V01–V12）→ 单事务落库：
//!
//! ```text
//! BEGIN IMMEDIATE
//!   INSERT agent_run(ok|rejected|skipped, output_sha256, input_sha256)
//!   IF 不建/不改事项: 只记 run + judge_state
//!   IF item 不存在: INSERT work_item
//!   ELSE: UPDATE 决策字段（人工锁字段已由校验器跳过或整次拒绝）
//!   UPSERT link_evidence（origin / reference / similar_candidate）
//!   INSERT work_event(actor='agent', run_id)
//!   UPDATE source_record SET judge_state='judged' | 'rejected'
//! COMMIT
//! ```
//!
//! 幂等（§4.4）：同 `(kind, input_sha256, prompt_version, model_id)` 已有
//! `status='ok'` 的判读不重复落库（`BatchReport.status="skipped"`）；人工纠错
//! 写入 `field_override` 后，同一输入重放会被 V07/R_LOCKED 整次拒绝（A11），
//! 人工锁跨重启生效（锁存 DB，不存内存）。
//!
//! 开发用例（不入生产路径）：[`import_case`] 把合成夹具（`fixtures/cases`）
//! 经应用服务导入**临时库**（预筛 + 判读元数据列），[`plan_case`] 按
//! `fixtures/mock/manifest.json` 组出与 mock 夹具同哈希的批次，
//! [`judge_planned_batch`] 复用同一事务落库。默认数据库不预置任何假数据。

use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;
use workcontext_core::{
    JudgeContext, MeContext, PolicyValidator, RecordContext, ValidatedJudgement,
    WriteMode,
};
use workcontext_infra::classify_input::{
    self, ClassifyInput, ConversationRef, InputRecord, PersonRef, SourceRef,
    PROMPT_VERSION, RUN_KIND,
};
use workcontext_infra::fixture_case;
use workcontext_infra::mock::{MockAgentError, MockAgentRuntime, MockOutcome};
use workcontext_store::rusqlite::{self, params, Connection, OptionalExtension, TransactionBehavior};

use crate::items::{load_item, load_locks};
use crate::prefilter::prefilter_record;

// ========================= 判读 runtime 抽象（B7 可插拔） =========================

/// 判读运行时接口（§8.1）：mock（可回放）与真实 OMP CLI/HTTP 实现的统一入口。
/// trait 落在 app（编排层）；`MockAgentRuntime` 在 infra、`OmpCliRuntime`
///（RealAdapters）由 app 统一 impl——出域闸门 Blocked→[`RuntimeOutcome::Blocked`]
/// 的映射只允许存在这一层。
pub trait ClassifyRuntime {
 /// 写入 `agent_run.runtime`。
 fn runtime_name(&self) -> &'static str;
 /// 参与 `agent_run` 幂等唯一索引。
 fn model_id(&self) -> String;
 /// 判读：规范输入 → 原始 classify/v1 输出或失败分类。
 fn classify(&self, input: &ClassifyInput) -> RuntimeOutcome;
}

/// runtime 层结果（接口层口径；与业务校验 [`PolicyValidator`] 分层）。
#[derive(Debug, Clone)]
pub enum RuntimeOutcome {
 /// 拿到原始输出（合法性与否交校验器）。
 Ok(Value),
 /// 明确 unknown：无答案（mock 无夹具），不编造。
 Unknown,
 /// 出域闸门拦截（§7.3）：记录 `blocked`，事项不动。
 Blocked { reason: String },
 /// runtime 判定载荷非法（如 AGENT_BAD_JSON）。
 Rejected { code: String },
 /// 连接/执行失败。
 Failed { code: String },
 /// 超时（§8.3：不自动重试）。
 Timeout,
 /// runtime 不可用（连续失败暂停等）。
 Unavailable { code: String },
 /// 成本护栏拒绝。
 CostGuard,
}

impl ClassifyRuntime for MockAgentRuntime {
 fn runtime_name(&self) -> &'static str {
 MockAgentRuntime::runtime_name(self)
 }
 fn model_id(&self) -> String {
 MockAgentRuntime::model_id(self).to_string()
 }
 fn classify(&self, input: &ClassifyInput) -> RuntimeOutcome {
 match MockAgentRuntime::classify(self, input) {
 MockOutcome::Replay { output, .. } => RuntimeOutcome::Ok(output),
 MockOutcome::Unknown { .. } => RuntimeOutcome::Unknown,
 }
 }
}

/// 真实 runtime（OMP CLI）：`CliAgentOutcome` → [`RuntimeOutcome`] 的一层映射。
/// 出域闸门在 runtime 内部单层化；Blocked 在此透传，`egress_log` 由
/// desktop 判读触发方在 judge_pending 返回后按批次补写。
impl ClassifyRuntime for workcontext_infra::omp::OmpCliRuntime {
 fn runtime_name(&self) -> &'static str {
 workcontext_infra::omp::OmpCliRuntime::runtime_name(self)
 }
 fn model_id(&self) -> String {
 workcontext_infra::omp::OmpCliRuntime::model_id(self).to_string()
 }
 fn classify(&self, input: &ClassifyInput) -> RuntimeOutcome {
        classify_omp_outcome(input, self.run_judge(input))
    }
}

/// Shared ID translation and outcome mapping for measured and ordinary CLI calls.
pub fn classify_omp_outcome(
    input: &ClassifyInput,
    outcome: workcontext_infra::omp::CliAgentOutcome,
) -> RuntimeOutcome {
    use workcontext_infra::omp::CliAgentOutcome;
    match outcome {
        CliAgentOutcome::Ok(mut ok) => {
            let aliases = workcontext_infra::egress::short_id_map(input);
            for path in ["/source_ids", "/evidence", "/resolution/evidence_ids"] {
                if let Some(ids) = ok.output.pointer_mut(path).and_then(Value::as_array_mut) {
                    for id in ids {
                        let Some(local) = id.as_str().and_then(|short| aliases.get(short)) else {
                            return RuntimeOutcome::Rejected { code: "AGENT_UNKNOWN_SOURCE_ID".into() };
                        };
                        *id = Value::String(local.clone());
                    }
                }
            }
            RuntimeOutcome::Ok(ok.output)
        }
        CliAgentOutcome::Blocked { reason, .. } => RuntimeOutcome::Blocked {
            reason: reason.code().to_string(),
        },
        failed @ CliAgentOutcome::Rejected(_) => RuntimeOutcome::Rejected {
            code: failed.error_code().expect("rejected CLI code"),
        },
        failed @ CliAgentOutcome::Failed(_) => RuntimeOutcome::Failed {
            code: failed.error_code().expect("failed CLI code"),
        },
        CliAgentOutcome::Timeout(_) => RuntimeOutcome::Timeout,
        failed @ CliAgentOutcome::Unavailable(_) => RuntimeOutcome::Unavailable {
            code: failed.error_code().expect("unavailable CLI code"),
        },
        CliAgentOutcome::CostGuard => RuntimeOutcome::CostGuard,
    }
}

/// 判读失败（全部 fail-loud：不静默降级、不造答案）。
#[derive(Debug, thiserror::Error)]
pub enum JudgeError {
    #[error("数据库读写失败：{0}")]
    Db(#[from] rusqlite::Error),
    #[error("夹具加载失败：{0}")]
    Fixture(#[from] fixture_case::FixtureCaseError),
    #[error("mock runtime 加载失败：{0}")]
    Mock(#[from] MockAgentError),
    #[error("判读批次引用的记录未入库：{external_id}")]
    RecordMissing { external_id: String },
    #[error("归档证据命中多个事项，拒绝自动合并")]
    ArchiveAmbiguous,
    #[error("归档消息早于事项的最新证据，拒绝覆盖")]
    ArchiveStale,
    #[error("归档进展消息已不再关联已有事项")]
    ArchiveDetached,
    #[error("mock runtime 无该输入的判读夹具（input_sha256={sha}），拒绝编造答案")]
    UnknownJudgement { sha: String },
    #[error("manifest 声明的 input_sha256 与实际组装不一致（manifest={manifest}, actual={actual}）")]
    HashMismatch { manifest: String, actual: String },
    #[error("夹具文件结构不符：{0}")]
    BadCase(String),
    #[error("IO 失败 {path}：{source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// 一次判读批次的结果（camelCase，可直接进 IPC/诊断页）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchReport {
    pub run_id: String,
    /// ok | rejected | skipped。
    pub status: &'static str,
    pub input_sha256: String,
    #[serde(rename = "recordIds")]
    pub records: Vec<String>,
    pub intent_class: Option<String>,
    pub rejection_rule: Option<String>,
    pub item_id: Option<String>,
    /// evidence | reply_to（与既有事项的命中依据，透明留痕）。
    pub matched_by: Option<&'static str>,
    pub warnings: Vec<String>,
    pub skipped_reason: Option<&'static str>,
}

/// `judge_pending` 的汇总。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JudgeRunSummary {
    pub batches: Vec<BatchReport>,
    /// 仍处于 pending 的记录数。
    pub pending_left: usize,
    /// 因缺判读元数据（conversation_type 等，0002 迁移前旧记录）而跳过的条数。
    pub skipped_meta: usize,
}

/// `import_case` 的结果摘要。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSummary {
    pub case_id: String,
    pub inserted_records: usize,
    pub kept: usize,
    pub skipped: usize,
}

// ========================= judge_pending（生产入口） =========================

/// 判读全部待处理记录（§9 `judge_pending`；同步后由 desktop 触发）。
///
/// - `me` 由调用方从配置注入（判读身份不读 source 表）；
/// - `runtime` 注入判读运行时（当前为离线 mock；B7 真实 runtime 落地后换 trait）；
/// - 缺判读元数据（0002 列为 NULL）的记录**不臆断**：保持 pending，计入
///   `skipped_meta`，等同步引擎补齐后再判。
pub fn judge_pending(
    conn: &mut Connection,
    runtime: &dyn ClassifyRuntime,
    validator: &PolicyValidator,
    me: &MeContext,
    now_ms: i64,
    limit: u16,
) -> Result<JudgeRunSummary, JudgeError> {
    let mut stmt = conn.prepare(
        "SELECT r.source_id, r.external_id, r.kind, r.conversation_id, r.conversation_type, \
                r.direction, r.sender_id, r.sender_name, r.mentions_json, r.reply_to_id, r.summary, \
                r.body, r.raw_state, r.occurred_at, r.ingested_at, s.kind \
         FROM source_record r JOIN source s ON s.id = r.source_id \
         WHERE r.judge_state IN ('pending','kept') \
         ORDER BY r.ingested_at ASC, r.rowid ASC LIMIT ?1",
    )?;
    let rows: Vec<RecordMeta> = stmt
        .query_map(params![i64::from(limit).max(1)], record_meta_row)?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);

    // 缺元数据的记录不判（不臆断）。
    let skipped_meta = rows.iter().filter(|r| r.conversation_type.is_none()).count();
    let judicable: Vec<&RecordMeta> =
        rows.iter().filter(|r| r.conversation_type.is_some()).collect();

    // 分批：**只按显式引用链成批**（§7.1 批 ≤5）。一条记录只有在它的
    // `reply_to_id` 指向批内已有记录且同会话时才并入；否则另起一批。
    // 不以「同会话」把互不引用的消息塞进同一次判读——否则模型可能把同一
    // 会话里两个独立交办并成一个事项。
    let mut batches: Vec<Vec<&RecordMeta>> = Vec::new();
    for rec in judicable {
        let chained = batches.last_mut().filter(|batch| {
            batch.len() < 5
                && batch[0].source_id == rec.source_id
                && batch[0].conversation_id == rec.conversation_id
                && rec.reply_to_id.as_ref().is_some_and(|reply_to| {
                    batch.iter().any(|b| &b.external_id == reply_to)
                })
        });
        match chained {
            Some(batch) => batch.push(rec),
            None => batches.push(vec![rec]),
        }
    }

    let mut reports = Vec::with_capacity(batches.len());
    for batch in batches {
        // 同会话内的记录才可能同批，conversation 元数据取批首即可。
        let first = batch[0];
        let input = build_input_from_rows(me, first, &batch)?;
        let contexts: Vec<RecordContext> = batch.iter().map(|r| r.to_context()).collect();
        let ids: Vec<String> = batch.iter().map(|r| r.external_id.clone()).collect();
        let report = judge_records(conn, runtime, validator, me, &input, &contexts, &ids, now_ms)?;
        reports.push(report);
    }

    let pending_left: i64 = conn.query_row(
        "SELECT COUNT(*) FROM source_record WHERE judge_state IN ('pending','kept')",
        [],
        |row| row.get(0),
    )?;
    Ok(JudgeRunSummary { batches: reports, pending_left: pending_left as usize, skipped_meta })
}

/// 从库行组装规范判读输入（参与人显示名回落 user_id；occurred_at 以 UTC
/// RFC3339 毫秒确定性渲染——真实 runtime 不依赖夹具哈希，mock 回放走
/// [`plan_case`] 的夹具同构路径）。
fn build_input_from_rows(
    me: &MeContext,
    first: &RecordMeta,
    batch: &[&RecordMeta],
) -> Result<ClassifyInput, JudgeError> {
    let conv_type = first.conversation_type.clone().unwrap_or_else(|| "other".to_string());
    let mut participants: Vec<PersonRef> = Vec::new();
    let seen = |p: PersonRef, participants: &mut Vec<PersonRef>| {
        if p.user_id != me.user_id && !participants.iter().any(|x| x.user_id == p.user_id) {
            participants.push(p);
        }
    };
    let mut records = Vec::with_capacity(batch.len());
    for rec in batch {
        seen(
            PersonRef {
                user_id: rec.sender_id.clone().unwrap_or_default(),
                display_name: rec.sender_name.clone().unwrap_or_default(),
            },
            &mut participants,
        );
        for m in &rec.mentions {
            seen(PersonRef { user_id: m.clone(), display_name: m.clone() }, &mut participants);
        }
        records.push(InputRecord {
            id: rec.external_id.clone(),
            direction: rec.direction.clone(),
            sender_id: rec.sender_id.clone().unwrap_or_default(),
            sender_name: rec.sender_name.clone().unwrap_or_default(),
            mentions: rec.mentions.clone(),
            reply_to: rec.reply_to_id.clone(),
            occurred_at: rfc3339_utc(rec.occurred_at),
            body: rec.body.clone().unwrap_or_default(),
        });
    }
    Ok(ClassifyInput::new(
        SourceRef { id: first.source_id.clone(), kind: first.source_kind.clone() },
        ConversationRef {
            id: first.conversation_id.clone().unwrap_or_default(),
            conversation_type: conv_type,
            display_name: first.conversation_id.clone().unwrap_or_default(),
        },
        PersonRef {
            user_id: me.user_id.clone(),
            display_name: me.display_name.clone().unwrap_or_else(|| me.user_id.clone()),
        },
        participants,
        records,
    ))
}

fn rfc3339_utc(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| "1970-01-01T00:00:00.000Z".to_string())
}

/// 判读所需的 `source_record` 行（0002 元数据列在内）。
struct RecordMeta {
    source_id: String,
    external_id: String,
    kind: String,
    conversation_id: Option<String>,
    conversation_type: Option<String>,
    direction: String,
    sender_id: Option<String>,
    sender_name: Option<String>,
    mentions: Vec<String>,
    reply_to_id: Option<String>,
    summary: Option<String>,
    body: Option<String>,
    raw_state: String,
    occurred_at: i64,
    ingested_at: i64,
    source_kind: String,
}

impl RecordMeta {
    fn to_context(&self) -> RecordContext {
        RecordContext {
            id: self.external_id.clone(),
            source_id: self.source_id.clone(),
            external_id: self.external_id.clone(),
            kind: serde_json::from_value(Value::String(self.kind.clone()))
                .unwrap_or(workcontext_core::RecordKind::ManualNote),
            conversation_id: self.conversation_id.clone(),
            conversation_type: serde_json::from_value(Value::String(
                self.conversation_type.clone().unwrap_or_else(|| "other".to_string()),
            ))
            .unwrap_or(workcontext_core::ConversationType::Other),
            direction: serde_json::from_value(Value::String(self.direction.clone()))
                .unwrap_or(workcontext_core::Direction::Na),
            sender_id: self.sender_id.clone(),
            mentions: self.mentions.clone(),
            occurred_at: self.occurred_at,
            ingested_at: self.ingested_at,
            summary: self.summary.clone(),
            // 出域/到期后正文不可得：V04 的 basis 匹配自然失败（清空时间）。
            body: if self.raw_state == "available" { self.body.clone() } else { None },
        }
    }
}

fn record_meta_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RecordMeta> {
    let mentions_json: String = row.get(8)?;
    let mentions = serde_json::from_str::<Vec<String>>(&mentions_json).unwrap_or_default();
    Ok(RecordMeta {
        source_id: row.get(0)?,
        external_id: row.get(1)?,
        kind: row.get(2)?,
        conversation_id: row.get(3)?,
        conversation_type: row.get(4)?,
        direction: row.get(5)?,
        sender_id: row.get(6)?,
        sender_name: row.get(7)?,
        mentions,
        reply_to_id: row.get(9)?,
        summary: row.get(10)?,
        body: row.get(11)?,
        raw_state: row.get(12)?,
        occurred_at: row.get(13)?,
        ingested_at: row.get(14)?,
        source_kind: row.get(15)?,
    })
}

// ========================= 判读事务核心 =========================

/// 一次判读批次的完整处理（§5.2）。`input` 必须与 `contexts`/`record_ids`
/// 同批同序。
///
/// **runtime 在 DB 写事务之外执行**（OMP 子进程可达 30 s，不能占着写锁）；
/// 只有拿到输出后才 `BEGIN IMMEDIATE` 落库。
#[allow(clippy::too_many_arguments)]
pub fn judge_records(
    conn: &mut Connection,
    runtime: &dyn ClassifyRuntime,
    validator: &PolicyValidator,
    me: &MeContext,
    input: &ClassifyInput,
    contexts: &[RecordContext],
    record_ids: &[String],
    now_ms: i64,
) -> Result<BatchReport, JudgeError> {
    let outcome = runtime.classify(input);
    apply_classified_records(conn, runtime, validator, me, input, contexts, record_ids, outcome, now_ms, None)
}

/// Apply a classification (already journaled by the archive runner when supplied).
/// `input`, `contexts` and `record_ids` must describe the same nonempty batch.
#[allow(clippy::too_many_arguments)]
pub fn apply_classified_records(
    conn: &mut Connection,
    runtime: &dyn ClassifyRuntime,
    validator: &PolicyValidator,
    me: &MeContext,
    input: &ClassifyInput,
    contexts: &[RecordContext],
    record_ids: &[String],
    outcome: RuntimeOutcome,
    now_ms: i64,
    archive: Option<&ArchiveMatch<'_>>,
) -> Result<BatchReport, JudgeError> {
    let sha = classify_input::input_sha256(input);
    let output = match outcome {
        RuntimeOutcome::Ok(output) => output,
        other => {
            return record_runtime_failure(conn, runtime, contexts, record_ids, &sha, &other, now_ms)
        }
    };
    let raw = serde_json::to_string(&output).expect("Value 序列化不会失败");

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let ids = locate_records(&tx, contexts, record_ids)?;
    let report = judge_in_tx(&tx, runtime, validator, me, contexts, record_ids, &sha, &raw, &ids, now_ms, archive)?;
    tx.commit()?;
    Ok(report)
}

/// runtime 层失败：分类落 `agent_run`；只有 Rejected 改判读状态，其余保持
/// pending/kept 等人工重试（§8.3 不自动重试）。
///
/// **Blocked 例外**（§7.3）：模型根本没被调用，不写 `agent_run`；只把批内记录
/// 置 `judge_state='blocked'`（事项不动），`egress_log` 由调用方按批补写。
#[allow(clippy::too_many_arguments)]
fn record_runtime_failure(
    conn: &mut Connection,
    runtime: &dyn ClassifyRuntime,
    contexts: &[RecordContext],
    record_ids: &[String],
    sha: &str,
    outcome: &RuntimeOutcome,
    now_ms: i64,
) -> Result<BatchReport, JudgeError> {
    if let RuntimeOutcome::Blocked { reason } = outcome {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Ok(pks) = locate_records(&tx, contexts, record_ids) {
            mark_records(&tx, &pks, "blocked")?;
        }
        tx.commit()?;
        return Ok(BatchReport {
            run_id: String::new(),
            status: "blocked",
            input_sha256: sha.to_string(),
            records: record_ids.to_vec(),
            intent_class: None,
            rejection_rule: None,
            item_id: None,
            matched_by: None,
            warnings: vec![reason.clone()],
            skipped_reason: Some("egress_blocked"),
        });
    }
    let (status, error_code, reason, judge_state, report_status): (
        &'static str,
        String,
        String,
        Option<&'static str>,
        &'static str,
    ) = match outcome {
        RuntimeOutcome::Unknown => {
            ("skipped", "AGENT_UNKNOWN".into(), "无该输入的判读结果".into(), None, "skipped")
        }
        RuntimeOutcome::Blocked { reason } => unreachable!("上面已处理 Blocked：{reason}"),
        RuntimeOutcome::Rejected { code } => (
            "rejected",
            code.clone(),
            "runtime 判定输出非法".into(),
            Some("rejected"),
            "rejected",
        ),
        RuntimeOutcome::Failed { code } => {
            ("failed", code.clone(), "runtime 执行失败".into(), None, "failed")
        }
        RuntimeOutcome::Timeout => {
            ("timeout", "AGENT_TIMEOUT".into(), "判读超时".into(), None, "failed")
        }
        RuntimeOutcome::Unavailable { code } => {
            ("failed", code.clone(), "runtime 不可用".into(), None, "failed")
        }
        RuntimeOutcome::CostGuard => {
            ("skipped", "COST_GUARD".into(), "成本护栏拒绝".into(), None, "skipped")
        }
        RuntimeOutcome::Ok(_) => unreachable!("调用方已分流 Ok"),
    };
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let run_id = next_run_id(&tx, now_ms, sha)?;
    let pks = locate_records(&tx, contexts, record_ids).ok();
    tx.execute(
        "INSERT INTO agent_run (id, kind, runtime, model_id, prompt_version, input_refs_json, \
         input_sha256, status, error_code, created_at, finished_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
        params![
            run_id,
            RUN_KIND,
            runtime.runtime_name(),
            runtime.model_id(),
            PROMPT_VERSION,
            input_refs_json(pks.as_deref().unwrap_or(&[]), record_ids),
            sha,
            status,
            error_code,
            now_ms
        ],
    )?;
    if let (Some(state), Some(pks)) = (judge_state, pks.as_ref()) {
        mark_records(&tx, pks, state)?;
    }
    tx.commit()?;
    Ok(BatchReport {
        run_id,
        status: report_status,
        input_sha256: sha.to_string(),
        records: record_ids.to_vec(),
        intent_class: None,
        rejection_rule: None,
        item_id: None,
        matched_by: None,
        warnings: vec![reason],
        skipped_reason: Some(match report_status {
            "blocked" => "egress_blocked",
            "skipped" => "runtime_skipped",
            _ => "runtime_failure",
        }),
    })
}

/// 写事务内的全部步骤：匹配 → 校验（含人工锁）→ 落库。任一步 `Err` 即由
/// 调用方丢弃事务整体回滚（业务字段不变，§5）。
#[allow(clippy::too_many_arguments)]
fn judge_in_tx(
    tx: &rusqlite::Transaction<'_>,
    runtime: &dyn ClassifyRuntime,
    validator: &PolicyValidator,
    me: &MeContext,
    contexts: &[RecordContext],
    record_ids: &[String],
    sha: &str,
    raw: &str,
    ids: &[String],
    now_ms: i64,
    archive: Option<&ArchiveMatch<'_>>,
) -> Result<BatchReport, JudgeError> {
    // ---- 既有事项匹配：只认显式证据链 / reply_to 续链，绝不以会话当线程。
    let matched = match archive {
        Some(keys) => match_archive_item(tx, keys)?,
        None => match_item(tx, ids)?,
    };
    if matched.is_none() && archive.is_some_and(|keys| keys.requires_existing) {
        return Err(JudgeError::ArchiveDetached);
    }

    // ---- 校验（真实 PolicyValidator；锁每次从 field_override 读 → 跨重启生效）。
    let existing_view = match &matched {
        Some((item_id, _)) => {
            let row = load_item(tx, item_id)?
                .ok_or_else(|| JudgeError::RecordMissing { external_id: item_id.clone() })?;
            let mut view = row.to_existing();
            if archive.is_some() && view.last_evidence_at.is_some_and(|at| at > contexts[0].occurred_at) {
                return Err(JudgeError::ArchiveStale);
            }
            view.locked_fields = load_locks(tx, item_id)?;
            Some(view)
        }
        None => None,
    };
    let ctx = JudgeContext::new(contexts, me).with_existing(existing_view.as_ref());
    let decision = validator.validate(raw, &ctx);
    if archive.is_some() && matched.is_none() {
        if let Ok(judgement) = &decision {
            // A removed evidence link must not make the Create/dedupe fallback
            // update a locked item that the validator has not seen.
            if judgement.item.is_some() && tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM work_item WHERE dedupe_key=?1)",
                [&judgement.dedupe_key], |row| row.get::<_, bool>(0),
            )? {
                return Err(JudgeError::ArchiveAmbiguous);
            }
        }
    }

    let output_sha = sha256_hex(raw.as_bytes());
    let run_id = next_run_id(tx, now_ms, sha)?;
    let refs = input_refs_json(ids, record_ids);
    let warnings_of =
        |v: &ValidatedJudgement| v.warnings.iter().map(|w| w.code.as_str().to_string()).collect::<Vec<_>>();

    match decision {
        // ---- 幂等：同输入已有成功判读 → 不重复落库/计费（§4.4）。人工锁不因此
        // 失效：锁冲突时校验器已在上面给出 R_LOCKED，走不到这一支。
        Ok(judgement) if has_prior_ok(tx, runtime, sha)? => Ok(BatchReport {
            run_id,
            status: "skipped",
            input_sha256: sha.to_string(),
            records: record_ids.to_vec(),
            intent_class: Some(judgement.intent_class.as_str().to_string()),
            rejection_rule: None,
            item_id: match &judgement.item {
                Some(d) => match &d.mode {
                    WriteMode::Create => None,
                    WriteMode::Update(id) => Some(id.clone()),
                },
                None => None,
            },
            matched_by: matched.map(|(_, r)| reason_str(r)),
            warnings: warnings_of(&judgement),
            skipped_reason: Some("idempotent_hit"),
        }),
        Ok(judgement) => {
            tx.execute(
                "INSERT INTO agent_run (id, kind, runtime, model_id, prompt_version, input_refs_json, \
                 input_sha256, output_sha256, raw_output, latency_ms, status, created_at, finished_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, 'ok', ?10, ?10)",
                params![
                    run_id,
                    RUN_KIND,
                    runtime.runtime_name(),
                    runtime.model_id(),
                    PROMPT_VERSION,
                    refs,
                    sha,
                    output_sha,
                    raw,
                    now_ms
                ],
            )?;
            let item_id = apply_judgement(tx, &judgement, ids, now_ms, &run_id)?;
            mark_records(tx, ids, "judged")?;
            Ok(BatchReport {
                run_id,
                status: "ok",
                input_sha256: sha.to_string(),
                records: record_ids.to_vec(),
                intent_class: Some(judgement.intent_class.as_str().to_string()),
                rejection_rule: None,
                item_id,
                matched_by: matched.map(|(_, r)| reason_str(r)),
                warnings: warnings_of(&judgement),
                skipped_reason: None,
            })
        }
        // ---- §5：拒绝级规则只记 agent_run(rejected) + work_event(rejected)，
        // 业务字段不变。人工锁（R_LOCKED）由此在重启后仍挡住 Agent 恢复被纠正的字段。
        Err(rejection) => {
            tx.execute(
                "INSERT INTO agent_run (id, kind, runtime, model_id, prompt_version, input_refs_json, \
                 input_sha256, output_sha256, raw_output, latency_ms, status, error_code, \
                 rejection_rule, created_at, finished_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, 'rejected', ?10, ?11, ?12, ?12)",
                params![
                    run_id,
                    RUN_KIND,
                    runtime.runtime_name(),
                    runtime.model_id(),
                    PROMPT_VERSION,
                    refs,
                    sha,
                    output_sha,
                    raw,
                    rejection.code.as_str(),
                    rejection.rule.as_str(),
                    now_ms
                ],
            )?;
            // 有既有事项记在事项上；否则记在批首记录（source）上。
            let (target_type, target_id, note) = match &matched {
                Some((item_id, _)) => ("work_item", item_id.clone(), None),
                None => ("source", ids[0].clone(), Some(rejection.message.clone())),
            };
            insert_event(
                tx,
                &format!("we-{now_ms:x}-rejected-{target_id}"),
                target_type,
                &target_id,
                "rejected",
                "agent",
                now_ms,
                Some(&ids[0]),
                Some(&run_id),
                note,
            )?;
            mark_records(tx, ids, "rejected")?;
            Ok(BatchReport {
                run_id,
                status: "rejected",
                input_sha256: sha.to_string(),
                records: record_ids.to_vec(),
                intent_class: None,
                rejection_rule: Some(rejection.rule.as_str().to_string()),
                matched_by: matched.as_ref().map(|(_, r)| reason_str(*r)),
                item_id: matched.map(|(id, _)| id),
                warnings: vec![rejection.rule.as_str().to_string()],
                skipped_reason: None,
            })
        }
    }
}

fn has_prior_ok(
    tx: &rusqlite::Transaction<'_>,
    runtime: &dyn ClassifyRuntime,
    sha: &str,
) -> Result<bool, JudgeError> {
    let prior: Option<String> = tx
        .query_row(
            "SELECT id FROM agent_run WHERE kind = ?1 AND input_sha256 = ?2 AND prompt_version = ?3 \
             AND IFNULL(model_id,'') = ?4 AND status = 'ok'",
            params![RUN_KIND, sha, PROMPT_VERSION, runtime.model_id()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(prior.is_some())
}

/// `agent_run.id`：同一毫秒内重放同一输入（A11 重试）也不得主键冲突。
fn next_run_id(tx: &rusqlite::Transaction<'_>, now_ms: i64, sha: &str) -> Result<String, JudgeError> {
    let prefix = format!("run-{now_ms:x}-{}", &sha[..12]);
    let n: i64 = tx.query_row(
        "SELECT COUNT(*) FROM agent_run WHERE id LIKE ?1 || '-%'",
        params![prefix],
        |row| row.get(0),
    )?;
    Ok(format!("{prefix}-{n:04}"))
}

/// 事项写入：Create / Update（V10 幂等、V07 人工锁均已由校验器裁决）。
fn apply_judgement(
    tx: &rusqlite::Transaction<'_>,
    judgement: &ValidatedJudgement,
    _batch_pks: &[String],
    now_ms: i64,
    run_id: &str,
) -> Result<Option<String>, JudgeError> {
    // V06：related_hint 只生成候选关联（state=candidate），永不自动归并；
    // 无论本次是否建/改事项都必须落库（非空候选不得丢）。
    for cand in &judgement.similar_candidates {
        if let Some(pk) = record_pk(tx, &cand.evidence_record_id)? {
            insert_link(tx, &pk, &cand.target_item_id, "similar_candidate", "candidate", None, now_ms)?;
        }
    }
    let Some(decision) = &judgement.item else {
        // info/noise/thread_update/followup+no：不建不改事项（§3.4）。
        return Ok(None);
    };
    let origin_pk = record_pk(tx, &judgement.origin_external_id)?;
    let ignore_reason = || {
        decision
            .ignore_reason
            .map(|r| r.as_str().to_string())
            .unwrap_or_else(|| "no_action".to_string())
    };
    let confidence = i64::from(decision.confidence);

    let (item_id, is_create) = match &decision.mode {
        WriteMode::Create => {
            // dedupe_key 唯一兜底（V10）：撞键即走更新路径，不建重复事项。
            let existing_id: Option<String> = tx
                .query_row(
                    "SELECT id FROM work_item WHERE dedupe_key = ?1",
                    params![judgement.dedupe_key],
                    |row| row.get(0),
                )
                .optional()?;
            match existing_id {
                Some(existing) => (existing, false),
                None => {
                    let id = format!("item-{}", &judgement.dedupe_key[..12]);
                    let (closed_at, close_reason) = match decision.lifecycle {
                        workcontext_core::Lifecycle::Ignored => (Some(now_ms), Some(ignore_reason())),
                        _ => (None, None),
                    };
                    tx.execute(
                        "INSERT INTO work_item (id, title, lifecycle, attention, next_action, \
                         followup_type, followup_at, followup_note, origin_source_record_id, \
                         dedupe_key, confidence, awaiting_confirmation, closed_at, close_reason, \
                         resolution_evidence, last_evidence_at, created_at, updated_at) \
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?17)",
                        params![
                            id,
                            decision.title,
                            decision.lifecycle.as_str(),
                            decision.attention.as_str(),
                            decision.next_action,
                            decision.followup_type.as_str(),
                            decision.followup_at,
                            decision.followup_note,
                            origin_pk,
                            judgement.dedupe_key,
                            confidence,
                            decision.awaiting_confirmation,
                            closed_at,
                            close_reason,
                            decision.resolution.as_ref().map(|r| r.evidence_type.as_str()),
                            decision.last_evidence_at,
                            now_ms
                        ],
                    )?;
                    (id, true)
                }
            }
        }
        WriteMode::Update(id) => (id.clone(), false),
    };

    if is_create {
        // 创建批全部证据 → origin 链。
        for ext in &judgement.evidence_ids {
            if let Some(pk) = record_pk(tx, ext)? {
                insert_link(tx, &pk, &item_id, "origin", "confirmed", Some(confidence), now_ms)?;
            }
        }
        insert_event(
            tx,
            &format!("we-{now_ms:x}-created-{item_id}"),
            "work_item",
            &item_id,
            "created",
            "agent",
            now_ms,
            origin_pk.as_deref(),
            Some(run_id),
            None,
        )?;
        return Ok(Some(item_id));
    }

    // 更新路径（含 dedupe 撞键）：只写校验器给出的决策字段；origin 不变。
    let Some(row) = load_item(tx, &item_id)? else { return Ok(None) };
    let (closed_at, close_reason) = match decision.lifecycle {
        workcontext_core::Lifecycle::Ignored => (Some(now_ms), Some(ignore_reason())),
        _ => (row.closed_at, row.close_reason),
    };
    tx.execute(
        "UPDATE work_item SET title = ?2, lifecycle = ?3, attention = ?4, next_action = ?5, \
         followup_type = ?6, followup_at = ?7, followup_note = ?8, awaiting_confirmation = ?9, \
         confidence = ?10, closed_at = ?11, close_reason = ?12, resolution_evidence = ?13, \
         last_evidence_at = ?14, updated_at = ?15 WHERE id = ?1",
        params![
            item_id,
            decision.title,
            decision.lifecycle.as_str(),
            decision.attention.as_str(),
            decision.next_action,
            decision.followup_type.as_str(),
            decision.followup_at,
            decision.followup_note,
            decision.awaiting_confirmation,
            confidence,
            closed_at,
            close_reason,
            decision.resolution.as_ref().map(|r| r.evidence_type.as_str()),
            decision.last_evidence_at,
            now_ms
        ],
    )?;
    // 更新批新证据并入证据链（INSERT OR IGNORE 幂等）。
    for ext in &judgement.evidence_ids {
        if let Some(pk) = record_pk(tx, ext)? {
            insert_link(tx, &pk, &item_id, "reference", "confirmed", Some(confidence), now_ms)?;
        }
    }
    insert_event(
        tx,
        &format!("we-{now_ms:x}-updated-{item_id}"),
        "work_item",
        &item_id,
        "updated",
        "agent",
        now_ms,
        row.origin_source_record_id.as_deref(),
        Some(run_id),
        None,
    )?;
    Ok(Some(item_id))
}

/// 既有事项匹配（与回放器 `CaseState::find_matching` 同语义）：
/// ① 批内记录已在某事项证据链（origin/reference）；② 批内记录的
/// `reply_to_id`（同源且同会话才有效）指向事项证据链上的记录。
/// 「同会话但无引用」一律不命中 → 新建，避免把同会话的多个独立事项误合并。
fn match_item(
    tx: &rusqlite::Transaction<'_>,
    batch_pks: &[String],
) -> Result<Option<(String, MatchReason)>, JudgeError> {
    let item_of = |pk: &str| -> Result<Option<String>, JudgeError> {
        Ok(tx
            .query_row(
                "SELECT le.target_id FROM link_evidence le \
                 JOIN work_item wi ON wi.id = le.target_id \
                 WHERE le.source_record_id = ?1 AND le.target_type = 'work_item' \
                 AND le.link_type IN ('origin','reference') AND le.state <> 'rejected' \
                 ORDER BY le.created_at DESC, le.rowid DESC LIMIT 1",
                params![pk],
                |row| row.get(0),
            )
            .optional()?)
    };
    // ① 证据链直连。
    for pk in batch_pks {
        if let Some(item_id) = item_of(pk)? {
            return Ok(Some((item_id, MatchReason::Evidence)));
        }
    }
    // ② reply_to 显式续链。
    let mut stmt = tx.prepare(
        "SELECT reply_to_id, source_id, conversation_id FROM source_record \
         WHERE id = ?1 AND reply_to_id IS NOT NULL",
    )?;
    let mut links: Vec<(String, String, Option<String>)> = Vec::new();
    for pk in batch_pks {
        if let Some(link) = stmt
            .query_row(params![pk], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .optional()?
        {
            links.push(link);
        }
    }
    for (reply_to, source_id, conversation_id) in links {
        let target: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT id, conversation_id FROM source_record WHERE source_id = ?1 AND external_id = ?2",
                params![source_id, reply_to],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((target_pk, target_conv)) = target else { continue };
        if target_conv != conversation_id {
            continue; // 跨会话引用无效
        }
        if let Some(item_id) = item_of(&target_pk)? {
            return Ok(Some((item_id, MatchReason::ReplyTo)));
        }
    }
    Ok(None)
}

/// Stable archive ID plus the raw DingTalk ID used by older scoped sync.
pub struct ArchiveMatch<'a> {
    pub conversation_id: &'a str,
    pub message: (&'a str, &'a str),
    /// Only present when ingestion verified the target belongs to this conversation.
    pub reply: Option<(&'a str, &'a str)>,
    pub requires_existing: bool,
}

/// Before spending a call, defer progress-only messages without an existing
/// thread and skip evidence older than the last applied message.
pub fn archive_can_judge(
    conn: &Connection, keys: &ArchiveMatch<'_>, occurred_at: i64,
) -> Result<bool, JudgeError> {
    let Some((id, _)) = match_archive_item(conn, keys)? else {
        return Ok(!keys.requires_existing);
    };
    let latest: Option<i64> = conn.query_row(
        "SELECT last_evidence_at FROM work_item WHERE id=?1", [id], |row| row.get(0),
    )?;
    Ok(latest.is_none_or(|at| at <= occurred_at))
}

fn match_archive_item(
    conn: &Connection, keys: &ArchiveMatch<'_>,
) -> Result<Option<(String, MatchReason)>, JudgeError> {
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT le.target_id FROM source_record sr \
         JOIN source src ON src.id=sr.source_id \
         JOIN link_evidence le ON le.source_record_id=sr.id \
         JOIN work_item wi ON wi.id=le.target_id \
         WHERE sr.conversation_id=?1 AND (sr.id=?2 OR (src.kind='dws_im' AND sr.external_id=?3)) \
         AND sr.kind='im_message' AND le.target_type='work_item' \
         AND le.link_type IN ('origin','reference') AND le.state<>'rejected' LIMIT 2",
    )?;
    for (reason, key) in [(MatchReason::Evidence, Some(keys.message)), (MatchReason::ReplyTo, keys.reply)] {
        let Some((archived, raw)) = key else { continue };
        let mut rows = stmt.query(params![keys.conversation_id, archived, raw])?;
        if let Some(row) = rows.next()? {
            let id = row.get(0)?;
            if rows.next()?.is_some() {
                return Err(JudgeError::ArchiveAmbiguous);
            }
            return Ok(Some((id, reason)));
        }
    }
    Ok(None)
}

#[derive(Clone, Copy)]
enum MatchReason {
    Evidence,
    ReplyTo,
}

fn reason_str(reason: MatchReason) -> &'static str {
    match reason {
        MatchReason::Evidence => "evidence",
        MatchReason::ReplyTo => "reply_to",
    }
}

/// 批内记录定位到 `source_record` 主键；缺失即 fail-loud。
fn locate_records(
    tx: &rusqlite::Transaction<'_>,
    contexts: &[RecordContext],
    record_ids: &[String],
) -> Result<Vec<String>, JudgeError> {
    let source_id = &contexts[0].source_id;
    let mut ids = Vec::with_capacity(record_ids.len());
    for external_id in record_ids {
        let row_id: Option<String> = tx
            .query_row(
                "SELECT id FROM source_record WHERE source_id = ?1 AND external_id = ?2",
                params![source_id, external_id],
                |row| row.get(0),
            )
            .optional()?;
        match row_id {
            Some(id) => ids.push(id),
            None => {
                return Err(JudgeError::RecordMissing {
                    external_id: external_id.clone(),
                })
            }
        }
    }
    Ok(ids)
}

/// 判读上下文里的记录 ID → `source_record` 主键（证据必须真实入库才落链）。
fn record_pk(tx: &rusqlite::Transaction<'_>, external_id: &str) -> Result<Option<String>, JudgeError> {
    Ok(tx
        .query_row(
            "SELECT id FROM source_record WHERE external_id = ?1 ORDER BY ingested_at DESC LIMIT 1",
            params![external_id],
            |row| row.get(0),
        )
        .optional()?)
}

#[allow(clippy::too_many_arguments)]
fn insert_link(
    tx: &rusqlite::Transaction<'_>,
    record_pk: &str,
    item_id: &str,
    link_type: &str,
    state: &str,
    confidence: Option<i64>,
    now_ms: i64,
) -> Result<(), JudgeError> {
    tx.execute(
        "INSERT OR IGNORE INTO link_evidence \
         (id, source_record_id, target_type, target_id, link_type, confidence, state, created_at) \
         VALUES (?1, ?2, 'work_item', ?3, ?4, ?5, ?6, ?7)",
        params![
            format!("{record_pk}:{item_id}:{link_type}"),
            record_pk,
            item_id,
            link_type,
            confidence,
            state,
            now_ms
        ],
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn insert_event(
    tx: &rusqlite::Transaction<'_>,
    id_prefix: &str,
    target_type: &str,
    target_id: &str,
    event_type: &str,
    actor: &str,
    now_ms: i64,
    source_record_id: Option<&str>,
    run_id: Option<&str>,
    note: Option<String>,
) -> Result<(), JudgeError> {
    let id = crate::items::next_event_id(tx, id_prefix)?;
    tx.execute(
        "INSERT INTO work_event (id, target_type, target_id, event_type, actor, at, \
         source_record_id, run_id, note) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![id, target_type, target_id, event_type, actor, now_ms, source_record_id, run_id, note],
    )?;
    Ok(())
}

fn mark_records(
    tx: &rusqlite::Transaction<'_>,
    pks: &[String],
    state: &str,
) -> Result<(), JudgeError> {
    let placeholders: Vec<String> = (1..=pks.len()).map(|i| format!("?{i}")).collect();
    let sql = format!(
        "UPDATE source_record SET judge_state = ?{} WHERE id IN ({})",
        pks.len() + 1,
        placeholders.join(",")
    );
    let mut values: Vec<&dyn rusqlite::ToSql> =
        pks.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
    values.push(&state);
    tx.execute(&sql, values.as_slice())?;
    Ok(())
}

/// `agent_run.input_refs_json`：`sourceRecordIds` 为 `source_record` 真实主键，
/// `externalIds` 为判读上下文口径；两种带引号 token 供快照/审计文本匹配。
fn input_refs_json(source_record_ids: &[String], external_ids: &[String]) -> String {
    json!({ "sourceRecordIds": source_record_ids, "externalIds": external_ids }).to_string()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

// ========================= 开发用例：合成夹具 → 临时库 =========================

/// 一个计划好的判读批次（与 mock 夹具同哈希，可确定性回放）。
#[derive(Clone)]
pub struct PlannedBatch {
    pub stage_index: u32,
    pub run: u32,
    pub records: Vec<String>,
    pub input: ClassifyInput,
    pub contexts: Vec<RecordContext>,
}

/// 合成夹具导入 + 批次规划结果。
pub struct PlannedCase {
    pub me: MeContext,
    pub batches: Vec<PlannedBatch>,
}

/// 规划一个 case 的判读批次（manifest 为准；组装哈希必须与清单一致，
/// 否则 fail-loud——未知输入不造答案）。
pub fn plan_case(fixtures_root: &Path, case_id: &str) -> Result<PlannedCase, JudgeError> {
    let case_dir = fixture_case::find_case_dir(&fixtures_root.join("cases"), case_id)?;
    let case = fixture_case::load_case_input(&case_dir)?;
    let me = case_me(&case)?;
    let manifest = read_manifest(fixtures_root)?;
    let runs: Vec<Value> = manifest
        .get("runs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut batches = Vec::new();
    for run in &runs {
        let run_case = run.get("case_id").and_then(Value::as_str).unwrap_or_default();
        if !case_id.starts_with(run_case) {
            continue;
        }
        let records: Vec<String> = run
            .get("records")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        if records.is_empty() {
            continue;
        }
        let ids: Vec<&str> = records.iter().map(String::as_str).collect();
        let input = fixture_case::build_classify_input(&case, &ids)?;
        let actual = classify_input::input_sha256(&input);
        let declared = run.get("input_sha256").and_then(Value::as_str).unwrap_or_default();
        if actual != declared {
            return Err(JudgeError::HashMismatch {
                manifest: declared.to_string(),
                actual,
            });
        }
        let contexts: Vec<RecordContext> = records
            .iter()
            .map(|id| case_record_context(&case, id))
            .collect::<Result<_, _>>()?;
        batches.push(PlannedBatch {
            stage_index: u32::try_from(
                run.get("stage_index").and_then(Value::as_i64).unwrap_or_default(),
            )
            .unwrap_or_default(),
            run: u32::try_from(run.get("run").and_then(Value::as_i64).unwrap_or_default())
                .unwrap_or_default(),
            records,
            input,
            contexts,
        });
    }
    batches.sort_by_key(|b| (b.stage_index, b.run));
    Ok(PlannedCase { me, batches })
}

/// 执行一个计划批次（与生产同一事务路径）。
pub fn judge_planned_batch(
    conn: &mut Connection,
    runtime: &dyn ClassifyRuntime,
    validator: &PolicyValidator,
    planned: &PlannedCase,
    batch: &PlannedBatch,
    now_ms: i64,
) -> Result<BatchReport, JudgeError> {
    judge_records(
        conn,
        runtime,
        validator,
        &planned.me,
        &batch.input,
        &batch.contexts,
        &batch.records,
        now_ms,
    )
}

/// 把合成 case 记录导入目标库（**仅开发/测试：目标应是临时库**）。
///
/// 写入路径与同步引擎一致：source（幂等）+ source_record（预筛落
/// `judge_state='kept'|'skipped'` + 0002 判读元数据列）+ `work_event(synced)`
/// 留档 reply_to 元数据。不碰默认数据库，不写假同步游标。
pub fn import_case(
    conn: &Connection,
    fixtures_root: &Path,
    case_id: &str,
    now_ms: i64,
) -> Result<ImportSummary, JudgeError> {
    let case_dir = fixture_case::find_case_dir(&fixtures_root.join("cases"), case_id)?;
    let case = fixture_case::load_case_input(&case_dir)?;
    let me = case_me(&case)?;
    let conversations = case.get("conversations").cloned().unwrap_or(Value::Null);
    let sources = case.get("sources").cloned().unwrap_or(Value::Null);

    // source 幂等导入。
    if let Some(arr) = sources.as_array() {
        for s in arr {
            let id = s.get("id").and_then(Value::as_str).unwrap_or_default();
            let kind = s.get("kind").and_then(Value::as_str).unwrap_or("import");
            let name = s.get("display_name").and_then(Value::as_str).unwrap_or(id);
            conn.execute(
                "INSERT OR IGNORE INTO source (id, kind, display_name, scope_json, enabled, \
                 authorized_at, created_at) VALUES (?1, ?2, ?3, ?4, 1, NULL, ?5)",
                params![id, kind, name, s.get("scope").map(|v| v.to_string()).unwrap_or_else(|| "[]".to_string()), now_ms],
            )?;
        }
    }

    let mut inserted = 0usize;
    let mut kept = 0usize;
    let mut skipped = 0usize;
    for stage in case.get("stages").and_then(Value::as_array).unwrap_or(&Vec::new()).clone() {
        for record in stage.get("records").and_then(Value::as_array).cloned().unwrap_or_default() {
            let external_id = record.get("external_id").and_then(Value::as_str).unwrap_or_default();
            let source_id = record.get("source_id").and_then(Value::as_str).unwrap_or_default();
            let conversation_id = record.get("conversation_id").and_then(Value::as_str);
            let conv_type = conversation_id
                .and_then(|cid| conversations.get(cid))
                .and_then(|c| c.get("type"))
                .and_then(Value::as_str);
            let mentions: Vec<String> = record
                .get("mentions")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            let reply_to = record
                .get("reply_to")
                .and_then(|r| r.get("external_id"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let occurred = parse_rfc3339(record.get("occurred_at").and_then(Value::as_str))?;
            let ingested = parse_rfc3339(record.get("ingested_at").and_then(Value::as_str))?;

            let verdict = prefilter_record(
                &record,
                &me.user_id,
                me.display_name.as_deref().unwrap_or_default(),
                conv_type.unwrap_or("other"),
            );
            let pk = format!("{source_id}/{external_id}");
            let body = record.get("body").and_then(Value::as_str).unwrap_or_default();
            conn.execute(
                "INSERT INTO source_record (id, source_id, external_id, kind, occurred_at, \
                 ingested_at, conversation_id, sender_id, sender_name, direction, summary, body, \
                 body_sha256, content_hash, raw_state, judge_state, prefilter_reason, \
                 mentions_json, reply_to_id, conversation_type) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,'available',?15,?16,?17,?18,?19) \
                 ON CONFLICT(source_id, external_id) DO NOTHING",
                params![
                    pk,
                    source_id,
                    external_id,
                    record.get("kind").and_then(Value::as_str).unwrap_or("manual_note"),
                    occurred,
                    ingested,
                    conversation_id,
                    record.get("sender_id").and_then(Value::as_str),
                    record.get("sender_name").and_then(Value::as_str),
                    record.get("direction").and_then(Value::as_str).unwrap_or("na"),
                    record.get("summary").and_then(Value::as_str),
                    body,
                    sha256_hex(body.as_bytes()),
                    sha256_hex(format!("{source_id}:{external_id}:{body}").as_bytes()),
                    if verdict.is_keep() { "kept" } else { "skipped" },
                    verdict.reason(),
                    serde_json::to_string(&mentions).expect("Vec<String> 序列化不会失败"),
                    reply_to,
                    conv_type
                ],
            )?;
            inserted += 1;
            if verdict.is_keep() {
                kept += 1;
            } else {
                skipped += 1;
            }
            // 入库留痕（事件流是唯一历史真相；A11 诊断可追溯）。
            conn.execute(
                "INSERT INTO work_event (id, target_type, target_id, event_type, actor, at, after_json) \
                 VALUES (?1,'source',?2,'synced','system',?3,?4)",
                params![
                    format!("we-{ingested:x}-synced-{pk}"),
                    pk,
                    ingested,
                    json!({ "prefilter": verdict.reason() }).to_string()
                ],
            )?;
        }
    }
    Ok(ImportSummary { case_id: case_id.to_string(), inserted_records: inserted, kept, skipped })
}

fn case_me(case: &Value) -> Result<MeContext, JudgeError> {
    let me = case.get("me").ok_or_else(|| JudgeError::BadCase("case 缺 me".into()))?;
    Ok(MeContext {
        user_id: me.get("user_id").and_then(Value::as_str).unwrap_or_default().to_string(),
        display_name: me
            .get("display_name")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// case 记录 → core 判读上下文（V02/V03/V04/V12 输入；与回放链路同构）。
fn case_record_context(case: &Value, external_id: &str) -> Result<RecordContext, JudgeError> {
    let conversations = case.get("conversations").and_then(Value::as_object);
    let mut record = None;
    for stage in case.get("stages").and_then(Value::as_array).into_iter().flatten() {
        for r in stage.get("records").and_then(Value::as_array).into_iter().flatten() {
            if r.get("external_id").and_then(Value::as_str) == Some(external_id) {
                record = Some(r);
            }
        }
    }
    let record = record.ok_or_else(|| {
        JudgeError::BadCase(format!("case 记录 {external_id} 不存在"))
    })?;
    let get = |key: &str| record.get(key).and_then(Value::as_str);
    let conversation_id = get("conversation_id");
    let conv_type_str = conversation_id
        .and_then(|cid| conversations.and_then(|c| c.get(cid)))
        .and_then(|c| c.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("other");
    Ok(RecordContext {
        id: external_id.to_string(),
        source_id: get("source_id").unwrap_or_default().to_string(),
        external_id: external_id.to_string(),
        kind: serde_json::from_value(Value::String(get("kind").unwrap_or("manual_note").into()))
            .map_err(|e| JudgeError::BadCase(format!("kind 解析失败：{e}")))?,
        conversation_id: conversation_id.map(str::to_string),
        conversation_type: serde_json::from_value(Value::String(conv_type_str.into()))
            .map_err(|e| JudgeError::BadCase(format!("conversation type 解析失败：{e}")))?,
        direction: serde_json::from_value(Value::String(get("direction").unwrap_or("na").into()))
            .map_err(|e| JudgeError::BadCase(format!("direction 解析失败：{e}")))?,
        sender_id: get("sender_id").map(str::to_string),
        mentions: record
            .get("mentions")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default(),
        occurred_at: parse_rfc3339(get("occurred_at"))?,
        ingested_at: parse_rfc3339(get("ingested_at"))?,
        summary: get("summary").map(str::to_string),
        body: get("body").map(str::to_string),
    })
}

fn parse_rfc3339(s: Option<&str>) -> Result<i64, JudgeError> {
    let s = s.ok_or_else(|| JudgeError::BadCase("缺少 RFC3339 时间".into()))?;
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|t| t.timestamp_millis())
        .map_err(|e| JudgeError::BadCase(format!("时间 {s} 解析失败：{e}")))
}

fn read_manifest(fixtures_root: &Path) -> Result<Value, JudgeError> {
    let path = fixtures_root.join("mock").join("manifest.json");
    let raw = std::fs::read_to_string(&path).map_err(|source| JudgeError::Io {
        path: path.display().to_string(),
        source,
    })?;
    serde_json::from_str(&raw)
        .map_err(|e| JudgeError::BadCase(format!("manifest.json 解析失败：{e}")))
}
