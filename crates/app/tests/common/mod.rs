//! 集成测试共享设施：回放入口 + `expected.json` 断言解释器。
//!
//! 回放（`workcontext_app::replay`）只消费 case `input.json` 与
//! `fixtures/mock` 夹具；本模块把 `expected.json` 的断言词汇表逐键比对到
//! 回放快照上。**未知断言键即失败**（防止拼写错误静默通过）。

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::Value;
use workcontext_infra::fixture_case;

use workcontext_app::replay::{EventView, ItemView, ReplayOutcome, StageSnapshot};

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("仓库根目录可定位")
}

pub fn fixtures_root() -> PathBuf {
    repo_root().join("fixtures")
}

/// 回放一个 case（结果只由 input + mock + core 链路产生）。
pub fn replay(case_id: &str) -> ReplayOutcome {
    workcontext_app::replay::replay_case(&fixtures_root(), case_id)
        .unwrap_or_else(|e| panic!("{case_id} 回放失败：{e}"))
}

/// 读取 case 的 expected.json（仅用于断言，不进入回放链路）。
pub fn expected(case_id: &str) -> Value {
    let dir = fixture_case::find_case_dir(&fixtures_root().join("cases"), case_id)
        .unwrap_or_else(|e| panic!("{case_id} 夹具定位失败：{e}"));
    let path = dir.join("expected.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{} 读取失败：{e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{} 解析失败：{e}", path.display()))
}

// ---------- 断言词汇表（严格键校验） ----------

const TOP_KEYS: &[&str] = &[
    "schema_version", "case_id", "assertion_basis", "assertion_scope", "stages", "final", "notes",
];
const STAGE_KEYS: &[&str] = &[
    "index", "records", "agent_runs", "items", "item_count", "work_events", "field_overrides",
];
const RECORD_KEYS: &[&str] = &["external_id", "prefilter", "prefilter_reason", "judge_state"];
const RUN_KEYS: &[&str] = &[
    "records", "status", "intent_class", "belongs_to_me", "raw_belongs_to_me", "followup_type",
    "confidence_min", "needs_action", "outcome", "post_validation", "note", "rejection_rule",
];
const POST_KEYS: &[&str] = &["belongs_to_me", "downgrade_rule"];
const ITEM_KEYS: &[&str] = &[
    "identity", "same_item_as_stage", "title_contains", "lifecycle", "lifecycle_must_not_be",
    "attention", "current_turn", "next_action_nonempty", "next_action_contains", "followup_type",
    "followup_at", "awaiting_confirmation", "ignore_reason_nonempty", "expected_ignore_reason",
    "close_reason_nonempty", "resolution_evidence", "today", "note",
];
const IDENTITY_KEYS: &[&str] = &["origin_record", "intent_class"];
const TODAY_KEYS: &[&str] = &["visible", "group"];
const EVENT_KEYS: &[&str] = &["event_type", "actor", "min_count"];
const OVERRIDE_KEYS: &[&str] = &["field", "value_json", "locked"];
const FINAL_KEYS: &[&str] = &[
    "item_count", "counts", "items", "work_events", "field_overrides", "no_item_for_persons",
    "no_item_from_records", "forbidden", "forbidden_strings_in_ui", "notes",
];

fn check_keys(value: &Value, path: &str, allowed: &[&str]) {
    let obj = value
        .as_object()
        .unwrap_or_else(|| panic!("{path} 必须是对象"));
    for key in obj.keys() {
        assert!(
            allowed.contains(&key.as_str()),
            "{path} 出现未实现的断言键 {key}（严格模式：防止断言静默失效）"
        );
    }
}

