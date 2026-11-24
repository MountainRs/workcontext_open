//! 跨层业务回放（M2/B2 完成判据：断网回放 A01/A02/A03/A06/A11）。
//!
//! 链路：case `input.json` → 预筛（§6.3）→ 按 `fixtures/mock/manifest.json`
//! 的批次划分组批 → [`MockAgentRuntime`] 精确回放判读 → core
//! [`PolicyValidator`] 校验（V01–V12）→ 内存业务状态（[`CaseState`]）→
//! 人工动作（可信 `human_confirmed` 收尾、纠错锁、重放）。
//!
//! 判读批次以 manifest 为准（其 `batching_rule` 声明与各 case
//! `expected.json` 的 `agent_runs[].records` 一致）；回放结果完全由链路产生，
//! **不读取 `expected.json`**——那是集成测试的断言输入，不是回放输入。
//! manifest 声明的每个批次都必须：重建出清单里的 `input_sha256`、在 mock
//! runtime 中命中，否则报错（未知输入不造答案）。

use crate::model::{CaseState, JudgeState, StoredRecord};
use crate::prefilter::prefilter_record;
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use workcontext_core::{
    Attention, BelongsToMe, Correction, JudgeContext, Lifecycle, MeContext, PolicyValidator,
    RecordContext, RecordKind, ResolutionEvidence, ResolutionEvidenceType, ConversationType,
    Direction,
};
use workcontext_infra::classify_input;
use workcontext_infra::fixture_case::{self, FixtureCaseError};
use workcontext_infra::mock::{MockAgentError, MockAgentRuntime, MockOutcome};

