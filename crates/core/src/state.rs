//! 事项状态机、人工锁与收尾证据白名单（详细设计 §3.1–§3.3、§5.1 V05/V07）。
//!
//! 全部为纯函数：不做 IO，可纯内存测试（§16.1）。枚举取值与
//! `migrations/0001_init.sql` 的 CHECK 约束一一对应（迁移契约），新增取值必须同步迁移。

use serde::{Deserialize, Serialize};
use std::fmt;

/// 事项生命周期：`candidate | active | closed | ignored`（§3.3 状态机）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Candidate,
    Active,
    Closed,
    Ignored,
}

impl Lifecycle {
    pub fn as_str(&self) -> &'static str {
        match self {
            Lifecycle::Candidate => "candidate",
            Lifecycle::Active => "active",
            Lifecycle::Closed => "closed",
            Lifecycle::Ignored => "ignored",
        }
    }

    /// 与 `field_override.value_json`（JSON 文本）可比对的存储形式。
    pub fn to_value_json(&self) -> String {
        format!("\"{}\"", self.as_str())
    }
}

impl fmt::Display for Lifecycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 「轮到谁」的唯一真相（§3.2）；`current_turn` 由它派生，两层数据不得各存一份。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attention {
    Me,
    WaitingOther,
    ReportBack,
    AwaitingAcceptance,
    NoNext,
}

impl Attention {
    pub fn as_str(&self) -> &'static str {
        match self {
            Attention::Me => "me",
            Attention::WaitingOther => "waiting_other",
            Attention::ReportBack => "report_back",
            Attention::AwaitingAcceptance => "awaiting_acceptance",
            Attention::NoNext => "no_next",
        }
    }
}

impl fmt::Display for Attention {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 判读输出的轮次口径（§8.2）：`me | other | none`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CurrentTurn {
    Me,
    Other,
    #[serde(rename = "none")]
    NoneTurn,
}

impl CurrentTurn {
    pub fn as_str(&self) -> &'static str {
        match self {
            CurrentTurn::Me => "me",
            CurrentTurn::Other => "other",
            CurrentTurn::NoneTurn => "none",
        }
    }
}

/// 触发者（与 `work_event.actor` 一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    System,
    Agent,
    Human,
}

/// 库内跟进类型（迁移契约 `work_item.followup_type`：none|event|deadline|manual_time）。
/// Agent 侧口径（§8.2）到库内口径的映射见 `validate`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DbFollowupType {
    #[serde(rename = "none")]
    None,
    Event,
    Deadline,
    ManualTime,
}

impl DbFollowupType {
    pub fn as_str(&self) -> &'static str {
        match self {
            DbFollowupType::None => "none",
            DbFollowupType::Event => "event",
            DbFollowupType::Deadline => "deadline",
            DbFollowupType::ManualTime => "manual_time",
        }
    }
}

/// 收尾证据类型（§3.3 白名单 / §5.1 V05）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionEvidenceType {
    /// 对方明确说「已完成/已上线/没问题了」。
    ExplicitComplete,
    /// 交付物被接收方确认。
    ArtifactAccepted,
    /// 人工确认收尾。
    HumanConfirmed,
    /// 仅「好的」「收到」——不算收尾证据。
    AckOnly,
    /// 文件出现 ≠ 交付满足（PRD A04）。
    ArtifactCreated,
}

impl ResolutionEvidenceType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ResolutionEvidenceType::ExplicitComplete => "explicit_complete",
            ResolutionEvidenceType::ArtifactAccepted => "artifact_accepted",
            ResolutionEvidenceType::HumanConfirmed => "human_confirmed",
            ResolutionEvidenceType::AckOnly => "ack_only",
            ResolutionEvidenceType::ArtifactCreated => "artifact_created",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "explicit_complete" => Some(ResolutionEvidenceType::ExplicitComplete),
            "artifact_accepted" => Some(ResolutionEvidenceType::ArtifactAccepted),
            "human_confirmed" => Some(ResolutionEvidenceType::HumanConfirmed),
            "ack_only" => Some(ResolutionEvidenceType::AckOnly),
            "artifact_created" => Some(ResolutionEvidenceType::ArtifactCreated),
            _ => None,
        }
    }
}

