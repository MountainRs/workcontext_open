//! 保留期清理（详细设计 §14.3；PRD §6.3「原文不可访问」的落地）。
//!
//! `retention_at` 到期且仍为 `available` 的 `source_record`：分批（每批 500，
//! 单批一个事务）置空 `body` / `body_masked` 并置 `raw_state = 'redacted'`；
//! `source_record_fts` 的 UPDATE 触发器同步把正文从索引里剔除。
//! 清理后保留 external_id、summary、content_hash、occurred_at、发送者与
//! link_evidence。全部批次完成后写一条 `work_event(actor=system,
//! event=retention_swept)` 汇总事件（note 含批次数）。

use rusqlite::{params, Connection};
use serde::Serialize;

use crate::backup::sha256_hex;

/// 每批最多清理的记录数（§14.3）。
const BATCH: i64 = 500;

const SWEEP_SQL: &str = "\
UPDATE source_record
SET body = NULL, body_masked = NULL, raw_state = 'redacted'
WHERE id IN (
  SELECT id FROM source_record
  WHERE retention_at IS NOT NULL AND retention_at <= ?1 AND raw_state = 'available'
  LIMIT 500
)";

/// 一次清理的报告。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetentionReport {
    /// 置空正文的记录数。
    pub swept: u64,
    /// 执行的事务（批）数。
    pub batches: u32,
}

/// 执行一轮保留清理（§14.3）。`now_ms` 为业务时钟，由调用方显式传入
/// （启动任务传当前时间；测试传固定时间）。重复执行幂等：已 `redacted`
/// 的记录不再匹配。清 0 条时不写事件。
pub fn retention_sweep(conn: &Connection, now_ms: i64) -> Result<RetentionReport, rusqlite::Error> {
    let mut swept: u64 = 0;
    let mut batches: u32 = 0;
    loop {
        // 每批各自一个自动提交事务，批间让路给其他写入。
        let changed = conn.execute(SWEEP_SQL, params![now_ms])? as u64;
        swept += changed;
        batches += 1;
        if changed < BATCH as u64 {
            break;
        }
    }
    if swept > 0 {
        let id = sha256_hex(format!("work_event|retention_swept|{now_ms}|{swept}").as_bytes());
        let note = serde_json::json!({
            "swept": swept,
            "batches": batches,
            "retentionAtOrBefore": now_ms,
        })
        .to_string();
        conn.execute(
            "INSERT INTO work_event (id, target_type, target_id, event_type, actor, at, note)
             VALUES (?1, 'source', '', 'retention_swept', 'system', ?2, ?3)",
            params![id, now_ms, note],
        )?;
    }
    Ok(RetentionReport { swept, batches })
}
