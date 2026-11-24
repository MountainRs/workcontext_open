//! 回放用内存业务状态（M2/B2）：`source_record` / `work_item` / `agent_run` /
//! `work_event` / `field_override` 五张关键表的进程内等价物。
//!
//! store 仓储在后续里程碑接入；结构与 `migrations/0001_init.sql` 的列一一
//! 对应（字段名即列名），保证将来可以原样落库。所有写入都来自
//! [`crate::replay`] 的链路结果（判读校验、人工动作），不从夹具期望生成。

use crate::prefilter::PrefilterVerdict;
use workcontext_core::{
    Actor, Attention, Correction, DbFollowupType, ExistingItem, ItemDecision, ItemField,
    Lifecycle, LockedField, TransitionRequest, ValidatedJudgement, WriteMode, correction_locks,
    evaluate_transition,
};
/// `source_record.judge_state`（迁移契约取值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JudgeState {
    Pending,
    Kept,
    Skipped,
    Judged,
    Rejected,
}

impl JudgeState {
    pub fn as_str(&self) -> &'static str {
        match self {
            JudgeState::Pending => "pending",
            JudgeState::Kept => "kept",
            JudgeState::Skipped => "skipped",
            JudgeState::Judged => "judged",
            JudgeState::Rejected => "rejected",
        }
    }
}

/// 一条已入库的来源记录（预筛结果 + 判读上下文元数据 + 显式引用）。
pub struct StoredRecord {
    pub external_id: String,
    pub prefilter: PrefilterVerdict,
    pub judge_state: JudgeState,
    /// `reply_to.external_id`：续链的唯一显式依据（无则本记录不指向任何线程）。
    pub reply_to: Option<String>,
    pub context: workcontext_core::RecordContext,
}

/// `work_event` 行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkEvent {
    pub event_type: &'static str,
    pub actor: Actor,
}

/// `field_override` 行（人工锁）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldOverride {
    pub field: ItemField,
    pub value_json: Option<String>,
    pub locked: bool,
}
/// 批次与既有事项的命中依据（透明留痕，进 `RunView.matched_by`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchReason {
    /// 批内记录本身在事项证据链（link_evidence origin/reference）上。
    Evidence,
    /// 批内记录的 `reply_to`（同源同会话、已校验）指向事项证据链上的记录。
    ReplyTo,
}

impl MatchReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            MatchReason::Evidence => "evidence",
            MatchReason::ReplyTo => "reply_to",
        }
    }
}
/// `work_item` 行 + 回放所需派生键。
#[derive(Debug, Clone)]
pub struct WorkItem {
    pub id: String,
    pub title: String,
    pub lifecycle: Lifecycle,
    pub attention: Attention,
    pub next_action: Option<String>,
    pub followup_type: DbFollowupType,
    pub followup_at: Option<i64>,
    pub followup_note: Option<String>,
    pub awaiting_confirmation: bool,
    pub confidence: u8,
    pub ignore_reason: Option<String>,
    /// Agent 收尾提议 / 人工收尾证据类型（`explicit_complete` / `human_confirmed`…）。
    pub resolution_evidence: Option<String>,
    pub close_reason: Option<String>,
    /// 创建时的判读意图（`identity.intent_class` 断言口径）。
    pub origin_intent_class: String,
    pub origin_record_id: String,
    pub conversation_id: Option<String>,
    pub dedupe_key: String,
    pub last_evidence_at: i64,
    pub locked_fields: Vec<LockedField>,
    /// 该事项累积的全部证据记录（`link_evidence(origin/reference)` 链的内存
    /// 等价）：创建批 evidence + 每次更新批 evidence。会话匹配只认这条链。
    pub evidence_records: Vec<String>,
}

impl WorkItem {
    /// 供 [`workcontext_core::PolicyValidator`] 更新路径使用的既有事项视图。
    pub fn to_existing(&self) -> ExistingItem {
        ExistingItem {
            id: self.id.clone(),
            lifecycle: self.lifecycle,
            attention: self.attention,
            title: self.title.clone(),
            next_action: self.next_action.clone(),
            followup_type: self.followup_type,
            followup_at: self.followup_at,
            awaiting_confirmation: self.awaiting_confirmation,
            resolution_evidence: self.resolution_evidence.clone(),
            last_evidence_at: Some(self.last_evidence_at),
            locked_fields: self.locked_fields.clone(),
        }
    }
}

