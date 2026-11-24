//! 诊断摘要（详细设计 §10.1 `diagnostics` / `diagnostics_export`、B6 诊断页）。
//!
//! [`Diagnostics`] 的序列化形状即 IPC 契约（ui/IPC.md §4）：`app` / `db` / `backups` /
//! `sources` / `judge` 五个区段字段名与契约逐一对应（`sources` 与 `sync_status`
//! 命令同为 `SyncStatus`），desktop 可直接透传；store 自有的补充指标（连接参数、
//! 页统计、各表行数与分布、采集错误）收在附加键 `storage`，供诊断包导出与排障。
//!
//! **任何字段都不含正文**：只做聚合与状态读取，不查询 `body` / `body_masked` /
//! `raw_output` / `summary` 列；`lastErrorMsg` 是同步引擎写入的简短错误说明，
//! 同样截断到 200 字。全部 serde camelCase。

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{params, Connection};
use serde::Serialize;

use crate::backup::{backup_list, db_sidecar_path, AppMeta};
use crate::{schema_version, supported_version};

/// 一天的毫秒数。
const MS_PER_DAY: i64 = 86_400_000;
/// 文本字段截断长度（与日志 `msg` 的 200 字约定一致）。
const MAX_TEXT: usize = 200;

/// 需要统计行数的表。
const TABLES: &[&str] = &[
    "source",
    "source_record",
    "work_thread",
    "work_item",
    "work_event",
    "agent_run",
    "link_evidence",
    "field_override",
    "sync_checkpoint",
    "sync_run",
    "egress_log",
    "feedback_snapshot",
];

/// IPC `app`：版本与 schema。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub version: String,
    pub platform: String,
    pub schema_version: u32,
    pub supported_schema_version: u32,
}

/// IPC `db.counts`：契约点名的四张表。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyCounts {
    pub source_record: i64,
    pub work_item: i64,
    pub work_event: i64,
    pub feedback_snapshot: i64,
}

/// IPC `db`：完整性与规模。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbHealth {
    pub path: String,
    /// `"ok"` 或 `"failed"`。
    pub integrity: String,
    /// 完整性不通过或无法执行时的说明（截断；integrity_check 输出不含正文）。
    pub integrity_detail: Option<String>,
    /// 库文件大小；读取失败为 `None`。
    pub size_bytes: Option<u64>,
    pub counts: KeyCounts,
}

/// IPC `backups.last`。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LastBackup {
    /// 备份目录（不透明，与 `backup_list[].path` 同义）。
    pub path: String,
    pub created_at: i64,
    pub size_bytes: u64,
}

/// IPC `backups`。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupSummary {
    pub count: usize,
    pub last: Option<LastBackup>,
}

/// IPC `SyncStatus`（ui/IPC.md §1，与 `sync_status` 命令同形）：
/// 只含有检查点记录的数据源——没有记录代表「尚未同步」，不是「离线」。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatus {
    pub source_id: String,
    pub display_name: String,
    pub state: String,
    pub last_attempt_at: Option<i64>,
    pub last_success_at: Option<i64>,
    pub consecutive_failures: i64,
    pub last_error_code: Option<String>,
    /// 同步引擎写入的简短错误说明，截断 200 字；不是消息正文。
    pub last_error_msg: Option<String>,
    pub window_start: Option<i64>,
    pub window_end: Option<i64>,
    pub cursor_at: Option<i64>,
}

/// 拒绝规则及其出现次数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleCount {
    pub rule: String,
    pub count: i64,
}

/// IPC `judge`：时间窗口内的判读结果。`rejectionRules` 按次数降序、规则名升序。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JudgeStats {
    /// 统计窗口天数（`now_ms` 往前）。
    pub window_days: u32,
    pub ok: i64,
    pub rejected: i64,
    pub failed: i64,
    pub timeout: i64,
    pub rejection_rules: Vec<RuleCount>,
}

impl JudgeStats {
    /// 窗口内已结束的判读数（ok + rejected + failed + timeout）。
    pub fn finished(&self) -> i64 {
        self.ok + self.rejected + self.failed + self.timeout
    }

    /// 判读成功率 = ok / finished；无记录时为 0。
    pub fn success_rate(&self) -> f64 {
        let finished = self.finished();
        if finished > 0 {
            self.ok as f64 / finished as f64
        } else {
            0.0
        }
    }
}

