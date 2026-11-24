use std::slice;

use workcontext_app::items::{ensure_item_visible, item_get, item_set_followup, items_query_for_profile,
    today_list_for_profile, ItemsQueryArgs, SetFollowupArgs};
use workcontext_app::ownership_flow::{move_card, set_conclusion, undo, view, Candidate, EvidenceDetail, FlowCard, Receipt, SourceKey, SourceSummary, State};
use workcontext_store::rusqlite::{params, Connection};

const A: &str = "org:alice";
const B: &str = "org:bob";
const NOW: i64 = 1000;

fn db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    conn.execute_batch(include_str!("../../../migrations/0001_init.sql")).unwrap();
    conn.execute_batch(include_str!("../../../migrations/0002_judge_meta.sql")).unwrap();
    conn.execute_batch(include_str!("../../../migrations/0003_ownership_flow.sql")).unwrap();
    conn.execute_batch(include_str!("../../../migrations/0004_ownership_conclusion.sql")).unwrap();
    conn.execute_batch(include_str!("../../../migrations/0005_ownership_project.sql")).unwrap();
    conn.execute_batch(include_str!("../../../migrations/0006_context.sql")).unwrap();
    conn
}

fn candidate(title: &str, keys: &[&str]) -> Candidate {
    Candidate { state: State::Triage, needs_verification: false, work: title.into(), source_work: title.into(),
        anchors: Vec::new(), evidence_details: Vec::new(), project: None,
        action: format!("核实{title}"), when: String::new(), source_summary: SourceSummary::default(),
        evidence: keys.iter().map(|mid| SourceKey { cid: "chat".into(), mid: (*mid).into() }).collect() }
}

fn show(db: &mut Connection, c: &Candidate) -> FlowCard {
    view(db, A, slice::from_ref(c)).unwrap().remove(0)
}

fn apply(db: &mut Connection, c: &FlowCard, target: State, action: Option<&str>) -> Receipt {
    move_card(db, A, &c.id, &c.version, target, action, NOW).unwrap()
}

fn total(db: &Connection, profile: Option<&str>, query: Option<&str>) -> i64 {
    items_query_for_profile(db, &ItemsQueryArgs { query: query.map(str::to_owned), ..Default::default() }, profile).unwrap().total
}

#[test]
fn verification_survives_legacy_candidates_and_first_move_undo() {
    let mut db = db();
    let mut c = candidate("核对样图", &["drawing"]);
    c.state = State::Mine;
    let original = view(&mut db, A, &[c.clone(), c.clone()]).unwrap();
    db.execute("UPDATE ownership_card SET candidate_json=json_remove(candidate_json,'$.needsVerification')", []).unwrap();
    let legacy = view(&mut db, A, &[c.clone(), c.clone()]).unwrap();
    for (old, new) in original.iter().zip(&legacy) {
        assert_eq!(old.id, new.id);
        assert_eq!(old.version, new.version);
        assert!(!new.needs_verification);
    }
    c.needs_verification = true;
    let snapshot = [c.clone(), c];
    let current = view(&mut db, A, &snapshot).unwrap();
    for (old, new) in original.iter().zip(&current) {
        assert_eq!(old.id, new.id);
        assert_ne!(old.version, new.version);
        assert!(new.needs_verification);
    }
    let receipt = apply(&mut db, &current[0], State::Waiting, Some("等对方回复"));
    let waiting = view(&mut db, A, &snapshot).unwrap();
    assert!(!waiting[0].needs_verification);
    assert!(waiting[1].needs_verification);
    undo(&mut db, A, &receipt.undo_token, NOW).unwrap();
    let restored = view(&mut db, A, &snapshot).unwrap();
    assert_eq!(restored[0].id, original[0].id);
    assert_eq!(restored[0].state, State::Mine);
    assert!(restored[0].needs_verification);
}

