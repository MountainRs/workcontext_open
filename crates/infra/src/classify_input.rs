//! 判读输入的规范形（canonical form）与 `input_sha256` 生成规则。
//!
//! 这是判读幂等键的参考实现（详细设计 §7.2「相同输入 → 相同字符串 → 相同
//! `input_sha256` → 幂等」的落地），规则全文也可复核地记录在
//! `fixtures/mock/manifest.json` 的 `hash_rule` / `identity_rule` 字段中：
//!
//! 1. 一次判读的输入 = [`ClassifyInput`]：批内记录（按给定顺序）+ 会话/身份上下文。
//!    **不包含**当前时钟、运行环境与邻近记录窗口——同一批记录重跑必得同一
//!    `input_sha256`，这是 A11 纠错重放不被重复计费/重复建项、以及
//!    `agent_run` 幂等唯一索引的根基。
//! 2. 规范 JSON（[`canonical_json`]）：对象键按 Unicode 码点升序排序；无任何
//!    空白分隔；数组保持原序；非 ASCII 字符不转义（UTF-8 原样输出）；
//!    文档中只允许 string / integer / boolean / null / array / object（无浮点）。
//! 3. [`input_sha256`] = SHA-256(规范 JSON 的 UTF-8 字节) 的小写十六进制（64 字符）。
//! 4. `prompt_version` 参与哈希：`classify/v1` 契约升版 → 判读身份变化 → 重新判读；
//!    同一 `prompt_version` 内的系统提示词措辞调整不改变身份。
//!
//! 回放夹具按本规则生成：`fixtures/mock/classify/<input_sha256>.json`，
//! 人工复核用的输入复原件在 `fixtures/mock/inputs/`，清单在
//! `fixtures/mock/manifest.json`；集成测试会用 `fixtures/cases/*.json`
//! 重算哈希校验三者一致。

use serde_json::Value;

use serde::{Deserialize, Serialize};
use std::collections::{btree_map::Entry, BTreeMap};
use sha2::{Digest, Sha256};

/// 规范输入文档的契约版本（参与哈希）。
pub const INPUT_SCHEMA_VERSION: &str = "classify-input/v1";
/// 判读输出的契约版本（参与哈希，与 [`crate::contract::OUTPUT_SCHEMA_VERSION`] 一致）。
pub const PROMPT_VERSION: &str = "classify/v1";
/// 运行种类（对应 `agent_run.kind`），当前只有 classify。
pub const RUN_KIND: &str = "classify";

/// 一次判读的规范输入。
///
/// 字段集即判读身份（见模块文档）；`participants` 由 [`ClassifyInput::new`]
/// 强制按 `user_id` 升序去重，`records` 保持调用方给定的批内顺序。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifyInput {
    pub schema_version: String,
    pub kind: String,
    pub prompt_version: String,
    pub source: SourceRef,
    pub conversation: ConversationRef,
    /// 本人身份：只含 `user_id` / `display_name`（工号等 HR 属性不参与判读身份）。
    pub me: PersonRef,
    /// 批内出现的其他参与者（发送者、被 @ 者、私聊对端），按 `user_id` 升序。
    pub participants: Vec<PersonRef>,
    /// 批内记录，按判读批次给定顺序（通常为时间升序）。由调用方保证非空。
    pub records: Vec<InputRecord>,
    /// Archive-only material: never a source ID, ownership anchor or time basis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_context: Option<Vec<ArchiveExcerpt>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveExcerpt {
    pub kind: ArchiveExcerptKind,
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveExcerptKind {
    Reply,
    Earlier,
    Attachment,
}

impl ClassifyInput {
    /// 构造规范输入；`participants` 按 `user_id` 升序去重，确定性由构造保证：
    /// 同一 `user_id` 出现多条时取 `display_name` **字典序最小**者——该规则对
    /// 输入顺序不敏感（「后写覆盖」会随顺序翻转 display_name，破坏幂等）。
    pub fn new(
        source: SourceRef,
        conversation: ConversationRef,
        me: PersonRef,
        participants: Vec<PersonRef>,
        records: Vec<InputRecord>,
    ) -> Self {
        let mut by_user_id: BTreeMap<String, String> = BTreeMap::new();
        for person in participants {
            match by_user_id.entry(person.user_id) {
                Entry::Occupied(mut occupied) => {
                    if person.display_name < *occupied.get() {
                        occupied.insert(person.display_name);
                    }
                }
                Entry::Vacant(vacant) => {
                    vacant.insert(person.display_name);
                }
            }
        }
        let participants = by_user_id
            .into_iter()
            .map(|(user_id, display_name)| PersonRef {
                user_id,
                display_name,
            })
            .collect();
        Self {
            schema_version: INPUT_SCHEMA_VERSION.to_string(),
            kind: RUN_KIND.to_string(),
            prompt_version: PROMPT_VERSION.to_string(),
            source,
            conversation,
            me,
            participants,
            records,
            archive_context: None,
        }
    }
}

