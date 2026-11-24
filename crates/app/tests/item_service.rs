//! app 独立集成测试：SQLite 判读事务 + 事项用例（临时库，不碰默认数据库）。
//!
//! 数据来源只有合成夹具（`fixtures/cases`）经应用服务导入
//! （`judge::import_case`），判读经真实 `PolicyValidator` + 可回放
//! `MockAgentRuntime`（与 `fixtures/mock/manifest.json` 同哈希）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use workcontext_app::items::{
    item_close, item_correct, item_get, item_set_followup, items_query, today_list,
    CloseArgs, CorrectionArgs, ItemsQueryArgs, SetFollowupArgs,
};
use workcontext_app::judge::{
    import_case, judge_planned_batch, plan_case, PlannedCase,
};
use workcontext_core::PolicyValidator;
use workcontext_infra::mock::MockAgentRuntime;
use workcontext_store::rusqlite::{params, Connection};
use workcontext_store::open;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("仓库根目录可定位")
}

static SEQ: AtomicU64 = AtomicU64::new(0);

/// 夹具判读时钟（A01/A11 case clock：2026-11-21 09:50 +08:00 的 UTC 毫秒）。
const NOW: i64 = 1_763_689_800_000;

/// 临时库：合成数据只进这里，测试结束整目录删除。
struct TempDb {
    dir: PathBuf,
    conn: Option<Connection>,
}

impl TempDb {
    fn open(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "workcontext-app-items-{}-{tag}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录失败");
        let conn = open(&dir.join("workcontext.sqlite3")).expect("打开临时库失败");
        Self { dir, conn: Some(conn) }
    }

    fn conn(&mut self) -> &mut Connection {
        self.conn.as_mut().expect("连接尚在")
    }

    /// 关闭再重开：验证重启后人工锁/事项状态完全来自库，不靠内存。
    fn restart(&mut self) {
        self.conn = None;
        self.conn = Some(open(&self.dir.join("workcontext.sqlite3")).expect("重开临时库失败"));
    }
}

struct Rig {
    db: TempDb,
    runtime: MockAgentRuntime,
    validator: PolicyValidator,
    planned: PlannedCase,
}

fn rig(case_id: &str) -> Rig {
    let mut db = TempDb::open(case_id);
    let root = repo_root().join("fixtures");
    let summary = import_case(db.conn(), &root, case_id, NOW).expect("导入失败");
    assert!(summary.inserted_records > 0, "case 必须有记录");
    let planned = plan_case(&root, case_id).expect("批次规划失败");
    let runtime = MockAgentRuntime::open(&root).expect("mock runtime 加载失败");
    Rig { db, runtime, validator: PolicyValidator::new(), planned }
}

impl Rig {
    fn judge(&mut self, stage: u32) -> workcontext_app::judge::BatchReport {
        let batch = self
            .planned
            .batches
            .iter()
            .find(|b| b.stage_index == stage)
            .cloned()
            .expect("批次存在");
        // 独立批次的 input/context 已在 PlannedBatch 内克隆持有。
        let report = judge_planned_batch(
            self.db.conn(),
            &self.runtime,
            &self.validator,
            &self.planned,
            &batch,
            NOW,
        )
        .expect("判读失败");
        report
    }
}



fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).expect("计数查询失败")
}

// ---------- A01：全链路 + today + 人工收尾 ----------

