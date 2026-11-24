//! case/v1 夹具 → [`ClassifyInput`] 的桥接（回放器与测试共用）。
//!
//! 判读批次的划分以各 case `expected.json` 的 `agent_runs[].records` 为准
//! （见 `fixtures/mock/manifest.json` 的 `batching_rule`）；本模块只负责把
//! 指定的一批记录连同 case 级上下文组装成规范输入：
//!
//! - `participants` = 批内发送者 ∪ 批内被 @ 者 ∪ 私聊对端，去掉本人，
//!   按 `user_id` 升序；`display_name` 取自 `persons`，缺省回落为 `user_id`。
//! - 记录索引覆盖全部 `stages` 与 `prelude`；`external_id` 在 case 内必须唯一
//!   （与库表 `UNIQUE(source_id, external_id)` 对齐）。
//! - `reply_to` 取被引用消息的 `external_id`（原始对象不带进规范输入）。

use crate::classify_input::{
    ClassifyInput, ConversationRef, InputRecord, PersonRef, SourceRef,
};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// case 夹具结构不符合预期。
#[derive(Debug, thiserror::Error)]
#[error("case 夹具结构不符合预期 {path}：{reason}")]
pub struct FixtureCaseError {
    pub path: String,
    pub reason: String,
}

fn err(path: impl Into<String>, reason: impl Into<String>) -> FixtureCaseError {
    FixtureCaseError {
        path: path.into(),
        reason: reason.into(),
    }
}

type Result<T, E = FixtureCaseError> = std::result::Result<T, E>;

/// 在 `fixtures/cases` 下按 `case_id` 前缀定位唯一目录（如 `A01_oa_feedback`）。
pub fn find_case_dir(cases_root: &Path, case_id: &str) -> Result<PathBuf> {
    let prefix = format!("{case_id}_");
    let mut matches: Vec<PathBuf> = std::fs::read_dir(cases_root)
        .map_err(|source| {
            err(
                cases_root.display().to_string(),
                format!("读取目录失败：{source}"),
            )
        })?
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_str()?;
            name.starts_with(&prefix).then_some(path)
        })
        .collect();
    matches.sort();
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(err(
            cases_root.display().to_string(),
            format!("找不到 case 目录：{prefix}*"),
        )),
        n => Err(err(
            cases_root.display().to_string(),
            format!("case 目录不唯一：{prefix}* 匹配到 {n} 个"),
        )),
    }
}

/// 读取 case 的 `input.json`。
pub fn load_case_input(case_dir: &Path) -> Result<Value> {
    let file = case_dir.join("input.json");
    let raw = std::fs::read_to_string(&file).map_err(|source| {
        err(
            file.display().to_string(),
            format!("读取失败：{source}"),
        )
    })?;
    serde_json::from_str(&raw)
        .map_err(|source| err(file.display().to_string(), format!("JSON 解析失败：{source}")))
}

