//! classify/v1 的类型化输入输出（详细设计 §8.2）。
//!
//! - 输出契约以本模块的 Rust 类型为唯一实现事实；JSON Schema 文件由 infra 维护在
//!   `fixtures/schema/classify_v1.json`，两者由集成验证统一比对，不在 core 重复一份。
//! - 除 `resolution`（core 补全的收尾提议通道，V05/§3.3 白名单的输入）外，
//!   字段与详设 §8.2 表逐字一致；未知字段一律拒绝（V01/V11）。
//! - 输入侧 [`JudgeContext`] 描述一次判读可引用的记录全集与本人标识，
//!   是 V02（引用）、V03（归属锚点）、V04（时间依据）、V12（时钟）的判定依据。

use serde::{Deserialize, Serialize};

use crate::state::{CurrentTurn, LockedField, ResolutionEvidence, ResolutionEvidenceType};

/// 输出契约版本常量（§8.2：`schema_version` 必须等于它）。
pub const SCHEMA_VERSION: &str = "classify/v1";

/// 判读分类（§8.2）：`random` 不是合法值；`noise` 表示系统消息/广告/无关。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    Info,
    OneOff,
    Followup,
    ThreadUpdate,
    NeedsConfirmation,
    Noise,
}

/// 归属断言（§8.2）。`yes` 必须过 V03 归属锚点，否则降级 `unclear`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BelongsToMe {
    Yes,
    No,
    Unclear,
}

/// Agent 侧跟进类型（§8.2）；到库内 `work_item.followup_type` 的映射见 `validate`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentFollowupType {
    #[serde(rename = "none")]
    None,
    Deadline,
    PromisedBy,
    Manual,
    CheckLater,
}

/// `followup` 对象：`at` 为字符串（模型友好），入库前转 UTC 毫秒。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Followup {
    #[serde(rename = "type")]
    pub r#type: AgentFollowupType,
    pub at: Option<String>,
    pub basis: Option<String>,
}

/// 收尾提议（core 对 §8.2 的补全通道）：对象出现即「提议收尾」。
/// `evidence_type` 取值即 §3.3 收尾证据白名单的类型全集；
/// `ack_only` / `artifact_created` 会被 V05 收尾门降级（A01 第 4 阶段）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resolution {
    pub evidence_type: ResolutionEvidenceType,
    #[serde(default)]
    pub evidence_ids: Vec<String>,
    pub note: Option<String>,
}

impl From<Resolution> for ResolutionEvidence {
    fn from(r: Resolution) -> Self {
        ResolutionEvidence {
            evidence_type: r.evidence_type,
            evidence_ids: r.evidence_ids,
            note: r.note,
        }
    }
}

/// 关联提示（§5.1 V06）：只能生成 `similar_candidate`，永不自动置 `thread_id`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelatedHint {
    #[serde(default)]
    pub target_item_ids: Vec<String>,
    pub reason: Option<String>,
}

/// classify/v1 输出的类型化表示。`deny_unknown_fields` 兜底：含
/// `actions`/`tool_calls` 等副作用字段直接解析失败（V01/V11）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassifyOutput {
    pub schema_version: String,
    /// minItems 1；且必须 ⊆ 本次上下文提供的 ID（V02）。
    pub source_ids: Vec<String>,
    pub classification: Classification,
    pub belongs_to_me: BelongsToMe,
    pub needs_action: bool,
    /// 1–60；超长按 V09 截断。
    pub suggested_title: String,
    /// ≤200；超长按 V09 截断。
    pub next_action: Option<String>,
    pub current_turn: CurrentTurn,
    pub followup: Followup,
    /// 收尾提议；缺省表示不提议。
    #[serde(default)]
    pub resolution: Option<Resolution>,
    /// minItems 1；引用无效 ID 按 V02 剔除。
    pub evidence: Vec<String>,
    /// 1–500（R08：必须有）；超长按 V09 截断。
    pub rationale: String,
    /// 0–100 整数；<60 触发 V08。
    pub confidence: i64,
    /// ≤5。
    pub uncertainties: Vec<String>,
    pub related_hint: Option<RelatedHint>,
}

/// 记录类型（迁移契约 `source_record.kind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    ImMessage,
    WeeklyReport,
    File,
    ManualNote,
}

/// 会话类型（V03 锚点①需要区分私聊）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationType {
    Dm,
    Group,
    Other,
}