/// 一个 case 的全部内存业务状态。
#[derive(Default)]
pub struct CaseState {
    pub items: Vec<WorkItem>,
    pub events: Vec<WorkEvent>,
    pub overrides: Vec<FieldOverride>,
    next_item_seq: u32,
}

impl CaseState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 判读批次 → 既有事项的匹配（详细设计 §4.2 `link_evidence`、§5.1 V10）。
    ///
    /// **只认显式证据关联，绝不以会话作隐式线程**（私聊同样可并行两个交办）：
    ///
    /// 1. `evidence`：批内记录本身出现在某事项的证据链（origin/reference，
    ///    即 `WorkItem.evidence_records`）中 → 命中；**不限生命周期**（A11
    ///    阶段3 重放必须命中被人工纠正的 ignored 事项才能触发 R_LOCKED）。
    /// 2. `reply_to`：批内记录显式引用了某事项证据链中的记录，且引用目标
    ///    经校验真实存在、**同 source 且同 conversation**（跨源/跨会话引用
    ///    无效）→ 命中。这是 A01 阶段2–4 在 input.json 里明示的续链方式。
    /// 3. 其余（含「同会话但无引用」）一律新建，待 V06/人工归并。
    pub fn find_matching(
        &self,
        batch: &[&StoredRecord],
        records: &[StoredRecord],
    ) -> Option<(usize, MatchReason)> {
        // ① 直接证据：批内记录本身在事项证据链上。
        if let Some(idx) = self.items.iter().rposition(|it| {
            batch
                .iter()
                .any(|b| it.evidence_records.contains(&b.external_id))
        }) {
            return Some((idx, MatchReason::Evidence));
        }
        // ② 显式引用链：reply_to 指向事项证据（引用目标须同源同会话）。
        let reply_links: Vec<&str> = batch
            .iter()
            .filter_map(|b| {
                let target_id = b.reply_to.as_deref()?;
                let target = records
                    .iter()
                    .find(|r| r.external_id == target_id)?;
                let same_source = target.context.source_id == b.context.source_id;
                let same_conversation = target.context.conversation_id == b.context.conversation_id;
                (same_source && same_conversation).then_some(target_id)
            })
            .collect();
        if reply_links.is_empty() {
            return None;
        }
        self.items
            .iter()
            .rposition(|it| {
                it.evidence_records
                    .iter()
                    .any(|e| reply_links.contains(&e.as_str()))
            })
            .map(|idx| (idx, MatchReason::ReplyTo))
    }

    /// 应用一次校验通过的判读：建项或更新，返回写入的事件。
    pub fn apply_judgement(&mut self, judgement: &ValidatedJudgement) -> Vec<WorkEvent> {
        let Some(decision) = &judgement.item else {
            return Vec::new();
        };
        let events = match &decision.mode {
            WriteMode::Create => {
                self.next_item_seq += 1;
                self.items.push(self.item_from_decision(
                    format!("item-{:04}", self.next_item_seq),
                    decision,
                    judgement,
                ));
                vec![WorkEvent { event_type: "created", actor: Actor::Agent }]
            }
            WriteMode::Update(id) => {
                let Some(item) = self.items.iter_mut().find(|it| it.id == *id) else {
                    return Vec::new();
                };
                Self::write_decision(item, decision);
                Self::link_evidence(item, judgement);
                vec![WorkEvent { event_type: "updated", actor: Actor::Agent }]
            }
        };
        self.events.extend(events.iter().cloned());
        events
    }

    /// 判读被拒绝级规则拦截（V01/V02/V11/R_LOCKED）：业务字段不变，
    /// 只追加 `work_event(rejected, agent)` 供期末审计序列（§5）。
    pub fn record_rejection(&mut self) {
        self.events.push(WorkEvent { event_type: "rejected", actor: Actor::Agent });
    }

    fn item_from_decision(
        &self,
        id: String,
        decision: &ItemDecision,
        judgement: &ValidatedJudgement,
    ) -> WorkItem {
        let mut item = WorkItem {
            id,
            title: decision.title.clone(),
            lifecycle: decision.lifecycle,
            attention: decision.attention,
            next_action: decision.next_action.clone(),
            followup_type: decision.followup_type,
            followup_at: decision.followup_at,
            followup_note: decision.followup_note.clone(),
            awaiting_confirmation: decision.awaiting_confirmation,
            confidence: decision.confidence,
            ignore_reason: decision.ignore_reason.map(|r| r.as_str().to_string()),
            resolution_evidence: None,
            close_reason: None,
            origin_intent_class: judgement.intent_class.as_str().to_string(),
            origin_record_id: judgement.origin_external_id.clone(),
            conversation_id: judgement.conversation_id.clone(),
            dedupe_key: judgement.dedupe_key.clone(),
            last_evidence_at: decision.last_evidence_at,
            locked_fields: Vec::new(),
            evidence_records: {
                let mut ids = judgement.evidence_ids.clone();
                ids.sort();
                ids.dedup();
                ids
            },
        };
        if let Some(res) = &decision.resolution {
            item.resolution_evidence = Some(res.evidence_type.as_str().to_string());
        }
        item
    }

    /// 更新路径把本批证据并入事项证据链（link_evidence reference 的内存等价，
    /// 去重保序）。
    fn link_evidence(item: &mut WorkItem, judgement: &ValidatedJudgement) {
        for id in &judgement.evidence_ids {
            if !item.evidence_records.contains(id) {
                item.evidence_records.push(id.clone());
            }
        }
    }

    /// 更新路径只写决策字段；title 由 core 保持稳定不重写。
    fn write_decision(item: &mut WorkItem, decision: &ItemDecision) {
        item.title = decision.title.clone();
        item.lifecycle = decision.lifecycle;
        item.attention = decision.attention;
        item.next_action = decision.next_action.clone();
        item.followup_type = decision.followup_type;
        item.followup_at = decision.followup_at;
        item.followup_note = decision.followup_note.clone();
        item.awaiting_confirmation = decision.awaiting_confirmation;
        item.confidence = decision.confidence;
        item.ignore_reason = decision.ignore_reason.map(|r| r.as_str().to_string());
        item.last_evidence_at = decision.last_evidence_at;
        item.resolution_evidence = decision
            .resolution
            .as_ref()
            .map(|res| res.evidence_type.as_str().to_string());
    }

    /// 人工纠错（§9）：校验语义 → 推导锁 → 走状态机守卫 → 落锁与事件。
    pub fn apply_correction(
        &mut self,
        item_id: &str,
        correction: &Correction,
    ) -> Result<Vec<WorkEvent>, String> {
        workcontext_core::validate_correction(correction).map_err(|e| e.to_string())?;
        let Some(item) = self.items.iter().find(|it| it.id == item_id) else {
            return Err(format!("纠错目标事项不存在：{item_id}"));
        };
        // §9：纠错可改 lifecycle（A11）或仅改 attention；改 lifecycle 必须过守卫。
        if let Some(target) = correction.lifecycle {
            if target != item.lifecycle {
                let req = TransitionRequest {
                    from: item.lifecycle,
                    to: target,
                    actor: Actor::Human,
                    reason: Some(correction.reason.as_str()),
                    new_source_record_id: None,
                    resolution: None,
                    anchor_satisfied: true,
                    locked_fields: &[],
                };
                match evaluate_transition(&req) {
                    workcontext_core::TransitionOutcome::Allowed => {}
                    workcontext_core::TransitionOutcome::Denied(denial) => {
                        return Err(format!("纠错转换被守卫拒绝：{denial:?}"));
                    }
                }
                let Some(item) = self.items.iter_mut().find(|it| it.id == item_id) else {
                    unreachable!("上面已确认存在");
                };
                item.lifecycle = target;
                if target == Lifecycle::Ignored {
                    item.ignore_reason = Some(correction.reason.clone());
                    item.awaiting_confirmation = false;
                }
            }
        }
        if let Some(attention) = correction.attention {
            if let Some(item) = self.items.iter_mut().find(|it| it.id == item_id) {
                item.attention = attention;
            }
        }

        let locks = correction_locks(correction)
            .into_iter()
            .map(|field| LockedField {
                value_json: lock_value(field, correction),
                field,
                locked: true,
            })
            .collect::<Vec<_>>();
        self.overrides.extend(locks.iter().map(|l| FieldOverride {
            field: l.field,
            value_json: l.value_json.clone(),
            locked: l.locked,
        }));
        if let Some(item) = self.items.iter_mut().find(|it| it.id == item_id) {
            item.locked_fields = locks;
        }
        let events = vec![WorkEvent { event_type: "corrected", actor: Actor::Human }];
        self.events.extend(events.iter().cloned());
        Ok(events)
    }

    /// 人工收尾（A01 阶段5）：`human_confirmed` 是可信人类证据，走守卫后落
    /// `closed` + `close_reason` + `resolution_evidence`。
    pub fn apply_human_close(
        &mut self,
        item_id: &str,
        evidence: &workcontext_core::ResolutionEvidence,
        note: &str,
    ) -> Result<Vec<WorkEvent>, String> {
        let Some(item) = self.items.iter().find(|it| it.id == item_id) else {
            return Err(format!("收尾目标事项不存在：{item_id}"));
        };
        let req = TransitionRequest {
            from: item.lifecycle,
            to: Lifecycle::Closed,
            actor: Actor::Human,
            reason: Some(note),
            new_source_record_id: None,
            resolution: Some(evidence),
            anchor_satisfied: true,
            locked_fields: &[],
        };
        match evaluate_transition(&req) {
            workcontext_core::TransitionOutcome::Allowed => {}
            workcontext_core::TransitionOutcome::Denied(denial) => {
                return Err(format!("人工收尾被守卫拒绝：{denial:?}"));
            }
        }
        let Some(item) = self.items.iter_mut().find(|it| it.id == item_id) else {
            unreachable!("上面已确认存在");
        };
        item.lifecycle = Lifecycle::Closed;
        item.close_reason = Some(note.to_string());
        item.resolution_evidence = Some(evidence.evidence_type.as_str().to_string());
        item.awaiting_confirmation = false;
        let events = vec![WorkEvent { event_type: "closed", actor: Actor::Human }];
        self.events.extend(events.iter().cloned());
        Ok(events)
    }

    /// 按来源记录 ID 定位事项（`human_actions[].item_origin_record` 的解析）。
    pub fn item_id_by_origin(&self, origin_record: &str) -> Option<&str> {
        self.items
            .iter()
            .rfind(|it| it.origin_record_id == origin_record)
            .map(|it| it.id.as_str())
    }
}