fn opt_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// 完整回放一个 case 并对照 expected.json 断言。
pub fn assert_case_matches_expected(case_id: &str) -> ReplayOutcome {
    let outcome = replay(case_id);
    let exp = expected(case_id);
    check_keys(&exp, "expected", TOP_KEYS);

    // (stage_index, origin_record) → item id，供 same_item_as_stage 断言。
    let mut item_ids: HashMap<(u32, String), String> = HashMap::new();

    for exp_stage in exp["stages"].as_array().expect("stages 必须是数组") {
        check_keys(exp_stage, "expected stage", STAGE_KEYS);
        let index = exp_stage["index"].as_u64().expect("index") as u32;
        let stage = outcome
            .stages
            .iter()
            .find(|s| s.index == index)
            .unwrap_or_else(|| panic!("{case_id} 阶段 {index} 未在回放中出现"));
        assert_stage_records(case_id, stage, exp_stage);
        assert_stage_runs(case_id, stage, exp_stage);
        assert_stage_items(case_id, stage, exp_stage, &mut item_ids);
        assert_eq!(
            stage.item_count,
            exp_stage["item_count"].as_u64().expect("item_count") as usize,
            "{case_id} 阶段 {index} item_count"
        );
        if let Some(events) = exp_stage.get("work_events").and_then(Value::as_array) {
            assert_events(&format!("{case_id} 阶段 {index}"), events, &stage.work_events);
        }
        if let Some(overrides) = exp_stage.get("field_overrides").and_then(Value::as_array) {
            assert_overrides(&format!("{case_id} 阶段 {index}"), overrides, &stage.field_overrides);
        }
    }

    assert_final(case_id, &outcome, &exp["final"], &mut item_ids);
    outcome
}

fn assert_stage_records(case_id: &str, stage: &StageSnapshot, exp_stage: &Value) {
    let Some(records) = exp_stage.get("records").and_then(Value::as_array) else {
        return;
    };
    for rec in records {
        check_keys(rec, "expected record", RECORD_KEYS);
        let id = rec["external_id"].as_str().expect("external_id");
        let actual = stage
            .records
            .iter()
            .find(|r| r.external_id == id)
            .unwrap_or_else(|| panic!("{case_id} 阶段 {} 记录 {id} 未入库", stage.index));
        assert_eq!(actual.prefilter, rec["prefilter"].as_str().expect("prefilter"), "{case_id} {id} prefilter");
        assert_eq!(actual.prefilter_reason, rec["prefilter_reason"].as_str().expect("prefilter_reason"), "{case_id} {id} prefilter_reason");
        assert_eq!(actual.judge_state, rec["judge_state"].as_str().expect("judge_state"), "{case_id} {id} judge_state");
    }
}

fn assert_stage_runs(case_id: &str, stage: &StageSnapshot, exp_stage: &Value) {
    let Some(runs) = exp_stage.get("agent_runs").and_then(Value::as_array) else {
        return;
    };
    assert_eq!(
        runs.len(),
        stage.agent_runs.len(),
        "{case_id} 阶段 {} agent_runs 数量",
        stage.index
    );
    for (exp_run, actual) in runs.iter().zip(&stage.agent_runs) {
        check_keys(exp_run, "expected agent_run", RUN_KEYS);
        let records: Vec<&str> = exp_run["records"]
            .as_array()
            .expect("records")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(actual.records, records, "{case_id} 阶段 {} 判读批次记录", stage.index);
        assert_eq!(actual.status, exp_run["status"].as_str().expect("status"), "{case_id} 阶段 {} run status", stage.index);
        if let Some(v) = opt_str(exp_run, "intent_class") {
            assert_eq!(actual.intent_class.as_deref(), Some(v), "{case_id} 阶段 {} intent_class", stage.index);
        }
        if let Some(v) = opt_str(exp_run, "belongs_to_me") {
            assert_eq!(actual.belongs_to_me.as_deref(), Some(v), "{case_id} 阶段 {} belongs_to_me", stage.index);
        }
        if let Some(v) = opt_str(exp_run, "raw_belongs_to_me") {
            assert_eq!(actual.raw_belongs_to_me.as_deref(), Some(v), "{case_id} 阶段 {} raw_belongs_to_me", stage.index);
        }
        if let Some(v) = opt_str(exp_run, "followup_type") {
            assert_eq!(actual.followup_type.as_deref(), Some(v), "{case_id} 阶段 {} followup_type", stage.index);
        }
        if let Some(v) = exp_run.get("needs_action").and_then(Value::as_bool) {
            assert_eq!(actual.needs_action, Some(v), "{case_id} 阶段 {} needs_action", stage.index);
        }
        if let Some(v) = opt_str(exp_run, "rejection_rule") {
            assert_eq!(actual.rejection_rule.as_deref(), Some(v), "{case_id} 阶段 {} rejection_rule", stage.index);
        }
        if let Some(min) = exp_run.get("confidence_min").and_then(Value::as_i64) {
            let actual_conf = actual.confidence.unwrap_or(i64::MIN);
            assert!(actual_conf >= min, "{case_id} 阶段 {} 置信 {} 低于下限 {min}", stage.index, actual_conf);
        }
        if let Some(post) = exp_run.get("post_validation") {
            check_keys(post, "expected post_validation", POST_KEYS);
            assert_eq!(opt_str(post, "belongs_to_me"), actual.belongs_to_me.as_deref());
            if let Some(rule) = opt_str(post, "downgrade_rule") {
                assert!(
                    actual.warnings.iter().any(|w| w == rule),
                    "{case_id} 阶段 {} 期望降级规则 {rule}，实际告警 {:?}",
                    stage.index, actual.warnings
                );
            }
        }
    }
}