/// 按记录 ID 列表构造该 case 一次判读的规范输入。
///
/// `record_ids` 顺序即批内顺序（对应 `agent_runs[].records`）；必须非空，
/// 且各 ID 必须能在 case 的任一 stage / prelude 中找到。
pub fn build_classify_input(case: &Value, record_ids: &[&str]) -> Result<ClassifyInput> {
    let case_id = str_field(case, "case_id", "case")?;
    let base = format!("case/{case_id}");

    let me_obj = field(case, "me", &base)?;
    let me = PersonRef {
        user_id: str_field(me_obj, "user_id", &format!("{base}/me"))?.to_string(),
        display_name: str_field(me_obj, "display_name", &format!("{base}/me"))?.to_string(),
    };

    let persons = field(case, "persons", &base)?
        .as_object()
        .ok_or_else(|| err(format!("{base}/persons"), "必须是对象"))?;
    let conversations = field(case, "conversations", &base)?
        .as_object()
        .ok_or_else(|| err(format!("{base}/conversations"), "必须是对象"))?;
    let sources = field(case, "sources", &base)?
        .as_array()
        .ok_or_else(|| err(format!("{base}/sources"), "必须是数组"))?;

    let record_map = index_records(case, &base)?;

    if record_ids.is_empty() {
        return Err(err(&base, "判读批次 record_ids 不能为空"));
    }

    let mut inputs: Vec<InputRecord> = Vec::with_capacity(record_ids.len());
    let mut first_conversation_id: Option<&str> = None;
    let mut first_source_id: Option<&str> = None;
    let mut participant_ids: BTreeSet<&str> = BTreeSet::new();

    for record_id in record_ids {
        let rec_path = format!("{base}/record/{record_id}");
        let rec = record_map
            .get(*record_id)
            .ok_or_else(|| err(&rec_path, "case 中不存在该记录"))?;

        let sender_id = str_field(rec, "sender_id", &rec_path)?;
        participant_ids.insert(sender_id);
        let mentions = field(rec, "mentions", &rec_path)?
            .as_array()
            .ok_or_else(|| err(format!("{rec_path}/mentions"), "必须是数组"))?;
        let mut mention_ids = Vec::with_capacity(mentions.len());
        for (i, m) in mentions.iter().enumerate() {
            let id = m.as_str().ok_or_else(|| {
                err(format!("{rec_path}/mentions/{i}"), "必须是字符串")
            })?;
            mention_ids.push(id);
            participant_ids.insert(id);
        }

        let reply_to = match rec.get("reply_to") {
            None | Some(Value::Null) => None,
            Some(obj) => Some(str_field(obj, "external_id", &format!("{rec_path}/reply_to"))?),
        };
        if first_conversation_id.is_none() {
            first_conversation_id = Some(str_field(rec, "conversation_id", &rec_path)?);
        }
        if first_source_id.is_none() {
            first_source_id = Some(str_field(rec, "source_id", &rec_path)?);
        }

        inputs.push(InputRecord {
            id: (*record_id).to_string(),
            direction: str_field(rec, "direction", &rec_path)?.to_string(),
            sender_id: sender_id.to_string(),
            sender_name: str_field(rec, "sender_name", &rec_path)?.to_string(),
            mentions: mention_ids.into_iter().map(str::to_string).collect(),
            reply_to: reply_to.map(str::to_string),
            occurred_at: str_field(rec, "occurred_at", &rec_path)?.to_string(),
            body: str_field(rec, "body", &rec_path)?.to_string(),
        });
    }

    let conversation_id = first_conversation_id
        .expect("record_ids 非空，至少处理了一条记录");
    let conv_path = format!("{base}/conversations/{conversation_id}");
    let conv = conversations
        .get(conversation_id)
        .ok_or_else(|| err(&conv_path, "case 中不存在该会话"))?;
    let conversation_type = str_field(conv, "type", &conv_path)?;
    if conversation_type == "dm" {
        if let Some(peer) = opt_str_field(conv, "peer", &conv_path)? {
            participant_ids.insert(peer);
        }
    }
    participant_ids.remove(me.user_id.as_str());

    let participants: Vec<PersonRef> = participant_ids
        .into_iter()
        .map(|uid| PersonRef {
            display_name: persons
                .get(uid)
                .and_then(|p| p.get("display_name"))
                .and_then(Value::as_str)
                .unwrap_or(uid)
                .to_string(),
            user_id: uid.to_string(),
        })
        .collect();

    let source_id = first_source_id.expect("record_ids 非空，至少处理了一条记录");
    let source_obj = sources
        .iter()
        .find(|s| s.get("id").and_then(Value::as_str) == Some(source_id))
        .ok_or_else(|| err(format!("{base}/sources/{source_id}"), "case 中不存在该来源"))?;

    let source = SourceRef {
        id: source_id.to_string(),
        kind: str_field(source_obj, "kind", &format!("{base}/sources/{source_id}"))?.to_string(),
    };
    let conversation = ConversationRef {
        id: conversation_id.to_string(),
        conversation_type: conversation_type.to_string(),
        display_name: str_field(conv, "display_name", &conv_path)?.to_string(),
    };

    Ok(ClassifyInput::new(
        source,
        conversation,
        me,
        participants,
        inputs,
    ))
}

/// 全量记录索引：`prelude.records` + 各 `stages[].records`；external_id 重复即报错。
fn index_records<'a>(case: &'a Value, base: &str) -> Result<HashMap<&'a str, &'a Value>> {
    let mut map: HashMap<&str, &Value> = HashMap::new();
    let mut ingest = |records: &'a [Value], scope: String| -> Result<()> {
        for rec in records {
            let external_id = str_field(rec, "external_id", &scope)?;
            if map.insert(external_id, rec).is_some() {
                return Err(err(
                    format!("{scope}/{external_id}"),
                    "external_id 在 case 内重复（应与 UNIQUE(source_id, external_id) 对齐）",
                ));
            }
        }
        Ok(())
    };
    if let Some(prelude) = case.get("prelude") {
        let scope = format!("{base}/prelude");
        if let Some(records) = prelude.get("records") {
            let records = records
                .as_array()
                .ok_or_else(|| err(format!("{scope}/records"), "必须是数组"))?;
            ingest(records, scope)?;
        }
    }
    let stages = field(case, "stages", base)?
        .as_array()
        .ok_or_else(|| err(format!("{base}/stages"), "必须是数组"))?;
    for stage in stages {
        let index = stage.get("index").and_then(Value::as_u64).unwrap_or_default();
        let scope = format!("{base}/stages/{index}");
        if let Some(records) = stage.get("records") {
            let records = records
                .as_array()
                .ok_or_else(|| err(format!("{scope}/records"), "必须是数组"))?;
            ingest(records, scope)?;
        }
    }
    Ok(map)
}

fn field<'a>(obj: &'a Value, key: &str, path: &str) -> Result<&'a Value> {
    obj.get(key)
        .ok_or_else(|| err(path, format!("缺少字段：{key}")))
}

fn str_field<'a>(obj: &'a Value, key: &str, path: &str) -> Result<&'a str> {
    obj.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| err(path, format!("字段 {key} 必须是字符串")))
}

fn opt_str_field<'a>(obj: &'a Value, key: &str, path: &str) -> Result<Option<&'a str>> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(err(path, format!("字段 {key} 必须是字符串或 null"))),
    }
}