#[test]
fn a01_full_flow_single_item_today_and_close() {
    let mut rig = rig("A01");
    // 阶段1：建 active 事项（attention=me）。
    let r1 = rig.judge(1);
    assert_eq!(r1.status, "ok");
    let item_id = r1.item_id.clone().expect("应建事项");
    let detail = item_get(rig.db.conn(), &item_id).expect("详情");
    assert_eq!(detail.item.lifecycle, "active");
    assert_eq!(detail.item.attention, "me");
    assert!(!detail.evidence.is_empty(), "创建批证据应入 link_evidence");
    assert!(!detail.item.awaiting_confirmation);

    // today：轮到我组出现，新鲜度 unknown（未写同步游标，不臆断）。
    let view = today_list(rig.db.conn(), NOW).expect("today");
    assert_eq!(view.groups.mine.len(), 1);
    assert_eq!(view.groups.waiting.len(), 0);
    assert_eq!(view.freshness.state, "unknown");
    assert!(view.freshness.last_success_at.is_none());
    assert!(!view.groups.mine[0].next_action.is_empty());
    assert_eq!(view.groups.mine[0].source_ref.kind, "im_message");

    // 阶段2-4：reply_to 显式续链 → 更新同一事项，不重复建项。
    for stage in [2u32, 3, 4] {
        let r = rig.judge(stage);
        assert_eq!(r.status, "ok", "阶段 {stage} 判读应成功");
        assert_eq!(r.item_id.as_deref(), Some(item_id.as_str()), "阶段 {stage} 应更新同一事项");
        assert_eq!(r.matched_by, Some("reply_to"), "阶段 {stage} 应走显式续链");
    }
    assert_eq!(count(rig.db.conn(), "SELECT COUNT(*) FROM work_item"), 1);

    // 分组与 fixtures/cases/A01/expected.json 对齐：阶段2 waiting_other、阶段3
    // awaiting_acceptance、阶段4 ack_only 降级 → active + awaiting_acceptance +
    // awaiting_confirmation=true，仍在 waiting_other 组（不是 confirm 组）。
    let view = today_list(rig.db.conn(), NOW).expect("today");
    assert_eq!(view.groups.waiting.len(), 1, "阶段4 应在「等待别人」组");
    assert_eq!(view.groups.confirm.len(), 0, "active 事项的待确认不进 confirm 组");
    assert_eq!(view.groups.mine.len(), 0);
    let detail = item_get(rig.db.conn(), &item_id).expect("详情");
    assert_eq!(detail.item.attention, "awaiting_acceptance");
    assert!(detail.item.awaiting_confirmation);
    // whyHere 取证据原文摘要，不生成套话；nextAction 非空。
    assert!(!view.groups.waiting[0].why_here.is_empty());
    assert!(!view.groups.waiting[0].next_action.is_empty());
    // input_refs_json 带 source_record 真实主键与 external_id 两种 token。
    let refs: String = rig
        .db
        .conn()
        .query_row("SELECT input_refs_json FROM agent_run WHERE status = 'ok' LIMIT 1", [], |r| r.get(0))
        .expect("refs");
    let refs: serde_json::Value = serde_json::from_str(&refs).expect("refs json");
    let pk: String = rig
        .db
        .conn()
        .query_row("SELECT id FROM source_record WHERE external_id = 'A01-R1'", [], |r| r.get(0))
        .expect("pk");
    assert_eq!(refs["sourceRecordIds"][0].as_str(), Some(pk.as_str()));
    assert_eq!(refs["externalIds"][0].as_str(), Some("A01-R1"));

    // 人工收尾（human_confirmed 可信证据）→ closed + 今日页消失。
    let closed = item_close(
        rig.db.conn(),
        &CloseArgs {
            id: item_id.clone(),
            evidence_type: "human_confirmed".to_string(),
            note: Some("交办人线下确认完成".to_string()),
        },
        NOW,
    )
    .expect("收尾");
    assert_eq!(closed.item.lifecycle, "closed");
    assert_eq!(closed.item.resolution_evidence.as_deref(), Some("human_confirmed"));
    assert!(closed.item.closed_at.is_some());
    let view = today_list(rig.db.conn(), NOW).expect("today");
    assert!(view.groups.mine.is_empty() && view.groups.waiting.is_empty() && view.groups.confirm.is_empty());

    // 幂等重放：同输入已 ok → skipped，不重复建项/计费。
    let replay = rig.judge(1);
    assert_eq!(replay.status, "skipped");
    assert_eq!(count(rig.db.conn(), "SELECT COUNT(*) FROM work_item"), 1);
    assert_eq!(
        count(rig.db.conn(), "SELECT COUNT(*) FROM agent_run WHERE status = 'ok'"),
        4,
        "每批恰一条 ok"
    );
}

// ---------- A11：人工锁跨重启拒绝重放恢复 ----------