/// 回放失败（夹具缺失 / mock 未覆盖 / 守卫拒绝人工操作等，全部 fail-loud）。
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("case 夹具问题：{0}")]
    Fixture(#[from] FixtureCaseError),
    #[error("mock runtime 问题：{0}")]
    Mock(#[from] MockAgentError),
    #[error("夹具 JSON 读取失败 {path}：{source}")]
    LoadJson { path: PathBuf, source: serde_json::Error },
    #[error("case 夹具结构不符合预期 {path}：{reason}")]
    BadCase { path: PathBuf, reason: String },
    #[error("判读批次 {stage} 阶段 run{run} 在 mock runtime 中无匹配夹具（input_sha256={sha}）；未知输入不造答案")]
    UnknownJudgement { stage: u32, run: u32, sha: String },
    #[error("判读批次 {stage} 阶段 run{run} 重建的 input_sha256 与 manifest 不一致：manifest={manifest} 实际={actual}")]
    HashMismatch { stage: u32, run: u32, manifest: String, actual: String },
    #[error("人工动作失败（阶段 {stage}）：{op}：{reason}")]
    HumanAction { stage: u32, op: String, reason: String },
    #[error("不支持的人工动作（阶段 {stage}）：{op}")]
    UnknownHumanOp { stage: u32, op: String },
    #[error("阶段 {stage} 的 keep 记录 {record} 始终未被判读（manifest 批次不完整）")]
    KeptButNeverJudged { stage: u32, record: String },
}

/// 一条记录的阶段视图（对应 expected 的 `records[]` 断言）。
#[derive(Debug, Clone, Serialize)]
pub struct RecordView {
    pub external_id: String,
    pub prefilter: &'static str,
    pub prefilter_reason: &'static str,
    pub judge_state: &'static str,
}

/// 一次判读的阶段视图（对应 `agent_runs[]` 断言）。
#[derive(Debug, Clone, Serialize)]
pub struct RunView {
    pub run: u32,
    pub input_sha256: String,
    pub records: Vec<String>,
    pub status: &'static str,
    pub intent_class: Option<String>,
    pub raw_belongs_to_me: Option<String>,
    pub belongs_to_me: Option<String>,
    pub needs_action: Option<bool>,
    pub followup_type: Option<String>,
    pub confidence: Option<i64>,
    pub rejection_rule: Option<String>,
    /// 批次与既有事项的命中依据（evidence=批内记录在证据链上；reply_to=显式
    /// 引用（同源同会话）指向证据链；None=新建）。绝不按会话隐式合并。
    pub matched_by: Option<&'static str>,
    pub warnings: Vec<String>,
}

/// 一个事项的阶段视图（对应 `items[]` / `final.items[]` 断言）。
#[derive(Debug, Clone, Serialize)]
pub struct ItemView {
    pub id: String,
    pub origin_record: String,
    pub origin_intent_class: String,
    pub title: String,
    pub lifecycle: &'static str,
    pub attention: &'static str,
    pub current_turn: &'static str,
    pub next_action: Option<String>,
    pub followup_type: &'static str,
    pub followup_at: Option<i64>,
    pub awaiting_confirmation: bool,
    pub ignore_reason: Option<String>,
    pub close_reason: Option<String>,
    pub resolution_evidence: Option<String>,
    pub locked_fields: Vec<String>,
    pub today_visible: bool,
    pub today_group: &'static str,
}

impl ItemView {
    /// 从内存事项派生断言视图（today 分组按 §3.2）。
    fn of(item: &crate::model::WorkItem) -> Self {
        let group = workcontext_core::today_group(
            item.lifecycle,
            item.attention,
            item.awaiting_confirmation,
        );
        ItemView {
            id: item.id.clone(),
            origin_record: item.origin_record_id.clone(),
            origin_intent_class: item.origin_intent_class.clone(),
            title: item.title.clone(),
            lifecycle: item.lifecycle.as_str(),
            attention: item.attention.as_str(),
            current_turn: workcontext_core::current_turn(item.attention).as_str(),
            next_action: item.next_action.clone(),
            followup_type: item.followup_type.as_str(),
            followup_at: item.followup_at,
            awaiting_confirmation: item.awaiting_confirmation,
            ignore_reason: item.ignore_reason.clone(),
            close_reason: item.close_reason.clone(),
            resolution_evidence: item.resolution_evidence.clone(),
            locked_fields: item
                .locked_fields
                .iter()
                .map(|l| l.field.as_str().to_string())
                .collect(),
            today_visible: !matches!(group, workcontext_core::TodayGroup::Hidden),
            today_group: today_group_str(group),
        }
    }
}

/// 一条 work_event 的视图。
#[derive(Debug, Clone, Serialize)]
pub struct EventView {
    pub event_type: &'static str,
    pub actor: &'static str,
}

/// 一条 field_override 的视图。
#[derive(Debug, Clone, Serialize)]
pub struct OverrideView {
    pub field: String,
    pub value_json: Option<String>,
    pub locked: bool,
}

/// 单个阶段结束时的快照。
#[derive(Debug, Clone, Serialize)]
pub struct StageSnapshot {
    pub index: u32,
    pub records: Vec<RecordView>,
    pub agent_runs: Vec<RunView>,
    pub items: Vec<ItemView>,
    pub item_count: usize,
    pub work_events: Vec<EventView>,
    pub field_overrides: Vec<OverrideView>,
}

/// 整个 case 结束时的快照。
#[derive(Debug, Clone, Serialize)]
pub struct FinalSnapshot {
    pub item_count: usize,
    pub counts: Counts,
    pub items: Vec<ItemView>,
    pub work_events: Vec<EventView>,
    pub field_overrides: Vec<OverrideView>,
}

impl FinalSnapshot {
    /// 按断言键取期末计数（active/candidate/closed/ignored；未知键 0）。
    pub fn counts_for(&self, key: &str) -> usize {
        match key {
            "active" => self.counts.active,
            "candidate" => self.counts.candidate,
            "closed" => self.counts.closed,
            "ignored" => self.counts.ignored,
            _ => 0,
        }
    }
}

/// 按生命周期的期末计数（对应 expected `final.counts`）。
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Counts {
    pub active: usize,
    pub candidate: usize,
    pub closed: usize,
    pub ignored: usize,
}

/// `TodayGroup` → 断言口径字符串（core 未提供 as_str）。
fn today_group_str(group: workcontext_core::TodayGroup) -> &'static str {
    match group {
        workcontext_core::TodayGroup::Me => "me",
        workcontext_core::TodayGroup::WaitingOther => "waiting_other",
        workcontext_core::TodayGroup::Confirm => "confirm",
        workcontext_core::TodayGroup::Hidden => "hidden",
    }
}

