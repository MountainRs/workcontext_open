//! `classify/v1` 输出契约校验（详细设计 §8.2；JSON Schema 见
//! `fixtures/schema/classify_v1.json`，两者由测试互相对账）。
//!
//! 只做结构 / 枚举 / 长度层校验；V01–V12 业务策略（归属锚点、收尾门、时间依据…）
//! 属 core 的 `PolicyValidator`，不在本层。
//!
//! 与 JSON Schema 的两点口径差异（有意为之）：
//! - `followup.at` 非空即通过：date-time 的解析失败由策略层按 V04 清空（§8.2 处理约定）；
//! - 长度按 Unicode 字符数（`str::chars().count()`）计，与 Schema 的码点语义一致。
//!
//! - `resolution` 可选：**出现即提议收尾**。`evidence_type` 只允许消息证据可
//!   支撑的类型（[`RESOLUTION_EVIDENCE_TYPES`]）；`human_confirmed` 属人工
//!   操作，模型输出一律拒绝。是否允许收尾由策略层按 §3.3 白名单 + V05 收尾门
//!   裁决（ack_only → 降级待人工确认，如 A01 阶段4）。

use serde_json::Value;

/// 输出契约版本。
pub const OUTPUT_SCHEMA_VERSION: &str = "classify/v1";

/// `classification` 合法值（未知枚举 → 整次 rejected，不猜测）。
pub const CLASSIFICATIONS: [&str; 6] = [
    "info",
    "one_off",
    "followup",
    "thread_update",
    "needs_confirmation",
    "noise",
];
/// `belongs_to_me` 合法值。
pub const BELONGS_TO_ME: [&str; 3] = ["yes", "no", "unclear"];
/// `current_turn` 合法值。
pub const CURRENT_TURNS: [&str; 3] = ["me", "other", "none"];
/// `followup.type` 合法值。
pub const FOLLOWUP_TYPES: [&str; 5] = ["none", "deadline", "promised_by", "manual", "check_later"];
/// `resolution.evidence_type` 模型可用的收尾证据类型。
///
/// §3.3 收尾证据白名单中可由**消息证据**支撑的 4 种。`human_confirmed` 只能由
/// 人工操作产生（如 A01 阶段5 的 `human_actions.item_close`），模型输出中声明
/// 一律拒绝——不让模型替人类确认。
pub const RESOLUTION_EVIDENCE_TYPES: [&str; 4] = [
    "explicit_complete",
    "artifact_accepted",
    "ack_only",
    "artifact_created",
];

/// 顶层允许的字段（`additionalProperties=false`）。
pub const ALLOWED_KEYS: [&str; 15] = [
    "schema_version",
    "source_ids",
    "classification",
    "belongs_to_me",
    "needs_action",
    "suggested_title",
    "next_action",
    "current_turn",
    "followup",
    "evidence",
    "rationale",
    "confidence",
    "uncertainties",
    "related_hint",
    "resolution",
];

/// 必填字段（`resolution` 可选：**出现即提议收尾**，缺省 = 不提议收尾）。
pub const REQUIRED_KEYS: [&str; 14] = [
    "schema_version",
    "source_ids",
    "classification",
    "belongs_to_me",
    "needs_action",
    "suggested_title",
    "next_action",
    "current_turn",
    "followup",
    "evidence",
    "rationale",
    "confidence",
    "uncertainties",
    "related_hint",
];

/// 契约违例：首个错误（按下方校验顺序，保证错误信息确定）。
#[derive(Debug, thiserror::Error)]
#[error("classify/v1 契约违例 {path}：{reason}")]
pub struct ContractError {
    pub path: String,
    pub reason: String,
}

fn violation(path: &str, reason: impl Into<String>) -> ContractError {
    ContractError {
        path: path.to_string(),
        reason: reason.into(),
    }
}

type Result<T, E = ContractError> = std::result::Result<T, E>;

