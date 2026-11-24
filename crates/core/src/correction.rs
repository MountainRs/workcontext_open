//! 人工纠错语义（详细设计 §9）：只锁用户真正表态的字段，不是固定全锁。
//!
//! A11：纠错写入 `field_override` 后，同一 `input_sha256` 的重复判读不得静默覆盖
//! （`validate` 的 V07 消费这里推导出的锁）。

use crate::state::{Attention, ItemField, Lifecycle};

/// 一次纠错（`item_correct` 用例的入参语义）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Correction {
    /// 不是我的事 → 锁 `belongs_to_me`（虚拟字段：阻止 Agent 重新认领）。
    pub belongs_to_me: Option<bool>,
    /// 已结束 / 忽略 → 锁 `lifecycle`。
    pub lifecycle: Option<Lifecycle>,
    /// 关联错了（`Some(None)` = 解除关联）→ 锁 `thread_id`。
    pub thread_id: Option<Option<String>>,
    pub attention: Option<Attention>,
    /// 必填：枚举 + 可自由补充。
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CorrectionError {
    #[error("纠错必须带原因（reason 不能为空）")]
    EmptyReason,
    #[error("纠错至少要改动一个字段")]
    NoChange,
}

/// 校验纠错语义：原因必填、至少改动一个字段。
pub fn validate_correction(c: &Correction) -> Result<(), CorrectionError> {
    if c.reason.trim().is_empty() {
        return Err(CorrectionError::EmptyReason);
    }
    if c.belongs_to_me.is_none()
        && c.lifecycle.is_none()
        && c.thread_id.is_none()
        && c.attention.is_none()
    {
        return Err(CorrectionError::NoChange);
    }
    Ok(())
}

/// 由实际改动的字段推导锁集合（§9：只锁真正表态的维度，避免改一次归属就冻死整个事项）。
pub fn correction_locks(c: &Correction) -> Vec<ItemField> {
    let mut locks = Vec::new();
    if c.belongs_to_me.is_some() {
        locks.push(ItemField::BelongsToMe);
    }
    if c.lifecycle.is_some() {
        locks.push(ItemField::Lifecycle);
    }
    if c.attention.is_some() {
        locks.push(ItemField::Attention);
    }
    if c.thread_id.is_some() {
        locks.push(ItemField::ThreadId);
    }
    locks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a11_patch_locks_exactly_the_asserted_fields() {
        // A11：只锁 belongs_to_me 与 lifecycle；不得把 title/next_action 等一起冻死。
        let c = Correction {
            belongs_to_me: Some(false),
            lifecycle: Some(Lifecycle::Ignored),
            thread_id: None,
            attention: None,
            reason: "not_mine：已口头改由李四接手".to_string(),
        };
        assert_eq!(validate_correction(&c), Ok(()));
        assert_eq!(
            correction_locks(&c),
            vec![ItemField::BelongsToMe, ItemField::Lifecycle]
        );
    }

    #[test]
    fn partial_patch_locks_only_that_field() {
        let c = Correction {
            belongs_to_me: None,
            lifecycle: None,
            thread_id: Some(None),
            attention: Some(Attention::WaitingOther),
            reason: "关联错了，先解除".to_string(),
        };
        assert_eq!(
            correction_locks(&c),
            vec![ItemField::Attention, ItemField::ThreadId]
        );
    }

    #[test]
    fn rejects_empty_reason_or_no_change() {
        let empty_reason = Correction {
            belongs_to_me: Some(true),
            ..Correction::default()
        };
        // Default 的 reason 为空串。
        assert_eq!(validate_correction(&empty_reason), Err(CorrectionError::EmptyReason));
        let no_change = Correction {
            belongs_to_me: None,
            lifecycle: None,
            thread_id: None,
            attention: None,
            reason: "not_mine".to_string(),
        };
        assert_eq!(validate_correction(&no_change), Err(CorrectionError::NoChange));
    }
}