/// 领域层的收尾证据（判读输出的 `resolution` 经转换得到，供转换守卫使用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolutionEvidence {
    pub evidence_type: ResolutionEvidenceType,
    #[serde(default)]
    pub evidence_ids: Vec<String>,
    pub note: Option<String>,
}

/// §3.3 白名单裁决：`explicit_complete` 轻事项可收尾、长期事项仍需人工确认；
/// `ack_only` / `artifact_created` 一律不允许收尾（PRD R05 / A04）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseVerdict {
    Allow,
    NeedsHumanConfirm,
    Forbid,
}

/// 收尾证据白名单裁决（§3.3 表）。
pub fn close_verdict(evidence_type: ResolutionEvidenceType) -> CloseVerdict {
    match evidence_type {
        ResolutionEvidenceType::ArtifactAccepted | ResolutionEvidenceType::HumanConfirmed => {
            CloseVerdict::Allow
        }
        ResolutionEvidenceType::ExplicitComplete => CloseVerdict::NeedsHumanConfirm,
        ResolutionEvidenceType::AckOnly | ResolutionEvidenceType::ArtifactCreated => {
            CloseVerdict::Forbid
        }
    }
}

/// 证据类型是否属于收尾白名单（§3.3：非白名单不得收尾）。
pub fn is_close_evidence_whitelisted(evidence_type: ResolutionEvidenceType) -> bool {
    !matches!(close_verdict(evidence_type), CloseVerdict::Forbid)
}

/// 忽略原因枚举（§3.3「必须带原因枚举」；人工可自由补充文本）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IgnoreReason {
    /// 判读映射：one_off 且无剩余行动（§3.4，A03）。
    NoAction,
    /// 人工：「不是我的事」（A11）。
    NotMine,
    /// 人工：「普通信息」（§3.3 candidate → ignored）。
    PlainInfo,
}

impl IgnoreReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            IgnoreReason::NoAction => "no_action",
            IgnoreReason::NotMine => "not_mine",
            IgnoreReason::PlainInfo => "plain_info",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "no_action" => Some(IgnoreReason::NoAction),
            "not_mine" => Some(IgnoreReason::NotMine),
            "plain_info" => Some(IgnoreReason::PlainInfo),
            _ => None,
        }
    }
}

/// `field_override` 可锁定的 `work_item` 字段（§3.1 / §9）。
/// `belongs_to_me` 是纠错语义字段（`work_item` 无同名列），用于阻止 Agent 重新认领。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemField {
    BelongsToMe,
    Lifecycle,
    Attention,
    NextAction,
    Title,
    FollowupAt,
    ThreadId,
}

impl ItemField {
    pub fn as_str(&self) -> &'static str {
        match self {
            ItemField::BelongsToMe => "belongs_to_me",
            ItemField::Lifecycle => "lifecycle",
            ItemField::Attention => "attention",
            ItemField::NextAction => "next_action",
            ItemField::Title => "title",
            ItemField::FollowupAt => "followup_at",
            ItemField::ThreadId => "thread_id",
        }
    }

    /// 解析 `field_override.field`；未知字段返回 `None`（前向兼容旧库）。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "belongs_to_me" => Some(ItemField::BelongsToMe),
            "lifecycle" => Some(ItemField::Lifecycle),
            "attention" => Some(ItemField::Attention),
            "next_action" => Some(ItemField::NextAction),
            "title" => Some(ItemField::Title),
            "followup_at" => Some(ItemField::FollowupAt),
            "thread_id" => Some(ItemField::ThreadId),
            _ => None,
        }
    }
}

impl fmt::Display for ItemField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一条人工锁（`field_override` 行）。`value_json` 为 JSON 文本（如 `false`、`"ignored"`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedField {
    pub field: ItemField,
    pub value_json: Option<String>,
    #[serde(default = "default_locked")]
    pub locked: bool,
}

fn default_locked() -> bool {
    true
}

/// 字段是否被人工锁定（§3.1：存在的 `field` 即锁定；`locked=0` 视为未锁）。
pub fn is_locked(locks: &[LockedField], field: ItemField) -> bool {
    locks.iter().any(|l| l.locked && l.field == field)
}

/// 读取被锁定的布尔值（`belongs_to_me` 的 `value_json` 为 `true`/`false` 文本）。
pub fn locked_bool(locks: &[LockedField], field: ItemField) -> Option<bool> {
    locks
        .iter()
        .find(|l| l.locked && l.field == field)
        .and_then(|l| l.value_json.as_deref())
        .and_then(|v| match v.trim() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        })
}