/// 消息方向（迁移契约 `source_record.direction`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    In,
    Out,
    Na,
}

/// 一条进入判读上下文的记录元数据（V02/V03/V04/V12 的输入）。
///
/// `id` 是判读上下文中使用的记录标识（回放夹具中即 `external_id`，如 `A01-R1`）；
/// `source_id`/`external_id` 用于幂等键计算（§4.4）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordContext {
    pub id: String,
    pub source_id: String,
    pub external_id: String,
    pub kind: RecordKind,
    pub conversation_id: Option<String>,
    pub conversation_type: ConversationType,
    pub direction: Direction,
    pub sender_id: Option<String>,
    /// @ 标记的用户 ID 列表；正文里出现姓名不算（A06-R3）。
    pub mentions: Vec<String>,
    /// UTC 毫秒。
    pub occurred_at: i64,
    /// UTC 毫秒。
    pub ingested_at: i64,
    pub summary: Option<String>,
    /// 证据正文（V04 basis 子串匹配的对象）；到期可为 `None`。
    pub body: Option<String>,
}

/// 本人标识（V03 锚点②③的判定基准）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeContext {
    pub user_id: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

/// 判读目标可能命中的既有事项（由应用层按 dedupe_key / 会话匹配后传入）。
/// 携带人工锁（V07）与既有状态（V10 更新路径、状态机守卫）。
#[derive(Debug, Clone, PartialEq)]
pub struct ExistingItem {
    pub id: String,
    pub lifecycle: crate::state::Lifecycle,
    pub attention: crate::state::Attention,
    pub title: String,
    pub next_action: Option<String>,
    pub followup_type: crate::state::DbFollowupType,
    pub followup_at: Option<i64>,
    pub awaiting_confirmation: bool,
    pub resolution_evidence: Option<String>,
    pub last_evidence_at: Option<i64>,
    pub locked_fields: Vec<LockedField>,
}

/// 一次判读的输入上下文：合法引用全集 + 本人 + 可能的既有目标事项。
#[derive(Debug)]
pub struct JudgeContext<'a> {
    pub records: &'a [RecordContext],
    pub me: &'a MeContext,
    pub existing: Option<&'a ExistingItem>,
}

impl<'a> JudgeContext<'a> {
    pub fn new(records: &'a [RecordContext], me: &'a MeContext) -> Self {
        JudgeContext {
            records,
            me,
            existing: None,
        }
    }

    pub fn with_existing(mut self, existing: Option<&'a ExistingItem>) -> Self {
        self.existing = existing;
        self
    }

    pub fn record(&self, id: &str) -> Option<&'a RecordContext> {
        self.records.iter().find(|r| r.id == id)
    }
}

/// 解析失败分类：`BadJson`/`Schema` 计 V01，`UnknownField` 计 V11（会话被污染）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("输出不是合法 JSON：{0}")]
    BadJson(String),
    #[error("输出含未知字段 {field}（不得含 actions/tool_calls 等副作用字段）")]
    UnknownField { field: String },
    #[error("schema 校验失败 {path}：{message}")]
    Schema { path: String, message: String },
}

/// classify/v1 顶层字段全集（与 `ClassifyOutput` 一一对应；改动须同步 infra 的
/// `fixtures/schema/classify_v1.json`）。
pub const OUTPUT_FIELDS: &[&str] = &[
    "schema_version",
    "source_ids",
    "classification",
    "belongs_to_me",
    "needs_action",
    "suggested_title",
    "next_action",
    "current_turn",
    "followup",
    "resolution",
    "evidence",
    "rationale",
    "confidence",
    "uncertainties",
    "related_hint",
];

const FOLLOWUP_FIELDS: &[&str] = &["type", "at", "basis"];
const RESOLUTION_FIELDS: &[&str] = &["evidence_type", "evidence_ids", "note"];
const RELATED_HINT_FIELDS: &[&str] = &["target_item_ids", "reason"];

/// 解析并校验一条 classify/v1 输出。
///
/// 步骤：JSON 解析（V01）→ 未知字段扫描（V11，顶层与 `followup`/`resolution`/
/// `related_hint` 嵌套对象）→ 类型化反序列化（V01：必填齐、枚举合法）→
/// 语义边界（V01：常量版本、minItems、confidence 区间、必填文本非空）。
pub fn parse_classify_output(raw: &str) -> Result<ClassifyOutput, ParseError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| ParseError::BadJson(e.to_string()))?;
    if let Some(field) = first_unknown_field(&value) {
        return Err(ParseError::UnknownField { field });
    }
    let output: ClassifyOutput =
        serde_json::from_value(value).map_err(|e| schema_error(&e))?;
    semantic_schema_checks(&output)?;
    Ok(output)
}