/// 数据源引用（记录的来源，如 `dws_im`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    pub id: String,
    pub kind: String,
}

/// 会话引用。`conversation_type` 序列化为 `type`（`dm` / `group`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationRef {
    pub id: String,
    #[serde(rename = "type")]
    pub conversation_type: String,
    pub display_name: String,
}

/// 人员引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonRef {
    pub user_id: String,
    pub display_name: String,
}

/// 批内一条记录。`id` 为 `external_id`：判读输出的 `source_ids` / `evidence`
/// 直接引用它（真实 runtime 中模型看到的 `s1..s5` 短编号在回填时翻译回 external_id）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputRecord {
    pub id: String,
    /// `in` / `out` / `na`。
    pub direction: String,
    pub sender_id: String,
    pub sender_name: String,
    /// 被 @ 的 user_id 列表（原序）。
    pub mentions: Vec<String>,
    /// 被引用消息的 external_id。
    #[serde(default)]
    pub reply_to: Option<String>,
    /// RFC3339 字面量（含时区偏移），与库内记录保持一致。
    pub occurred_at: String,
    pub body: String,
}

/// 把任意 JSON 值写成规范字符串（对象键升序、紧凑、非 ASCII 不转义）。
///
/// 与 `serde_json::Map` 的特性开关无关：键排序在遍历时显式执行，
/// 因此 `preserve_order` 等特性不会影响输出。字符串转义委托
/// `serde_json::to_string`（`"`、`\`、`\b\f\n\r\t` 及 <0x20 控制字符）。
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => out.push_str(&quote(s)),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort();
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&quote(key));
                out.push(':');
                write_canonical(&map[key], out);
            }
            out.push('}');
        }
    }
}

fn quote(s: &str) -> String {
    serde_json::to_string(s).expect("JSON 字符串序列化不会失败")
}