fn assert_stage_items(
    case_id: &str,
    stage: &StageSnapshot,
    exp_stage: &Value,
    item_ids: &mut HashMap<(u32, String), String>,
) {
    let Some(items) = exp_stage.get("items").and_then(Value::as_array) else {
        return;
    };
    for exp_item in items {
        check_keys(exp_item, "expected item", ITEM_KEYS);
        let identity = &exp_item["identity"];
        check_keys(identity, "expected identity", IDENTITY_KEYS);
        let origin = identity["origin_record"].as_str().expect("origin_record");
        let actual = stage
            .items
            .iter()
            .find(|it| it.origin_record == origin)
            .unwrap_or_else(|| panic!("{case_id} 阶段 {} 缺少源于 {origin} 的事项", stage.index));
        item_ids.insert((stage.index, origin.to_string()), actual.id.clone());
        if let Some(intent) = opt_str(identity, "intent_class") {
            assert_eq!(actual.origin_intent_class, intent, "{case_id} 阶段 {} {origin} intent_class", stage.index);
        }
        if let Some(same_as) = exp_item.get("same_item_as_stage").and_then(Value::as_u64) {
            let expected_id = item_ids
                .get(&(same_as as u32, origin.to_string()))
                .unwrap_or_else(|| panic!("{case_id} 阶段 {} 未登记阶段 {same_as} 的 {origin}", stage.index));
            assert_eq!(&actual.id, expected_id, "{case_id} 阶段 {} {origin} 不是阶段 {same_as} 的同一事项", stage.index);
        }
        assert_item_fields(case_id, &format!("阶段 {}", stage.index), exp_item, actual);
    }
}