/// 校验一个 `classify/v1` 输出载荷。
pub fn validate_classify_v1(output: &Value) -> Result<()> {
    let root = output
        .as_object()
        .ok_or_else(|| violation("$", "顶层必须是对象"))?;

    for key in root.keys() {
        if !ALLOWED_KEYS.contains(&key.as_str()) {
            return Err(violation(
                &format!("$.{key}"),
                format!("未知字段（additionalProperties=false）：{key}"),
            ));
        }
    }
    for key in REQUIRED_KEYS {
        if !root.contains_key(key) {
            return Err(violation("$", format!("缺少必填字段：{key}")));
        }
    }

    let schema_version = str_at(root, "$.schema_version")?;
    if schema_version != OUTPUT_SCHEMA_VERSION {
        return Err(violation(
            "$.schema_version",
            format!("必须为 {OUTPUT_SCHEMA_VERSION}，实际为 {schema_version}"),
        ));
    }

    let source_ids = root["source_ids"]
        .as_array()
        .ok_or_else(|| violation("$.source_ids", "必须是字符串数组"))?;
    if source_ids.is_empty() {
        return Err(violation("$.source_ids", "至少 1 个元素（minItems=1）"));
    }
    for (i, id) in source_ids.iter().enumerate() {
        if !id.is_string() {
            return Err(violation(&format!("$.source_ids[{i}]"), "必须是字符串"));
        }
    }

    let classification = str_at(root, "$.classification")?;
    if !CLASSIFICATIONS.contains(&classification) {
        return Err(violation(
            "$.classification",
            format!("未知枚举值：{classification}（未知枚举不猜测，整次 rejected）"),
        ));
    }

    let belongs = str_at(root, "$.belongs_to_me")?;
    if !BELONGS_TO_ME.contains(&belongs) {
        return Err(violation("$.belongs_to_me", format!("未知枚举值：{belongs}")));
    }

    if !root["needs_action"].is_boolean() {
        return Err(violation("$.needs_action", "必须是布尔值"));
    }

    let title = str_at(root, "$.suggested_title")?;
    let title_len = title.chars().count();
    if title_len == 0 || title_len > 60 {
        return Err(violation("$.suggested_title", format!("长度必须在 1..=60，实际 {title_len}")));
    }

    match &root["next_action"] {
        Value::Null => {}
        v => {
            let s = v
                .as_str()
                .ok_or_else(|| violation("$.next_action", "必须是字符串或 null"))?;
            let len = s.chars().count();
            if len > 200 {
                return Err(violation("$.next_action", format!("长度必须 ≤200，实际 {len}")));
            }
        }
    }

    let turn = str_at(root, "$.current_turn")?;
    if !CURRENT_TURNS.contains(&turn) {
        return Err(violation("$.current_turn", format!("未知枚举值：{turn}")));
    }

    let followup = root["followup"]
        .as_object()
        .ok_or_else(|| violation("$.followup", "必须是对象 {type, at, basis}"))?;
    for key in followup.keys() {
        if !matches!(key.as_str(), "type" | "at" | "basis") {
            return Err(violation(
                &format!("$.followup.{key}"),
                format!("未知字段：{key}"),
            ));
        }
    }
    for key in ["type", "at", "basis"] {
        if !followup.contains_key(key) {
            return Err(violation("$.followup", format!("缺少字段：{key}")));
        }
    }
    let followup_type = str_at(followup, "$.followup.type")?;
    if !FOLLOWUP_TYPES.contains(&followup_type) {
        return Err(violation(
            "$.followup.type",
            format!("未知枚举值：{followup_type}"),
        ));
    }
    match &followup["at"] {
        Value::Null => {}
        Value::String(s) if !s.is_empty() => {}
        _ => {
            return Err(violation(
                "$.followup.at",
                "必须是 null 或非空 date-time 字符串（解析失败由策略层按 V04 清空）",
            ))
        }
    }
    match &followup["basis"] {
        Value::Null => {}
        v => {
            let s = v
                .as_str()
                .ok_or_else(|| violation("$.followup.basis", "必须是字符串或 null"))?;
            let len = s.chars().count();
            if len > 200 {
                return Err(violation("$.followup.basis", format!("长度必须 ≤200，实际 {len}")));
            }
        }
    }

    let evidence = root["evidence"]
        .as_array()
        .ok_or_else(|| violation("$.evidence", "必须是字符串数组"))?;
    if evidence.is_empty() {
        return Err(violation("$.evidence", "至少 1 个元素（minItems=1）"));
    }
    for (i, e) in evidence.iter().enumerate() {
        if !e.is_string() {
            return Err(violation(&format!("$.evidence[{i}]"), "必须是字符串"));
        }
    }

    let rationale = str_at(root, "$.rationale")?;
    let rationale_len = rationale.chars().count();
    if rationale_len == 0 || rationale_len > 500 {
        return Err(violation("$.rationale", format!("长度必须在 1..=500，实际 {rationale_len}（R08 必须给出理由）")));
    }

    let confidence = root["confidence"]
        .as_i64()
        .ok_or_else(|| violation("$.confidence", "必须是 0..=100 的整数"))?;
    if !(0..=100).contains(&confidence) {
        return Err(violation("$.confidence", format!("必须在 0..=100，实际 {confidence}")));
    }

    let uncertainties = root["uncertainties"]
        .as_array()
        .ok_or_else(|| violation("$.uncertainties", "必须是字符串数组"))?;
    if uncertainties.len() > 5 {
        return Err(violation(
            "$.uncertainties",
            format!("最多 5 个元素，实际 {}", uncertainties.len()),
        ));
    }
    for (i, u) in uncertainties.iter().enumerate() {
        if !u.is_string() {
            return Err(violation(&format!("$.uncertainties[{i}]"), "必须是字符串"));
        }
    }

    if !root["related_hint"].is_null() {
        let hint = root["related_hint"].as_object().ok_or_else(|| violation("$.related_hint", "必须是对象或 null"))?;
        if hint.keys().any(|k| !matches!(k.as_str(), "target_item_ids" | "reason"))
            || hint.get("target_item_ids").is_some_and(|v| !v.as_array().is_some_and(|ids| ids.iter().all(Value::is_string)))
            || hint.get("reason").is_some_and(|v| !v.is_null() && !v.is_string()) {
            return Err(violation("$.related_hint", "关联候选字段非法"));
        }
    }

    // resolution：可选；出现即提议收尾（是否允许收尾由策略层按 §3.3 白名单
    // 与 V05 收尾门裁决，本层只做结构/枚举校验）。
    if let Some(resolution) = root.get("resolution") {
        let res = resolution
            .as_object()
            .ok_or_else(|| violation("$.resolution", "必须是对象；不提议收尾时省略该字段"))?;
        for key in res.keys() {
            if !matches!(key.as_str(), "evidence_type" | "evidence_ids" | "note") {
                return Err(violation(
                    &format!("$.resolution.{key}"),
                    format!("未知字段：{key}"),
                ));
            }
        }
        for key in ["evidence_type", "evidence_ids"] {
            if !res.contains_key(key) {
                return Err(violation("$.resolution", format!("缺少字段：{key}")));
            }
        }
        let evidence_type = str_at(res, "$.resolution.evidence_type")?;
        if evidence_type == "human_confirmed" {
            return Err(violation(
                "$.resolution.evidence_type",
                "human_confirmed 只能由人工操作产生（§3.3；A01 阶段5 的 item_close），模型不得声明人工确认",
            ));
        }
        if !RESOLUTION_EVIDENCE_TYPES.contains(&evidence_type) {
            return Err(violation(
                "$.resolution.evidence_type",
                format!("未知枚举值：{evidence_type}"),
            ));
        }
        let evidence_ids = res["evidence_ids"]
            .as_array()
            .ok_or_else(|| violation("$.resolution.evidence_ids", "必须是字符串数组"))?;
        if evidence_ids.is_empty() {
            return Err(violation(
                "$.resolution.evidence_ids",
                "至少 1 个元素：提议收尾必须引用证据记录",
            ));
        }
        for (i, id) in evidence_ids.iter().enumerate() {
            if !id.is_string() {
                return Err(violation(
                    &format!("$.resolution.evidence_ids[{i}]"),
                    "必须是字符串",
                ));
            }
        }
        if let Some(note) = res.get("note") {
            let note = note
                .as_str()
                .ok_or_else(|| violation("$.resolution.note", "必须是字符串（可省略）"))?;
            let len = note.chars().count();
            if len > 200 {
                return Err(violation(
                    "$.resolution.note",
                    format!("长度必须 ≤200，实际 {len}"),
                ));
            }
        }
    }

    Ok(())
}

