//! §6.3 预筛：只靠确定性元数据决定记录是否进入判读（无语义猜测）。
//!
//! keep 规则（夹具断言口径，`fixtures/cases/*/expected.json` 的
//! `prefilter_reason`）：
//!
//! | reason | 条件 |
//! |---|---|
//! | `dm_in` | 私聊且 `direction=in` |
//! | `dm_out` | 私聊且 `direction=out`（V03 锚点③依赖本人承诺，A01-R2） |
//! | `at_me` | 群聊且 `mentions` 含本人 |
//! | `my_reply` | 群聊本人外发且回复/点名了他人（A03-R2） |
//! | `name_mention` | 群聊来件正文含本人姓名但未 @本人（A06-R3，仍 keep 交 V03 兜底） |
//!
//! 其余（含群聊无信号消息）skip(`no_signal`)：仍入库、`judge_state=skipped`
//! （§6.3「看过但判断无关」可事后证明）。

use serde_json::Value;

/// 预筛裁决：keep（进入判读）或 skip（只入库），附机器可读原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefilterVerdict {
    Keep(&'static str),
    Skip(&'static str),
}

impl PrefilterVerdict {
    /// 是否进入判读。
    pub fn is_keep(&self) -> bool {
        matches!(self, PrefilterVerdict::Keep(_))
    }

    /// 原因码（`source_record.prefilter_reason`）。
    pub fn reason(&self) -> &'static str {
        match self {
            PrefilterVerdict::Keep(r) | PrefilterVerdict::Skip(r) => r,
        }
    }

    pub fn action(&self) -> &'static str {
        match self {
            PrefilterVerdict::Keep(_) => "keep",
            PrefilterVerdict::Skip(_) => "skip",
        }
    }
}

/// 对一条 case 记录做预筛。
///
/// `me_id` 本人 user_id；`me_name` 本人姓名（`name_mention` 匹配用）；
/// `conversation_type` 取值 `dm` / `group` / 其它。
pub fn prefilter_record(
    record: &Value,
    me_id: &str,
    me_name: &str,
    conversation_type: &str,
) -> PrefilterVerdict {
    if record.get("kind").and_then(Value::as_str) != Some("im_message") {
        return PrefilterVerdict::Skip("non_im");
    }
    let direction = record.get("direction").and_then(Value::as_str).unwrap_or("na");
    let mentions_me = record
        .get("mentions")
        .and_then(Value::as_array)
        .map(|ms| ms.iter().any(|m| m.as_str() == Some(me_id)))
        .unwrap_or(false);

    if conversation_type == "dm" {
        return match direction {
            "in" => PrefilterVerdict::Keep("dm_in"),
            "out" => PrefilterVerdict::Keep("dm_out"),
            _ => PrefilterVerdict::Skip("no_signal"),
        };
    }

    // 群聊：@本人最优先（A03-R1 / A06-R2）。
    if mentions_me {
        return PrefilterVerdict::Keep("at_me");
    }
    let has_reply = record
        .get("reply_to")
        .map(|r| !r.is_null())
        .unwrap_or(false);
    let has_mentions = record
        .get("mentions")
        .and_then(Value::as_array)
        .map(|ms| !ms.is_empty())
        .unwrap_or(false);
    if direction == "out" {
        // 本人外发且回复/点名他人 → 参与了判读可见的对话（A03-R2）。
        return if has_reply || has_mentions {
            PrefilterVerdict::Keep("my_reply")
        } else {
            PrefilterVerdict::Skip("no_signal")
        };
    }
    if direction == "in" {
        // 正文出现姓名但未 @本人：keep 交 V03 锚点兜底（A06-R3）。
        let body = record.get("body").and_then(Value::as_str).unwrap_or("");
        if !me_name.is_empty() && body.contains(me_name) {
            return PrefilterVerdict::Keep("name_mention");
        }
    }
    PrefilterVerdict::Skip("no_signal")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rec(direction: &str, mentions: &[&str], reply_to: Option<&str>, body: &str) -> Value {
        json!({
            "kind": "im_message",
            "direction": direction,
            "mentions": mentions,
            "reply_to": reply_to.map(|r| json!({ "external_id": r })),
            "body": body,
        })
    }

    #[test]
    fn dm_rules() {
        let r = rec("in", &[], None, "你跟一下");
        assert_eq!(prefilter_record(&r, "U_ME", "张三", "dm"), PrefilterVerdict::Keep("dm_in"));
        let r = rec("out", &[], None, "好的我先联系供应商");
        assert_eq!(prefilter_record(&r, "U_ME", "张三", "dm"), PrefilterVerdict::Keep("dm_out"));
    }

    #[test]
    fn group_rules() {
        // A03-R1：@本人 → at_me。
        let r = rec("in", &["U_ME"], None, "@张三 帮忙开权限");
        assert_eq!(prefilter_record(&r, "U_ME", "张三", "group"), PrefilterVerdict::Keep("at_me"));
        // A03-R2：本人回复他人 → my_reply。
        let r = rec("out", &["U_PEER"], Some("A03-R1"), "ok了，已开通");
        assert_eq!(prefilter_record(&r, "U_ME", "张三", "group"), PrefilterVerdict::Keep("my_reply"));
        // A06-R0：本人提问无信号 → skip。
        let r = rec("out", &[], None, "图纸库今天响应有点慢");
        assert_eq!(prefilter_record(&r, "U_ME", "张三", "group"), PrefilterVerdict::Skip("no_signal"));
        // A06-R1：点名他人且正文无本人姓名 → skip。
        let r = rec("in", &["U_LISI"], None, "@李四 你负责排查一下");
        assert_eq!(prefilter_record(&r, "U_ME", "张三", "group"), PrefilterVerdict::Skip("no_signal"));
        // A06-R3：正文含姓名未 @本人 → name_mention keep。
        let r = rec("in", &["U_MAGONG"], None, "@马工 你牵头；张三作为接口人配合");
        assert_eq!(prefilter_record(&r, "U_ME", "张三", "group"), PrefilterVerdict::Keep("name_mention"));
    }
}