#[test]
fn a11_correction_lock_blocks_replay_after_restart() {
    let mut rig = rig("A11");
    let r1 = rig.judge(1);
    assert_eq!(r1.status, "ok");
    let item_id = r1.item_id.clone().expect("应建事项");
    assert_eq!(today_list(rig.db.conn(), NOW).expect("today").groups.mine.len(), 1);

    // UI wire payload 原样（IPC §2「不是我的事」）：只表态 belongsToMe=false。
    let wire = serde_json::json!({
        "id": item_id,
        "patch": { "belongsToMe": false },
        "reason": "not_mine",
        "note": "已口头改由李四接手推进"
    });
    let args: CorrectionArgs = serde_json::from_value(wire).expect("wire payload 可反序列化");
    let corrected = item_correct(rig.db.conn(), &args, NOW).expect("纠错");
    // 服务端归一化：ignored + no_next；只锁 belongs_to_me 与 lifecycle（A11）。
    assert_eq!(corrected.item.lifecycle, "ignored");
    assert_eq!(corrected.item.attention, "no_next");
    assert_eq!(
        corrected.item.locked_fields,
        vec!["belongs_to_me".to_string(), "lifecycle".to_string()]
    );
    assert_eq!(corrected.corrections.len(), 1);
    assert_eq!(corrected.corrections[0].reason, "not_mine");
    assert_eq!(
        corrected.corrections[0].fields,
        vec!["belongs_to_me".to_string(), "lifecycle".to_string()]
    );
    // 立即不出现在今日页任何一组。
    let view = today_list(rig.db.conn(), NOW).expect("today");
    assert!(view.groups.mine.is_empty() && view.groups.waiting.is_empty() && view.groups.confirm.is_empty());

    // 重启：锁与状态必须完全来自库（重开连接，内存里没有任何残留）。
    rig.db.restart();
    let detail = item_get(rig.db.conn(), &item_id).expect("重启后详情");
    assert_eq!(detail.item.lifecycle, "ignored", "重启不得丢失人工状态");
    assert_eq!(detail.item.attention, "no_next");
    assert_eq!(detail.item.locked_fields.len(), 2);

    // 同 input_sha256 重放（judge_retry）：R_LOCKED 整次拒绝，事项不得被静默恢复。
    let replay = rig.judge(1);
    assert_eq!(replay.status, "rejected");
    assert_eq!(replay.rejection_rule.as_deref(), Some("R_LOCKED"));
    let after = item_get(rig.db.conn(), &item_id).expect("重放后详情");
    assert_eq!(after.item.lifecycle, "ignored", "人工锁必须挡住 Agent 恢复");
    assert_eq!(after.item.attention, "no_next");
    assert_eq!(after.item.locked_fields.len(), 2);
    assert_eq!(
        count(
            rig.db.conn(),
            "SELECT COUNT(*) FROM agent_run WHERE status = 'rejected' AND rejection_rule = 'R_LOCKED'"
        ),
        1
    );
    assert_eq!(count(rig.db.conn(), "SELECT COUNT(*) FROM agent_run WHERE status = 'ok'"), 1);
    assert!(!after.evidence.is_empty());
    let view = today_list(rig.db.conn(), NOW).expect("today");
    assert!(view.groups.mine.is_empty() && view.groups.waiting.is_empty() && view.groups.confirm.is_empty());

    // 再重启一次重放：拒绝路径幂等（每次重试各记一条 rejected，不重复 ok）。
    rig.db.restart();
    let again = rig.judge(1);
    assert_eq!(again.status, "rejected");
    assert_eq!(again.rejection_rule.as_deref(), Some("R_LOCKED"));
    assert_eq!(item_get(rig.db.conn(), &item_id).expect("详情").item.lifecycle, "ignored");
    assert_eq!(count(rig.db.conn(), "SELECT COUNT(*) FROM agent_run WHERE status = 'ok'"), 1);
}

// ---------- A03：one_off 闭环，不留悬挂 ----------

#[test]
fn a03_one_off_judged_ignored_no_hanging() {
    let mut rig = rig("A03");
    let r = rig.judge(1);
    assert_eq!(r.status, "ok");
    let id = r.item_id.expect("one_off(no_action) 建 candidate→ignored 事项");
    let detail = item_get(rig.db.conn(), &id).expect("详情");
    assert_eq!(detail.item.lifecycle, "ignored");
    let view = today_list(rig.db.conn(), NOW).expect("today");
    assert!(view.groups.mine.is_empty() && view.groups.waiting.is_empty());
}

// ---------- V06：related_hint 只落候选关联，永不自动归并 ----------

/// 测试用 runtime：原样回放给定的 runtime 结局（不查夹具哈希）。
struct ScriptedRuntime(workcontext_app::judge::RuntimeOutcome);

impl workcontext_app::judge::ClassifyRuntime for ScriptedRuntime {
    fn runtime_name(&self) -> &'static str {
        "mock"
    }
    fn model_id(&self) -> String {
        "scripted-test".to_string()
    }
    fn classify(
        &self,
        _input: &workcontext_infra::classify_input::ClassifyInput,
    ) -> workcontext_app::judge::RuntimeOutcome {
        self.0.clone()
    }
}