/// 计算判读输入的幂等键：SHA-256(规范 JSON) 的小写十六进制。
pub fn input_sha256(input: &ClassifyInput) -> String {
    let mut value = serde_json::to_value(input).expect("ClassifyInput 序列化不会失败");
    // 反序列化与直接构造均可绕过 new；幂等键不能依赖调用方的排序。
    if !input.participants.windows(2).all(|p| p[0].user_id < p[1].user_id) {
        let people = value["participants"].as_array_mut().expect("参与者序列化为数组");
        people.sort_unstable_by(|a, b| {
            (a["user_id"].as_str(), a["display_name"].as_str())
                .cmp(&(b["user_id"].as_str(), b["display_name"].as_str()))
        });
        people.dedup_by(|a, b| a["user_id"] == b["user_id"]);
    }
    let canonical = canonical_json(&value);
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 已知答案测试：规范字符串与 SHA-256 由生成夹具的同一规则独立计算而来
    /// （Python `json.dumps(ensure_ascii=False, sort_keys=True, separators=(",", ":"))`
    /// 与本实现逐字节一致），锁定跨实现一致性。
    #[test]
    fn canonical_json_and_sha256_known_answer() {
        let value = serde_json::json!({
            "b": 1,
            "a": "中文 \"引号\" 与\n换行\t制表",
            "c": [true, null, "x"]
        });
        let canonical = canonical_json(&value);
        assert_eq!(
            canonical,
            "{\"a\":\"中文 \\\"引号\\\" 与\\n换行\\t制表\",\"b\":1,\"c\":[true,null,\"x\"]}"
        );

        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        let digest = hasher.finalize();
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "abe7d0ef1f41698deca1f8c3fff5f5380cf27dc836cc901d7181c39c62089ed5"
        );
    }

    #[test]
    fn canonical_json_sorts_keys_at_every_depth() {
        let value = serde_json::json!({"z": {"b": 1, "a": 2}, "a": [ {"y": 1, "x": 2} ]});
        assert_eq!(
            canonical_json(&value),
            "{\"a\":[{\"x\":2,\"y\":1}],\"z\":{\"a\":2,\"b\":1}}"
        );
    }

    fn sample_input() -> ClassifyInput {
        ClassifyInput::new(
            SourceRef {
                id: "src-1".into(),
                kind: "dws_im".into(),
            },
            ConversationRef {
                id: "conv-1".into(),
                conversation_type: "dm".into(),
                display_name: "私聊".into(),
            },
            PersonRef {
                user_id: "U_ME".into(),
                display_name: "我".into(),
            },
            vec![
                PersonRef {
                    user_id: "U_B".into(),
                    display_name: "乙".into(),
                },
                PersonRef {
                    user_id: "U_A".into(),
                    display_name: "甲".into(),
                },
                PersonRef {
                    user_id: "U_A".into(),
                    display_name: "甲(重复)".into(),
                },
            ],
            vec![InputRecord {
                id: "R1".into(),
                direction: "in".into(),
                sender_id: "U_A".into(),
                sender_name: "甲".into(),
                mentions: vec![],
                reply_to: None,
                occurred_at: "2026-11-20T09:12:00+08:00".into(),
                body: "正文".into(),
            }],
        )
    }

    #[test]
    fn participants_are_sorted_and_deduped() {
        let input = sample_input();
        let ids: Vec<&str> = input.participants.iter().map(|p| p.user_id.as_str()).collect();
        assert_eq!(ids, ["U_A", "U_B"]);
        // 同一 user_id 冲突时取 display_name 字典序最小者（与输入顺序无关）
        let names: Vec<&str> = input
            .participants
            .iter()
            .map(|p| p.display_name.as_str())
            .collect();
        assert_eq!(names, ["甲", "乙"]);
    }

    #[test]
    fn input_sha256_is_stable_and_order_insensitive_for_participants() {
        let a = sample_input();
        // 再次构造（participants 顺序不同、含重复）→ 同一哈希
        let mut b = sample_input();
        b.participants.reverse();
        b.participants.push(PersonRef {
            user_id: "U_A".into(),
            display_name: "甲".into(),
        });
        assert_eq!(input_sha256(&a), input_sha256(&b));
        // 记录正文变化 → 哈希变化
        let mut c = a.clone();
        c.records[0].body.push('！');
        assert_ne!(input_sha256(&a), input_sha256(&c));
        // 记录批次顺序参与身份
        let mut d = a.clone();
        d.records.push(InputRecord {
            id: "R2".into(),
            direction: "out".into(),
            sender_id: "U_ME".into(),
            sender_name: "我".into(),
            mentions: vec![],
            reply_to: None,
            occurred_at: "2026-11-20T09:15:00+08:00".into(),
            body: "答复".into(),
        });
        assert_ne!(input_sha256(&a), input_sha256(&d));
    }

    #[test]
    fn roundtrip_serde_matches_canonical_shape() {
        let input = sample_input();
        let value = serde_json::to_value(&input).expect("序列化失败");
        let parsed: ClassifyInput = serde_json::from_value(value).expect("反序列化失败");
        assert_eq!(input, parsed);
        assert_eq!(parsed.schema_version, INPUT_SCHEMA_VERSION);
        assert_eq!(parsed.prompt_version, PROMPT_VERSION);
        assert_eq!(parsed.kind, RUN_KIND);
    }
}

/// 供 crate 内其它模块的 `#[cfg(test)]` 复用的样例输入（集成测试走
/// [`crate::fixture_case`]，不使用本模块）。
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

    pub fn sample_input() -> ClassifyInput {
        ClassifyInput::new(
            SourceRef {
                id: "src-1".into(),
                kind: "dws_im".into(),
            },
            ConversationRef {
                id: "conv-1".into(),
                conversation_type: "dm".into(),
                display_name: "私聊".into(),
            },
            PersonRef {
                user_id: "U_ME".into(),
                display_name: "我".into(),
            },
            vec![PersonRef {
                user_id: "U_PEER".into(),
                display_name: "对端".into(),
            }],
            vec![InputRecord {
                id: "R1".into(),
                direction: "in".into(),
                sender_id: "U_PEER".into(),
                sender_name: "对端".into(),
                mentions: vec![],
                reply_to: None,
                occurred_at: "2026-11-20T09:12:00+08:00".into(),
                body: "请跟进这件事".into(),
            }],
        )
    }
}
