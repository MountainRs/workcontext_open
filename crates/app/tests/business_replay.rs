//! 跨层业务回放集成测试（M2/B2 完成判据，详细设计 §16.2、§19 步 2）：
//!
//! 断网（mock 判读、无任何网络）回放 A01/A02/A03/A06/A11，断言——
//!
//! - **A01**：同一交办连续更新（me → waiting_other → awaiting_acceptance）；
//!   「好的」是 ack_only 不允许自动收尾（V05 降级待人工确认）；
//!   仅可信人类 `human_confirmed` 才能关单（§3.3）。
//! - **A02**：「优先」不凭空造日期（V04/R03，followup_at=null）；
//!   可咨询对象（周工）不生成事项（R01）。
//! - **A03**：one_off + needs_action=false → ignored(no_action)，不悬挂。
//! - **A06**：他人负责不进本人待办；误判 yes 被 V03 锚点强制降级 unclear →
//!   candidate 待确认；无信号群消息 skip(no_signal) 仍入库。
//! - **A11**：人工纠错（不是我的任务）写锁后，同一 input_sha256 重放被
//!   R_LOCKED 拒绝，事项不被静默恢复（PRD §13.2-3）。
//!
//! 回放结果由 input.json + mock 夹具 + core 策略链路产生；
//! `expected.json` 只作断言输入（见 `common` 的严格键解释器）。

mod common;

use common::assert_case_matches_expected;

fn stage(outcome: &workcontext_app::replay::ReplayOutcome, index: u32) -> &workcontext_app::replay::StageSnapshot {
    outcome
        .stages
        .iter()
        .find(|s| s.index == index)
        .unwrap_or_else(|| panic!("阶段 {index} 未出现"))
}

fn item_of<'a>(
    stage: &'a workcontext_app::replay::StageSnapshot,
    origin: &str,
) -> &'a workcontext_app::replay::ItemView {
    stage
        .items
        .iter()
        .find(|it| it.origin_record == origin)
        .unwrap_or_else(|| panic!("缺少源于 {origin} 的事项"))
}

#[test]
fn a01_full_lifecycle_and_human_confirmed_close() {
    let outcome = assert_case_matches_expected("A01");

    // 同一会话同一事项：5 个阶段 item_count 恒为 1（更新而非新建）。
    for s in &outcome.stages {
        assert_eq!(s.item_count, 1, "A01 阶段 {} item_count", s.index);
    }

    // 匹配留痕：阶段1 新建；阶段2–4 靠 input.json 里明示的 reply_to 显式
    // 续链（同源同会话校验）；绝不以会话作隐式线程。
    assert_eq!(stage(&outcome, 1).agent_runs[0].matched_by, None);
    for i in [2, 3, 4] {
        assert_eq!(
            stage(&outcome, i).agent_runs[0].matched_by,
            Some("reply_to"),
            "A01 阶段 {i} 应以显式 reply_to 续链"
        );
    }

    // 阶段3：本人提议收尾 = 白名单提议（explicit_complete）→ 等验收、不关单。
    let s3 = item_of(stage(&outcome, 3), "A01-R1");
    assert_eq!(s3.resolution_evidence.as_deref(), Some("explicit_complete"));
    assert_eq!(s3.lifecycle, "active");

    // 阶段4：「好的」ack_only → V05 降级：保持 active + 待人工确认 + 无收尾提议。
    let s4 = item_of(stage(&outcome, 4), "A01-R1");
    assert!(s4.awaiting_confirmation, "ack_only 必须转人工确认");
    assert_eq!(s4.resolution_evidence, None, "ack_only 不得记录为收尾证据");
    assert!(
        stage(&outcome, 4).agent_runs[0]
            .warnings
            .iter()
            .any(|w| w == "V05"),
        "ack_only 应触发 V05 告警"
    );

    // 阶段5：human_confirmed 收尾；trace 保留 close_reason。
    let s5 = item_of(stage(&outcome, 5), "A01-R1");
    assert_eq!(s5.lifecycle, "closed");
    assert_eq!(s5.resolution_evidence.as_deref(), Some("human_confirmed"));
    assert!(s5.close_reason.as_deref().map(|r| !r.is_empty()).unwrap_or(false));

    // 全程不凭空造时间。
    for s in &outcome.stages {
        for it in &s.items {
            assert_eq!(it.followup_at, None, "A01 不得产出跟进时间");
            assert_eq!(it.followup_type, "none");
        }
    }
}