/// `lifecycle` / `attention` 的夹具取值解析（case patch 用）。
fn parse_lifecycle(s: &str) -> Option<Lifecycle> {
    match s {
        "candidate" => Some(Lifecycle::Candidate),
        "active" => Some(Lifecycle::Active),
        "closed" => Some(Lifecycle::Closed),
        "ignored" => Some(Lifecycle::Ignored),
        _ => None,
    }
}

fn parse_attention(s: &str) -> Option<Attention> {
    match s {
        "me" => Some(Attention::Me),
        "waiting_other" => Some(Attention::WaitingOther),
        "report_back" => Some(Attention::ReportBack),
        "awaiting_acceptance" => Some(Attention::AwaitingAcceptance),
        "no_next" => Some(Attention::NoNext),
        _ => None,
    }
}

/// 整个 case 的回放结果。
#[derive(Debug, Clone, Serialize)]
pub struct ReplayOutcome {
    pub case_id: String,
    pub stages: Vec<StageSnapshot>,
    pub final_snapshot: FinalSnapshot,
}

/// manifest 里的一个判读批次。
#[derive(Debug, Clone)]
struct ManifestRun {
    stage_index: u32,
    run: u32,
    records: Vec<String>,
    input_sha256: String,
}

/// 回放一个 case（A01/A02/A03/A06/A11）。
///
/// `fixtures_root` 指向仓库 `fixtures/` 目录（含 `cases/` 与 `mock/`）。
pub fn replay_case(fixtures_root: &Path, case_id: &str) -> Result<ReplayOutcome, ReplayError> {
    let cases_root = fixtures_root.join("cases");
    let case_dir = fixture_case::find_case_dir(&cases_root, case_id)?;
    let case = fixture_case::load_case_input(&case_dir)?;
    let runtime = MockAgentRuntime::open(fixtures_root)?;
    let runs = load_manifest_runs(fixtures_root, case_id)?;
    let validator = PolicyValidator::new();
    let me = parse_me(&case, &case_dir)?;
    let engine = CaseReplay {
        case: &case,
        case_dir: &case_dir,
        me: &me,
        runtime: &runtime,
        validator: &validator,
    };

    let mut state = CaseState::new();
    let mut records: Vec<StoredRecord> = Vec::new();
    let mut stage_snapshots = Vec::new();

    let stages = case
        .get("stages")
        .and_then(Value::as_array)
        .ok_or_else(|| bad_case(&case_dir, "stages 必须是非空数组"))?;

    for stage in stages {
        let index = u32::try_from(
            stage
                .get("index")
                .and_then(Value::as_i64)
                .ok_or_else(|| bad_case(&case_dir, "stage.index 必须是整数"))?,
        )
        .map_err(|_| bad_case(&case_dir, "stage.index 溢出"))?;

        // ---- ① 入库 + 预筛 ----
        let stage_records = stage
            .get("records")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut ingested_ids = Vec::new();
        for record in &stage_records {
            let stored = ingest_record(&case, record, &me, &case_dir)?;
            ingested_ids.push(stored.external_id.clone());
            records.push(stored);
        }

        // ---- ② manifest 批次判读：mock → 校验 → 内存状态 ----
        let mut stage_events = Vec::new();
        let mut run_views = Vec::new();
        for manifest_run in runs.iter().filter(|r| r.stage_index == index) {
            let view = engine.judge_batch(manifest_run, &mut records, &mut state)?;
            stage_events.extend(view.1);
            run_views.push(view.0);
        }
        // manifest 批次未覆盖阶段内的 keep 记录 = 批次清单不完整，fail-loud。
        for id in &ingested_ids {
            let rec = records
                .iter()
                .rfind(|r| r.external_id == *id)
                .expect("本阶段刚入库");
            if rec.prefilter.is_keep() && rec.judge_state == JudgeState::Kept {
                return Err(ReplayError::KeptButNeverJudged {
                    stage: index,
                    record: id.clone(),
                });
            }
        }

        // ---- ③ 人工动作（可信人类操作）----
        for action in stage
            .get("human_actions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let events = apply_human_action(&action, &mut state, &runs, index)?;
            stage_events.extend(events);
        }

        stage_snapshots.push(snapshot_stage(index, &records, &run_views, &state, &stage_events));
    }

    let final_snapshot = snapshot_final(&state);
    Ok(ReplayOutcome {
        case_id: case_id.to_string(),
        stages: stage_snapshots,
        final_snapshot,
    })
}