fn assert_item_fields(case_id: &str, where_: &str, exp_item: &Value, actual: &ItemView) {
    let ctx = format!("{case_id} {where_} {}", actual.origin_record);
    let f = |key: &str| exp_item.get(key);
    if let Some(v) = f("title_contains").and_then(Value::as_array) {
        for needle in v.iter().filter_map(Value::as_str) {
            assert!(actual.title.contains(needle), "{ctx} 标题 {:?} 应含 {needle}", actual.title);
        }
    }
    if let Some(v) = f("lifecycle") {
        assert_eq!(actual.lifecycle, v.as_str().expect("lifecycle"), "{ctx} lifecycle");
    }
    if let Some(v) = f("lifecycle_must_not_be").and_then(Value::as_array) {
        for bad in v.iter().filter_map(Value::as_str) {
            assert_ne!(actual.lifecycle, bad, "{ctx} lifecycle 不得为 {bad}");
        }
    }
    if let Some(v) = f("attention") {
        assert_eq!(actual.attention, v.as_str().expect("attention"), "{ctx} attention");
    }
    if let Some(v) = f("current_turn") {
        assert_eq!(actual.current_turn, v.as_str().expect("current_turn"), "{ctx} current_turn");
    }
    if let Some(v) = f("next_action_nonempty") {
        let nonempty = actual.next_action.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false);
        assert_eq!(nonempty, v.as_bool().expect("next_action_nonempty"), "{ctx} next_action_nonempty");
    }
    if let Some(v) = f("next_action_contains").and_then(Value::as_array) {
        let next = actual.next_action.as_deref().unwrap_or_default();
        for needle in v.iter().filter_map(Value::as_str) {
            assert!(next.contains(needle), "{ctx} next_action {:?} 应含 {needle}", next);
        }
    }
    if let Some(v) = f("followup_type") {
        assert_eq!(actual.followup_type, v.as_str().expect("followup_type"), "{ctx} followup_type");
    }
    if let Some(v) = f("followup_at") {
        assert_eq!(actual.followup_at, v.as_i64(), "{ctx} followup_at（不得凭空造时间）");
    }
    if let Some(v) = f("awaiting_confirmation") {
        assert_eq!(actual.awaiting_confirmation, v.as_bool().expect("awaiting_confirmation"), "{ctx} awaiting_confirmation");
    }
    if let Some(v) = f("ignore_reason_nonempty") {
        let nonempty = actual.ignore_reason.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false);
        assert_eq!(nonempty, v.as_bool().expect("ignore_reason_nonempty"), "{ctx} ignore_reason_nonempty");
    }
    if let Some(v) = f("expected_ignore_reason") {
        assert_eq!(actual.ignore_reason.as_deref(), Some(v.as_str().expect("expected_ignore_reason")), "{ctx} ignore_reason");
    }
    if let Some(v) = f("close_reason_nonempty") {
        let nonempty = actual.close_reason.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false);
        assert_eq!(nonempty, v.as_bool().expect("close_reason_nonempty"), "{ctx} close_reason_nonempty");
    }
    if let Some(v) = f("resolution_evidence") {
        assert_eq!(actual.resolution_evidence.as_deref(), Some(v.as_str().expect("resolution_evidence")), "{ctx} resolution_evidence");
    }
    if let Some(today) = f("today") {
        check_keys(today, "expected today", TODAY_KEYS);
        if let Some(v) = today.get("visible") {
            assert_eq!(actual.today_visible, v.as_bool().expect("visible"), "{ctx} today.visible");
        }
        if let Some(v) = opt_str(today, "group") {
            assert_eq!(actual.today_group, v, "{ctx} today.group");
        }
    }
}

fn assert_events(where_: &str, expected: &[Value], actual: &[EventView]) {
    let mut ai = 0;
    for ev in expected {
        check_keys(ev, "expected work_event", EVENT_KEYS);
        let event_type = ev["event_type"].as_str().expect("event_type");
        let actor = ev["actor"].as_str().expect("actor");
        let min = ev.get("min_count").and_then(Value::as_i64).unwrap_or(1).max(1) as usize;
        let mut matched = 0;
        while ai < actual.len() && actual[ai].event_type == event_type && actual[ai].actor == actor {
            matched += 1;
            ai += 1;
        }
        assert!(
            matched >= min,
            "{where_} 期望事件 {event_type}({actor})×≥{min}，实际仅 {matched} 次；完整序列：{:?}",
            actual.iter().map(|e| (e.event_type, e.actor)).collect::<Vec<_>>()
        );
    }
}

fn assert_overrides(where_: &str, expected: &[Value], actual: &[workcontext_app::replay::OverrideView]) {
    assert_eq!(
        expected.len(),
        actual.len(),
        "{where_} field_overrides 数量：{:?}",
        actual
            .iter()
            .map(|o| (o.field.as_str(), o.value_json.as_deref()))
            .collect::<Vec<_>>()
    );
    for (exp, act) in expected.iter().zip(actual) {
        check_keys(exp, "expected field_override", OVERRIDE_KEYS);
        assert_eq!(act.field, exp["field"].as_str().expect("field"), "{where_} override.field");
        assert_eq!(act.value_json.as_deref(), exp.get("value_json").and_then(Value::as_str), "{where_} override.value_json");
        assert_eq!(act.locked, exp.get("locked").and_then(Value::as_bool).unwrap_or(true), "{where_} override.locked");
    }
}