#[test]
fn a02_priority_word_never_becomes_deadline() {
    let outcome = assert_case_matches_expected("A02");
    // 「优先」只有 V04 验证通过的 followup 才能落时间：本例全程 none/null。
    for it in &outcome.final_snapshot.items {
        assert_eq!(it.followup_at, None);
        assert_eq!(it.followup_type, "none");
    }
    // 唯一事项源于领导来件（A02-R1），不为咨询对象周工建项。
    assert_eq!(outcome.final_snapshot.item_count, 1);
    assert_eq!(outcome.final_snapshot.items[0].origin_record, "A02-R1");
}

#[test]
fn a03_one_off_closes_without_hanging() {
    let outcome = assert_case_matches_expected("A03");
    let final_ = &outcome.final_snapshot;
    assert_eq!(final_.counts.active, 0);
    assert_eq!(final_.counts.candidate, 0);
    assert_eq!(final_.counts.ignored, 1);
    let item = &final_.items[0];
    assert_eq!(item.ignore_reason.as_deref(), Some("no_action"));
    assert!(!item.awaiting_confirmation, "轻事务闭环不得进待确认");
    assert!(!item.today_visible);
}

#[test]
fn a06_v03_anchor_downgrades_wrong_owner() {
    let outcome = assert_case_matches_expected("A06");
    let s1 = stage(&outcome, 1);

    // 误判样本：mock 声称 yes，但三锚点全缺 → V03 强制 unclear → candidate。
    let misjudged = &s1.agent_runs[1];
    assert_eq!(misjudged.raw_belongs_to_me.as_deref(), Some("yes"));
    assert_eq!(misjudged.belongs_to_me.as_deref(), Some("unclear"));
    assert!(misjudged.warnings.iter().any(|w| w == "V03"));
    let item = item_of(s1, "A06-R3");
    assert_eq!(item.lifecycle, "candidate");
    assert!(item.awaiting_confirmation);
    assert_eq!(item.today_group, "confirm", "归属不明只进待确认组");

    // 正确负例：答复本人的信息（R2）不建事项；无信号消息仍入库 skipped。
    let r2 = s1.records.iter().find(|r| r.external_id == "A06-R2").unwrap();
    assert_eq!(r2.judge_state, "judged");
    for id in ["A06-R0", "A06-R1"] {
        let rec = s1.records.iter().find(|r| r.external_id == id).unwrap();
        assert_eq!(rec.prefilter, "skip");
        assert_eq!(rec.prefilter_reason, "no_signal");
        assert_eq!(rec.judge_state, "skipped", "skip 记录仍入库留痕");
    }
    assert_eq!(outcome.final_snapshot.counts.active, 0, "A06 不得有 active 事项");
}

#[test]
fn a11_correction_lock_blocks_replay_restore() {
    let outcome = assert_case_matches_expected("A11");

    // 阶段3 重放与阶段1 同一 input_sha256（判读身份不变），输出相同，
    // 但人工锁已落 → 整次拒绝 R_LOCKED，事项保持 ignored。
    let s1_run = &stage(&outcome, 1).agent_runs[0];
    let s3_run = &stage(&outcome, 3).agent_runs[0];
    assert_eq!(s1_run.input_sha256, s3_run.input_sha256, "同批重跑判读身份不变");
    assert_eq!(s3_run.status, "rejected");
    assert_eq!(s3_run.rejection_rule.as_deref(), Some("R_LOCKED"));

    let item = item_of(stage(&outcome, 3), "A11-R1");
    assert_eq!(item.lifecycle, "ignored", "人工纠正不得被静默恢复");
    assert!(!item.today_visible);
    assert_eq!(item.locked_fields, vec!["belongs_to_me".to_string(), "lifecycle".to_string()]);

    // 重放批次靠证据链交集命中被锁事项（ignored 也命中），非会话回退。
    assert_eq!(s3_run.matched_by, Some("evidence"));

    // 事件序列：建项（agent）→ 纠正（human）→ 拒绝（agent）。
    let events: Vec<_> = outcome
        .final_snapshot
        .work_events
        .iter()
        .map(|e| (e.event_type, e.actor))
        .collect();
    assert_eq!(
        events,
        vec![("created", "agent"), ("corrected", "human"), ("rejected", "agent")]
    );
}