fn str_at<'a>(
    map: &'a serde_json::Map<String, Value>,
    path: &str,
) -> std::result::Result<&'a str, ContractError> {
    // 仅在字段已在场的前提下调用（上方已做必填检查）；`map[key]` 不会 panic。
    map[path.rsplit('.').next().unwrap_or_default()]
        .as_str()
        .ok_or_else(|| violation(path, "必须是字符串"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid() -> Value {
        json!({
            "schema_version": "classify/v1",
            "source_ids": ["s1"],
            "classification": "followup",
            "belongs_to_me": "yes",
            "needs_action": true,
            "suggested_title": "标题",
            "next_action": "下一步",
            "current_turn": "me",
            "followup": {"type": "none", "at": null, "basis": null},
            "evidence": ["s1"],
            "rationale": "理由",
            "confidence": 70,
            "uncertainties": [],
            "related_hint": null
        })
    }

    #[test]
    fn accepts_valid_output() {
        assert!(validate_classify_v1(&valid()).is_ok());
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let mut v = valid();
        v["tool_calls"] = json!([]);
        let err = validate_classify_v1(&v).unwrap_err();
        assert!(err.path == "$.tool_calls", "{err}");
    }

    #[test]
    fn rejects_missing_field() {
        let mut v = valid();
        v.as_object_mut().unwrap().remove("rationale");
        let err = validate_classify_v1(&v).unwrap_err();
        assert!(err.reason.contains("rationale"), "{err}");
    }

    #[test]
    fn rejects_unknown_classification() {
        let mut v = valid();
        v["classification"] = json!("random");
        let err = validate_classify_v1(&v).unwrap_err();
        assert!(err.path == "$.classification", "{err}");
    }

    #[test]
    fn rejects_belongs_and_turn_enums() {
        let mut v = valid();
        v["belongs_to_me"] = json!("maybe");
        assert!(validate_classify_v1(&v).is_err());
        let mut v = valid();
        v["current_turn"] = json!("us");
        assert!(validate_classify_v1(&v).is_err());
    }

    #[test]
    fn rejects_confidence_out_of_range_and_non_integer() {
        let mut v = valid();
        v["confidence"] = json!(101);
        assert!(validate_classify_v1(&v).is_err());
        let mut v = valid();
        v["confidence"] = json!(70.5);
        assert!(validate_classify_v1(&v).is_err());
    }

    #[test]
    fn rejects_title_bounds_and_empty_evidence() {
        let mut v = valid();
        v["suggested_title"] = json!("");
        assert!(validate_classify_v1(&v).is_err());
        let mut v = valid();
        v["suggested_title"] = json!("长".repeat(61));
        assert!(validate_classify_v1(&v).is_err());
        let mut v = valid();
        v["suggested_title"] = json!("长".repeat(60));
        assert!(validate_classify_v1(&v).is_ok());
        let mut v = valid();
        v["evidence"] = json!([]);
        assert!(validate_classify_v1(&v).is_err());
    }

    #[test]
    fn rejects_followup_shape_violations() {
        let mut v = valid();
        v["followup"] = json!({"type": "deadline", "at": "", "basis": null});
        assert!(validate_classify_v1(&v).is_err(), "空 at 不是合法 date-time");
        let mut v = valid();
        v["followup"] = json!({"type": "someday", "at": null, "basis": null});
        assert!(validate_classify_v1(&v).is_err());
        let mut v = valid();
        v["followup"] = json!({"type": "none", "at": null});
        assert!(validate_classify_v1(&v).is_err(), "缺 basis");
    }

    #[test]
    fn rejects_too_many_uncertainties() {
        let mut v = valid();
        v["uncertainties"] = json!(["a", "b", "c", "d", "e", "f"]);
        assert!(validate_classify_v1(&v).is_err());
        v["uncertainties"] = json!(["a", "b", "c", "d", "e"]);
        assert!(validate_classify_v1(&v).is_ok());
    }

    #[test]
    fn rejects_non_object_root() {
        assert!(validate_classify_v1(&json!("ok")).is_err());
        assert!(validate_classify_v1(&json!([])).is_err());
    }

    #[test]
    fn accepts_optional_resolution_and_absence() {
        let v = valid();
        assert!(
            validate_classify_v1(&v).is_ok(),
            "缺省 resolution = 不提议收尾"
        );
        let mut v = valid();
        v["resolution"] = json!({
            "evidence_type": "ack_only",
            "evidence_ids": ["R5"],
            "note": "「好的」仅为确认性回复，不足以自动收尾"
        });
        assert!(validate_classify_v1(&v).is_ok());
    }

    #[test]
    fn resolution_must_not_declare_human_confirmed() {
        let mut v = valid();
        v["resolution"] = json!({"evidence_type": "human_confirmed", "evidence_ids": ["R5"]});
        let err = validate_classify_v1(&v).unwrap_err();
        assert!(err.path == "$.resolution.evidence_type", "{err}");
        assert!(err.reason.contains("人工"), "{err}");
    }

    #[test]
    fn rejects_malformed_resolution() {
        let mut v = valid();
        v["resolution"] = json!({"evidence_ids": ["R5"]});
        assert!(validate_classify_v1(&v).is_err(), "缺 evidence_type");
        let mut v = valid();
        v["resolution"] = json!({"evidence_type": "ack_only", "evidence_ids": []});
        assert!(validate_classify_v1(&v).is_err(), "提议收尾必须引用证据");
        let mut v = valid();
        v["resolution"] = json!({"evidence_type": "someday", "evidence_ids": ["R5"]});
        assert!(validate_classify_v1(&v).is_err(), "未知枚举");
        let mut v = valid();
        v["resolution"] =
            json!({"evidence_type": "ack_only", "evidence_ids": ["R5"], "extra": 1});
        assert!(validate_classify_v1(&v).is_err(), "未知字段");
        let mut v = valid();
        v["resolution"] = json!(null);
        assert!(validate_classify_v1(&v).is_err(), "要么省略，要么是对象");
        let mut v = valid();
        v["resolution"] = json!({
            "evidence_type": "ack_only",
            "evidence_ids": ["R5"],
            "note": "好".repeat(201)
        });
        assert!(validate_classify_v1(&v).is_err(), "note ≤200");
    }
}