fn assert_final(
    case_id: &str,
    outcome: &ReplayOutcome,
    exp_final: &Value,
    item_ids: &mut HashMap<(u32, String), String>,
) {
    let final_snapshot = &outcome.final_snapshot;
    check_keys(exp_final, "expected final", FINAL_KEYS);
    assert_eq!(
        final_snapshot.item_count,
        exp_final["item_count"].as_u64().expect("item_count") as usize,
        "{case_id} final.item_count"
    );
    if let Some(counts) = exp_final.get("counts") {
        for key in ["active", "candidate", "closed", "ignored"] {
            assert_eq!(
                final_snapshot.counts_for(key),
                counts[key].as_u64().expect(key) as usize,
                "{case_id} final.counts.{key}"
            );
        }
    }
    // 期末事项集合必须与断言清单完全一致（多一条即失败：不给他人/咨询对象建项）。
    if let Some(items) = exp_final.get("items").and_then(Value::as_array) {
        assert_eq!(items.len(), final_snapshot.items.len(), "{case_id} final.items 数量");
        for exp_item in items {
            check_keys(exp_item, "expected final item", ITEM_KEYS);
            let identity = &exp_item["identity"];
            check_keys(identity, "expected identity", IDENTITY_KEYS);
            let origin = identity["origin_record"].as_str().expect("origin_record");
            let actual = final_snapshot
                .items
                .iter()
                .find(|it| it.origin_record == origin)
                .unwrap_or_else(|| panic!("{case_id} final 缺少源于 {origin} 的事项"));
            item_ids.insert((u32::MAX, origin.to_string()), actual.id.clone());
            assert_item_fields(case_id, "final", exp_item, actual);
        }
    }
    if let Some(events) = exp_final.get("work_events") {
        assert_events(&format!("{case_id} final"), events.as_array().expect("work_events"), &final_snapshot.work_events);
    }
    if let Some(overrides) = exp_final.get("field_overrides").and_then(Value::as_array) {
        assert_overrides(&format!("{case_id} final"), overrides, &final_snapshot.field_overrides);
    }
    if let Some(records) = exp_final.get("no_item_from_records").and_then(Value::as_array) {
        for id in records.iter().filter_map(Value::as_str) {
            assert!(
                final_snapshot.items.iter().all(|it| it.origin_record != id),
                "{case_id} 不得从无关记录 {id} 建项"
            );
        }
    }
    if let Some(forbidden) = exp_final.get("forbidden").and_then(Value::as_object) {
        for (key, _) in forbidden {
            match key.as_str() {
                // 悬挂：期末不得存在未关闭/未忽略的开放事项。
                "hanging_item" => assert!(
                    final_snapshot.items.iter().all(|it| !matches!(it.lifecycle, "active" | "candidate")),
                    "{case_id} 期末不得有悬挂事项"
                ),
                "awaiting_confirmation" => assert!(
                    final_snapshot.items.iter().all(|it| !it.awaiting_confirmation),
                    "{case_id} 期末不得有待确认事项"
                ),
                "active_item" | "silent_restore" => assert!(
                    final_snapshot.items.iter().all(|it| it.lifecycle != "active"),
                    "{case_id} 期末不得有 active 事项（{key}）"
                ),
                // 无依据时间：所有事项 followup_at 必须为空。
                "followup_at_not_null" => assert!(
                    final_snapshot.items.iter().all(|it| it.followup_at.is_none()),
                    "{case_id} 不得凭空产出跟进时间"
                ),
                // 多建事项 / 为他人建项：期末集合已与断言清单精确对账。
                "extra_items" | "assign_to_lisi_or_magong" => {}
                // 全锁：只允许锁用户真正表态的字段（与 field_overrides 对账）。
                "full_lock" => {
                    let expected_locks: Vec<String> = exp_final["field_overrides"]
                        .as_array()
                        .map(|os| {
                            os.iter()
                                .filter_map(|o| o["field"].as_str())
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    for item in &final_snapshot.items {
                        assert!(
                            item.locked_fields.iter().all(|f| expected_locks.contains(f)),
                            "{case_id} 事项 {} 锁定字段 {:?} 超出人工表态范围 {:?}",
                            item.id, item.locked_fields, expected_locks
                        );
                    }
                }
                other => panic!("{case_id} final.forbidden 出现未实现的键 {other}"),
            }
        }
    }
    if let Some(strings) = exp_final.get("forbidden_strings_in_ui").and_then(Value::as_array) {
        for bad in strings.iter().filter_map(Value::as_str) {
            for stage in &outcome.stages {
                for item in &stage.items {
                    for text in [
                        Some(item.title.as_str()),
                        item.next_action.as_deref(),
                        item.close_reason.as_deref(),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        assert!(!text.contains(bad), "{case_id} UI 字段 {:?} 不得包含 {bad}", text);
                    }
                }
            }
        }
    }
}
