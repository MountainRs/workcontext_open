//! `PolicyValidator`：判读校验流水线（详细设计 §5），以及判定结果到状态的映射（§3.4）。
//!
//! 流水线：`schema(V01/V11) → 引用(V02) → 归属锚点(V03) → 时间依据(V04) →
//! 收尾门(V05) → 归并门(V06) → 人工锁(V07) → 置信(V08) → 长度(V09) →
//! 幂等(V10) → 时钟(V12)`。任一 **拒绝级** 失败返回 `Err(Rejection)`，
//! 业务字段保持不变；**降级级** 失败（V03/V04/V05/V08 等）不改判读为拒绝，
//! 而是携带 [`Warning`] 继续落地，且都可计数、可测试。
//!
//! 指定记录证据与人工锁优先：`evidence`/`source_ids` 只认 [`JudgeContext::records`]
//! 提供的 ID（V02）；`field_override` 锁定的核心字段（归属/生命周期）被 Agent 改写时
//! 整次拒绝并记 `R_LOCKED`（V07、A11），未锁字段照常更新。

use crate::classify::{
    parse_classify_output, AgentFollowupType, BelongsToMe, Classification, ClassifyOutput,
    ConversationType, Direction, Followup, JudgeContext, MeContext, ParseError, RecordContext,
    Resolution,
};
use crate::state::{
    close_verdict, evaluate_transition, is_locked, locked_bool, Attention, CloseVerdict,
    CurrentTurn, DbFollowupType, IgnoreReason, ItemField, Lifecycle, TransitionOutcome,
    TransitionRequest,
};

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

/// §4.4 幂等键取值：来自**校验后**的结果而非原始输出。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntentClass {
    Info,
    OneOff,
    Followup,
    ThreadUpdate,
    Confirm,
}

impl IntentClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            IntentClass::Info => "info",
            IntentClass::OneOff => "one_off",
            IntentClass::Followup => "followup",
            IntentClass::ThreadUpdate => "thread_update",
            IntentClass::Confirm => "confirm",
        }
    }
}

/// 拒绝级规则 ID。`RLocked` 落库时记为 `R_LOCKED`（§16.2 A11 / §4.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RejectionRule {
    V01,
    V02,
    RLocked,
    V11,
}

impl RejectionRule {
    pub fn as_str(&self) -> &'static str {
        match self {
            RejectionRule::V01 => "V01",
            RejectionRule::V02 => "V02",
            RejectionRule::RLocked => "R_LOCKED",
            RejectionRule::V11 => "V11",
        }
    }
}

/// core 层错误码子集（§18）；store/agent 层错误码不在 core。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    AgentBadJson,
    ValidationRejected,
    LockedField,
}

impl ErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::AgentBadJson => "AGENT_BAD_JSON",
            ErrorCode::ValidationRejected => "VALIDATION_REJECTED",
            ErrorCode::LockedField => "LOCKED_FIELD",
        }
    }
}

/// 降级/告警编码（可计数指标，§5.1「无依据推断」等）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WarningCode {
    /// V03：归属断言缺少锚点，降级 unclear。
    V03OwnershipDowngraded,
    /// V04：时间无依据（计入「无依据的时间推断」指标，R03）。
    R03TimeUnsupported,
    /// V05：收尾证据不在白名单（或模型自称 human_confirmed），降级待确认。
    V05CloseDowngraded,
    /// V07：人工锁跳过 Agent 写入（非核心字段）。
    RLockedFieldSkipped,
    /// V08：置信不足或缺下一步，转 candidate。
    V08LowConfidence,
    /// V09：超长截断。
    V09Truncated,
    /// V12：事件时点时钟异常，按 ingested_at 修正。
    V12ClockClamped,
    /// 更新路径被 §3.3 状态机守卫拒绝，保留既有状态。
    TransitionDenied,
}

impl WarningCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            WarningCode::V03OwnershipDowngraded => "V03",
            WarningCode::R03TimeUnsupported => "R03_TIME_UNSUPPORTED",
            WarningCode::V05CloseDowngraded => "V05",
            WarningCode::RLockedFieldSkipped => "R_LOCKED",
            WarningCode::V08LowConfidence => "V08",
            WarningCode::V09Truncated => "V09",
            WarningCode::V12ClockClamped => "V12",
            WarningCode::TransitionDenied => "TRANSITION_DENIED",
        }
    }
}

/// 一条告警：规则编码 + 人类可读说明（进 agent_run / 诊断指标）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub code: WarningCode,
    pub detail: String,
}

impl Warning {
    pub fn new(code: WarningCode, detail: impl Into<String>) -> Self {
        Warning {
            code,
            detail: detail.into(),
        }
    }
}

/// 拒绝结果：写 `agent_run(status='rejected', rejection_rule)` + `work_event(rejected)`，
/// 业务字段不变（§5）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub rule: RejectionRule,
    pub code: ErrorCode,
    pub message: String,
    /// 首个错误路径（§8.3）。
    pub path: Option<String>,
}

/// 校验后的完整判定。
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedJudgement {
    pub intent_class: IntentClass,
    /// V03 之后的归属（可能是降级后的 unclear）。
    pub belongs_to_me: BelongsToMe,
    /// R08：判读理由（V09 截断后）。
    pub rationale: String,
    /// `None` 表示本次判读不建/不改事项（info、noise、thread_update、followup+no 等）。
    pub item: Option<ItemDecision>,
    /// 通过 V02 的证据记录 ID。
    pub evidence_ids: Vec<String>,
    /// 被 V02 剔除的无效引用（透明化）。
    pub dropped_ids: Vec<String>,
    /// V06：related_hint 只能生成的候选关联。
    pub similar_candidates: Vec<SimilarCandidate>,
    pub warnings: Vec<Warning>,
    /// §4.4：sha256(source_id ':' external_id ':' intent_class)。
    pub dedupe_key: String,
    pub origin_record_id: String,
    pub origin_external_id: String,
    /// origin 记录的会话（应用层做会话内事项匹配，A01 note）。
    pub conversation_id: Option<String>,
}

/// 一次判读对事项的写入决策（§3.4 映射 + 各规则修正后的最终值）。
#[derive(Debug, Clone, PartialEq)]
pub struct ItemDecision {
    pub mode: WriteMode,
    pub title: String,
    pub lifecycle: Lifecycle,
    pub attention: Attention,
    pub next_action: Option<String>,
    pub followup_type: DbFollowupType,
    /// UTC 毫秒；`followup_type=none` 时必为 `None`（§3.1 不变量）。
    pub followup_at: Option<i64>,
    /// V04 的 basis（截断 ≤200），作 followup_note 留痕。
    pub followup_note: Option<String>,
    pub awaiting_confirmation: bool,
    pub confidence: u8,
    /// 仅 lifecycle=ignored 时存在（§3.4 no_action / 人工原因）。
    pub ignore_reason: Option<IgnoreReason>,
    /// V05 通过白名单的收尾提议（Agent 只提议不关单；关单走人工状态转换）。
    pub resolution: Option<AcceptedResolution>,
    /// V10：max(既有, 新证据)。
    pub last_evidence_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteMode {
    Create,
    /// 更新既有事项（dedupe_key 或会话匹配命中）。
    Update(String),
}

/// 通过 V05 白名单的收尾提议。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedResolution {
    pub evidence_type: crate::state::ResolutionEvidenceType,
    pub evidence_ids: Vec<String>,
    pub note: Option<String>,
}

/// V06：related_hint 的唯一合法产物（永不自动置 thread_id，PRD A08）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimilarCandidate {
    pub target_item_id: String,
    pub reason: Option<String>,
    pub evidence_record_id: String,
    /// 恒为 `similar_candidate`。
    pub link_type: &'static str,
}

/// 校验结果：`Ok` 落库 / `Err(Rejection)` 记拒绝事件（§5：拒绝时业务字段不变）。
pub type JudgeDecision = Result<ValidatedJudgement, Rejection>;

/// V03 归属锚点种类（§5.1：①私聊来件 ②@本人 ③本人外发承诺）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnchorKind {
    DmIncoming,
    MentionMe,
    MyOutgoing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorReport {
    pub satisfied: bool,
    pub anchor: Option<AnchorKind>,
    pub record_id: Option<String>,
}