#[test]
fn v06_related_hint_persists_candidate_link_without_merging() {
    let mut rig = rig("A01");
    // 先按夹具建出真实事项，作为 related_hint 的目标。
    let target = rig.judge(1).item_id.expect("事项");
    let batch = rig.planned.batches[0].clone();
    // 夹具对 A01 阶段1 的真实输出 + 一个 related_hint。
    let mut output: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            repo_root().join("fixtures/mock/classify/d2a090bed7815a14e7e9e9d30fe66491944c0cbfd2ce5e5dc33a4925fb2a5df1.json"),
        )
        .expect("读取夹具输出"),
    )
    .expect("夹具输出是 JSON");
    output["related_hint"] = serde_json::json!({
        "target_item_ids": [target.clone()],
        "reason": "同一图纸复核的后续沟通"
    });
    let report = judge_planned_batch(
        rig.db.conn(),
        &ScriptedRuntime(workcontext_app::judge::RuntimeOutcome::Ok(output)),
        &rig.validator,
        &rig.planned,
        &batch,
        NOW,
    )
    .expect("判读");
    assert_eq!(report.status, "ok");
    // 候选关联落库：state=candidate、link_type=similar_candidate，指向目标事项。
    let (link_type, state, target_id): (String, String, String) = rig
        .db
        .conn()
        .query_row(
            "SELECT link_type, state, target_id FROM link_evidence WHERE link_type = 'similar_candidate'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("similar_candidate 行");
    assert_eq!(link_type, "similar_candidate");
    assert_eq!(state, "candidate");
    assert_eq!(target_id, target);
    // 永不自动归并：仍只有一个事项，thread_id 未被置上，候选只是候选。
    assert_eq!(count(rig.db.conn(), "SELECT COUNT(*) FROM work_item"), 1);
    assert_eq!(count(rig.db.conn(), "SELECT COUNT(*) FROM work_item WHERE thread_id IS NOT NULL"), 0);
    assert_eq!(
        count(rig.db.conn(), "SELECT COUNT(*) FROM link_evidence WHERE link_type IN ('origin','reference') AND state = 'candidate'"),
        0,
        "证据链不得被降级为候选"
    );
}

// ---------- 出域闸门 Blocked：不写 agent_run，记录置 blocked ----------

#[test]
fn egress_blocked_marks_records_without_agent_run_or_items() {
    let mut rig = rig("A11");
    let batch = rig.planned.batches[0].clone();
    let report = judge_planned_batch(
        rig.db.conn(),
        &ScriptedRuntime(workcontext_app::judge::RuntimeOutcome::Blocked {
            reason: "EGRESS_POLICY_NONE".to_string(),
        }),
        &rig.validator,
        &rig.planned,
        &batch,
        NOW,
    )
    .expect("blocked 也是正常结局");
    assert_eq!(report.status, "blocked");
    assert!(report.run_id.is_empty(), "模型未被调用，没有 run");
    assert_eq!(report.records, vec!["A11-R1".to_string()]);
    let conn = rig.db.conn();
    assert_eq!(count(conn, "SELECT COUNT(*) FROM agent_run"), 0);
    assert_eq!(count(conn, "SELECT COUNT(*) FROM work_item"), 0);
    assert_eq!(
        count(conn, "SELECT COUNT(*) FROM source_record WHERE judge_state = 'blocked'"),
        1
    );
}

// ---------- 缺可执行下一步 → 不出卡片，不生成占位 ----------

#[test]
fn today_hides_items_without_executable_next_action() {
    let mut rig = rig("A01");
    let id = rig.judge(1).item_id.expect("事项");
    // 库级不变量先兜底：active 事项不允许空下一步（§3.1 CHECK）。
    let rejected = rig.db.conn().execute(
        "UPDATE work_item SET next_action = '   ' WHERE id = ?1",
        params![id],
    );
    assert!(rejected.is_err(), "active 事项的空 next_action 应被 CHECK 拒绝");

    // candidate + 待确认但下一步为空：本应进 confirm 组，但没有可执行下一步 → 不出卡片。
    rig.db
        .conn()
        .execute(
            "UPDATE work_item SET lifecycle = 'candidate', attention = 'waiting_other', \
             next_action = NULL, awaiting_confirmation = 1 WHERE id = ?1",
            params![id],
        )
        .expect("置为无下一步的候选");
    let view = today_list(rig.db.conn(), NOW).expect("today");
    assert!(
        view.groups.mine.is_empty() && view.groups.waiting.is_empty() && view.groups.confirm.is_empty(),
        "没有可执行下一步的事项不得出卡片"
    );
    // 但在全部列表与详情里仍可见、可补，nextAction 原样为 null（不生成占位文案）。
    let page = items_query(
        rig.db.conn(),
        &ItemsQueryArgs { filter: Some("confirm".into()), ..Default::default() },
    )
    .expect("query");
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].next_action, None);
    assert_eq!(item_get(rig.db.conn(), &id).expect("详情").item.next_action, None);
}