/// §3.2 派生视图 `v_item_turn` 的 Rust 等价：`attention` 是唯一真相。
pub fn current_turn(attention: Attention) -> CurrentTurn {
    match attention {
        Attention::Me | Attention::ReportBack => CurrentTurn::Me,
        Attention::WaitingOther | Attention::AwaitingAcceptance => CurrentTurn::Other,
        Attention::NoNext => CurrentTurn::NoneTurn,
    }
}

/// 首页分组（§3.2 表 + 夹具断言）：待确认只收 `candidate + awaiting_confirmation`；
/// active 的待验收归「等待别人」组（A01 第 4 阶段），closed/ignored 不出现。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodayGroup {
    Me,
    WaitingOther,
    Confirm,
    Hidden,
}

/// 按生命周期 + 注意力 + 待确认标记推导首页分组（§3.2）。
pub fn today_group(
    lifecycle: Lifecycle,
    attention: Attention,
    awaiting_confirmation: bool,
) -> TodayGroup {
    match lifecycle {
        Lifecycle::Candidate => {
            if awaiting_confirmation {
                TodayGroup::Confirm
            } else {
                TodayGroup::Hidden
            }
        }
        Lifecycle::Active => match attention {
            Attention::Me | Attention::ReportBack => TodayGroup::Me,
            Attention::WaitingOther | Attention::AwaitingAcceptance => TodayGroup::WaitingOther,
            Attention::NoNext => TodayGroup::Hidden,
        },
        Lifecycle::Closed | Lifecycle::Ignored => TodayGroup::Hidden,
    }
}

/// 状态转换请求（§3.3「允许的转换与守卫」表的代码化）。
#[derive(Debug, Clone, PartialEq)]
pub struct TransitionRequest<'a> {
    pub from: Lifecycle,
    pub to: Lifecycle,
    pub actor: Actor,
    /// 忽略/收尾原因（忽略必须带原因枚举；收尾需 close_reason，§3.1 不变量）。
    pub reason: Option<&'a str>,
    /// `closed → active` 人工重开必须携带的新 `source_record_id`。
    pub new_source_record_id: Option<&'a str>,
    /// `active → closed` 的收尾证据。
    pub resolution: Option<&'a ResolutionEvidence>,
    /// 归属锚点是否成立（V03）；人工确认可替代锚点。
    pub anchor_satisfied: bool,
    /// 人工锁（§3.3 末行：目标字段被锁时非 human 操作拒绝）。
    pub locked_fields: &'a [LockedField],
}

/// 守卫拒绝原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denial {
    /// 状态图上不存在的转换。
    InvalidTransition { from: Lifecycle, to: Lifecycle },
    /// 目标字段被人工锁锁定且触发者非人工（V07，人工优先）。
    LockedField { field: ItemField },
    /// 忽略缺少原因。
    MissingIgnoreReason,
    /// Agent 只能以 `no_action` 忽略候选（§3.4）；其余原因仅人工可用。
    AgentCannotSetIgnoreReason,
    /// 仅人工可触发的转换（人工判定无关 / 重开 / 恢复 / 收尾候选）。
    HumanOnly,
    /// 收尾缺少 close_reason（§3.1 不变量）。
    MissingCloseReason,
    /// 收尾缺少证据。
    MissingCloseEvidence,
    /// 收尾证据不在白名单（§3.3 / V05）。
    CloseEvidenceForbidden { evidence_type: ResolutionEvidenceType },
    /// `human_confirmed` 只能由可信人类操作产生：模型自称 human_confirmed 不得关单。
    HumanEvidenceFromAgent,
    /// 重开缺少新证据记录。
    MissingReopenEvidence,
    /// `candidate → active` 缺少归属锚点（V03）。
    MissingOwnershipAnchor,
}

/// 转换裁决。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionOutcome {
    Allowed,
    Denied(Denial),
}

fn non_empty(reason: Option<&str>) -> Option<&str> {
    reason.map(str::trim).filter(|s| !s.is_empty())
}