/// `field_override.value_json` 的纠错取值（与 expected 断言口径一致：
/// 布尔裸文本、枚举 JSON 引号文本）。
fn lock_value(field: ItemField, correction: &Correction) -> Option<String> {
    match field {
        ItemField::BelongsToMe => correction.belongs_to_me.map(|b| b.to_string()),
        ItemField::Lifecycle => correction.lifecycle.map(|l| l.to_value_json()),
        ItemField::Attention => correction
            .attention
            .map(|a| format!("\"{}\"", a.as_str())),
        _ => None,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use workcontext_core::{ConversationType, Direction, RecordContext, RecordKind};

    fn item(id: &str, conv: &str, lifecycle: Lifecycle, evidence: &[&str]) -> WorkItem {
        WorkItem {
            id: id.to_string(),
            title: format!("{id} 标题"),
            lifecycle,
            attention: Attention::Me,
            next_action: Some("下一步".to_string()),
            followup_type: DbFollowupType::None,
            followup_at: None,
            followup_note: None,
            awaiting_confirmation: false,
            confidence: 80,
            ignore_reason: None,
            resolution_evidence: None,
            close_reason: None,
            origin_intent_class: "followup".to_string(),
            origin_record_id: evidence[0].to_string(),
            conversation_id: Some(conv.to_string()),
            dedupe_key: format!("key-{id}"),
            last_evidence_at: 0,
            locked_fields: Vec::new(),
            evidence_records: evidence.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn record(id: &str, source: &str, conv: &str, reply_to: Option<&str>) -> StoredRecord {
        StoredRecord {
            external_id: id.to_string(),
            prefilter: crate::prefilter::PrefilterVerdict::Keep("dm_in"),
            judge_state: JudgeState::Judged,
            reply_to: reply_to.map(str::to_string),
            context: RecordContext {
                id: id.to_string(),
                source_id: source.to_string(),
                external_id: id.to_string(),
                kind: RecordKind::ImMessage,
                conversation_id: Some(conv.to_string()),
                conversation_type: ConversationType::Dm,
                direction: Direction::In,
                sender_id: None,
                mentions: Vec::new(),
                occurred_at: 0,
                ingested_at: 0,
                summary: None,
                body: None,
            },
        }
    }

    #[test]
    fn same_group_two_items_never_merge_by_conversation() {
        // 反例：同群并行两个交办（各自独立证据链）。新群消息与两条链都无
        // 交集、也无 reply_to → 不命中任何一个，必须新建（V06/人工归并）。
        let mut state = CaseState::new();
        state.items = vec![
            item("item-0001", "grp-ops", Lifecycle::Active, &["G-R1", "G-R2"]),
            item("item-0002", "grp-ops", Lifecycle::Active, &["G-R3", "G-R4"]),
        ];
        let records = vec![
            record("G-R1", "src", "grp-ops", None),
            record("G-R4", "src", "grp-ops", None),
            record("G-R9", "src", "grp-ops", None),
        ];
        let batch = vec![&records[2]];
        assert_eq!(state.find_matching(&batch, &records), None, "同会话无证据关联 → 新建");

        // 批内记录本身在证据链上 → 命中对应事项（而非「最新的那个」）。
        let batch = vec![&records[1]];
        let (idx, reason) = state
            .find_matching(&batch, &records)
            .expect("证据链命中 item-0002");
        assert_eq!(state.items[idx].id, "item-0002");
        assert_eq!(reason, MatchReason::Evidence);
    }

    #[test]
    fn dm_second_item_without_reference_is_never_merged() {
        // 反例：私聊同样可并行两个交办。已有开放事项 + 新私信无任何显式
        // 引用 → 绝不因「同私聊、只有一个开放事项」而续链（无隐式线程）。
        let mut state = CaseState::new();
        state.items = vec![item("item-0001", "dm-li", Lifecycle::Active, &["D-R1", "D-R2"])];
        let records = vec![
            record("D-R1", "src", "dm-li", None),
            record("D-R2", "src", "dm-li", None),
            record("D-R9", "src", "dm-li", None),
        ];
        let batch = vec![&records[2]];
        assert_eq!(
            state.find_matching(&batch, &records),
            None,
            "私聊无显式引用 → 新建第二事项，不续链"
        );
    }

    #[test]
    fn validated_reply_to_chain_hits_target_item() {
        // A01 阶段2：R3 显式 reply_to R2（同源同会话）→ 续链到 R1/R2 的事项。
        let mut state = CaseState::new();
        state.items = vec![item("item-0001", "dm-li", Lifecycle::Active, &["D-R1", "D-R2"])];
        let records = vec![
            record("D-R1", "src", "dm-li", None),
            record("D-R2", "src", "dm-li", None),
            record("D-R3", "src", "dm-li", Some("D-R2")),
        ];
        let batch = vec![&records[2]];
        let (idx, reason) = state
            .find_matching(&batch, &records)
            .expect("显式引用链应命中");
        assert_eq!(state.items[idx].id, "item-0001");
        assert_eq!(reason, MatchReason::ReplyTo);

        // 跨会话引用无效：reply_to 指向另一会话的记录 → 不命中。
        let records_cross = vec![
            record("D-R1", "src", "dm-li", None),
            record("X-R1", "src", "dm-other", None),
            record("D-R3", "src", "dm-li", Some("X-R1")),
        ];
        let batch = vec![&records_cross[2]];
        assert_eq!(state.find_matching(&batch, &records_cross), None, "跨会话引用无效");

        // 跨源引用无效：同会话但不同 source → 不命中。
        let records_src = vec![
            record("D-R1", "src-a", "dm-li", None),
            record("D-R2", "src-a", "dm-li", None),
            record("D-R3", "src-b", "dm-li", Some("D-R2")),
        ];
        let batch = vec![&records_src[2]];
        assert_eq!(state.find_matching(&batch, &records_src), None, "跨源引用无效");

        // 引用目标不存在 → 不命中（不猜测）。
        let records_ghost = vec![record("D-R3", "src", "dm-li", Some("GHOST"))];
        let batch = vec![&records_ghost[0]];
        assert_eq!(state.find_matching(&batch, &records_ghost), None, "幽灵引用无效");
    }

    #[test]
    fn evidence_match_ignores_lifecycle_for_locked_replay() {
        // A11 阶段3：人工纠正后事项 ignored，重放批次仍须命中它（触发 R_LOCKED）。
        let mut state = CaseState::new();
        state.items = vec![item("item-0001", "dm-qian", Lifecycle::Ignored, &["Q-R1"])];
        let records = vec![record("Q-R1", "src", "dm-qian", None)];
        let batch = vec![&records[0]];
        let (idx, reason) = state
            .find_matching(&batch, &records)
            .expect("证据命中被锁事项");
        assert_eq!(state.items[idx].id, "item-0001");
        assert_eq!(reason, MatchReason::Evidence);
    }
}