// ---------- 同会话但不互相引用的消息不得成批 ----------

#[test]
fn judge_pending_batches_only_explicit_reply_chains() {
    let mut db = TempDb::open("batching");
    let root = repo_root().join("fixtures");
    // A01：R1/R2 互不引用；R3 reply_to R2；R4 reply_to R3；R5 reply_to R4。
    import_case(db.conn(), &root, "A01", NOW).expect("导入");
    let planned = plan_case(&root, "A01").expect("规划");
    let runtime = MockAgentRuntime::open(&root).expect("mock");
    let mut conn = db.conn();
    let summary = workcontext_app::judge::judge_pending(
        conn,
        &runtime,
        &PolicyValidator::new(),
        &planned.me,
        NOW,
        25,
    )
    .expect("judge_pending");
    // R1 单独一批；R2–R5 按 reply 链成一批（R3→R2 链接）。
    let sizes: Vec<usize> = summary.batches.iter().map(|b| b.records.len()).collect();
    assert_eq!(sizes, vec![1, 4], "R1 独立，R2..R5 链式成批");
    assert_eq!(summary.skipped_meta, 0);
    // mock 对这两个库内组装输入没有夹具 → 明确 skipped（不编造），记录保持 kept 等重试。
    assert!(summary.batches.iter().all(|b| b.status == "skipped"));
    assert_eq!(summary.pending_left, 5);
    assert_eq!(
        count(conn, "SELECT COUNT(*) FROM source_record WHERE judge_state = 'kept'"),
        5
    );
    assert_eq!(count(conn, "SELECT COUNT(*) FROM work_item"), 0);
    assert_eq!(count(conn, "SELECT COUNT(*) FROM agent_run WHERE status = 'skipped'"), 2);
}

// ---------- 证据时效（freshness） ----------

#[test]
fn freshness_reflects_sync_checkpoint_and_evidence_age() {
    let mut rig = rig("A01");
    rig.judge(1);
    // 未同步 → unknown。
    let view = today_list(rig.db.conn(), NOW).expect("today");
    assert_eq!(view.freshness.state, "unknown");

    // 写入最近一次成功同步 → fresh，窗口字段透出。
    rig.db.conn()
        .execute(
            "INSERT INTO sync_checkpoint (source_id, last_success_at, window_start, window_end, updated_at) \
             VALUES ('src-dm-li', ?1, ?2, ?3, ?1)",
            params![NOW, NOW - 3_600_000, NOW],
        )
        .expect("写游标");
    let view = today_list(rig.db.conn(), NOW).expect("today");
    assert_eq!(view.freshness.state, "fresh");
    assert_eq!(view.freshness.last_success_at, Some(NOW));
    assert_eq!(view.freshness.window_end, Some(NOW));

    // 7 小时无同步 → stale。
    let view = today_list(rig.db.conn(), NOW + 7 * 3_600_000).expect("today");
    assert_eq!(view.freshness.state, "stale");

    // 50 小时无同步 → expired（今日页提示「数据可能陈旧」的依据）。
    let view = today_list(rig.db.conn(), NOW + 50 * 3_600_000).expect("today");
    assert_eq!(view.freshness.state, "expired");

    // 同步已停（来源降级）→ 即使最后成功很近也 expired。
    rig.db
        .conn()
        .execute("UPDATE sync_checkpoint SET state = 'degraded'", [])
        .expect("降级");
    let view = today_list(rig.db.conn(), NOW).expect("today");
    assert_eq!(view.freshness.state, "expired");
    let page = items_query(
        rig.db.conn(),
        &ItemsQueryArgs { filter: Some("all".into()), ..Default::default() },
    )
    .expect("query");
    assert_eq!(page.freshness.state, "expired", "列表页与今日页同一新鲜度口径");
    rig.db
        .conn()
        .execute("UPDATE sync_checkpoint SET state = 'idle'", [])
        .expect("恢复");

    // 证据时效：把 last_evidence_at 拨回 8 天前，卡片仍透出真实时间戳。
    rig.db.conn()
        .execute("UPDATE work_item SET last_evidence_at = ?1", params![NOW - 8 * 86_400_000])
        .expect("拨回证据时间");
    let view = today_list(rig.db.conn(), NOW).expect("today");
    assert_eq!(view.groups.mine[0].last_evidence_at, Some(NOW - 8 * 86_400_000));
}