#[test]
fn shared_messages_keep_distinct_items_after_reordering_and_rewriting() {
    let mut db = db();
    let mut launch = candidate("发布窗口", &["common", "launch"]);
    let mut review = candidate("验收清单", &["review", "common"]);
    let original = view(&mut db, A, &[launch.clone(), review.clone()]).unwrap();
    assert_ne!(original[0].id, original[1].id);
    apply(&mut db, &original[0], State::Waiting, Some("等运维确认"));
    launch.evidence.reverse();
    launch.work = "上线时间".into();
    launch.action = "新报告建议".into();
    review.work = "验收点".into();
    let refreshed = view(&mut db, A, &[review.clone(), launch.clone()]).unwrap();
    assert_eq!(refreshed.len(), 2);
    assert_eq!(refreshed[0].id, original[1].id);
    assert_eq!(refreshed[0].state, State::Triage);
    assert_eq!(refreshed[1].id, original[0].id);
    assert_eq!(refreshed[1].action, "等运维确认");
    assert_eq!(refreshed[1].state, State::Waiting);
    assert_eq!(refreshed[1].work, "发布窗口");
    let mut unrelated = candidate("发布窗口", &["common", "new"]);
    unrelated.work = "另一批次".into();
    let latest = view(&mut db, A, &[unrelated]).unwrap();
    assert_eq!(latest.len(), 2);
    assert_ne!(latest[0].id, original[0].id);
    assert_eq!(latest[1].id, original[0].id);
}

#[test]
fn identical_source_sets_need_subject_or_unambiguous_corroboration() {
    let mut db = db();
    let mut first = candidate("报价", &["meeting"]);
    let mut second = candidate("排期", &["meeting"]);
    let old = view(&mut db, A, &[first.clone(), second.clone()]).unwrap();
    first.work = "确认报价".into();
    second.work = "确认排期".into();
    let reordered = view(&mut db, A, &[second, first]).unwrap();
    assert_eq!(reordered[0].id, old[1].id);
    assert_eq!(reordered[1].id, old[0].id);
    let unrelated = candidate("合同", &["meeting"]);
    let third = show(&mut db, &unrelated);
    assert!(old.iter().all(|c| c.id != third.id));
    assert_eq!(total(&db, Some(A), None), 3);
}