/// §5.1 V03：`belongs_to_me=yes` 需要证据记录满足其一：
/// ① 私聊且 `direction=in`；② `mentions` 含本人（正文出现姓名不算，A06-R3）；
/// ③ 本人 `direction=out` 的消息（「承诺」语义由判读负责，这里做确定性元数据核查）。
pub fn ownership_anchor(records: &[&RecordContext], me: &MeContext) -> AnchorReport {
    let kinds = [
        (
            AnchorKind::DmIncoming,
            Box::new(|r: &RecordContext| {
                r.conversation_type == ConversationType::Dm && r.direction == Direction::In
            }) as Box<dyn Fn(&RecordContext) -> bool>,
        ),
        (
            AnchorKind::MentionMe,
            Box::new(|r: &RecordContext| r.mentions.contains(&me.user_id)),
        ),
        (
            AnchorKind::MyOutgoing,
            Box::new(|r: &RecordContext| r.direction == Direction::Out),
        ),
    ];
    for (kind, matches) in kinds {
        if let Some(r) = records.iter().find(|r| matches(r)) {
            return AnchorReport {
                satisfied: true,
                anchor: Some(kind),
                record_id: Some(r.id.clone()),
            };
        }
    }
    AnchorReport {
        satisfied: false,
        anchor: None,
        record_id: None,
    }
}

/// §4.4 幂等键：sha256(source_id || ':' || external_id || ':' || intent_class)。
pub fn dedupe_key(source_id: &str, external_id: &str, intent: IntentClass) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source_id.as_bytes());
    hasher.update(b":");
    hasher.update(external_id.as_bytes());
    hasher.update(b":");
    hasher.update(intent.as_str().as_bytes());
    hex(&hasher.finalize())
}

/// 判读校验器（§5）。纯内存、无 IO；同一输入永远得到同一结果。
#[derive(Debug, Clone, Copy, Default)]
pub struct PolicyValidator;

impl PolicyValidator {
    pub fn new() -> Self {
        Self
    }

    /// 校验一条 classify/v1 原始输出。`ctx` 提供合法引用全集与人工锁状态。
    pub fn validate(&self, raw_output: &str, ctx: &JudgeContext<'_>) -> JudgeDecision {
        validate_classify(raw_output, ctx)
    }
}

/// V12：occurred_at 晚于 ingested_at + 1h 视为时钟异常。
const CLOCK_SKEW_MS: i64 = 3_600_000;
/// V09 长度上限（字符数，非字节数）。
const TITLE_MAX_CHARS: usize = 60;
const NEXT_ACTION_MAX_CHARS: usize = 200;
const RATIONALE_MAX_CHARS: usize = 500;
const BASIS_MAX_CHARS: usize = 200;