// ---------- 列表/搜索 ----------

#[test]
fn items_query_filters_and_search() {
    let mut rig = rig("A01");
    rig.judge(1);
    let all = items_query(
        rig.db.conn(),
        &ItemsQueryArgs { filter: Some("all".into()), ..Default::default() },
    )
    .expect("query");
    assert_eq!(all.total, 1);
    assert_eq!(all.items[0].lifecycle, "active");

    let closed = items_query(
        rig.db.conn(),
        &ItemsQueryArgs { filter: Some("closed".into()), ..Default::default() },
    )
    .expect("query");
    assert_eq!(closed.total, 0);

    // 未知筛选 → INVALID_ARGUMENT（不静默当 all）。
    let err = items_query(
        rig.db.conn(),
        &ItemsQueryArgs { filter: Some("nonsense".into()), ..Default::default() },
    )
    .unwrap_err();
    assert_eq!(err.code(), "INVALID_ARGUMENT");

    // <3 字符走 LIKE、≥3 字符走 FTS trigram（标题「跟进图纸打印模板报错」）。
    for q in ["图", "图纸", "图纸打印", "模板报错", "图纸打印模板报错"] {
        let hit = items_query(
            rig.db.conn(),
            &ItemsQueryArgs { filter: None, query: Some(q.to_string()), limit: Some(100), offset: Some(0) },
        )
        .expect("query");
        assert_eq!(hit.total, 1, "查询 {q} 应命中事项");
    }
}

// ---------- 跟进与收尾守卫 ----------

#[test]
fn followup_and_close_guards() {
    let mut rig = rig("A01");
    let r = rig.judge(1);
    let id = r.item_id.expect("事项");

    // deadline 缺时间 → INVALID_ARGUMENT，不落库。
    let err = item_set_followup(
        rig.db.conn(),
        &SetFollowupArgs {
            id: id.clone(),
            followup_type: "deadline".into(),
            at: None,
            note: None,
        },
        NOW,
    )
    .unwrap_err();
    assert_eq!(err.code(), "INVALID_ARGUMENT");

    // event 缺说明 → INVALID_ARGUMENT。
    let err = item_set_followup(
        rig.db.conn(),
        &SetFollowupArgs {
            id: id.clone(),
            followup_type: "event".into(),
            at: None,
            note: Some("  ".into()),
        },
        NOW,
    )
    .unwrap_err();
    assert_eq!(err.code(), "INVALID_ARGUMENT");

    // 合法 manual_time → 落库并可清除。
    let set = item_set_followup(
        rig.db.conn(),
        &SetFollowupArgs {
            id: id.clone(),
            followup_type: "manual_time".into(),
            at: Some(NOW + 86_400_000),
            note: Some("明天再看".into()),
        },
        NOW,
    )
    .expect("设置跟进");
    assert_eq!(set.item.followup_type, "manual_time");
    assert_eq!(set.item.followup_at, Some(NOW + 86_400_000));
    let cleared = item_set_followup(
        rig.db.conn(),
        &SetFollowupArgs { id: id.clone(), followup_type: "none".into(), at: None, note: None },
        NOW,
    )
    .expect("清除跟进");
    assert_eq!(cleared.item.followup_type, "none");
    assert!(cleared.item.followup_at.is_none());

    // ack_only / artifact_created 不构成收尾证据（V05）。
    for bad in ["ack_only", "artifact_created"] {
        let err = item_close(
            rig.db.conn(),
            &CloseArgs { id: id.clone(), evidence_type: bad.to_string(), note: None },
            NOW,
        )
        .unwrap_err();
        assert_eq!(err.code(), "VALIDATION_REJECTED", "{bad} 应被收尾门拒绝");
    }
    let detail = item_get(rig.db.conn(), &id).expect("详情");
    assert_eq!(detail.item.lifecycle, "active", "拒绝不得改变状态");

    // 不存在的事项 → NOT_FOUND。
    let err = item_get(rig.db.conn(), "item-nope").unwrap_err();
    assert_eq!(err.code(), "NOT_FOUND");
}