/// 单 case 回放的不变上下文：夹具元数据 + 判读 runtime + 校验器。
/// 可变状态（records / state）由方法参数显式传入，避免长参数列表。
struct CaseReplay<'a> {
    case: &'a Value,
    case_dir: &'a Path,
    me: &'a MeContext,
    runtime: &'a MockAgentRuntime,
    validator: &'a PolicyValidator,
}

impl<'a> CaseReplay<'a> {
    /// 一次判读批次的完整处理，返回 (run 视图, 本批写入的 work_event)。
    #[allow(clippy::type_complexity)]
    fn judge_batch(
        &self,
        manifest_run: &ManifestRun,
        records: &mut [StoredRecord],
        state: &mut CaseState,
    ) -> Result<(RunView, Vec<EventView>), ReplayError> {
        let stage_index = manifest_run.stage_index;
    let ids: Vec<&str> = manifest_run
        .records
        .iter()
        .map(String::as_str)
        .collect();
    let input = fixture_case::build_classify_input(self.case, &ids)?;
    let sha = classify_input::input_sha256(&input);
    if sha != manifest_run.input_sha256 {
        return Err(ReplayError::HashMismatch {
            stage: stage_index,
            run: manifest_run.run,
            manifest: manifest_run.input_sha256.clone(),
            actual: sha,
        });
    }
    let output = match self.runtime.classify(&input) {
        MockOutcome::Replay { output, .. } => output,
        MockOutcome::Unknown { input_sha256 } => {
            return Err(ReplayError::UnknownJudgement {
                stage: stage_index,
                run: manifest_run.run,
                sha: input_sha256,
            })
        }
    };

    // 批内记录的判读上下文（core 类型）。
    let contexts: Vec<RecordContext> = manifest_run
        .records
        .iter()
        .map(|id| {
            records
                .iter()
                .rfind(|r| r.external_id == *id)
                .map(|r| r.context.clone())
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            bad_case(
                self.case_dir,
                &format!("判读批次引用了未入库记录：{:?}", manifest_run.records),
            )
        })?;

    // 既有事项匹配：只认显式证据关联（批内记录 ∈ 证据链，或 reply_to 同源
    // 同会话指向证据链）；绝不以会话作隐式线程。
    let batch_records: Vec<&StoredRecord> = manifest_run
        .records
        .iter()
        .map(|id| {
            records
                .iter()
                .rfind(|r| r.external_id == *id)
                .expect("上面已校验批内记录均已入库")
        })
        .collect();
    let matched = state.find_matching(&batch_records, records);
    let existing = matched.map(|(idx, _)| state.items[idx].to_existing());
    let ctx = JudgeContext::new(&contexts, self.me).with_existing(existing.as_ref());

    let raw = serde_json::to_string(&output).expect("Value 序列化不会失败");
    let raw_belongs = output
        .get("belongs_to_me")
        .and_then(Value::as_str)
        .map(str::to_string);
    let needs_action = output.get("needs_action").and_then(Value::as_bool);
    let confidence = output.get("confidence").and_then(Value::as_i64);

    let decision = self.validator.validate(&raw, &ctx);
    let mut events = Vec::new();
    let mut view = RunView {
        run: manifest_run.run,
        input_sha256: sha,
        records: manifest_run.records.clone(),
        status: "ok",
        intent_class: None,
        raw_belongs_to_me: raw_belongs,
        belongs_to_me: None,
        needs_action,
        followup_type: None,
        confidence,
        rejection_rule: None,
        matched_by: matched.map(|(_, reason)| reason.as_str()),
        warnings: Vec::new(),
    };
    match decision {
        Ok(judgement) => {
            view.intent_class = Some(judgement.intent_class.as_str().to_string());
            view.belongs_to_me = Some(belongs_str(judgement.belongs_to_me).to_string());
            view.followup_type = judgement
                .item
                .as_ref()
                .map(|item| item.followup_type.as_str().to_string());
            view.warnings = judgement
                .warnings
                .iter()
                .map(|w| w.code.as_str().to_string())
                .collect();
            for event in state.apply_judgement(&judgement) {
                events.push(event_view(&event));
            }
            mark_records(records, &manifest_run.records, JudgeState::Judged);
        }
        Err(rejection) => {
            view.status = "rejected";
            view.rejection_rule = Some(rejection.rule.as_str().to_string());
            view.warnings.push(rejection.rule.as_str().to_string());
            events.push(EventView {
                event_type: "rejected",
                actor: "agent",
            });
            state.record_rejection();
            mark_records(records, &manifest_run.records, JudgeState::Rejected);
        }
    }
    Ok((view, events))
    }
}