/// 校验入口（流水线顺序见模块文档）。
pub fn validate_classify(raw_output: &str, ctx: &JudgeContext<'_>) -> JudgeDecision {
    // ---- V01 / V11：schema ----
    let output = match parse_classify_output(raw_output) {
        Ok(o) => o,
        Err(ParseError::BadJson(m)) => {
            return reject(
                RejectionRule::V01,
                ErrorCode::AgentBadJson,
                format!("输出不是合法 JSON：{m}"),
                None,
            )
        }
        Err(ParseError::UnknownField { field }) => {
            return reject(
                RejectionRule::V11,
                ErrorCode::ValidationRejected,
                format!("输出含未知字段 {field}（不得含 actions/tool_calls 等副作用字段，会话可能被污染）"),
                Some(field),
            )
        }
        Err(ParseError::Schema { path, message }) => {
            return reject(
                RejectionRule::V01,
                ErrorCode::ValidationRejected,
                message,
                Some(path),
            )
        }
    };
    let mut warnings: Vec<Warning> = Vec::new();

    // ---- V02：引用 ⊆ 本次上下文 ----
    let valid_source: Vec<&str> = output
        .source_ids
        .iter()
        .map(String::as_str)
        .filter(|id| ctx.record(id).is_some())
        .collect();
    if valid_source.is_empty() {
        return reject(
            RejectionRule::V02,
            ErrorCode::ValidationRejected,
            "source_ids 剔除无效 ID 后为空（无一属于本次上下文）".to_string(),
            Some("source_ids".to_string()),
        );
    }
    let valid_evidence: Vec<&RecordContext> =
        output.evidence.iter().filter_map(|id| ctx.record(id)).collect();
    if valid_evidence.is_empty() {
        return reject(
            RejectionRule::V02,
            ErrorCode::ValidationRejected,
            "evidence 剔除无效 ID 后为空（无一属于本次上下文）".to_string(),
            Some("evidence".to_string()),
        );
    }
    let mut dropped_ids: Vec<String> = output
        .source_ids
        .iter()
        .filter(|id| ctx.record(id).is_none())
        .chain(output.evidence.iter().filter(|id| ctx.record(id).is_none()))
        .cloned()
        .collect();
    dropped_ids.sort();
    dropped_ids.dedup();

    // ---- V12：证据时点（先算好，后续 last_evidence / origin 都用修正值）----
    let evidence_times: Vec<i64> = valid_evidence
        .iter()
        .map(|r| clamped_occurred_at(r, &mut warnings))
        .collect();

    // ---- V03：归属锚点（A06 的实现点）----
    let mut belongs = output.belongs_to_me;
    let mut v03_downgraded = false;
    if belongs == BelongsToMe::Yes {
        let anchor = ownership_anchor(&valid_evidence, ctx.me);
        if !anchor.satisfied {
            belongs = BelongsToMe::Unclear;
            v03_downgraded = true;
            warnings.push(Warning::new(
                WarningCode::V03OwnershipDowngraded,
                "belongs_to_me=yes 缺少归属锚点（无私聊来件/@本人/本人外发承诺），降级为 unclear 待确认（V03）",
            ));
        }
    }
    let anchor_satisfied = output.belongs_to_me == BelongsToMe::Yes && !v03_downgraded;

    // ---- V04：时间依据 ----
    let (mut followup_type, mut followup_at, mut followup_note) =
        validate_followup(&output.followup, &valid_evidence, &mut warnings);

    // ---- V05：收尾门 ----
    let (close_pending, accepted_resolution, v05_downgraded) =
        validate_resolution(output.resolution.as_ref(), &mut warnings);

    // ---- §3.4 映射 + V08 ----
    let agent_has_next = output
        .next_action
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    // 更新路径：Agent 未给下一步时，既有下一步继续有效（不允许因缺字段而降级）。
    let existing_has_next = ctx
        .existing
        .and_then(|e| e.next_action.as_ref())
        .is_some_and(|s| !s.trim().is_empty());
    let has_next = agent_has_next || existing_has_next;
    if output.confidence < 60 {
        warnings.push(Warning::new(
            WarningCode::V08LowConfidence,
            format!("confidence {} 低于 60，转 candidate 待确认（V08）", output.confidence),
        ));
    }
    let (intent_class, plan) = map_classification(&output, belongs, close_pending, has_next);

    // ---- V07：人工锁（核心字段冲突 → 整次拒绝；§3.3 末行守卫）----
    if let Some(existing) = ctx.existing {
        // 归属以 Agent 原始断言判冲突（V03 降级不消除锁定冲突）。
        let asserts = match output.belongs_to_me {
            BelongsToMe::Yes => Some(true),
            BelongsToMe::No => Some(false),
            BelongsToMe::Unclear => None,
        };
        if let (Some(locked), Some(says)) =
            (locked_bool(&existing.locked_fields, ItemField::BelongsToMe), asserts)
        {
            if locked != says {
                return reject(
                    RejectionRule::RLocked,
                    ErrorCode::LockedField,
                    format!(
                        "belongs_to_me 已被人工锁定为 {locked}，Agent 断言 {says}，拒绝覆盖（V07/R_LOCKED）"
                    ),
                    Some("belongs_to_me".to_string()),
                );
            }
        }
        if let Some(p) = plan.as_ref() {
            if is_locked(&existing.locked_fields, ItemField::Lifecycle)
                && p.lifecycle != existing.lifecycle
            {
                return reject(
                    RejectionRule::RLocked,
                    ErrorCode::LockedField,
                    format!(
                        "lifecycle 已被人工锁定为 {}，Agent 试图改为 {}，拒绝覆盖（V07/R_LOCKED）",
                        existing.lifecycle, p.lifecycle
                    ),
                    Some("lifecycle".to_string()),
                );
            }
        }
    }

    // ---- 更新路径套 §3.3 状态机守卫（Agent 不得违反状态图）----
    let mut lifecycle = plan.as_ref().map(|p| p.lifecycle);
    let mut attention = plan.as_ref().map(|p| p.attention);
    let mut awaiting = plan.as_ref().is_some_and(|p| p.awaiting) || v05_downgraded;
    let mut ignore_reason = plan.as_ref().and_then(|p| p.ignore_reason);
    if let (Some(existing), Some(p)) = (ctx.existing, plan.as_ref()) {
        if p.lifecycle != existing.lifecycle {
            let req = TransitionRequest {
                from: existing.lifecycle,
                to: p.lifecycle,
                actor: crate::state::Actor::Agent,
                reason: p.ignore_reason.map(|r| r.as_str()),
                new_source_record_id: None,
                resolution: None,
                anchor_satisfied,
                locked_fields: &existing.locked_fields,
            };
            if let TransitionOutcome::Denied(denial) = evaluate_transition(&req) {
                warnings.push(Warning::new(
                    WarningCode::TransitionDenied,
                    format!(
                        "状态机守卫拒绝 {}→{}（agent）：{denial:?}；保留既有状态",
                        existing.lifecycle, p.lifecycle
                    ),
                ));
                lifecycle = Some(existing.lifecycle);
                attention = Some(existing.attention);
                awaiting = existing.awaiting_confirmation || awaiting;
                ignore_reason = None;
            }
        }
    }

    // ---- V09：长度截断（截断不拒绝）----
    let mut next_action: Option<String> =
        match output
            .next_action
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(raw) => {
                let (t, w) = truncate_chars(raw, NEXT_ACTION_MAX_CHARS);
                if w {
                    warnings.push(Warning::new(
                        WarningCode::V09Truncated,
                        format!("next_action 超过 {NEXT_ACTION_MAX_CHARS} 字，已截断"),
                    ));
                }
                Some(t)
            }
            None => None,
        };
    let (rationale, rationale_warned) = truncate_chars(output.rationale.trim(), RATIONALE_MAX_CHARS);
    if rationale_warned {
        warnings.push(Warning::new(
            WarningCode::V09Truncated,
            format!("rationale 超过 {RATIONALE_MAX_CHARS} 字，已截断"),
        ));
    }

    // ---- V07 次级锁：未锁字段照常，已锁字段保留人工值（V07「跳过该字段」语义）----
    if let Some(existing) = ctx.existing {
        // Agent 未给出下一步时保留既有值（不允许因缺字段丢失下一步）。
        if next_action.is_none() {
            next_action = existing.next_action.clone();
        }
        if let Some(att) = attention {
            if is_locked(&existing.locked_fields, ItemField::Attention) && att != existing.attention
            {
                attention = Some(existing.attention);
                warnings.push(Warning::new(
                    WarningCode::RLockedFieldSkipped,
                    format!(
                        "attention 已被人工锁定为 {}，跳过 Agent 写入（V07/R_LOCKED）",
                        existing.attention
                    ),
                ));
            }
        }
        if is_locked(&existing.locked_fields, ItemField::NextAction)
            && next_action != existing.next_action
        {
            next_action = existing.next_action.clone();
            warnings.push(Warning::new(
                WarningCode::RLockedFieldSkipped,
                "next_action 已被人工锁定，跳过 Agent 写入（V07/R_LOCKED）",
            ));
        }
        if is_locked(&existing.locked_fields, ItemField::FollowupAt)
            && (followup_type, followup_at) != (existing.followup_type, existing.followup_at)
        {
            followup_type = existing.followup_type;
            followup_at = existing.followup_at;
            followup_note = None;
            warnings.push(Warning::new(
                WarningCode::RLockedFieldSkipped,
                "followup_at 已被人工锁定，跳过 Agent 写入（V07/R_LOCKED）",
            ));
        }
    }

    // ---- §3.1 不变量兜底：active / attention=me 必须有 next_action ----
    if lifecycle == Some(Lifecycle::Active)
        && next_action
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
    {
        warnings.push(Warning::new(
            WarningCode::V08LowConfidence,
            "active 事项缺 next_action（§3.1 不变量），降级为 candidate 待确认（V08）",
        ));
        lifecycle = Some(Lifecycle::Candidate);
        if attention == Some(Attention::Me) {
            attention = Some(Attention::NoNext);
        }
        awaiting = true;
    }

    // ---- 标题：创建用建议值（V09 截断），更新保持稳定不重写 ----
    let title = match ctx.existing {
        Some(existing) => existing.title.clone(),
        None => {
            let (t, w) = truncate_chars(output.suggested_title.trim(), TITLE_MAX_CHARS);
            if w {
                warnings.push(Warning::new(
                    WarningCode::V09Truncated,
                    format!("suggested_title 超过 {TITLE_MAX_CHARS} 字，已截断"),
                ));
            }
            t
        }
    };

    // ---- V10：幂等与 last_evidence_at 取 max ----
    let new_evidence_at = *evidence_times.iter().max().unwrap_or(&0);
    let (mode, last_evidence_at) = match ctx.existing {
        Some(existing) => (
            WriteMode::Update(existing.id.clone()),
            existing
                .last_evidence_at
                .map_or(new_evidence_at, |prev| prev.max(new_evidence_at)),
        ),
        None => (WriteMode::Create, new_evidence_at),
    };
    let origin = pick_origin(&valid_evidence, &evidence_times);

    // ---- V06：归并门（related_hint 只生成 similar_candidate）----
    let similar_candidates = output
        .related_hint
        .as_ref()
        .map(|hint| {
            hint.target_item_ids
                .iter()
                .map(|id| SimilarCandidate {
                    target_item_id: id.clone(),
                    reason: hint.reason.clone(),
                    evidence_record_id: origin.id.clone(),
                    link_type: "similar_candidate",
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let key = dedupe_key(&origin.source_id, &origin.external_id, intent_class);

    let item = lifecycle.map(|lc| ItemDecision {
        mode,
        title,
        lifecycle: lc,
        attention: attention.unwrap_or(Attention::NoNext),
        next_action,
        followup_type,
        followup_at,
        followup_note,
        awaiting_confirmation: awaiting,
        confidence: output.confidence as u8,
        ignore_reason,
        resolution: accepted_resolution,
        last_evidence_at,
    });

    Ok(ValidatedJudgement {
        intent_class,
        belongs_to_me: belongs,
        rationale,
        item,
        evidence_ids: valid_evidence.iter().map(|r| r.id.clone()).collect(),
        dropped_ids,
        similar_candidates,
        warnings,
        dedupe_key: key,
        origin_record_id: origin.id.clone(),
        origin_external_id: origin.external_id.clone(),
        conversation_id: origin.conversation_id.clone(),
    })
}

fn reject(
    rule: RejectionRule,
    code: ErrorCode,
    message: String,
    path: Option<String>,
) -> JudgeDecision {
    Err(Rejection {
        rule,
        code,
        message,
        path,
    })
}

/// §3.4 映射 + V08。返回 (intent_class, 事项计划)；`None` 表示不建/不改事项。
fn map_classification(
    o: &ClassifyOutput,
    belongs: BelongsToMe,
    close_pending: bool,
    has_next: bool,
) -> (IntentClass, Option<Plan>) {
    let candidate = |awaiting: bool| Plan {
        lifecycle: Lifecycle::Candidate,
        attention: derive_attention(o.current_turn, has_next, close_pending),
        awaiting,
        ignore_reason: None,
    };
    match o.classification {
        // PRD B03/B09：info/noise 只存记录与索引，不建事项。
        Classification::Info | Classification::Noise => (IntentClass::Info, None),
        // V0.1 只写 work_event + link_evidence，不建/不改事项。
        Classification::ThreadUpdate => (IntentClass::ThreadUpdate, None),
        Classification::NeedsConfirmation => (IntentClass::Confirm, Some(candidate(true))),
        Classification::OneOff => (
            IntentClass::OneOff,
            match belongs {
                BelongsToMe::No => None,
                BelongsToMe::Unclear => Some(candidate(true)),
                BelongsToMe::Yes => {
                    if !o.needs_action {
                        // A03：轻事务闭环 → 直接 ignored(no_action)，不得悬挂、不得进待确认。
                        Some(Plan {
                            lifecycle: Lifecycle::Ignored,
                            attention: Attention::NoNext,
                            awaiting: false,
                            ignore_reason: Some(IgnoreReason::NoAction),
                        })
                    } else if o.confidence < 60 {
                        Some(candidate(true))
                    } else {
                        Some(candidate(false))
                    }
                }
            },
        ),
        Classification::Followup => (
            IntentClass::Followup,
            match belongs {
                BelongsToMe::No => None,
                BelongsToMe::Unclear => Some(candidate(true)),
                BelongsToMe::Yes => {
                    if o.confidence < 60 {
                        // V08：低置信 → candidate 待确认。
                        Some(candidate(true))
                    } else if !has_next {
                        // V08：缺 next_action 不得 active（§3.1 不变量）。
                        Some(Plan {
                            lifecycle: Lifecycle::Candidate,
                            attention: derive_attention(o.current_turn, false, close_pending),
                            awaiting: true,
                            ignore_reason: None,
                        })
                    } else {
                        Some(Plan {
                            lifecycle: Lifecycle::Active,
                            attention: derive_attention(o.current_turn, true, close_pending),
                            awaiting: false,
                            ignore_reason: None,
                        })
                    }
                }
            },
        ),
    }
}

/// 内部映射计划（未套人工锁与状态机守卫前的意图）。
struct Plan {
    lifecycle: Lifecycle,
    attention: Attention,
    awaiting: bool,
    ignore_reason: Option<IgnoreReason>,
}

/// attention 派生：收尾提议优先（等验收）；声称轮到我但缺下一步 → no_next（V08）。
fn derive_attention(turn: CurrentTurn, has_next: bool, close_pending: bool) -> Attention {
    if close_pending {
        return Attention::AwaitingAcceptance;
    }
    match turn {
        CurrentTurn::Me if has_next => Attention::Me,
        CurrentTurn::Me => Attention::NoNext,
        CurrentTurn::Other => Attention::WaitingOther,
        CurrentTurn::NoneTurn => Attention::NoNext,
    }
}

/// V04：`followup.at` 非空时，`basis` 必须是某条证据正文（NFC + 去空白后）的连续子串，
/// 且 at 可解析为 RFC3339；失败清空 at、type=none，记 R03_TIME_UNSUPPORTED。
fn validate_followup(
    f: &Followup,
    evidence: &[&RecordContext],
    warnings: &mut Vec<Warning>,
) -> (DbFollowupType, Option<i64>, Option<String>) {
    let at = f.at.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let basis = f
        .basis
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mut at_ms: Option<i64> = None;
    if let Some(at) = at {
        let parsed = parse_datetime_to_utc_ms(at);
        let basis_ok = basis
            .map(|b| evidence.iter().any(|r| body_contains(r.body.as_deref(), b)))
            .unwrap_or(false);
        match parsed {
            Some(ms) if basis_ok => at_ms = Some(ms),
            _ => {
                warnings.push(Warning::new(
                    WarningCode::R03TimeUnsupported,
                    "followup.at 缺少可验证依据（basis 非证据正文连续子串或时间不可解析），已清空 at 并置 type=none（V04/R03，计入「无依据的时间推断」指标）",
                ));
                return (DbFollowupType::None, None, None);
            }
        }
    }

    let note = basis.map(|b| truncate_chars(b, BASIS_MAX_CHARS).0);
    match (f.r#type, at_ms) {
        (AgentFollowupType::None, _) => (DbFollowupType::None, None, None),
        (AgentFollowupType::CheckLater, ms) => (DbFollowupType::Event, ms, note),
        (AgentFollowupType::Deadline, Some(ms)) | (AgentFollowupType::PromisedBy, Some(ms)) => {
            (DbFollowupType::Deadline, Some(ms), note)
        }
        (AgentFollowupType::Manual, Some(ms)) => (DbFollowupType::ManualTime, Some(ms), note),
        (_, None) => {
            // 声称有时间类型却无可验证时间：同样按 V04 清空（不凭空造日期，A02）。
            warnings.push(Warning::new(
                WarningCode::R03TimeUnsupported,
                "followup 声称时间类型但未给出可验证的 at，已置 type=none（V04/R03）",
            ));
            (DbFollowupType::None, None, None)
        }
    }
}

/// V05：收尾门。Agent 只能「提议」：白名单证据 → attention=awaiting_acceptance 且记录提议；
/// `ack_only`/`artifact_created` → 降级保持 active + awaiting_acceptance + 待确认；
/// 模型自称 `human_confirmed` 不可信（仅可信人类操作可产生），同样降级待人工裁决。
fn validate_resolution(
    res: Option<&Resolution>,
    warnings: &mut Vec<Warning>,
) -> (bool, Option<AcceptedResolution>, bool) {
    let Some(r) = res else {
        return (false, None, false);
    };
    let untrusted_human = r.evidence_type == crate::state::ResolutionEvidenceType::HumanConfirmed;
    match close_verdict(r.evidence_type) {
        CloseVerdict::Forbid => {
            warnings.push(Warning::new(
                WarningCode::V05CloseDowngraded,
                format!(
                    "收尾证据类型 {} 不在白名单（§3.3：ack_only/artifact_created 不允许收尾），降级为保持 active + awaiting_acceptance + 待确认（V05）",
                    r.evidence_type.as_str()
                ),
            ));
            (true, None, true)
        }
        CloseVerdict::Allow | CloseVerdict::NeedsHumanConfirm if untrusted_human => {
            warnings.push(Warning::new(
                WarningCode::V05CloseDowngraded,
                "human_confirmed 只能由可信人类操作产生，模型自称无效；降级为保持 active + 待确认（V05）",
            ));
            (true, None, true)
        }
        CloseVerdict::Allow | CloseVerdict::NeedsHumanConfirm => (
            true,
            Some(AcceptedResolution {
                evidence_type: r.evidence_type,
                evidence_ids: r.evidence_ids.clone(),
                note: r.note.clone(),
            }),
            false,
        ),
    }
}

/// V12：occurred_at 晚于 ingested_at + 1h 视为时钟异常，修正为 ingested_at 并告警。
fn clamped_occurred_at(r: &RecordContext, warnings: &mut Vec<Warning>) -> i64 {
    if r.occurred_at > r.ingested_at + CLOCK_SKEW_MS {
        warnings.push(Warning::new(
            WarningCode::V12ClockClamped,
            format!(
                "记录 {} 的 occurred_at 晚于 ingested_at + 1h，按 ingested_at 修正（V12）",
                r.id
            ),
        ));
        r.ingested_at
    } else {
        r.occurred_at
    }
}

/// 最早证据作为 origin 记录（同刻取列表顺序，保证确定性）。
fn pick_origin<'a>(records: &[&'a RecordContext], times: &[i64]) -> &'a RecordContext {
    let mut best = 0usize;
    for i in 1..records.len() {
        if times[i] < times[best] {
            best = i;
        }
    }
    records[best]
}

