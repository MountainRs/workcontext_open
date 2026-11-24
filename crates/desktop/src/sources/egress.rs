//! 出域审计落库（`egress_log`，详细设计 §7.3）。
//!
//! 闸门本体在 `workcontext_infra::egress`（`OmpCliRuntime::run_judge` 内部执行，
//! 单层）；本模块只负责把闸门决定持久化：
//!
//! - `decision='blocked'`：策略拦截（默认 `policy=none` 时一切判读如此）——
//!   「N 条因出域策略未判读」的可审计来源；
//! - `decision='allowed'`：cli 模式下过闸、正文确已发往已批准模型的批次
//!   （mock 模式不落库——内容从未离开本机）。
//!
//! `refs_json` 存该批 `source_record.id` 列表；`run_id` 关联 `agent_run`。

use workcontext_store::rusqlite::{params, Connection};

/// 记录一次「拦截」：一批记录因出域策略未送出。
pub fn log_blocked(
    conn: &Connection,
    now_ms: i64,
    policy: &str,
    approved_model: Option<&str>,
    refs: &[String],
    run_id: Option<&str>,
) -> Result<(), workcontext_store::rusqlite::Error> {
    insert(conn, now_ms, policy, "blocked", approved_model, refs, run_id)
}

/// 记录一次「放行」：正文已发往已批准模型（仅真实 CLI 路径调用）。
pub fn log_allowed(
    conn: &Connection,
    now_ms: i64,
    policy: &str,
    approved_model: Option<&str>,
    refs: &[String],
    run_id: &str,
) -> Result<(), workcontext_store::rusqlite::Error> {
    insert(conn, now_ms, policy, "allowed", approved_model, refs, Some(run_id))
}

fn insert(
    conn: &Connection,
    now_ms: i64,
    policy: &str,
    decision: &str,
    approved_model: Option<&str>,
    refs: &[String],
    run_id: Option<&str>,
) -> Result<(), workcontext_store::rusqlite::Error> {
    let id: String = conn.query_row(
        "SELECT lower(hex(randomblob(16)))",
        [],
        |row| row.get(0),
    )?;
    let refs_json = serde_json::to_string(refs).expect("字符串数组序列化不会失败");
    conn.execute(
        "INSERT INTO egress_log(id, at, policy, decision, target, refs_json, run_id)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, now_ms, policy, decision, approved_model, refs_json, run_id],
    )?;
    Ok(())
}