#[test]
fn revisions_and_first_undo_survive_a_restart_and_a_window_change() {
    let dir = std::env::temp_dir().join(format!("workcontext-flow-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("flow.sqlite");
    let mut conn = workcontext_store::open(&path).unwrap();
    let mut c = candidate("发布", &["a"]);
    let old = show(&mut conn, &c);
    c.action = "确定运维值班人".into();
    let fresh = show(&mut conn, &c);
    assert_eq!(old.id, fresh.id);
    assert_ne!(old.version, fresh.version);
    assert!(move_card(&mut conn, A, &old.id, &old.version, State::Mine, None, NOW).is_err());
    let receipt = apply(&mut conn, &fresh, State::Mine, Some("联系运维"));
    drop(conn);
    let mut conn = workcontext_store::open(&path).unwrap();
    let outside = view(&mut conn, A, &[]).unwrap();
    assert_eq!(outside.len(), 1);
    assert_eq!(outside[0].id, fresh.id);
    assert_eq!(outside[0].action, "联系运维");
    undo(&mut conn, A, &receipt.undo_token, NOW).unwrap();
    assert!(view(&mut conn, A, &[]).unwrap().is_empty());
    assert_eq!(total(&conn, Some(A), None), 1);
    assert!(item_get(&conn, &fresh.id).is_err());
    assert_eq!(show(&mut conn, &c).id, fresh.id);
    assert_eq!(conn.query_row("SELECT count(*) FROM field_override", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    assert!(undo(&mut conn, A, &receipt.undo_token, NOW).is_err());
    drop(conn);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn profiles_scope_search_pagination_today_detail_and_receipts() {
    let mut db = db();
    let c = candidate("合同报价", &["private"]);
    let alice = show(&mut db, &c);
    let bob = view(&mut db, B, slice::from_ref(&c)).unwrap().remove(0);
    assert_ne!(alice.id, bob.id);
    assert!(move_card(&mut db, B, &alice.id, &alice.version, State::Mine, None, NOW).is_err());
    let receipt = apply(&mut db, &alice, State::Mine, None);
    assert!(undo(&mut db, B, &receipt.undo_token, NOW).is_err());
    db.execute("INSERT INTO work_item(id,title,dedupe_key,created_at,updated_at) VALUES ('legacy','历史','legacy',1,1)", []).unwrap();
    assert_eq!(total(&db, Some(A), None), 2);
    assert_eq!(total(&db, Some(B), None), 2);
    assert_eq!(total(&db, None, None), 1);
    for search in ["报价", "合同报价"] {
        assert_eq!(total(&db, Some(A), Some(search)), 1);
        assert_eq!(total(&db, Some(B), Some(search)), 1);
    }
    assert!(ensure_item_visible(&db, &alice.id, Some(B)).is_err());
    assert!(ensure_item_visible(&db, &alice.id, None).is_err());
    assert!(ensure_item_visible(&db, "legacy", None).is_ok());
    assert_eq!(today_list_for_profile(&db, NOW, Some(A)).unwrap().groups.mine[0].id, alice.id);
    assert!(today_list_for_profile(&db, NOW, Some(B)).unwrap().groups.mine.is_empty());
    assert!(view(&mut db, B, &[]).unwrap().is_empty());
    assert_eq!(item_get(&db, &alice.id).unwrap().item.attention, "me");
}

#[test]
fn adopting_a_suggestion_confirms_the_same_card_and_keeps_the_catalog_total() {
    for state in [State::Mine, State::Waiting] {
        let mut db = db();
        let mut c = candidate("首批复核", &["origin"]);
        c.state = state;
        let card = show(&mut db, &c);
        assert!(move_card(&mut db, A, &card.id, &card.version, state, None, NOW).is_err());
        let receipt = apply(&mut db, &card, state, Some(&c.action));
        assert_eq!(item_get(&db, &card.id).unwrap().item.lifecycle, "active");
        assert_eq!(total(&db, Some(A), None), 1);
        undo(&mut db, A, &receipt.undo_token, NOW).unwrap();
        assert!(item_get(&db, &card.id).is_err());
        assert_eq!(total(&db, Some(A), None), 1);
    }
}

#[test]
fn project_overrides_on_historical_deliveries_are_account_scoped_and_preserve_state() {
    use workcontext_app::items::item_get_for_profile;
    use workcontext_app::ownership_flow::set_project;
    let mut db = db();
    let mut old = candidate("旧表", &["old"]);
    old.project = Some("原项目".into());
    let id = show(&mut db, &old).id;
    view(&mut db, A, &[]).unwrap();
    let detail = |db: &Connection| item_get_for_profile(db, &id, Some(A)).unwrap().delivery.unwrap();
    let version = detail(&db).version;
    assert!(set_project(&db, B, &id, &version, Some("另一个项目")).is_err());
    set_project(&db, A, &id, &version, Some(" 新项目 ")).unwrap();
    let assigned = detail(&db);
    assert_eq!(assigned.project.as_deref(), Some("新项目"));
    assert!(!assigned.formal && !assigned.present);
    assert!(set_project(&db, A, &id, &version, None).is_err());
    set_project(&db, A, &id, &assigned.version, None).unwrap();
    old.project = Some("后续报告项目".into());
    assert_eq!(show(&mut db, &old).id, id);
    assert!(detail(&db).project.is_none());
}

#[test]
fn external_writes_invalidate_moves_and_first_or_later_undo_even_at_same_timestamp() {
    let mut db = db();
    let mut c = candidate("评审", &["one"]);
    let first = show(&mut db, &c);
    let receipt = apply(&mut db, &first, State::Mine, Some("检查文档"));
    let active = show(&mut db, &c);
    item_set_followup(&mut db, &SetFollowupArgs { id: first.id.clone(), followup_type: "event".into(),
        at: None, note: Some("等评审会".into()) }, NOW).unwrap();
    assert!(undo(&mut db, A, &receipt.undo_token, NOW).is_err());
    assert!(move_card(&mut db, A, &active.id, &active.version, State::Done, None, NOW).is_err());
    let latest = show(&mut db, &c);
    let receipt = apply(&mut db, &latest, State::Waiting, Some("等同事复核"));
    db.execute("UPDATE work_item SET title='详细评审', attention='report_back', next_action='回报结果' WHERE id=?1", [&first.id]).unwrap();
    assert!(undo(&mut db, A, &receipt.undo_token, NOW).is_err());
    c.state = State::Mine;
    c.needs_verification = true;
    let updated = show(&mut db, &c);
    assert_eq!(updated.id, first.id);
    assert!(!updated.needs_verification);
    assert_eq!((updated.state, updated.work.as_str(), updated.action.as_str()), (State::Mine, "详细评审", "回报结果"));
}

#[test]
fn restore_and_undo_preserve_formal_state_action_locks_and_human_evidence() {
    let mut db = db();
    let c = candidate("验收", &["one"]);
    let first = show(&mut db, &c);
    apply(&mut db, &first, State::Waiting, Some("等客户验收"));
    let waiting = show(&mut db, &c);
    let ignored = apply(&mut db, &waiting, State::Ignored, None);
    undo(&mut db, A, &ignored.undo_token, NOW).unwrap();
    let restored = show(&mut db, &c);
    assert_eq!((restored.state, restored.action.as_str()), (State::Waiting, "等客户验收"));
    let lock: String = db.query_row("SELECT value_json FROM field_override WHERE target_id=?1 AND field='attention'", [&restored.id], |r| r.get(0)).unwrap();
    assert_eq!(lock, "\"waiting_other\"");
    apply(&mut db, &restored, State::Ignored, None);
    let hidden = show(&mut db, &c);
    apply(&mut db, &hidden, State::Mine, Some("询问验收人"));
    let active = show(&mut db, &c);
    apply(&mut db, &active, State::Done, None);
    let done = show(&mut db, &c);
    assert_eq!(item_get(&db, &done.id).unwrap().item.resolution_evidence.as_deref(), Some("human_confirmed"));
    assert!(move_card(&mut db, A, &done.id, &done.version, State::Mine, None, NOW).is_err());
    let reopened = apply(&mut db, &done, State::Mine, Some("客户要求重新核对签字"));
    let detail = item_get(&db, &done.id).unwrap();
    assert_eq!(detail.item.lifecycle, "active");
    assert!(detail.item.closed_at.is_none() && detail.item.resolution_evidence.is_none());
    assert_eq!(detail.evidence[0].kind, "manual_note");
    assert_eq!(detail.evidence[0].summary.as_deref(), Some("客户要求重新核对签字"));
    let flags: (bool, String) = db.query_row("SELECT s.enabled,r.judge_state FROM source_record r JOIN source s ON s.id=r.source_id WHERE r.id=?1",
        params![detail.evidence[0].id], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!(flags, (false, "skipped".into()));
    undo(&mut db, A, &reopened.undo_token, NOW).unwrap();
    let final_card = show(&mut db, &c);
    assert_eq!((final_card.state, final_card.action.as_str()), (State::Done, "询问验收人"));
    assert!(item_get(&db, &done.id).unwrap().evidence.is_empty());
}

#[test]
fn failed_event_rolls_back_the_item_locks_and_version() {
    let mut db = db();
    let mut c = candidate("发布", &["a"]);
    c.state = State::Mine;
    let suggestion = show(&mut db, &c);
    db.execute_batch("CREATE TEMP TRIGGER reject_event BEFORE INSERT ON work_event BEGIN
        SELECT RAISE(ABORT,'forced event failure'); END;").unwrap();
    assert!(move_card(&mut db, A, &suggestion.id, &suggestion.version, State::Done, None, NOW).is_err());
    assert!(item_get(&db, &suggestion.id).is_err());
    assert_eq!(show(&mut db, &c).version, suggestion.version);
    assert_eq!(db.query_row("SELECT count(*) FROM field_override", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    db.execute_batch("DROP TRIGGER reject_event").unwrap();
    apply(&mut db, &suggestion, State::Done, None);
}

#[test]
fn competing_connections_can_commit_only_one_move_for_a_version() {
    use std::sync::{Arc, Barrier};
    let dir = std::env::temp_dir().join(format!("workcontext-flow-race-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("race.sqlite");
    let mut db = workcontext_store::open(&path).unwrap();
    let c = candidate("发布", &["a"]);
    let suggestion = show(&mut db, &c);
    let barrier = Arc::new(Barrier::new(2));
    let mut workers = Vec::new();
    for state in [State::Mine, State::Waiting] {
        let mut conn = workcontext_store::open(&path).unwrap();
        let card = suggestion.clone();
        let barrier = Arc::clone(&barrier);
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            move_card(&mut conn, A, &card.id, &card.version, state, Some("联系运维"), NOW)
                .map(|_| "ok").unwrap_or_else(|e| e.code())
        }));
    }
    let mut results: Vec<_> = workers.into_iter().map(|t| t.join().unwrap()).collect();
    results.sort_unstable();
    assert_eq!(results, ["VALIDATION_REJECTED", "ok"]);
    assert_eq!(total(&db, Some(A), None), 1);
    drop(db);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn handoff_requires_a_new_action_and_the_old_deadline_only_follows_its_action() {
    let mut db = db();
    let mut c = candidate("演示", &["a"]);
    c.when = "周二下午".into();
    let triage = show(&mut db, &c);
    assert!(move_card(&mut db, A, &triage.id, &triage.version, State::Done, None, NOW).is_err());
    assert!(move_card(&mut db, A, &triage.id, &triage.version, State::Waiting, None, NOW).is_err());
    assert!(item_get(&db, &triage.id).is_err());
    apply(&mut db, &triage, State::Mine, None);
    let mine = show(&mut db, &c);
    assert_eq!(mine.when, c.when);
    for (target, action) in [(State::Mine, Some("再次认领")), (State::Triage, None),
        (State::Waiting, None), (State::Waiting, Some("  "))] {
        assert!(move_card(&mut db, A, &mine.id, &mine.version, target, action, NOW).is_err());
    }
    let receipt = apply(&mut db, &mine, State::Waiting, Some("等工艺负责人评审样图"));
    let waiting = show(&mut db, &c);
    assert!(waiting.when.is_empty());
    assert!(move_card(&mut db, A, &waiting.id, &waiting.version, State::Mine, None, NOW).is_err());
    assert!(move_card(&mut db, A, &waiting.id, &waiting.version, State::Waiting, Some("原状态"), NOW).is_err());
    undo(&mut db, A, &receipt.undo_token, NOW).unwrap();
    let mine = show(&mut db, &c);
    assert_eq!(mine.when, c.when);
    apply(&mut db, &mine, State::Ignored, None);
    let ignored = show(&mut db, &c);
    for (target, action) in [(State::Waiting, Some("等答复")), (State::Done, None), (State::Mine, None)] {
        assert!(move_card(&mut db, A, &ignored.id, &ignored.version, target, action, NOW).is_err());
    }
    apply(&mut db, &ignored, State::Mine, Some("按意见修改样图"));
    let mine = show(&mut db, &c);
    apply(&mut db, &mine, State::Done, None);
    let done = show(&mut db, &c);
    assert!(move_card(&mut db, A, &done.id, &done.version, State::Waiting, Some("等回复"), NOW).is_err());
}

#[test]
fn unique_multi_message_source_survives_a_full_rewrite() {
    let mut db = db();
    let initial = candidate("报价资料", &["request", "clarification"]);
    let old = show(&mut db, &initial);
    apply(&mut db, &old, State::Mine, None);
    let rewritten = candidate("整理本季度的采购预算", &["clarification", "request"]);
    let current = view(&mut db, A, &[rewritten]).unwrap();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].id, old.id);
    assert_eq!(current[0].state, State::Mine);
}

#[test]
fn resolved_delivery_survives_restart_without_closing_the_next_delivery() {
    let dir = std::env::temp_dir().join(format!("workcontext-conclusion-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("flow.sqlite");
    let mut db = workcontext_store::open(&path).unwrap();
    let mut first = candidate("流程图", &["nov10", "nov20", "meeting"]);
    first.state = State::Waiting;
    first.anchors = vec![first.evidence[0].clone()];
    first.evidence_details.push(EvidenceDetail { time: "11/10".into(), conversation: "工艺组".into(),
        kind: "群聊".into(), excerpt: "20份流程图已在会上确认".into() });
    let old = show(&mut db, &first);
    assert_eq!(old.evidence_details, first.evidence_details);
    set_conclusion(&mut db, A, &old.id, &old.version, "会上已确认，无需跟进", true, NOW).unwrap();
    drop(db);
    let mut db = workcontext_store::open(&path).unwrap();
    let mut next = first.clone();
    next.anchors = vec![first.evidence[1].clone()];
    next.work = "20款手工线工序差异表".into();
    let latest = view(&mut db, A, slice::from_ref(&next)).unwrap();
    assert_eq!(latest.len(), 2);
    assert_ne!(latest[0].id, old.id);
    assert_eq!((latest[0].state, latest[0].conclusion.as_deref()), (State::Waiting, None));
    assert_eq!(latest[1].id, old.id);
    assert_eq!((latest[1].state, latest[1].conclusion.as_deref()), (State::Done, Some("会上已确认，无需跟进")));
    first.work = "会议纪要更新".into();
    first.evidence.clear();
    let rewritten = view(&mut db, A, &[first, next]).unwrap();
    assert_eq!(rewritten.len(), 2);
    assert_eq!((rewritten[0].id.as_str(), rewritten[0].state), (old.id.as_str(), State::Done));
    let receipt = apply(&mut db, &rewritten[0], State::Mine, Some("客户要求补充说明"));
    let resolved = |db: &Connection| db.query_row("SELECT resolved FROM ownership_conclusion WHERE card_id=?1",
        [&old.id], |r| r.get::<_, bool>(0)).unwrap();
    assert!(!resolved(&db));
    undo(&mut db, A, &receipt.undo_token, NOW).unwrap();
    assert!(resolved(&db));
    drop(db);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn note_only_validates_account_revision_length_and_preserves_manual_state() {
    let mut db = db();
    let c = candidate("交付", &["request"]);
    let original = show(&mut db, &c);
    assert!(set_conclusion(&mut db, B, &original.id, &original.version, "另一个账号", true, NOW).is_err());
    for note in [" \t".to_owned(), "字".repeat(1001)] {
        assert!(set_conclusion(&mut db, A, &original.id, &original.version, &note, false, NOW).is_err());
    }
    let note = "字".repeat(1000);
    set_conclusion(&mut db, A, &original.id, &original.version, &note, false, NOW).unwrap();
    assert!(set_conclusion(&mut db, A, &original.id, &original.version, "过期", true, NOW).is_err());
    let saved = show(&mut db, &c);
    assert_eq!((saved.state, saved.conclusion.as_deref()), (State::Triage, Some(note.as_str())));
    assert!(item_get(&db, &original.id).is_err());
    set_conclusion(&mut db, A, &saved.id, &saved.version, "处理完毕", true, NOW).unwrap();
    let done = show(&mut db, &c);
    set_conclusion(&mut db, A, &done.id, &done.version, "补充记录", false, NOW).unwrap();
    assert_eq!(show(&mut db, &c).state, State::Done);
    let (active, keys): (bool, String) = db.query_row("SELECT resolved, anchors_json FROM ownership_conclusion WHERE card_id=?1",
        [&done.id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert!(active);
    assert_eq!(serde_json::from_str::<Vec<SourceKey>>(&keys).unwrap(), c.evidence);
    assert!(view(&mut db, B, &[c]).unwrap()[0].conclusion.is_none());
}

#[test]
fn broad_legacy_evidence_does_not_supply_an_origin_for_a_new_delivery() {
    let mut db = db();
    let c = candidate("工艺流程图", &["nov10", "nov20", "meeting"]);
    let original = show(&mut db, &c);
    set_conclusion(&mut db, A, &original.id, &original.version, "会上已确认", true, NOW).unwrap();
    let mut later = c.clone();
    later.anchors = vec![c.evidence[1].clone()];
    let cards = view(&mut db, A, &[later]).unwrap();
    assert_ne!(cards[0].id, original.id);
    assert_eq!(cards[0].state, State::Triage);
    assert!(cards[0].conclusion.is_none());
    assert_eq!(cards[1].state, State::Done);
}

#[test]
fn failed_conclusion_write_rolls_back_closure_and_keeps_previous_receipt_valid() {
    let mut db = db();
    let c = candidate("验收单", &["request"]);
    let first = show(&mut db, &c);
    let receipt = apply(&mut db, &first, State::Waiting, Some("等签字"));
    let waiting = show(&mut db, &c);
    db.execute_batch("CREATE TEMP TRIGGER reject_conclusion BEFORE INSERT ON ownership_conclusion BEGIN
        SELECT RAISE(ABORT,'failed note'); END;").unwrap();
    assert!(set_conclusion(&mut db, A, &first.id, &waiting.version, "已签字", true, NOW).is_err());
    let current = show(&mut db, &c);
    assert_eq!((current.version, current.state, current.conclusion), (waiting.version, State::Waiting, None));
    undo(&mut db, A, &receipt.undo_token, NOW).unwrap();
    assert_eq!(show(&mut db, &c).state, State::Triage);
}

#[test]
fn shared_singleton_reference_is_not_exported_as_a_legacy_origin() {
    let mut db = db();
    let old = candidate("旧交付", &["meeting"]);
    let next = candidate("下一项交付", &["meeting", "request"]);
    let cards = view(&mut db, A, &[old, next]).unwrap();
    set_conclusion(&mut db, A, &cards[0].id, &cards[0].version, "已核对", true, NOW).unwrap();
    let keys: String = db.query_row("SELECT anchors_json FROM ownership_conclusion WHERE card_id=?1",
        [&cards[0].id], |r| r.get(0)).unwrap();
    assert_eq!(keys, "[]");
}