/// NFC 归一化 + 去空白（V04 子串匹配口径）。
fn normalize_for_match(s: &str) -> String {
    s.nfc().filter(|c| !c.is_whitespace()).collect()
}

/// basis 是否是某条证据正文的连续子串（NFC + 去空白后）。
fn body_contains(body: Option<&str>, needle: &str) -> bool {
    match body {
        Some(body) => {
            let needle = normalize_for_match(needle);
            !needle.is_empty() && normalize_for_match(body).contains(&needle)
        }
        None => false,
    }
}

/// RFC3339 → UTC 毫秒；解析失败返回 None（按 V04 清空，不猜测）。
fn parse_datetime_to_utc_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

/// 按字符数截断（中文安全），返回 (结果, 是否截断)。
fn truncate_chars(s: &str, max_chars: usize) -> (String, bool) {
    if s.chars().count() <= max_chars {
        (s.to_string(), false)
    } else {
        (s.chars().take(max_chars).collect(), true)
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::LockedField;
    use serde_json::{json, Value};

    // ---------- 夹具式辅助（对齐 fixtures/cases 的脱敏场景） ----------

    fn rec(id: &str) -> RecordContext {
        RecordContext {
            id: id.to_string(),
            source_id: "src-dm".to_string(),
            external_id: format!("EXT-{id}"),
            kind: crate::classify::RecordKind::ImMessage,
            conversation_id: Some("dm-1".to_string()),
            conversation_type: ConversationType::Dm,
            direction: Direction::In,
            sender_id: Some("U_BOSS".to_string()),
            mentions: vec![],
            occurred_at: 1_000,
            ingested_at: 1_200,
            summary: None,
            body: Some("图纸打印模板那个报错，你跟一下，先找供应商确认原因。".to_string()),
        }
    }

    fn rec_mut(id: &str, edit: impl FnOnce(&mut RecordContext)) -> RecordContext {
        let mut r = rec(id);
        edit(&mut r);
        r
    }

    fn me() -> MeContext {
        MeContext {
            user_id: "U_ME".to_string(),
            display_name: Some("张三".to_string()),
        }
    }

    fn base_output() -> Value {
        json!({
            "schema_version": "classify/v1",
            "source_ids": ["r1"],
            "classification": "followup",
            "belongs_to_me": "yes",
            "needs_action": true,
            "suggested_title": "跟进图纸打印模板报错",
            "next_action": "联系供应商确认报错原因",
            "current_turn": "me",
            "followup": { "type": "none", "at": null, "basis": null },
            "evidence": ["r1"],
            "rationale": "对方在私聊中要求本人跟进，本人已承诺先联系供应商",
            "confidence": 78,
            "uncertainties": [],
            "related_hint": null
        })
    }

    fn existing_active() -> crate::classify::ExistingItem {
        crate::classify::ExistingItem {
            id: "item-1".to_string(),
            lifecycle: Lifecycle::Active,
            attention: Attention::WaitingOther,
            title: "跟进图纸打印模板报错".to_string(),
            next_action: Some("等供应商更换模板".to_string()),
            followup_type: DbFollowupType::None,
            followup_at: None,
            awaiting_confirmation: false,
            resolution_evidence: None,
            last_evidence_at: Some(900),
            locked_fields: vec![],
        }
    }

    fn judge(v: Value, ctx: &JudgeContext<'_>) -> JudgeDecision {
        validate_classify(&v.to_string(), ctx)
    }

    fn expect_ok(d: JudgeDecision) -> ValidatedJudgement {
        match d {
            Ok(v) => v,
            Err(r) => panic!("期望 Ok，得到 Err：{r:?}"),
        }
    }

    fn has_warning(v: &ValidatedJudgement, code: WarningCode) -> bool {
        v.warnings.iter().any(|w| w.code == code)
    }

    // ---------- 正例：A01 第 1 阶段（私聊来件 + 本人承诺 → active/me） ----------

    #[test]
    fn a01_stage1_full_pipeline_positive() {
        let records = vec![
            rec("r1"),
            rec_mut("r2", |r| {
                r.direction = Direction::Out;
                r.sender_id = Some("U_ME".to_string());
                r.body =
                    Some("好的，我先联系供应商确认，有结果第一时间向您反馈。".to_string());
                r.occurred_at = 2_000;
                r.ingested_at = 2_100;
            }),
        ];
        let me = me();
        let ctx = JudgeContext::new(&records, &me);
        let mut v = base_output();
        v["source_ids"] = json!(["r1", "r2"]);
        v["evidence"] = json!(["r1", "r2"]);
        let judged = expect_ok(judge(v, &ctx));

        assert_eq!(judged.intent_class, IntentClass::Followup);
        assert_eq!(judged.belongs_to_me, BelongsToMe::Yes);
        assert_eq!(judged.evidence_ids, vec!["r1", "r2"]);
        // origin 取最早证据（r1），dedupe 键来自 origin + 校验后 intent。
        assert_eq!(judged.origin_record_id, "r1");
        assert_eq!(judged.origin_external_id, "EXT-r1");
        assert_eq!(judged.conversation_id.as_deref(), Some("dm-1"));
        assert_eq!(
            judged.dedupe_key,
            dedupe_key("src-dm", "EXT-r1", IntentClass::Followup)
        );
        // 正例：无降级告警。
        assert!(
            !has_warning(&judged, WarningCode::V03OwnershipDowngraded)
                && !has_warning(&judged, WarningCode::R03TimeUnsupported)
                && !has_warning(&judged, WarningCode::V05CloseDowngraded)
        );

        let item = judged.item.expect("应建事项");
        assert_eq!(item.mode, WriteMode::Create);
        assert_eq!(item.lifecycle, Lifecycle::Active);
        assert_eq!(item.attention, Attention::Me);
        assert_eq!(
            item.next_action.as_deref(),
            Some("联系供应商确认报错原因")
        );
        assert_eq!(item.followup_type, DbFollowupType::None);
        assert_eq!(item.followup_at, None);
        assert!(!item.awaiting_confirmation);
        assert_eq!(item.confidence, 78);
        // 今日页分组：轮到我。
        assert_eq!(
            crate::state::today_group(item.lifecycle, item.attention, item.awaiting_confirmation),
            crate::state::TodayGroup::Me
        );
    }

    // ---------- V01 / V11 拒绝 ----------

    #[test]
    fn v01_rejects_bad_json_and_schema_violations() {
        let records = vec![rec("r1")];
        let me = me();
        let ctx = JudgeContext::new(&records, &me);
        match validate_classify("{ broken", &ctx) {
            Err(r) => {
                assert_eq!(r.rule, RejectionRule::V01);
                assert_eq!(r.code, ErrorCode::AgentBadJson);
            }
            _ => panic!("坏 JSON 必须拒绝"),
        }
        let mut v = base_output();
        v["confidence"] = json!(150);
        match judge(v, &ctx) {
            Err(r) => {
                assert_eq!(r.rule, RejectionRule::V01);
                assert_eq!(r.code, ErrorCode::ValidationRejected);
                assert_eq!(r.path.as_deref(), Some("confidence"));
            }
            _ => panic!("confidence 越界必须拒绝"),
        }
    }

    #[test]
    fn v11_rejects_side_effect_fields() {
        let records = vec![rec("r1")];
        let me = me();
        let ctx = JudgeContext::new(&records, &me);
        let mut v = base_output();
        v["actions"] = json!([{ "tool": "send_message" }]);
        match judge(v, &ctx) {
            Err(r) => {
                assert_eq!(r.rule, RejectionRule::V11);
                assert_eq!(r.code, ErrorCode::ValidationRejected);
                assert_eq!(r.path.as_deref(), Some("actions"));
            }
            _ => panic!("副作用字段必须按 V11 拒绝"),
        }
    }

    // ---------- V02 引用 ----------

    #[test]
    fn v02_rejects_when_no_valid_reference() {
        let records = vec![rec("r1")];
        let me = me();
        let ctx = JudgeContext::new(&records, &me);
        let mut v = base_output();
        v["source_ids"] = json!(["ghost"]);
        v["evidence"] = json!(["ghost"]);
        match judge(v, &ctx) {
            Err(r) => assert_eq!(r.rule, RejectionRule::V02),
            _ => panic!("全无效引用必须拒绝"),
        }
        // source_ids 有效但 evidence 全无效：同样拒绝。
        let mut v = base_output();
        v["source_ids"] = json!(["r1"]);
        v["evidence"] = json!(["ghost"]);
        match judge(v, &ctx) {
            Err(r) => {
                assert_eq!(r.rule, RejectionRule::V02);
                assert_eq!(r.path.as_deref(), Some("evidence"));
            }
            _ => panic!("evidence 全无效必须拒绝"),
        }
    }

    #[test]
    fn v02_positive_partial_drops_invalid_ids() {
        let records = vec![rec("r1")];
        let me = me();
        let ctx = JudgeContext::new(&records, &me);
        let mut v = base_output();
        v["source_ids"] = json!(["r1", "ghost"]);
        v["evidence"] = json!(["r1", "ghost"]);
        let judged = expect_ok(judge(v, &ctx));
        assert_eq!(judged.evidence_ids, vec!["r1"]);
        assert_eq!(judged.dropped_ids, vec!["ghost".to_string()]);
    }

    // ---------- V03 归属锚点 ----------

    #[test]
    fn v03_positive_three_anchor_kinds() {
        let me = me();
        // ① 私聊来件（A01-R1）。
        let records = vec![rec("r1")];
        let ctx = JudgeContext::new(&records, &me);
        let judged = expect_ok(judge(base_output(), &ctx));
        assert_eq!(judged.belongs_to_me, BelongsToMe::Yes);
        assert!(judged.item.is_some());
        // ② 群内 @本人（A03-R1）。
        let records = vec![rec_mut("r1", |r| {
            r.conversation_type = ConversationType::Group;
            r.conversation_id = Some("grp".to_string());
            r.mentions = vec!["U_ME".to_string()];
        })];
        let ctx = JudgeContext::new(&records, &me);
        let judged = expect_ok(judge(base_output(), &ctx));
        assert_eq!(judged.belongs_to_me, BelongsToMe::Yes);
        // ③ 本人外发承诺（A01-R2 / A01-R3）。
        let records = vec![rec_mut("r1", |r| {
            r.direction = Direction::Out;
            r.sender_id = Some("U_ME".to_string());
        })];
        let ctx = JudgeContext::new(&records, &me);
        let judged = expect_ok(judge(base_output(), &ctx));
        assert_eq!(judged.belongs_to_me, BelongsToMe::Yes);
    }

    #[test]
    fn v03_negative_a06_downgrades_yes_without_anchor() {
        // A06-R3：群消息，正文含本人姓名但未 @本人（mentions 只有马工）。
        let me = me();
        let records = vec![rec_mut("r3", |r| {
            r.conversation_type = ConversationType::Group;
            r.conversation_id = Some("grp-ops".to_string());
            r.sender_id = Some("U_QIAN".to_string());
            r.mentions = vec!["U_MAGONG".to_string()];
            r.body = Some(
                "@马工 图纸评审这周你牵头；张三作为资料接口人配合对齐就行，不用额外出力。"
                    .to_string(),
            );
        })];
        let ctx = JudgeContext::new(&records, &me);
        let mut v = base_output();
        v["source_ids"] = json!(["r3"]);
        v["evidence"] = json!(["r3"]);
        v["suggested_title"] = json!("图纸评审配合对齐");
        let judged = expect_ok(judge(v, &ctx));

        // V03 强制降级：绝不进入「轮到我处理」。
        assert_eq!(judged.belongs_to_me, BelongsToMe::Unclear);
        assert!(has_warning(&judged, WarningCode::V03OwnershipDowngraded));
        let item = judged.item.expect("归属不明 → candidate 待确认");
        assert_eq!(item.lifecycle, Lifecycle::Candidate);
        assert_ne!(item.lifecycle, Lifecycle::Active);
        assert!(item.awaiting_confirmation);
        assert_eq!(
            crate::state::today_group(item.lifecycle, item.attention, item.awaiting_confirmation),
            crate::state::TodayGroup::Confirm
        );
    }

    // ---------- V04 时间依据 ----------

    #[test]
    fn v04_positive_basis_substring_sets_followup() {
        let records = vec![rec_mut("r1", |r| {
            r.body = Some("供应商说模板周三前更换完成，完成后我同步您。".to_string());
        })];
        let me = me();
        let ctx = JudgeContext::new(&records, &me);
        let mut v = base_output();
        v["followup"] = json!({
            "type": "deadline",
            "at": "2026-11-25T18:00:00+08:00",
            "basis": "周三前更换完成"
        });
        let judged = expect_ok(judge(v, &ctx));
        assert!(!has_warning(&judged, WarningCode::R03TimeUnsupported));
        let item = judged.item.unwrap();
        assert_eq!(item.followup_type, DbFollowupType::Deadline);
        let expected = chrono::DateTime::parse_from_rfc3339("2026-11-25T18:00:00+08:00")
            .unwrap()
            .timestamp_millis();
        assert_eq!(item.followup_at, Some(expected));
        assert_eq!(item.followup_note.as_deref(), Some("周三前更换完成"));
    }

    #[test]
    fn v04_positive_nfc_and_whitespace_tolerant_match() {
        let records = vec![rec_mut("r1", |r| {
            r.body = Some("他们说 周三前 更换完成\u{3000}之后同步".to_string());
        })];
        let me = me();
        let ctx = JudgeContext::new(&records, &me);
        let mut v = base_output();
        v["followup"] = json!({
            "type": "promised_by",
            "at": "2026-11-25T18:00:00+08:00",
            "basis": "周三前更换完成"
        });
        let judged = expect_ok(judge(v, &ctx));
        let item = judged.item.unwrap();
        // NFC + 去空白后子串匹配成功；promised_by 也映射为库内 deadline。
        assert_eq!(item.followup_type, DbFollowupType::Deadline);
        assert!(item.followup_at.is_some());
    }

    #[test]
    fn v04_negative_clears_unsupported_time() {
        let me = me();
        // a) basis 不在正文：不得凭空保留日期（A02「优先」反例）。
        let records = vec![rec_mut("r1", |r| {
            r.body = Some("图纸归档整改这项要优先推进，先把进度摸清楚。".to_string());
        })];
        let ctx = JudgeContext::new(&records, &me);
        let mut v = base_output();
        v["suggested_title"] = json!("摸清图纸归档整改进度");
        v["next_action"] = json!("梳理整改进度与遗留问题");
        v["followup"] = json!({
            "type": "deadline",
            "at": "2026-11-21T18:00:00+08:00",
            "basis": "周三前给结论"
        });
        let judged = expect_ok(judge(v, &ctx));
        assert!(has_warning(&judged, WarningCode::R03TimeUnsupported));
        let item = judged.item.unwrap();
        assert_eq!(item.followup_type, DbFollowupType::None);
        assert_eq!(item.followup_at, None);

        // b) at 不可解析。
        let mut v = base_output();
        v["followup"] = json!({ "type": "manual", "at": "明天下午", "basis": "优先推进" });
        let judged = expect_ok(judge(v, &ctx));
        assert!(has_warning(&judged, WarningCode::R03TimeUnsupported));
        assert_eq!(judged.item.unwrap().followup_at, None);

        // c) at 有但 basis 缺失。
        let mut v = base_output();
        v["followup"] = json!({
            "type": "deadline",
            "at": "2026-11-25T18:00:00+08:00",
            "basis": null
        });
        let judged = expect_ok(judge(v, &ctx));
        assert!(has_warning(&judged, WarningCode::R03TimeUnsupported));
        assert_eq!(judged.item.unwrap().followup_at, None);

        // d) 声称时间类型却没给 at。
        let mut v = base_output();
        v["followup"] = json!({ "type": "deadline", "at": null, "basis": null });
        let judged = expect_ok(judge(v, &ctx));
        assert!(has_warning(&judged, WarningCode::R03TimeUnsupported));
        let item = judged.item.unwrap();
        assert_eq!(item.followup_type, DbFollowupType::None);
        assert_eq!(item.followup_at, None);
    }

    // ---------- V05 收尾门 ----------

    #[test]
    fn v05_positive_whitelisted_proposal_waits_acceptance() {
        // A01 第 3 阶段：本人提议收尾（explicit_complete）→ 等验收，不自动关单。
        let records = vec![rec_mut("r4", |r| {
            r.direction = Direction::Out;
            r.sender_id = Some("U_ME".to_string());
            r.body = Some("模板已经更换完成，业务侧重试正常，这个事可以收尾了。".to_string());
        })];
        let me = me();
        let existing = existing_active();
        let ctx = JudgeContext::new(&records, &me).with_existing(Some(&existing));
        let mut v = base_output();
        v["source_ids"] = json!(["r4"]);
        v["evidence"] = json!(["r4"]);
        v["current_turn"] = json!("other");
        v["next_action"] = json!("等交办人确认收尾");
        v["resolution"] = json!({
            "evidence_type": "explicit_complete",
            "evidence_ids": ["r4"]
        });
        let judged = expect_ok(judge(v, &ctx));
        assert!(!has_warning(&judged, WarningCode::V05CloseDowngraded));
        let item = judged.item.unwrap();
        assert_eq!(item.lifecycle, Lifecycle::Active);
        assert_eq!(item.attention, Attention::AwaitingAcceptance);
        assert!(!item.awaiting_confirmation);
        let res = item.resolution.expect("白名单提议应记录");
        assert_eq!(
            res.evidence_type,
            crate::state::ResolutionEvidenceType::ExplicitComplete
        );
    }

    #[test]
    fn v05_negative_ack_only_never_closes() {
        // A01 第 4 阶段：「好的」是 ack_only，不允许自动收尾。
        let records = vec![rec("r5")];
        let me = me();
        let existing = existing_active();
        let ctx = JudgeContext::new(&records, &me).with_existing(Some(&existing));
        let mut v = base_output();
        v["source_ids"] = json!(["r5"]);
        v["evidence"] = json!(["r5"]);
        v["current_turn"] = json!("other");
        v["next_action"] = json!("等交办人确认收尾");
        v["resolution"] = json!({
            "evidence_type": "ack_only",
            "evidence_ids": ["r5"]
        });
        let judged = expect_ok(judge(v, &ctx));
        assert!(has_warning(&judged, WarningCode::V05CloseDowngraded));
        let item = judged.item.unwrap();
        assert_ne!(item.lifecycle, Lifecycle::Closed);
        assert_eq!(item.lifecycle, Lifecycle::Active);
        assert_eq!(item.attention, Attention::AwaitingAcceptance);
        assert!(item.awaiting_confirmation, "ack_only → 转人工确认");
        assert_eq!(item.resolution, None);
    }

    #[test]
    fn v05_negative_agent_claimed_human_confirmed_is_untrusted() {
        // 模型自称 human_confirmed 不得关单（仅可信人类操作可产生）。
        let records = vec![rec("r5")];
        let me = me();
        let existing = existing_active();
        let ctx = JudgeContext::new(&records, &me).with_existing(Some(&existing));
        let mut v = base_output();
        v["source_ids"] = json!(["r5"]);
        v["evidence"] = json!(["r5"]);
        v["current_turn"] = json!("other");
        v["resolution"] = json!({ "evidence_type": "human_confirmed" });
        let judged = expect_ok(judge(v, &ctx));
        assert!(has_warning(&judged, WarningCode::V05CloseDowngraded));
        let item = judged.item.unwrap();
        assert_ne!(item.lifecycle, Lifecycle::Closed);
        assert!(item.awaiting_confirmation);
        assert_eq!(item.resolution, None);
    }

    // ---------- V06 归并门 ----------

    #[test]
    fn v06_related_hint_only_yields_similar_candidates() {
        let records = vec![rec("r1")];
        let me = me();
        let ctx = JudgeContext::new(&records, &me);
        let mut v = base_output();
        v["related_hint"] = json!({
            "target_item_ids": ["item-a", "item-b"],
            "reason": "同一会话的图纸复核事项"
        });
        let judged = expect_ok(judge(v, &ctx));
        assert_eq!(judged.similar_candidates.len(), 2);
        for c in &judged.similar_candidates {
            assert_eq!(c.link_type, "similar_candidate");
            assert_eq!(c.evidence_record_id, "r1");
        }
        // ItemDecision 不携带 thread 赋值（结构上不可能自动置 thread_id）。
        let _ = judged.item.unwrap();
    }

    // ---------- V07 人工锁 ----------

    fn a11_existing_ignored() -> crate::classify::ExistingItem {
        crate::classify::ExistingItem {
            id: "item-a11".to_string(),
            lifecycle: Lifecycle::Ignored,
            attention: Attention::NoNext,
            title: "二期图纸复核推广".to_string(),
            next_action: None,
            followup_type: DbFollowupType::None,
            followup_at: None,
            awaiting_confirmation: false,
            resolution_evidence: None,
            last_evidence_at: Some(500),
            locked_fields: vec![
                LockedField {
                    field: ItemField::BelongsToMe,
                    value_json: Some("false".to_string()),
                    locked: true,
                },
                LockedField {
                    field: ItemField::Lifecycle,
                    value_json: Some(Lifecycle::Ignored.to_value_json()),
                    locked: true,
                },
            ],
        }
    }

    #[test]
    fn v07_negative_a11_replay_rejects_r_locked() {
        // A11 第 3 阶段：同一输出重跑，归属/生命周期已被人工锁锁定 → rejected(R_LOCKED)，
        // 事项不得被静默恢复。
        let records = vec![rec_mut("r1", |r| {
            r.body = Some("张三，二期的图纸复核推广接下来你跟进吧。".to_string());
        })];
        let me = me();
        let existing = a11_existing_ignored();
        let ctx = JudgeContext::new(&records, &me).with_existing(Some(&existing));
        let mut v = base_output();
        v["suggested_title"] = json!("二期图纸复核推广");
        v["next_action"] = json!("跟进二期图纸复核推广");
        match judge(v, &ctx) {
            Err(r) => {
                assert_eq!(r.rule, RejectionRule::RLocked);
                assert_eq!(r.rule.as_str(), "R_LOCKED");
                assert_eq!(r.code, ErrorCode::LockedField);
            }
            Ok(j) => panic!("人工锁必须拒绝，却得到 {j:?}"),
        }
    }

    #[test]
    fn v07_positive_skips_locked_attention_only() {
        // V07「跳过该字段，其余照常」：只锁 attention 时，attention 保留人工值，
        // 未表态的 next_action 仍可被新证据更新。
        let records = vec![rec("r3")];
        let me = me();
        let mut existing = existing_active();
        existing.attention = Attention::WaitingOther;
        existing.locked_fields = vec![LockedField {
            field: ItemField::Attention,
            value_json: Some("\"waiting_other\"".to_string()),
            locked: true,
        }];
        let ctx = JudgeContext::new(&records, &me).with_existing(Some(&existing));
        let mut v = base_output();
        v["source_ids"] = json!(["r3"]);
        v["evidence"] = json!(["r3"]);
        v["current_turn"] = json!("me");
        v["next_action"] = json!("催供应商给出更换时间");
        let judged = expect_ok(judge(v, &ctx));
        assert!(has_warning(&judged, WarningCode::RLockedFieldSkipped));
        let item = judged.item.unwrap();
        assert_eq!(item.attention, Attention::WaitingOther, "锁定字段保留人工值");
        assert_eq!(
            item.next_action.as_deref(),
            Some("催供应商给出更换时间"),
            "未锁字段照常更新"
        );
    }

    #[test]
    fn v07_positive_no_conflict_when_value_unchanged() {
        // 锁定 lifecycle=active 且本次决策也是 active：无冲突，照常通过。
        let records = vec![rec("r3")];
        let me = me();
        let mut existing = existing_active();
        existing.locked_fields = vec![LockedField {
            field: ItemField::Lifecycle,
            value_json: Some(Lifecycle::Active.to_value_json()),
            locked: true,
        }];
        let ctx = JudgeContext::new(&records, &me).with_existing(Some(&existing));
        let mut v = base_output();
        v["source_ids"] = json!(["r3"]);
        v["evidence"] = json!(["r3"]);
        v["current_turn"] = json!("other");
        v["next_action"] = json!("等供应商更换模板");
        let judged = expect_ok(judge(v, &ctx));
        assert_eq!(judged.item.unwrap().lifecycle, Lifecycle::Active);
    }

    // ---------- V08 置信 ----------

    #[test]
    fn v08_negative_low_confidence_demotes_to_candidate() {
        let records = vec![rec("r1")];
        let me = me();
        let ctx = JudgeContext::new(&records, &me);
        let mut v = base_output();
        v["confidence"] = json!(55);
        let judged = expect_ok(judge(v, &ctx));
        assert!(has_warning(&judged, WarningCode::V08LowConfidence));
        let item = judged.item.unwrap();
        assert_eq!(item.lifecycle, Lifecycle::Candidate);
        assert!(item.awaiting_confirmation);
    }

    #[test]
    fn v08_negative_me_without_next_action_sets_no_next() {
        let records = vec![rec("r1")];
        let me = me();
        let ctx = JudgeContext::new(&records, &me);
        let mut v = base_output();
        v["next_action"] = Value::Null;
        v["current_turn"] = json!("me");
        let judged = expect_ok(judge(v, &ctx));
        let item = judged.item.unwrap();
        // 声称轮到我但缺下一步 → no_next，且不得 active。
        assert_eq!(item.attention, Attention::NoNext);
        assert_eq!(item.lifecycle, Lifecycle::Candidate);
        assert!(item.awaiting_confirmation);
    }

    #[test]
    fn v08_positive_boundary_confidence_60_stays_active() {
        let records = vec![rec("r1")];
        let me = me();
        let ctx = JudgeContext::new(&records, &me);
        let mut v = base_output();
        v["confidence"] = json!(60);
        let judged = expect_ok(judge(v, &ctx));
        assert_eq!(judged.item.unwrap().lifecycle, Lifecycle::Active);
    }

    // ---------- V09 长度截断 ----------

    #[test]
    fn v09_truncates_overlength_fields() {
        let records = vec![rec("r1")];
        let me = me();
        let ctx = JudgeContext::new(&records, &me);
        let mut v = base_output();
        v["suggested_title"] = json!("标".repeat(80));
        v["next_action"] = json!("步".repeat(250));
        v["rationale"] = json!("理".repeat(600));
        let judged = expect_ok(judge(v, &ctx));
        let item = judged.item.unwrap();
        assert_eq!(item.title.chars().count(), 60);
        assert_eq!(item.next_action.as_ref().unwrap().chars().count(), 200);
        assert_eq!(judged.rationale.chars().count(), 500);
        assert_eq!(
            judged
                .warnings
                .iter()
                .filter(|w| w.code == WarningCode::V09Truncated)
                .count(),
            3
        );
    }

    #[test]
    fn v09_positive_within_bounds_no_warning() {
        let records = vec![rec("r1")];
        let me = me();
        let ctx = JudgeContext::new(&records, &me);
        let judged = expect_ok(judge(base_output(), &ctx));
        assert!(!has_warning(&judged, WarningCode::V09Truncated));
    }

    // ---------- V10 幂等 ----------

    #[test]
    fn v10_update_takes_max_last_evidence_and_keeps_title() {
        let records = vec![rec_mut("r3", |r| {
            r.occurred_at = 2_000;
            r.ingested_at = 2_100;
        })];
        let me = me();
        let mut existing = existing_active();
        existing.last_evidence_at = Some(5_000);
        let ctx = JudgeContext::new(&records, &me).with_existing(Some(&existing));
        let mut v = base_output();
        v["source_ids"] = json!(["r3"]);
        v["evidence"] = json!(["r3"]);
        v["current_turn"] = json!("other");
        let judged = expect_ok(judge(v.clone(), &ctx));
        let item = judged.item.unwrap();
        assert_eq!(item.mode, WriteMode::Update("item-1".to_string()));
        assert_eq!(item.last_evidence_at, 5_000, "V10：last_evidence_at 取 max");
        assert_eq!(item.title, existing.title, "更新不重写标题");

        // 既有值较旧 → 取新证据时间。
        let mut existing = existing_active();
        existing.last_evidence_at = Some(1_000);
        let ctx = JudgeContext::new(&records, &me).with_existing(Some(&existing));
        let judged = expect_ok(judge(v.clone(), &ctx));
        assert_eq!(judged.item.unwrap().last_evidence_at, 2_000);

        // 无既有 → 新建。
        let ctx = JudgeContext::new(&records, &me);
        let judged = expect_ok(judge(v, &ctx));
        assert_eq!(judged.item.unwrap().mode, WriteMode::Create);
    }

    // ---------- V12 时钟 ----------

    #[test]
    fn v12_clamps_future_clock_and_keeps_boundary() {
        let me = me();
        // occurred_at 晚于 ingested_at + 1h → 修正为 ingested_at。
        let records = vec![rec_mut("r1", |r| {
            r.occurred_at = 5_000_000;
            r.ingested_at = 1_000_000;
        })];
        let ctx = JudgeContext::new(&records, &me);
        let judged = expect_ok(judge(base_output(), &ctx));
        assert!(has_warning(&judged, WarningCode::V12ClockClamped));
        assert_eq!(judged.item.unwrap().last_evidence_at, 1_000_000);

        // 恰好 +1h：不算异常。
        let records = vec![rec_mut("r1", |r| {
            r.occurred_at = 4_600_000;
            r.ingested_at = 1_000_000;
        })];
        let ctx = JudgeContext::new(&records, &me);
        let judged = expect_ok(judge(base_output(), &ctx));
        assert!(!has_warning(&judged, WarningCode::V12ClockClamped));
        assert_eq!(judged.item.unwrap().last_evidence_at, 4_600_000);
    }

    // ---------- §3.4 映射表 ----------

    #[test]
    fn mapping_info_noise_and_thread_update_create_no_item() {
        let records = vec![rec("r1")];
        let me = me();
        let ctx = JudgeContext::new(&records, &me);
        let mut v = base_output();
        v["classification"] = json!("info");
        let judged = expect_ok(judge(v, &ctx));
        assert_eq!(judged.intent_class, IntentClass::Info);
        assert!(judged.item.is_none());

        let mut v = base_output();
        v["classification"] = json!("noise");
        let judged = expect_ok(judge(v, &ctx));
        assert_eq!(judged.intent_class, IntentClass::Info);
        assert!(judged.item.is_none());

        let mut v = base_output();
        v["classification"] = json!("thread_update");
        let judged = expect_ok(judge(v, &ctx));
        assert_eq!(judged.intent_class, IntentClass::ThreadUpdate);
        assert!(judged.item.is_none());
    }

    #[test]
    fn mapping_followup_no_creates_no_item_for_others() {
        // A06-R2：他人负责（判读正确负例）→ 不给他人生成事项。
        let me = me();
        let records = vec![rec_mut("r2", |r| {
            r.conversation_type = ConversationType::Group;
            r.mentions = vec!["U_ME".to_string()];
            r.body = Some("@张三 这块李四负责，你不用跟。".to_string());
        })];
        let ctx = JudgeContext::new(&records, &me);
        let mut v = base_output();
        v["source_ids"] = json!(["r2"]);
        v["evidence"] = json!(["r2"]);
        v["belongs_to_me"] = json!("no");
        v["classification"] = json!("info");
        let judged = expect_ok(judge(v, &ctx));
        assert!(judged.item.is_none());
    }

    #[test]
    fn mapping_one_off_without_action_ignores_immediately() {
        // A03：one_off + needs_action=false → ignored(no_action)，不悬挂、不进待确认。
        let me = me();
        let records = vec![rec_mut("r1", |r| {
            r.conversation_type = ConversationType::Group;
            r.mentions = vec!["U_ME".to_string()];
            r.body = Some("@张三 麻烦帮忙开一下 11 月图纸目录的下载权限。".to_string());
        })];
        let ctx = JudgeContext::new(&records, &me);
        let mut v = base_output();
        v["classification"] = json!("one_off");
        v["needs_action"] = json!(false);
        v["suggested_title"] = json!("开通 11 月图纸目录下载权限");
        v["next_action"] = Value::Null;
        v["current_turn"] = json!("none");
        let judged = expect_ok(judge(v, &ctx));
        let item = judged.item.expect("§3.4：先建事项再置 ignored，保留痕迹");
        assert_eq!(judged.intent_class, IntentClass::OneOff);
        assert_eq!(item.lifecycle, Lifecycle::Ignored);
        assert_eq!(item.ignore_reason, Some(IgnoreReason::NoAction));
        assert!(!item.awaiting_confirmation);
        assert_eq!(
            crate::state::today_group(item.lifecycle, item.attention, item.awaiting_confirmation),
            crate::state::TodayGroup::Hidden
        );
    }

    #[test]
    fn mapping_one_off_with_action_stays_candidate() {
        let records = vec![rec("r1")];
        let me = me();
        let ctx = JudgeContext::new(&records, &me);
        let mut v = base_output();
        v["classification"] = json!("one_off");
        let judged = expect_ok(judge(v, &ctx));
        let item = judged.item.unwrap();
        assert_eq!(item.lifecycle, Lifecycle::Candidate);
        assert!(!item.awaiting_confirmation);
    }

    #[test]
    fn mapping_needs_confirmation_awaits_human() {
        let records = vec![rec("r1")];
        let me = me();
        let ctx = JudgeContext::new(&records, &me);
        let mut v = base_output();
        v["classification"] = json!("needs_confirmation");
        let judged = expect_ok(judge(v, &ctx));
        assert_eq!(judged.intent_class, IntentClass::Confirm);
        let item = judged.item.unwrap();
        assert_eq!(item.lifecycle, Lifecycle::Candidate);
        assert!(item.awaiting_confirmation);
    }

    // ---------- 幂等键 ----------

    #[test]
    fn dedupe_key_is_sha256_of_contract_concatenation() {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(b"src-dm:EXT-r1:followup");
        let expected: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(dedupe_key("src-dm", "EXT-r1", IntentClass::Followup), expected);
        // intent 不同 → 键不同（§4.4）。
        assert_ne!(
            dedupe_key("src-dm", "EXT-r1", IntentClass::Followup),
            dedupe_key("src-dm", "EXT-r1", IntentClass::OneOff)
        );
    }

    // ---------- 更新路径与状态机守卫 ----------

    #[test]
    fn update_path_respects_state_machine() {
        let me = me();
        let records = vec![rec("r3")];
        let mut v = base_output();
        v["source_ids"] = json!(["r3"]);
        v["evidence"] = json!(["r3"]);
        v["current_turn"] = json!("other");
        v["next_action"] = json!("等供应商更换模板后同步交办人");

        // active → active：新证据更新，允许。
        let existing = existing_active();
        let ctx = JudgeContext::new(&records, &me).with_existing(Some(&existing));
        let judged = expect_ok(judge(v.clone(), &ctx));
        assert_eq!(judged.item.unwrap().lifecycle, Lifecycle::Active);

        // candidate + 新锚点证据 → 提升 active（§3.3「新证据补足」）。
        let mut existing = existing_active();
        existing.lifecycle = Lifecycle::Candidate;
        existing.awaiting_confirmation = true;
        let ctx = JudgeContext::new(&records, &me).with_existing(Some(&existing));
        let judged = expect_ok(judge(v.clone(), &ctx));
        let item = judged.item.unwrap();
        assert_eq!(item.lifecycle, Lifecycle::Active);
        assert!(!item.awaiting_confirmation, "新证据补足后不再待确认");

        // ignored（未加锁）+ Agent 试图恢复 → 守卫拒绝，保持 ignored（A11 兜底）。
        let mut existing = existing_active();
        existing.lifecycle = Lifecycle::Ignored;
        let ctx = JudgeContext::new(&records, &me).with_existing(Some(&existing));
        let judged = expect_ok(judge(v, &ctx));
        assert!(has_warning(&judged, WarningCode::TransitionDenied));
        let item = judged.item.unwrap();
        assert_eq!(item.lifecycle, Lifecycle::Ignored);
    }

    #[test]
    fn update_preserves_next_action_when_agent_omits_it() {
        let me = me();
        let records = vec![rec("r3")];
        let existing = existing_active();
        let ctx = JudgeContext::new(&records, &me).with_existing(Some(&existing));
        let mut v = base_output();
        v["source_ids"] = json!(["r3"]);
        v["evidence"] = json!(["r3"]);
        v["current_turn"] = json!("other");
        v["next_action"] = Value::Null;
        let judged = expect_ok(judge(v, &ctx));
        let item = judged.item.unwrap();
        assert_eq!(
            item.next_action.as_deref(),
            existing.next_action.as_deref(),
            "Agent 缺字段不得清空既有下一步"
        );
        assert_eq!(item.lifecycle, Lifecycle::Active);
    }

    // ---------- 结构化入口 ----------

    #[test]
    fn policy_validator_struct_matches_free_function() {
        let records = vec![rec("r1")];
        let me = me();
        let ctx = JudgeContext::new(&records, &me);
        let raw = base_output().to_string();
        let a = PolicyValidator::new().validate(&raw, &ctx);
        let b = validate_classify(&raw, &ctx);
        assert_eq!(a, b);
        assert!(a.is_ok());
    }
}