/// store 自有的补充指标（IPC 契约之外的附加键 `storage`）。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageStats {
    pub generated_at: i64,
    pub journal_mode: String,
    pub page_count: i64,
    pub freelist_pages: i64,
    pub wal_size_bytes: u64,
    /// 各表行数（表名 → 行数）。
    pub table_counts: BTreeMap<String, i64>,
    pub raw_state_counts: BTreeMap<String, i64>,
    pub judge_state_counts: BTreeMap<String, i64>,
    pub item_lifecycle_counts: BTreeMap<String, i64>,
    /// 采集失败的查询（SQL 与错误）；正常应为空。
    pub errors: Vec<String>,
}

/// 诊断摘要（自检 + 指标）；序列化即 IPC `Diagnostics`。
#[derive(Debug, Clone, Serialize)]
pub struct Diagnostics {
    pub app: AppInfo,
    pub db: DbHealth,
    pub backups: BackupSummary,
    pub sources: Vec<SyncStatus>,
    pub judge: JudgeStats,
    pub storage: StorageStats,
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

fn q_count(conn: &Connection, sql: &str, p: &[&dyn rusqlite::ToSql], errors: &mut Vec<String>) -> i64 {
    conn.query_row(sql, p, |r| r.get(0)).unwrap_or_else(|e| {
        errors.push(truncate(&format!("{sql}: {e}"), MAX_TEXT));
        0
    })
}

fn q_dist(conn: &Connection, sql: &str, errors: &mut Vec<String>) -> BTreeMap<String, i64> {
    let rows = (|| -> rusqlite::Result<Vec<(String, i64)>> {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        rows.collect()
    })();
    match rows {
        Ok(pairs) => pairs.into_iter().collect(),
        Err(e) => {
            errors.push(truncate(&format!("{sql}: {e}"), MAX_TEXT));
            BTreeMap::new()
        }
    }
}

/// 采集诊断摘要。只做聚合查询与状态读取，不读取任何正文列；
/// 单个查询失败不致命——记入 `storage.errors`，其余指标照常返回。
///
/// `now_ms` 为业务时钟，`judge_window_days` 为判读统计窗口；
/// 两者由调用方显式传入以便结果可复现。
pub fn collect_diagnostics(
    conn: &Connection,
    db_path: &Path,
    backups_root: &Path,
    meta: &AppMeta,
    now_ms: i64,
    judge_window_days: u32,
) -> Diagnostics {
    let mut errors = Vec::new();

    let (integrity, integrity_detail) =
        match conn.query_row("PRAGMA integrity_check", params![], |r| {
            r.get::<_, String>(0)
        }) {
            Ok(first) if first == "ok" => ("ok".to_string(), None),
            Ok(first) => ("failed".to_string(), Some(truncate(&first, 300))),
            Err(e) => (
                "failed".to_string(),
                Some(truncate(&format!("无法执行完整性校验: {e}"), 300)),
            ),
        };
    let pragma = |name: &str| {
        conn.pragma_query_value(None, name, |r| r.get::<_, String>(0))
            .unwrap_or_default()
    };
    let mut table_counts = BTreeMap::new();
    for table in TABLES {
        let n = q_count(
            conn,
            &format!("SELECT count(*) FROM {table}"),
            &[],
            &mut errors,
        );
        table_counts.insert((*table).to_string(), n);
    }
    let rows_of = |table: &str| table_counts.get(table).copied().unwrap_or(0);

    // 判读：只统计窗口内的运行。
    let since = now_ms - i64::from(judge_window_days) * MS_PER_DAY;
    let status_count = |errors: &mut Vec<String>, status: &str| {
        let p: [&dyn rusqlite::ToSql; 2] = [&status, &since];
        q_count(
            conn,
            "SELECT count(*) FROM agent_run WHERE status = ?1 AND created_at >= ?2",
            &p,
            errors,
        )
    };
    let mut rejection_rules = Vec::new();
    let rule_rows = (|| -> rusqlite::Result<Vec<RuleCount>> {
        let mut stmt = conn.prepare(
            "SELECT COALESCE(rejection_rule, '(none)'), count(*)
             FROM agent_run
             WHERE status = 'rejected' AND created_at >= ?1
             GROUP BY 1
             ORDER BY count(*) DESC, 1 ASC",
        )?;
        let rows = stmt.query_map(params![since], |r| {
            Ok(RuleCount {
                rule: r.get(0)?,
                count: r.get(1)?,
            })
        })?;
        rows.collect()
    })();
    match rule_rows {
        Ok(rules) => rejection_rules = rules,
        Err(e) => errors.push(truncate(&format!("查询拒绝规则分布: {e}"), MAX_TEXT)),
    }
    let judge = JudgeStats {
        window_days: judge_window_days,
        ok: status_count(&mut errors, "ok"),
        rejected: status_count(&mut errors, "rejected"),
        failed: status_count(&mut errors, "failed"),
        timeout: status_count(&mut errors, "timeout"),
        rejection_rules,
    };

    // 各源新鲜度：与 sync_status 同形，只含有检查点记录的数据源。
    let mut sources = Vec::new();
    let source_rows = (|| -> rusqlite::Result<Vec<SyncStatus>> {
        let mut stmt = conn.prepare(
            "SELECT s.id, s.display_name, c.state, c.last_attempt_at, c.last_success_at,
                    c.consecutive_failures, c.last_error_code, c.last_error_msg,
                    c.window_start, c.window_end, c.cursor_at
             FROM sync_checkpoint c JOIN source s ON s.id = c.source_id
             ORDER BY s.created_at, s.id",
        )?;
        let rows = stmt.query_map(params![], |r| {
            Ok(SyncStatus {
                source_id: r.get(0)?,
                display_name: r.get(1)?,
                state: r.get(2)?,
                last_attempt_at: r.get(3)?,
                last_success_at: r.get(4)?,
                consecutive_failures: r.get(5)?,
                last_error_code: r.get(6)?,
                last_error_msg: r
                    .get::<_, Option<String>>(7)?
                    .map(|msg| truncate(&msg, MAX_TEXT)),
                window_start: r.get(8)?,
                window_end: r.get(9)?,
                cursor_at: r.get(10)?,
            })
        })?;
        rows.collect()
    })();
    match source_rows {
        Ok(rows) => sources = rows,
        Err(e) => errors.push(truncate(&format!("查询数据源新鲜度: {e}"), MAX_TEXT)),
    }

    let all_backups = backup_list(backups_root);
    let backups = BackupSummary {
        count: all_backups.len(),
        last: all_backups.first().map(|entry| LastBackup {
            path: entry.dir.display().to_string(),
            created_at: entry.manifest.created_at,
            size_bytes: entry.manifest.size_bytes,
        }),
    };

    Diagnostics {
        app: AppInfo {
            version: meta.app_version.clone(),
            platform: meta.platform.clone(),
            schema_version: schema_version(conn).unwrap_or(0),
            supported_schema_version: supported_version(),
        },
        db: DbHealth {
            path: db_path.display().to_string(),
            integrity,
            integrity_detail,
            size_bytes: std::fs::metadata(db_path).ok().map(|m| m.len()),
            counts: KeyCounts {
                source_record: rows_of("source_record"),
                work_item: rows_of("work_item"),
                work_event: rows_of("work_event"),
                feedback_snapshot: rows_of("feedback_snapshot"),
            },
        },
        backups,
        sources,
        judge,
        storage: StorageStats {
            generated_at: now_ms,
            journal_mode: pragma("journal_mode"),
            page_count: pragma("page_count").parse().unwrap_or(0),
            freelist_pages: pragma("freelist_count").parse().unwrap_or(0),
            wal_size_bytes: std::fs::metadata(db_sidecar_path(db_path, "-wal"))
                .map(|m| m.len())
                .unwrap_or(0),
            table_counts,
            raw_state_counts: q_dist(
                conn,
                "SELECT raw_state, count(*) FROM source_record GROUP BY raw_state",
                &mut errors,
            ),
            judge_state_counts: q_dist(
                conn,
                "SELECT judge_state, count(*) FROM source_record GROUP BY judge_state",
                &mut errors,
            ),
            item_lifecycle_counts: q_dist(
                conn,
                "SELECT lifecycle, count(*) FROM work_item GROUP BY lifecycle",
                &mut errors,
            ),
            errors,
        },
    }
}