fn schema_error(e: &serde_json::Error) -> ParseError {
    let message = e.to_string();
    // 首个错误路径（§8.3）：missing field 尽量给出字段名，其余记 "$"。
    let path = match message.strip_prefix("missing field `") {
        Some(rest) => rest.split('`').next().unwrap_or("$").to_string(),
        None => "$".to_string(),
    };
    ParseError::Schema { path, message }
}

fn unknown_in(
    value: &serde_json::Value,
    allowed: &[&str],
    prefix: &str,
) -> Option<String> {
    let object = value.as_object()?;
    let key = object.keys().find(|k| !allowed.contains(&k.as_str()))?;
    Some(if prefix.is_empty() {
        key.clone()
    } else {
        format!("{prefix}.{key}")
    })
}

/// 顶层与嵌套对象（followup/resolution/related_hint）的未知字段扫描（V11）。
fn first_unknown_field(value: &serde_json::Value) -> Option<String> {
    unknown_in(value, OUTPUT_FIELDS, "")
        .or_else(|| {
            value
                .get("followup")
                .and_then(|v| unknown_in(v, FOLLOWUP_FIELDS, "followup"))
        })
        .or_else(|| {
            value
                .get("resolution")
                .and_then(|v| unknown_in(v, RESOLUTION_FIELDS, "resolution"))
        })
        .or_else(|| {
            value
                .get("related_hint")
                .and_then(|v| unknown_in(v, RELATED_HINT_FIELDS, "related_hint"))
        })
}