/// §3.3 状态机守卫：判定 `req` 描述的转换是否允许。
///
/// 人工锁优先（最后一行守卫）：`lifecycle` 被锁且触发者非人工时，任何生命周期变更直接拒绝；
/// 人工操作可以越过锁定（纠错语义，A11）。
pub fn evaluate_transition(req: &TransitionRequest<'_>) -> TransitionOutcome {
    let human = req.actor == Actor::Human;

    // 人工锁守卫：目标字段被锁定时，非 human 操作拒绝（§3.3 / V07）。
    if !human && req.from != req.to && is_locked(req.locked_fields, ItemField::Lifecycle) {
        return TransitionOutcome::Denied(Denial::LockedField {
            field: ItemField::Lifecycle,
        });
    }

    // §3.3 图：active 自环（新证据更新下一步/等待方）允许；其余自环无意义。
    if req.from == req.to {
        return if req.from == Lifecycle::Active {
            TransitionOutcome::Allowed
        } else {
            TransitionOutcome::Denied(Denial::InvalidTransition {
                from: req.from,
                to: req.to,
            })
        };
    }

    match (req.from, req.to) {
        // 判读产出（[*] → candidate/active/ignored）由 §3.4 映射负责，不走本守卫。
        (Lifecycle::Candidate, Lifecycle::Active) => {
            // 守卫：归属锚点存在（V03）；人工确认本身就是锚点的替代。
            if !human && !req.anchor_satisfied {
                TransitionOutcome::Denied(Denial::MissingOwnershipAnchor)
            } else {
                TransitionOutcome::Allowed
            }
        }
        (Lifecycle::Candidate, Lifecycle::Ignored) => {
            // 人工「不是我的事/普通信息」；Agent 仅可按 §3.4 以 no_action 归档轻事务。
            match non_empty(req.reason) {
                None => TransitionOutcome::Denied(Denial::MissingIgnoreReason),
                Some(reason) => {
                    if !human && IgnoreReason::parse(reason) != Some(IgnoreReason::NoAction) {
                        TransitionOutcome::Denied(Denial::AgentCannotSetIgnoreReason)
                    } else {
                        TransitionOutcome::Allowed
                    }
                }
            }
        }
        (Lifecycle::Candidate, Lifecycle::Closed) => {
            // 人工收尾：需白名单证据 + close_reason。
            if !human {
                return TransitionOutcome::Denied(Denial::HumanOnly);
            }
            close_outcome(req.resolution, req.reason, human)
        }
        (Lifecycle::Active, Lifecycle::Closed) => {
            // agent 提议 / human；必须有白名单证据（§3.3），否则降级（V05）。
            close_outcome(req.resolution, req.reason, human)
        }
        (Lifecycle::Active, Lifecycle::Ignored) => {
            // 人工判定无关：必须带原因枚举。
            if !human {
                return TransitionOutcome::Denied(Denial::HumanOnly);
            }
            match non_empty(req.reason) {
                None => TransitionOutcome::Denied(Denial::MissingIgnoreReason),
                Some(_) => TransitionOutcome::Allowed,
            }
        }
        (Lifecycle::Closed, Lifecycle::Active) => {
            // 人工重开：必须带新 source_record_id。
            if !human {
                return TransitionOutcome::Denied(Denial::HumanOnly);
            }
            match non_empty(req.new_source_record_id) {
                None => TransitionOutcome::Denied(Denial::MissingReopenEvidence),
                Some(_) => TransitionOutcome::Allowed,
            }
        }
        (Lifecycle::Ignored, Lifecycle::Active) => {
            // 人工恢复。
            if human {
                TransitionOutcome::Allowed
            } else {
                TransitionOutcome::Denied(Denial::HumanOnly)
            }
        }
        (from, to) => TransitionOutcome::Denied(Denial::InvalidTransition { from, to }),
    }
}