/// 单条记录入库：case JSON → 预筛 → core 判读上下文。
fn ingest_record(
    case: &Value,
    record: &Value,
    me: &MeContext,
    case_dir: &Path,
) -> Result<StoredRecord, ReplayError> {
    let external_id = record
        .get("external_id")
        .and_then(Value::as_str)
        .ok_or_else(|| bad_case(case_dir, "record.external_id 必填"))?;
    let conversation_id = record
        .get("conversation_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let conversation = case
        .get("conversations")
        .and_then(Value::as_object)
        .and_then(|cs| cs.get(conversation_id))
        .ok_or_else(|| bad_case(case_dir, &format!("会话不存在：{conversation_id}")))?;
    let conversation_type = conversation.get("type").and_then(Value::as_str).unwrap_or("other");

    let verdict = prefilter_record(record, &me.user_id, me.display_name.as_deref().unwrap_or(""), conversation_type);
    let context = RecordContext {
        id: external_id.to_string(),
        source_id: record
            .get("source_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        external_id: external_id.to_string(),
        kind: RecordKind::ImMessage,
        conversation_id: (!conversation_id.is_empty()).then(|| conversation_id.to_string()),
        conversation_type: match conversation_type {
            "dm" => ConversationType::Dm,
            "group" => ConversationType::Group,
            _ => ConversationType::Other,
        },
        direction: match record.get("direction").and_then(Value::as_str) {
            Some("in") => Direction::In,
            Some("out") => Direction::Out,
            _ => Direction::Na,
        },
        sender_id: record
            .get("sender_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        mentions: record
            .get("mentions")
            .and_then(Value::as_array)
            .map(|ms| {
                ms.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        occurred_at: parse_utc_ms(
            record.get("occurred_at").and_then(Value::as_str),
            case_dir,
            "occurred_at",
        )?,
        ingested_at: parse_utc_ms(
            record.get("ingested_at").and_then(Value::as_str),
            case_dir,
            "ingested_at",
        )?,
        summary: record
            .get("summary")
            .and_then(Value::as_str)
            .map(str::to_string),
        body: record
            .get("body")
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    let judge_state = if verdict.is_keep() {
        JudgeState::Kept
    } else {
        JudgeState::Skipped
    };
    let reply_to = record
        .get("reply_to")
        .filter(|r| !r.is_null())
        .and_then(|r| r.get("external_id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(StoredRecord {
        external_id: external_id.to_string(),
        prefilter: verdict,
        judge_state,
        reply_to,
        context,
    })
}

/// 人工动作：可信收尾 / 纠错锁 / 重放标记。
fn apply_human_action(
    action: &Value,
    state: &mut CaseState,
    runs: &[ManifestRun],
    stage_index: u32,
) -> Result<Vec<EventView>, ReplayError> {
    let op = action
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| ReplayError::BadCase {
            path: PathBuf::new(),
            reason: "human_action.op 必填".to_string(),
        })?;
    let fail = |reason: String| ReplayError::HumanAction {
        stage: stage_index,
        op: op.to_string(),
        reason,
    };
    match op {
        "item_close" => {
            let origin = action
                .get("item_origin_record")
                .and_then(Value::as_str)
                .ok_or_else(|| fail("item_origin_record 必填".to_string()))?;
            let evidence_type = action
                .get("evidence_type")
                .and_then(Value::as_str)
                .ok_or_else(|| fail("evidence_type 必填".to_string()))?;
            let note = action
                .get("note")
                .and_then(Value::as_str)
                .ok_or_else(|| fail("note 必填（close_reason）".to_string()))?;
            let evidence_type = ResolutionEvidenceType::parse(evidence_type)
                .ok_or_else(|| fail(format!("未知收尾证据类型：{evidence_type}")))?;
            let item_id = state
                .item_id_by_origin(origin)
                .ok_or_else(|| fail(format!("找不到源于 {origin} 的事项")))?
                .to_string();
            let evidence = ResolutionEvidence {
                evidence_type,
                evidence_ids: vec![origin.to_string()],
                note: (!note.is_empty()).then(|| note.to_string()),
            };
            let events = state
                .apply_human_close(&item_id, &evidence, note)
                .map_err(fail)?;
            Ok(events.iter().map(event_view).collect())
        }
        "item_correct" => {
            let origin = action
                .get("item_origin_record")
                .and_then(Value::as_str)
                .ok_or_else(|| fail("item_origin_record 必填".to_string()))?;
            let reason = action
                .get("reason")
                .and_then(Value::as_str)
                .ok_or_else(|| fail("reason 必填（§9）".to_string()))?;
            let patch = action
                .get("patch")
                .ok_or_else(|| fail("patch 必填".to_string()))?;
            let correction = Correction {
                belongs_to_me: patch
                    .get("belongs_to_me")
                    .and_then(Value::as_bool),
                lifecycle: patch
                    .get("lifecycle")
                    .and_then(Value::as_str)
                    .map(|s| {
                        parse_lifecycle(s).ok_or_else(|| fail(format!("未知 lifecycle：{s}")))
                    })
                    .transpose()?,
                thread_id: None,
                attention: patch
                    .get("attention")
                    .and_then(Value::as_str)
                    .map(|s| {
                        parse_attention(s).ok_or_else(|| fail(format!("未知 attention：{s}")))
                    })
                    .transpose()?,
                reason: reason.to_string(),
            };
            let item_id = state
                .item_id_by_origin(origin)
                .ok_or_else(|| fail(format!("找不到源于 {origin} 的事项")))?
                .to_string();
            let events = state
                .apply_correction(&item_id, &correction)
                .map_err(fail)?;
            Ok(events.iter().map(event_view).collect())
        }
        "judge_retry" => {
            // 重放由 manifest 批次驱动（identity_rule：同批重跑同 sha）；
            // 这里只校验该阶段确实声明了覆盖这些记录的批次，防止清单缺失。
            let targets = action
                .get("records")
                .and_then(Value::as_array)
                .map(|rs| {
                    rs.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let covered = runs.iter().any(|r| {
                r.stage_index == stage_index
                    && targets
                        .iter()
                        .all(|t| r.records.contains(t))
            });
            if !covered {
                return Err(fail(format!(
                    "manifest 未声明阶段 {stage_index} 覆盖 {targets:?} 的判读批次"
                )));
            }
            Ok(Vec::new())
        }
        other => Err(ReplayError::UnknownHumanOp {
            stage: stage_index,
            op: other.to_string(),
        }),
    }
}

/// manifest 里该 case 的全部批次，按 (stage_index, run) 排序。
fn load_manifest_runs(fixtures_root: &Path, case_id: &str) -> Result<Vec<ManifestRun>, ReplayError> {
    let path = fixtures_root.join("mock").join("manifest.json");
    let manifest: Value = std::fs::read_to_string(&path)
        .map_err(|source| ReplayError::BadCase {
            path: path.clone(),
            reason: format!("manifest 读取失败：{source}"),
        })?
        .parse()
        .map_err(|source| ReplayError::LoadJson { path: path.clone(), source })?;
    let mut runs = Vec::new();
    for entry in manifest
        .get("runs")
        .and_then(Value::as_array)
        .ok_or_else(|| ReplayError::BadCase {
            path: path.clone(),
            reason: "manifest.runs 必须是数组".to_string(),
        })?
    {
        if entry.get("case_id").and_then(Value::as_str) != Some(case_id) {
            continue;
        }
        runs.push(ManifestRun {
            stage_index: entry
                .get("stage_index")
                .and_then(Value::as_i64)
                .ok_or_else(|| bad_case(&path, "run.stage_index 必须是整数"))? as u32,
            run: entry
                .get("run")
                .and_then(Value::as_i64)
                .unwrap_or(1) as u32,
            records: entry
                .get("records")
                .and_then(Value::as_array)
                .map(|rs| {
                    rs.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .ok_or_else(|| bad_case(&path, "run.records 必须是字符串数组"))?,
            input_sha256: entry
                .get("input_sha256")
                .and_then(Value::as_str)
                .ok_or_else(|| bad_case(&path, "run.input_sha256 必填"))?
                .to_string(),
        });
    }
    if runs.is_empty() {
        return Err(ReplayError::BadCase {
            path,
            reason: format!("manifest 未声明 {case_id} 的任何判读批次"),
        });
    }
    runs.sort_by_key(|r| (r.stage_index, r.run));
    Ok(runs)
}

fn parse_me(case: &Value, case_dir: &Path) -> Result<MeContext, ReplayError> {
    let me = case
        .get("me")
        .ok_or_else(|| bad_case(case_dir, "case.me 必填"))?;
    Ok(MeContext {
        user_id: me
            .get("user_id")
            .and_then(Value::as_str)
            .ok_or_else(|| bad_case(case_dir, "me.user_id 必填"))?
            .to_string(),
        display_name: me
            .get("display_name")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn parse_utc_ms(value: Option<&str>, case_dir: &Path, field: &str) -> Result<i64, ReplayError> {
    let raw = value.ok_or_else(|| bad_case(case_dir, &format!("record.{field} 必填")))?;
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.timestamp_millis())
        .map_err(|e| bad_case(case_dir, &format!("record.{field} 不是 RFC3339：{e}")))
}

fn belongs_str(b: BelongsToMe) -> &'static str {
    match b {
        BelongsToMe::Yes => "yes",
        BelongsToMe::No => "no",
        BelongsToMe::Unclear => "unclear",
    }
}

/// 判读落点回写记录状态：已 judged 的记录不因后续重放被拒绝而回退
/// （judge_state 是入库管线状态，重放拒绝记录在 agent_run/work_event）。
fn mark_records(records: &mut [StoredRecord], batch: &[String], state: JudgeState) {
    for record in records.iter_mut() {
        if batch.contains(&record.external_id)
            && !matches!(record.judge_state, JudgeState::Judged | JudgeState::Skipped)
        {
            record.judge_state = state;
        }
    }
}

fn event_view(event: &crate::model::WorkEvent) -> EventView {
    EventView {
        event_type: event.event_type,
        actor: match event.actor {
            workcontext_core::Actor::System => "system",
            workcontext_core::Actor::Agent => "agent",
            workcontext_core::Actor::Human => "human",
        },
    }
}

fn snapshot_stage(
    index: u32,
    records: &[StoredRecord],
    runs: &[RunView],
    state: &CaseState,
    stage_events: &[EventView],
) -> StageSnapshot {
    StageSnapshot {
        index,
        records: records
            .iter()
            .map(|r| RecordView {
                external_id: r.external_id.clone(),
                prefilter: r.prefilter.action(),
                prefilter_reason: r.prefilter.reason(),
                judge_state: r.judge_state.as_str(),
            })
            .collect(),
        agent_runs: runs.to_vec(),
        items: state.items.iter().map(ItemView::of).collect(),
        item_count: state.items.len(),
        work_events: stage_events.to_vec(),
        field_overrides: state.overrides.iter().map(override_view).collect(),
    }
}

fn snapshot_final(state: &CaseState) -> FinalSnapshot {
    let mut counts = Counts::default();
    for item in &state.items {
        match item.lifecycle {
            Lifecycle::Active => counts.active += 1,
            Lifecycle::Candidate => counts.candidate += 1,
            Lifecycle::Closed => counts.closed += 1,
            Lifecycle::Ignored => counts.ignored += 1,
        }
    }
    FinalSnapshot {
        item_count: state.items.len(),
        counts,
        items: state.items.iter().map(ItemView::of).collect(),
        work_events: state.events.iter().map(event_view).collect(),
        field_overrides: state.overrides.iter().map(override_view).collect(),
    }
}

fn override_view(o: &crate::model::FieldOverride) -> OverrideView {
    OverrideView {
        field: o.field.as_str().to_string(),
        value_json: o.value_json.clone(),
        locked: o.locked,
    }
}

fn bad_case(path: &Path, reason: &str) -> ReplayError {
    ReplayError::BadCase {
        path: path.to_path_buf(),
        reason: reason.to_string(),
    }
}