/// serde 覆盖不到的 schema 约束（§8.2 表）。
fn semantic_schema_checks(o: &ClassifyOutput) -> Result<(), ParseError> {
    let reject = |path: &str, message: String| {
        Err(ParseError::Schema {
            path: path.to_string(),
            message,
        })
    };
    if o.schema_version != SCHEMA_VERSION {
        return reject(
            "schema_version",
            format!("必须为 \"{SCHEMA_VERSION}\"，实际为 {:?}", o.schema_version),
        );
    }
    if o.source_ids.is_empty() {
        return reject("source_ids", "source_ids 至少 1 项".to_string());
    }
    if o.evidence.is_empty() {
        return reject("evidence", "evidence 至少 1 项".to_string());
    }
    if !(0..=100).contains(&o.confidence) {
        return reject(
            "confidence",
            format!("confidence 必须是 0–100 的整数，实际为 {}", o.confidence),
        );
    }
    if o.suggested_title.trim().is_empty() {
        return reject("suggested_title", "suggested_title 不能为空".to_string());
    }
    if o.rationale.trim().is_empty() {
        return reject("rationale", "rationale 必须有（R08）".to_string());
    }
    if o.uncertainties.len() > 5 {
        return reject(
            "uncertainties",
            format!("uncertainties 至多 5 项，实际 {} 项", o.uncertainties.len()),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 详设 §8.2 的示例输出（补 `resolution: null` 之外的默认形状）。
    fn sample() -> serde_json::Value {
        json!({
            "schema_version": "classify/v1",
            "source_ids": ["s1"],
            "classification": "followup",
            "belongs_to_me": "yes",
            "needs_action": true,
            "suggested_title": "确认图纸业务复核结果",
            "next_action": "等待业务方复测并确认是否仍存在问题",
            "current_turn": "other",
            "followup": { "type": "none", "at": null, "basis": null },
            "evidence": ["s1"],
            "rationale": "对方明确请本人协调复测，当前等待业务方结果",
            "confidence": 78,
            "uncertainties": ["未确认业务方是否已收到复测通知"],
            "related_hint": null
        })
    }

    fn parse(v: serde_json::Value) -> Result<ClassifyOutput, ParseError> {
        parse_classify_output(&v.to_string())
    }

    #[test]
    fn parses_design_example() {
        let out = parse(sample()).expect("详设 §8.2 示例必须可解析");
        assert_eq!(out.schema_version, "classify/v1");
        assert_eq!(out.classification, Classification::Followup);
        assert_eq!(out.belongs_to_me, BelongsToMe::Yes);
        assert_eq!(out.current_turn, CurrentTurn::Other);
        assert_eq!(out.followup.r#type, AgentFollowupType::None);
        assert_eq!(out.confidence, 78);
        assert_eq!(out.resolution, None);
    }

    #[test]
    fn rejects_bad_json() {
        let err = parse_classify_output("{ not json").unwrap_err();
        assert!(matches!(err, ParseError::BadJson(_)));
    }

    #[test]
    fn rejects_unknown_top_level_field_as_v11() {
        // V11：输出不得含 actions/tool_calls 等副作用字段（会话被污染）。
        let mut v = sample();
        v["actions"] = json!([{ "tool": "send_message" }]);
        let err = parse(v).unwrap_err();
        assert_eq!(err, ParseError::UnknownField { field: "actions".into() });
        let mut v = sample();
        v["tool_calls"] = json!([]);
        assert_eq!(
            parse(v).unwrap_err(),
            ParseError::UnknownField { field: "tool_calls".into() }
        );
    }

    #[test]
    fn rejects_unknown_nested_field_as_v11() {
        let mut v = sample();
        v["followup"]["deadline_v2"] = json!(true);
        assert_eq!(
            parse(v).unwrap_err(),
            ParseError::UnknownField { field: "followup.deadline_v2".into() }
        );
        let mut v = sample();
        v["resolution"] = json!({ "evidence_type": "ack_only", "tool": "x" });
        assert_eq!(
            parse(v).unwrap_err(),
            ParseError::UnknownField { field: "resolution.tool".into() }
        );
    }

    #[test]
    fn rejects_missing_and_invalid_schema_values_as_v01() {
        // 缺必填：rationale（R08 必须有）。
        let mut v = sample();
        v.as_object_mut().unwrap().remove("rationale");
        match parse(v).unwrap_err() {
            ParseError::Schema { path, .. } => assert_eq!(path, "rationale"),
            other => panic!("期望 Schema 错误，得到 {other:?}"),
        }
        // 非法枚举：random 不是合法值（§8.2）。
        let mut v = sample();
        v["classification"] = json!("random");
        assert!(matches!(parse(v).unwrap_err(), ParseError::Schema { .. }));
        // 类型错误：confidence 必须是整数。
        let mut v = sample();
        v["confidence"] = json!("high");
        assert!(matches!(parse(v).unwrap_err(), ParseError::Schema { .. }));
        // 版本常量。
        let mut v = sample();
        v["schema_version"] = json!("classify/v2");
        assert!(matches!(parse(v).unwrap_err(), ParseError::Schema { .. }));
    }

    #[test]
    fn semantic_bounds_are_v01() {
        for (path, mutate) in [
            ("confidence", Box::new(|v: &mut serde_json::Value| {
                v["confidence"] = json!(101);
            }) as Box<dyn Fn(&mut serde_json::Value)>,
            ),
            ("confidence", Box::new(|v: &mut serde_json::Value| {
                v["confidence"] = json!(-1);
            })),
            ("source_ids", Box::new(|v: &mut serde_json::Value| {
                v["source_ids"] = json!([]);
            })),
            ("evidence", Box::new(|v: &mut serde_json::Value| {
                v["evidence"] = json!([]);
            })),
            ("suggested_title", Box::new(|v: &mut serde_json::Value| {
                v["suggested_title"] = json!("  ");
            })),
            ("rationale", Box::new(|v: &mut serde_json::Value| {
                v["rationale"] = json!("");
            })),
            ("uncertainties", Box::new(|v: &mut serde_json::Value| {
                v["uncertainties"] = json!(["a", "b", "c", "d", "e", "f"]);
            })),
        ] {
            let mut v = sample();
            mutate(&mut v);
            match parse(v).unwrap_err() {
                ParseError::Schema { path: p, .. } => assert_eq!(p, path),
                other => panic!("期望 Schema({path})，得到 {other:?}"),
            }
        }
    }

    #[test]
    fn output_fields_match_struct() {
        // 顶层字段全集与详设 §8.2 表 + resolution 补全一致；
        // schema 文件由 infra 维护，集成时按本清单比对。
        assert_eq!(OUTPUT_FIELDS.len(), 15);
        for f in ["resolution", "related_hint", "uncertainties"] {
            assert!(OUTPUT_FIELDS.contains(&f), "缺少字段 {f}");
        }
    }
}