/// 收尾守卫：白名单证据 + close_reason（§3.1 不变量）；`human_confirmed`
/// 仅在人工触发时可信（模型自称 human_confirmed 不得关单）。
fn close_outcome(
    resolution: Option<&ResolutionEvidence>,
    reason: Option<&str>,
    human: bool,
) -> TransitionOutcome {
    if non_empty(reason).is_none() {
        return TransitionOutcome::Denied(Denial::MissingCloseReason);
    }
    match resolution {
        None => TransitionOutcome::Denied(Denial::MissingCloseEvidence),
        Some(ev) if ev.evidence_type == ResolutionEvidenceType::HumanConfirmed && !human => {
            TransitionOutcome::Denied(Denial::HumanEvidenceFromAgent)
        }
        Some(ev) if is_close_evidence_whitelisted(ev.evidence_type) => TransitionOutcome::Allowed,
        Some(ev) => TransitionOutcome::Denied(Denial::CloseEvidenceForbidden {
            evidence_type: ev.evidence_type,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req<'a>(from: Lifecycle, to: Lifecycle, actor: Actor) -> TransitionRequest<'a> {
        TransitionRequest {
            from,
            to,
            actor,
            reason: None,
            new_source_record_id: None,
            resolution: None,
            anchor_satisfied: true,
            locked_fields: &[],
        }
    }

    fn evidence(t: ResolutionEvidenceType) -> ResolutionEvidence {
        ResolutionEvidence {
            evidence_type: t,
            evidence_ids: vec!["r1".to_string()],
            note: None,
        }
    }

    #[test]
    fn current_turn_matches_section_3_2_view() {
        assert_eq!(current_turn(Attention::Me), CurrentTurn::Me);
        assert_eq!(current_turn(Attention::ReportBack), CurrentTurn::Me);
        assert_eq!(current_turn(Attention::WaitingOther), CurrentTurn::Other);
        assert_eq!(
            current_turn(Attention::AwaitingAcceptance),
            CurrentTurn::Other
        );
        assert_eq!(current_turn(Attention::NoNext), CurrentTurn::NoneTurn);
    }

    #[test]
    fn today_group_matrix() {
        use TodayGroup::*;
        // A01 第 1 阶段：active + me → 轮到我处理。
        assert_eq!(today_group(Lifecycle::Active, Attention::Me, false), Me);
        // A01 第 4 阶段：active + 待验收 + 待确认 → 仍归「等待别人」组。
        assert_eq!(
            today_group(Lifecycle::Active, Attention::AwaitingAcceptance, true),
            WaitingOther
        );
        // A06：candidate + 待确认 → 待确认组。
        assert_eq!(today_group(Lifecycle::Candidate, Attention::NoNext, true), Confirm);
        // candidate 未待确认不出现（§3.2）。
        assert_eq!(today_group(Lifecycle::Candidate, Attention::Me, false), Hidden);
        // no_next 不出现。
        assert_eq!(today_group(Lifecycle::Active, Attention::NoNext, false), Hidden);
        // 收尾/忽略不出现。
        assert_eq!(today_group(Lifecycle::Closed, Attention::NoNext, false), Hidden);
        assert_eq!(today_group(Lifecycle::Ignored, Attention::NoNext, false), Hidden);
    }

    #[test]
    fn close_verdict_follows_whitelist() {
        assert_eq!(
            close_verdict(ResolutionEvidenceType::HumanConfirmed),
            CloseVerdict::Allow
        );
        assert_eq!(
            close_verdict(ResolutionEvidenceType::ArtifactAccepted),
            CloseVerdict::Allow
        );
        assert_eq!(
            close_verdict(ResolutionEvidenceType::ExplicitComplete),
            CloseVerdict::NeedsHumanConfirm
        );
        assert_eq!(
            close_verdict(ResolutionEvidenceType::AckOnly),
            CloseVerdict::Forbid
        );
        assert_eq!(
            close_verdict(ResolutionEvidenceType::ArtifactCreated),
            CloseVerdict::Forbid
        );
        assert!(is_close_evidence_whitelisted(ResolutionEvidenceType::HumanConfirmed));
        assert!(!is_close_evidence_whitelisted(ResolutionEvidenceType::AckOnly));
    }

    #[test]
    fn transitions_allowed_by_diagram() {
        // candidate → active：人工确认（无锚点也可）与 agent 新证据（有锚点）都允许。
        let mut r = req(Lifecycle::Candidate, Lifecycle::Active, Actor::Human);
        r.anchor_satisfied = false;
        assert_eq!(evaluate_transition(&r), TransitionOutcome::Allowed);
        let r = req(Lifecycle::Candidate, Lifecycle::Active, Actor::Agent);
        assert_eq!(evaluate_transition(&r), TransitionOutcome::Allowed);
        // active 自环：新证据更新。
        assert_eq!(
            evaluate_transition(&req(Lifecycle::Active, Lifecycle::Active, Actor::Agent)),
            TransitionOutcome::Allowed
        );
        // candidate → ignored：人工带原因；agent 仅 no_action。
        let mut r = req(Lifecycle::Candidate, Lifecycle::Ignored, Actor::Human);
        r.reason = Some("not_mine");
        assert_eq!(evaluate_transition(&r), TransitionOutcome::Allowed);
        let mut r = req(Lifecycle::Candidate, Lifecycle::Ignored, Actor::Agent);
        r.reason = Some(IgnoreReason::NoAction.as_str());
        assert_eq!(evaluate_transition(&r), TransitionOutcome::Allowed);
        // active → closed：human_confirmed + 原因（A01 第 5 阶段）。
        let mut r = req(Lifecycle::Active, Lifecycle::Closed, Actor::Human);
        r.reason = Some("交办人已确认，本次追问闭环");
        let ev = evidence(ResolutionEvidenceType::HumanConfirmed);
        r.resolution = Some(&ev);
        assert_eq!(evaluate_transition(&r), TransitionOutcome::Allowed);
        // closed → active：人工 + 新证据记录。
        let mut r = req(Lifecycle::Closed, Lifecycle::Active, Actor::Human);
        r.new_source_record_id = Some("rec-new");
        assert_eq!(evaluate_transition(&r), TransitionOutcome::Allowed);
        // ignored → active：人工恢复。
        assert_eq!(
            evaluate_transition(&req(Lifecycle::Ignored, Lifecycle::Active, Actor::Human)),
            TransitionOutcome::Allowed
        );
    }

    #[test]
    fn transitions_denied_by_guards() {
        // candidate → active：agent 无锚点拒绝（V03）。
        let mut r = req(Lifecycle::Candidate, Lifecycle::Active, Actor::Agent);
        r.anchor_satisfied = false;
        assert_eq!(
            evaluate_transition(&r),
            TransitionOutcome::Denied(Denial::MissingOwnershipAnchor)
        );
        // active → closed：ack_only 不在白名单（V05 / A01 第 4 阶段）。
        let mut r = req(Lifecycle::Active, Lifecycle::Closed, Actor::Agent);
        r.reason = Some("对方说好的");
        let ev = evidence(ResolutionEvidenceType::AckOnly);
        r.resolution = Some(&ev);
        assert_eq!(
            evaluate_transition(&r),
            TransitionOutcome::Denied(Denial::CloseEvidenceForbidden {
                evidence_type: ResolutionEvidenceType::AckOnly
            })
        );
        // artifact_created 同样拒绝（文件出现 ≠ 交付满足）。
        let mut r = req(Lifecycle::Active, Lifecycle::Closed, Actor::Agent);
        r.reason = Some("文件已生成");
        let ev = evidence(ResolutionEvidenceType::ArtifactCreated);
        r.resolution = Some(&ev);
        assert!(matches!(
            evaluate_transition(&r),
            TransitionOutcome::Denied(Denial::CloseEvidenceForbidden { .. })
        ));
        // active → closed：无证据 / 无原因。
        let mut r = req(Lifecycle::Active, Lifecycle::Closed, Actor::Human);
        r.reason = Some("收尾");
        assert_eq!(
            evaluate_transition(&r),
            TransitionOutcome::Denied(Denial::MissingCloseEvidence)
        );
        let mut r = req(Lifecycle::Active, Lifecycle::Closed, Actor::Human);
        let ev = evidence(ResolutionEvidenceType::HumanConfirmed);
        r.resolution = Some(&ev);
        assert_eq!(
            evaluate_transition(&r),
            TransitionOutcome::Denied(Denial::MissingCloseReason)
        );
        // active → ignored：必须带原因；agent 一律拒绝。
        assert_eq!(
            evaluate_transition(&req(Lifecycle::Active, Lifecycle::Ignored, Actor::Human)),
            TransitionOutcome::Denied(Denial::MissingIgnoreReason)
        );
        assert_eq!(
            evaluate_transition(&req(Lifecycle::Active, Lifecycle::Ignored, Actor::Agent)),
            TransitionOutcome::Denied(Denial::HumanOnly)
        );
        // candidate → ignored：agent 不能用人工原因。
        let mut r = req(Lifecycle::Candidate, Lifecycle::Ignored, Actor::Agent);
        r.reason = Some(IgnoreReason::NotMine.as_str());
        assert_eq!(
            evaluate_transition(&r),
            TransitionOutcome::Denied(Denial::AgentCannotSetIgnoreReason)
        );
        // closed → active：agent 拒绝；人工缺新证据也拒绝。
        assert_eq!(
            evaluate_transition(&req(Lifecycle::Closed, Lifecycle::Active, Actor::Agent)),
            TransitionOutcome::Denied(Denial::HumanOnly)
        );
        assert_eq!(
            evaluate_transition(&req(Lifecycle::Closed, Lifecycle::Active, Actor::Human)),
            TransitionOutcome::Denied(Denial::MissingReopenEvidence)
        );
        // 状态图上不存在：closed → ignored、ignored → closed、candidate 自环。
        assert!(matches!(
            evaluate_transition(&req(Lifecycle::Closed, Lifecycle::Ignored, Actor::Human)),
            TransitionOutcome::Denied(Denial::InvalidTransition { .. })
        ));
        assert!(matches!(
            evaluate_transition(&req(Lifecycle::Ignored, Lifecycle::Closed, Actor::Human)),
            TransitionOutcome::Denied(Denial::InvalidTransition { .. })
        ));
        assert!(matches!(
            evaluate_transition(&req(Lifecycle::Candidate, Lifecycle::Candidate, Actor::Agent)),
            TransitionOutcome::Denied(Denial::InvalidTransition { .. })
        ));
        // 模型自称 human_confirmed 不得关单：仅可信人类操作可产生该证据。
        let mut r = req(Lifecycle::Active, Lifecycle::Closed, Actor::Agent);
        r.reason = Some("对方确认了");
        let ev = evidence(ResolutionEvidenceType::HumanConfirmed);
        r.resolution = Some(&ev);
        assert_eq!(
            evaluate_transition(&r),
            TransitionOutcome::Denied(Denial::HumanEvidenceFromAgent)
        );
        // 人工触发时 human_confirmed 可信（A01 第 5 阶段）。
        let mut r = req(Lifecycle::Active, Lifecycle::Closed, Actor::Human);
        r.reason = Some("交办人已确认");
        let ev = evidence(ResolutionEvidenceType::HumanConfirmed);
        r.resolution = Some(&ev);
        assert_eq!(evaluate_transition(&r), TransitionOutcome::Allowed);
    }

    #[test]
    fn human_lock_blocks_agent_but_not_human() {
        let locks = vec![LockedField {
            field: ItemField::Lifecycle,
            value_json: Some(Lifecycle::Ignored.to_value_json()),
            locked: true,
        }];
        // A11 第 3 阶段：agent 试图 ignored → active 被锁拒绝。
        let mut r = req(Lifecycle::Ignored, Lifecycle::Active, Actor::Agent);
        r.locked_fields = &locks;
        assert_eq!(
            evaluate_transition(&r),
            TransitionOutcome::Denied(Denial::LockedField {
                field: ItemField::Lifecycle
            })
        );
        // 人工操作越过锁定（人工锁只拦 Agent，人工优先）。
        let mut r = req(Lifecycle::Ignored, Lifecycle::Active, Actor::Human);
        r.locked_fields = &locks;
        assert_eq!(evaluate_transition(&r), TransitionOutcome::Allowed);
        // 字段值不变的 agent 自环不受影响。
        let mut r = req(Lifecycle::Active, Lifecycle::Active, Actor::Agent);
        r.locked_fields = &locks;
        assert_eq!(evaluate_transition(&r), TransitionOutcome::Allowed);
    }

    #[test]
    fn locked_field_helpers() {
        let locks = vec![
            LockedField {
                field: ItemField::BelongsToMe,
                value_json: Some("false".to_string()),
                locked: true,
            },
            LockedField {
                field: ItemField::Attention,
                value_json: None,
                locked: false,
            },
        ];
        assert!(is_locked(&locks, ItemField::BelongsToMe));
        assert!(!is_locked(&locks, ItemField::Attention));
        assert_eq!(locked_bool(&locks, ItemField::BelongsToMe), Some(false));
        assert_eq!(locked_bool(&locks, ItemField::Attention), None);
        // A11：lifecycle 锁的 value_json 是 "ignored"（JSON 字符串文本）。
        let locks = [LockedField {
            field: ItemField::Lifecycle,
            value_json: Some(Lifecycle::Ignored.to_value_json()),
            locked: true,
        }];
        assert_eq!(locks[0].value_json.as_deref(), Some("\"ignored\""));
    }
}
