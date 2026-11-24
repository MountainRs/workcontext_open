//! 迭代池快照用例（M4 / A12，详细设计 §9 `snapshot_create` / `snapshot_list` /
//! `snapshot_export`、§16.2 A12、PRD §11.2、§13.2-9）。
//!
//! 真实 SQLite 落库（`migrations/0001_init.sql` 的 `feedback_snapshot` +
//! `work_event`），与 M2 的内存回放（[`crate::replay`]）无关：
//!
//! - **创建**：单事务写 `feedback_snapshot`（`redaction='masked'`、`status='open'`）
//!   与 `work_event(actor=human, event_type='created')`；只携带**引用与脱敏摘要**
//!   （`source_record.summary`），绝不保存整段聊天正文（PRD §10.2）。
//! - **列表**：按状态可选过滤，`created_at DESC`（索引 `ix_fb`）。
//! - **导出**：`snapshot/v1` JSON（机器复放，snake_case，对外长期兼容）与
//!   Markdown（人读）两种表示；导出内容一律过 [`redact_text`]（门槛 12：
//!   手机号/邮箱/token 不出现在导出）。**正文（`body_masked`）只在
//!   [`IncludeBody::Explicit`]（用户明确同意）时附带**；raw `body` 列永远不出库。
//!
//! 时钟与应用版本由调用方注入（[`CreateContext`]），本模块不自取系统时间。

use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use workcontext_store::rusqlite::{Connection, OptionalExtension, Row, params};

/// 快照载荷的 schema 版本（`feedback_snapshot.schema_version`）。
pub const SNAPSHOT_SCHEMA_VERSION: i64 = 1;

/// 导出 JSON 的对外格式标识（详细设计 §2：`snapshot/v1` JSON + Markdown 模板）。
pub const SNAPSHOT_JSON_SCHEMA: &str = "snapshot/v1";

// ---------------------------------------------------------------------------
// 枚举（与迁移 CHECK 约束一一对应）
// ---------------------------------------------------------------------------

/// 误判类型（`feedback_snapshot.kind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotKind {
    WrongOwner,
    WrongState,
    MissingItem,
    SpuriousItem,
    WrongLink,
    Ui,
    Other,
}

impl SnapshotKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SnapshotKind::WrongOwner => "wrong_owner",
            SnapshotKind::WrongState => "wrong_state",
            SnapshotKind::MissingItem => "missing_item",
            SnapshotKind::SpuriousItem => "spurious_item",
            SnapshotKind::WrongLink => "wrong_link",
            SnapshotKind::Ui => "ui",
            SnapshotKind::Other => "other",
        }
    }

    /// IPC 字符串 → 枚举；未知值不猜测，返回 `None`（UI 层转 INVALID_ARGUMENT）。
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "wrong_owner" => SnapshotKind::WrongOwner,
            "wrong_state" => SnapshotKind::WrongState,
            "missing_item" => SnapshotKind::MissingItem,
            "spurious_item" => SnapshotKind::SpuriousItem,
            "wrong_link" => SnapshotKind::WrongLink,
            "ui" => SnapshotKind::Ui,
            "other" => SnapshotKind::Other,
            _ => return None,
        })
    }
}

/// 严重度（`feedback_snapshot.severity`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Low,
    Medium,
    High,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "low" => Severity::Low,
            "medium" => Severity::Medium,
            "high" => Severity::High,
            _ => return None,
        })
    }
}

/// 快照状态（`feedback_snapshot.status`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotStatus {
    Open,
    Resolved,
    /// 序列化为契约值 `wontfix`（无下划线，与迁移 CHECK 一致）。
    #[serde(rename = "wontfix")]
    WontFix,
}

impl SnapshotStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SnapshotStatus::Open => "open",
            SnapshotStatus::Resolved => "resolved",
            SnapshotStatus::WontFix => "wontfix",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "open" => SnapshotStatus::Open,
            "resolved" => SnapshotStatus::Resolved,
            "wontfix" => SnapshotStatus::WontFix,
            _ => return None,
        })
    }
}

/// 存储载荷的脱敏标记（`feedback_snapshot.redaction`）。创建路径只写 `masked`；
/// 序列化为契约值 `masked` / `none`（ui/IPC.md §3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Redaction {
    Masked,
    /// 列值 `none`（历史/外部行可能存在；本模块不产生）。
    #[serde(rename = "none")]
    Raw,
}

impl Redaction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Redaction::Masked => "masked",
            Redaction::Raw => "none",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "masked" => Some(Redaction::Masked),
            "none" => Some(Redaction::Raw),
            _ => None,
        }
    }
}

/// 导出格式（IPC `format: "md" | "json"`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Json,
    Markdown,
}

impl ExportFormat {
    /// IPC 字符串（`json` / `md`）→ 枚举；未知值返回 `None`。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "json" => Some(ExportFormat::Json),
            "md" => Some(ExportFormat::Markdown),
            _ => None,
        }
    }

    /// 文件扩展名（也是 IPC 返回值里的格式名）。
    pub fn extension(&self) -> &'static str {
        match self {
            ExportFormat::Json => "json",
            ExportFormat::Markdown => "md",
        }
    }
}

/// 正文是否随导出（PRD §10.2 / §11.2：默认不自动附带整段对话）。
///
/// `No` 是默认；`Explicit` 只应由 UI 层在用户明确勾选「包含正文」后构造。
/// 即便 `Explicit`，附带的也是 `body_masked`（脱敏正文），raw `body`
/// 列永远不会进入任何导出。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IncludeBody {
    #[default]
    No,
    Explicit,
}

// ---------------------------------------------------------------------------
// 入参与行视图
// ---------------------------------------------------------------------------

/// `snapshot_create` 入参（IPC 契约 §3；`title` 由描述首行截 60 字生成）。
#[derive(Debug, Clone)]
pub struct NewSnapshot {
    /// 触发页面，如 `/items/wi_01`。
    pub route: Option<String>,
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    /// 哪里错，必填非空。
    pub description: String,
    /// 期望的正确结果。
    pub expected: Option<String>,
    pub kind: SnapshotKind,
    pub severity: Option<Severity>,
    /// 最小可复现证据的 `source_record.external_id` 引用；为空且目标是
    /// `work_item` 时自动从 `link_evidence` 反查。
    pub record_refs: Vec<String>,
}

/// 创建上下文：时钟与应用版本由调用方注入。
#[derive(Debug, Clone)]
pub struct CreateContext {
    pub now_ms: i64,
    pub app_version: String,
}

/// 一条证据引用（存进 `input_json`，camelCase 与 IPC 口径一致）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotRef {
    pub external_id: String,
    pub source_id: Option<String>,
    pub kind: Option<String>,
    pub occurred_at: Option<i64>,
    pub sender_name: Option<String>,
    /// 已过 [`redact_text`] 的摘要（不含正文）。
    pub summary: Option<String>,
}

/// 引用的判读运行元数据（存进 `output_json`；**不含** `raw_output`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRef {
    pub run_id: String,
    pub kind: Option<String>,
    pub runtime: Option<String>,
    pub model_id: Option<String>,
    pub prompt_version: Option<String>,
    pub status: Option<String>,
    pub created_at: Option<i64>,
}

/// `feedback_snapshot` 行视图（IPC camelCase；`refs`/`runs` 为契约外附加字段）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotRow {
    pub id: String,
    pub route: Option<String>,
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    pub title: String,
    pub description: String,
    pub expected: Option<String>,
    pub kind: Option<SnapshotKind>,
    pub severity: Option<Severity>,
    pub redaction: Redaction,
    pub app_version: String,
    pub schema_version: i64,
    pub status: SnapshotStatus,
    pub fixed_in_release: Option<String>,
    pub created_at: i64,
    pub refs: Vec<SnapshotRef>,
    pub runs: Vec<RunRef>,
}

/// 导出产物：文件名 + 内容（写盘与保存对话框由 desktop 层负责）。
#[derive(Debug, Clone)]
pub struct ExportFile {
    pub file_name: String,
    pub content: String,
    pub snapshot_count: usize,
    pub body_included: bool,
}

/// 快照用例失败分类。
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// 参数错误（IPC `INVALID_ARGUMENT`）。
    #[error("{0}")]
    InvalidArgument(String),
    /// 快照不存在（IPC `NOT_FOUND`）。
    #[error("{0}")]
    NotFound(String),
    #[error("数据库读写失败：{0}")]
    Db(#[from] workcontext_store::rusqlite::Error),
    #[error("导出序列化失败：{0}")]
    Json(#[from] serde_json::Error),
    /// 库中数据超出本模块契约（CHECK 约束下不应出现，fail-loud）。
    #[error("存储数据不符合契约：{0}")]
    Data(String),
}

// ---------------------------------------------------------------------------
// 创建
// ---------------------------------------------------------------------------

static SEQ: AtomicU64 = AtomicU64::new(0);

fn next_id(prefix: &str, now_ms: i64) -> String {
    format!("{}_{:x}{:04x}", prefix, now_ms, SEQ.fetch_add(1, Ordering::Relaxed) & 0xffff)
}

/// 创建快照：单事务写 `feedback_snapshot` + `work_event`，读回持久化行。
pub fn create(
    conn: &Connection,
    new: &NewSnapshot,
    ctx: &CreateContext,
) -> Result<SnapshotRow, SnapshotError> {
    let description = normalize_text(&new.description, "问题描述", 2000)?
        .ok_or_else(|| SnapshotError::InvalidArgument("问题描述不能为空".to_string()))?;
    let expected = normalize_text(new.expected.as_deref().unwrap_or(""), "期望结果", 2000)?;
    let route = normalize_text(new.route.as_deref().unwrap_or(""), "触发页面", 200)?;
    let target_type = normalize_text(new.target_type.as_deref().unwrap_or(""), "目标类型", 100)?;
    let target_id = normalize_text(new.target_id.as_deref().unwrap_or(""), "目标 ID", 100)?;

    let mut refs = normalize_refs(&new.record_refs)?;
    if refs.is_empty() && target_type.as_deref() == Some("work_item") {
        if let Some(item_id) = target_id.as_deref() {
            refs = record_refs_for_item(conn, item_id)?;
        }
    }

    let (snapshot_refs, record_ids) = collect_refs(conn, &refs)?;
    let runs = collect_runs(conn, &record_ids)?;
    let input_json = serde_json::to_string(&RefsPayload { records: snapshot_refs.clone() })?;
    let output_json = serde_json::to_string(&RunsPayload { runs: runs.clone() })?;
    let title = title_from_description(&description);
    let id = next_id("fb", ctx.now_ms);

    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO feedback_snapshot(
            id, route, target_type, target_id, title, description, expected,
            kind, severity, input_json, output_json, log_excerpt, redaction,
            app_version, schema_version, status, fixed_in_release, created_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,NULL,'masked',
                 ?12,?13,'open',NULL,?14)",
        params![
            id,
            route,
            target_type,
            target_id,
            title,
            description,
            expected,
            new.kind.as_str(),
            new.severity.map(|s| s.as_str()),
            input_json,
            output_json,
            ctx.app_version,
            SNAPSHOT_SCHEMA_VERSION,
            ctx.now_ms,
        ],
    )?;
    tx.execute(
        "INSERT INTO work_event(id, target_type, target_id, event_type, actor, at, after_json)
         VALUES (?1,'snapshot',?2,'created','human',?3,?4)",
        params![
            next_id("ev", ctx.now_ms),
            id,
            ctx.now_ms,
            json!({ "status": "open", "kind": new.kind.as_str() }).to_string(),
        ],
    )?;
    tx.commit()?;

    load(conn, &id)?.ok_or_else(|| SnapshotError::Data(format!("快照 {id} 写入后不可读")))
}

/// 事项 → 证据链上的 `external_id` 列表（`origin` 在前，去掉 rejected），
/// 供「一键记录问题快照」自动附带最小证据。
pub fn record_refs_for_item(conn: &Connection, work_item_id: &str) -> Result<Vec<String>, SnapshotError> {
    let mut stmt = conn.prepare(
        "SELECT sr.external_id
         FROM link_evidence le
         JOIN source_record sr ON sr.id = le.source_record_id
         WHERE le.target_type = 'work_item' AND le.target_id = ?1 AND le.state <> 'rejected'
         ORDER BY CASE le.link_type WHEN 'origin' THEN 0 ELSE 1 END, le.created_at, le.id",
    )?;
    let mut refs: Vec<String> = Vec::new();
    for row in stmt.query_map(params![work_item_id], |r| r.get::<_, String>(0))? {
        let external_id = row?;
        if !refs.contains(&external_id) {
            refs.push(external_id);
        }
    }
    Ok(refs)
}

// ---------------------------------------------------------------------------
// 列表
// ---------------------------------------------------------------------------

/// 列出快照（`created_at DESC`；`status` 可选过滤）。
pub fn list(
    conn: &Connection,
    status: Option<SnapshotStatus>,
) -> Result<Vec<SnapshotRow>, SnapshotError> {
    const COLS: &str = "id, route, target_type, target_id, title, description, expected, \
                        kind, severity, input_json, output_json, redaction, app_version, \
                        schema_version, status, fixed_in_release, created_at";
    let mut rows: Vec<SnapshotRow> = Vec::new();
    if let Some(status) = status {
        let sql = format!("SELECT {COLS} FROM feedback_snapshot WHERE status = ?1 ORDER BY created_at DESC, id");
        let mut stmt = conn.prepare(&sql)?;
        for raw in stmt.query_map(params![status.as_str()], raw_from_row)? {
            rows.push(into_snapshot_row(raw?)?);
        }
    } else {
        let sql = format!("SELECT {COLS} FROM feedback_snapshot ORDER BY created_at DESC, id");
        let mut stmt = conn.prepare(&sql)?;
        for raw in stmt.query_map(params![], raw_from_row)? {
            rows.push(into_snapshot_row(raw?)?);
        }
    }
    Ok(rows)
}

/// 按 ID 读一行（导出与创建读回共用）。
fn load(conn: &Connection, id: &str) -> Result<Option<SnapshotRow>, SnapshotError> {
    const COLS: &str = "id, route, target_type, target_id, title, description, expected, \
                        kind, severity, input_json, output_json, redaction, app_version, \
                        schema_version, status, fixed_in_release, created_at";
    let sql = format!("SELECT {COLS} FROM feedback_snapshot WHERE id = ?1");
    conn.query_row(&sql, params![id], raw_from_row)
        .optional()?
        .map(into_snapshot_row)
        .transpose()
}

/// 数据库原始行（列顺序与上面的 `COLS` 一一对应）。
struct RawRow {
    id: String,
    route: Option<String>,
    target_type: Option<String>,
    target_id: Option<String>,
    title: String,
    description: String,
    expected: Option<String>,
    kind: Option<String>,
    severity: Option<String>,
    input_json: Option<String>,
    output_json: Option<String>,
    redaction: String,
    app_version: String,
    schema_version: i64,
    status: String,
    fixed_in_release: Option<String>,
    created_at: i64,
}

fn raw_from_row(row: &Row) -> workcontext_store::rusqlite::Result<RawRow> {
    Ok(RawRow {
        id: row.get(0)?,
        route: row.get(1)?,
        target_type: row.get(2)?,
        target_id: row.get(3)?,
        title: row.get(4)?,
        description: row.get(5)?,
        expected: row.get(6)?,
        kind: row.get(7)?,
        severity: row.get(8)?,
        input_json: row.get(9)?,
        output_json: row.get(10)?,
        redaction: row.get(11)?,
        app_version: row.get(12)?,
        schema_version: row.get(13)?,
        status: row.get(14)?,
        fixed_in_release: row.get(15)?,
        created_at: row.get(16)?,
    })
}

fn into_snapshot_row(raw: RawRow) -> Result<SnapshotRow, SnapshotError> {
    let id = raw.id.clone();
    let bad = |field: &str, value: &str| {
        SnapshotError::Data(format!("快照 {id} 的 {field} 超出契约：{value}"))
    };
    let kind = raw
        .kind
        .as_deref()
        .map(|s| SnapshotKind::parse(s).ok_or_else(|| bad("kind", s)))
        .transpose()?;
    let severity = match raw.severity.as_deref() {
        None => None,
        Some(s) => Some(Severity::parse(s).ok_or_else(|| bad("severity", s))?),
    };
    let status = SnapshotStatus::parse(&raw.status)
        .ok_or_else(|| bad("status", &raw.status))?;
    let redaction = Redaction::parse(&raw.redaction)
        .ok_or_else(|| bad("redaction", &raw.redaction))?;
    let refs = raw
        .input_json
        .as_deref()
        .map(serde_json::from_str::<RefsPayload>)
        .transpose()?
        .map(|p| p.records)
        .unwrap_or_default();
    let runs = raw
        .output_json
        .as_deref()
        .map(serde_json::from_str::<RunsPayload>)
        .transpose()?
        .map(|p| p.runs)
        .unwrap_or_default();
    Ok(SnapshotRow {
        id: raw.id,
        route: raw.route,
        target_type: raw.target_type,
        target_id: raw.target_id,
        title: raw.title,
        description: raw.description,
        expected: raw.expected,
        kind,
        severity,
        redaction,
        app_version: raw.app_version,
        schema_version: raw.schema_version,
        status,
        fixed_in_release: raw.fixed_in_release,
        created_at: raw.created_at,
        refs,
        runs,
    })
}

#[derive(Default, Serialize, Deserialize)]
struct RefsPayload {
    #[serde(default)]
    records: Vec<SnapshotRef>,
}

#[derive(Default, Serialize, Deserialize)]
struct RunsPayload {
    #[serde(default)]
    runs: Vec<RunRef>,
}

// ---------------------------------------------------------------------------
// 导出
// ---------------------------------------------------------------------------

/// 导出选中快照。内容一律脱敏；正文只在 [`IncludeBody::Explicit`] 时附带
/// （且只带 `body_masked`）。`now_ms` 用于导出头与文件名。
pub fn export(
    conn: &Connection,
    ids: &[String],
    format: ExportFormat,
    include_body: IncludeBody,
    now_ms: i64,
) -> Result<ExportFile, SnapshotError> {
    if ids.is_empty() {
        return Err(SnapshotError::InvalidArgument("未选择要导出的快照".to_string()));
    }
    let mut rows: Vec<SnapshotRow> = Vec::new();
    for id in ids {
        let id = id.trim();
        if id.is_empty() {
            return Err(SnapshotError::InvalidArgument("快照 ID 不能为空".to_string()));
        }
        if rows.iter().any(|r| r.id == id) {
            continue;
        }
        match load(conn, id)? {
            Some(row) => rows.push(row),
            None => return Err(SnapshotError::NotFound(format!("快照不存在：{id}"))),
        }
    }
    let bodies = if include_body == IncludeBody::Explicit {
        fetch_bodies(conn, &rows)?
    } else {
        Vec::new()
    };
    let content = match format {
        ExportFormat::Json => export_json(&rows, &bodies, include_body, now_ms)?,
        ExportFormat::Markdown => export_md(&rows, &bodies, include_body, now_ms),
    };
    Ok(ExportFile {
        file_name: format!("workcontext-snapshots-{}.{}", now_ms / 1000, format.extension()),
        content,
        snapshot_count: rows.len(),
        body_included: include_body == IncludeBody::Explicit,
    })
}

/// 正文条目（仅 Explicit 时存在；`body_masked` 再过一遍 [`redact_text`]）。
struct BodyInfo {
    external_id: String,
    body_masked: Option<String>,
    raw_state: String,
}

fn fetch_bodies(conn: &Connection, rows: &[SnapshotRow]) -> Result<Vec<BodyInfo>, SnapshotError> {
    let mut bodies: Vec<BodyInfo> = Vec::new();
    for row in rows {
        for r in &row.refs {
            if bodies.iter().any(|b| b.external_id == r.external_id) {
                continue;
            }
            let found: Option<(Option<String>, String)> = conn
                .query_row(
                    "SELECT body_masked, raw_state FROM source_record
                     WHERE external_id = ?1 ORDER BY rowid LIMIT 1",
                    params![r.external_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((body_masked, raw_state)) = found {
                bodies.push(BodyInfo {
                    external_id: r.external_id.clone(),
                    body_masked: body_masked.as_deref().map(redact_text),
                    raw_state,
                });
            }
        }
    }
    Ok(bodies)
}

fn export_json(
    rows: &[SnapshotRow],
    bodies: &[BodyInfo],
    include_body: IncludeBody,
    now_ms: i64,
) -> Result<String, SnapshotError> {
    let explicit = include_body == IncludeBody::Explicit;
    let snapshots: Vec<Value> = rows
        .iter()
        .map(|row| {
            let refs: Vec<&str> = row.refs.iter().map(|r| r.external_id.as_str()).collect();
            let records: Vec<Value> = row
                .refs
                .iter()
                .map(|r| {
                    json!({
                        "external_id": r.external_id,
                        "source_id": r.source_id,
                        "kind": r.kind,
                        "occurred_at": r.occurred_at,
                        "sender_name": r.sender_name.as_deref().map(redact_text),
                        "summary": r.summary.as_deref().map(redact_text),
                    })
                })
                .collect();
            let runs: Vec<Value> = row
                .runs
                .iter()
                .map(|run| {
                    json!({
                        "run_id": run.run_id,
                        "kind": run.kind,
                        "runtime": run.runtime,
                        "model_id": run.model_id,
                        "prompt_version": run.prompt_version,
                        "status": run.status,
                        "created_at": run.created_at,
                    })
                })
                .collect();
            let mut snapshot = json!({
                "id": row.id,
                "schema_version": row.schema_version,
                "app_version": row.app_version,
                "route": row.route,
                "target_type": row.target_type,
                "target_id": row.target_id,
                "title": redact_text(&row.title),
                "description": redact_text(&row.description),
                "expected": row.expected.as_deref().map(redact_text),
                "kind": row.kind.map(|k| k.as_str()),
                "severity": row.severity.map(|s| s.as_str()),
                "status": row.status.as_str(),
                "redaction": row.redaction.as_str(),
                "fixed_in_release": row.fixed_in_release,
                "created_at": row.created_at,
                "refs": refs,
                "records": records,
                "runs": runs,
            });
            if explicit {
                let bs: Vec<Value> = bodies
                    .iter()
                    .filter(|b| row.refs.iter().any(|r| r.external_id == b.external_id))
                    .map(|b| {
                        json!({
                            "external_id": b.external_id,
                            "body_masked": b.body_masked,
                            "raw_state": b.raw_state,
                        })
                    })
                    .collect();
                snapshot["bodies"] = Value::Array(bs);
            }
            snapshot
        })
        .collect();
    let doc = json!({
        "schema": SNAPSHOT_JSON_SCHEMA,
        "schema_version": SNAPSHOT_SCHEMA_VERSION,
        "exported_at": now_ms,
        "count": rows.len(),
        "body_included": explicit,
        "snapshots": snapshots,
    });
    Ok(serde_json::to_string_pretty(&doc)?)
}

fn export_md(
    rows: &[SnapshotRow],
    bodies: &[BodyInfo],
    include_body: IncludeBody,
    now_ms: i64,
) -> String {
    let explicit = include_body == IncludeBody::Explicit;
    let mut md = String::new();
    md.push_str("# WorkContext 迭代快照导出\n\n");
    md.push_str(&format!("- 格式：{SNAPSHOT_JSON_SCHEMA}（Markdown）\n"));
    md.push_str(&format!("- 快照数：{}\n", rows.len()));
    md.push_str(if explicit {
        "- 正文：已按用户明确同意附带脱敏正文（body_masked）\n"
    } else {
        "- 正文：未包含（默认脱敏导出，不含完整对话）\n"
    });
    md.push_str(&format!("- 导出时间：{}\n", rfc3339(now_ms)));
    for (i, row) in rows.iter().enumerate() {
        md.push_str("\n---\n\n");
        md.push_str(&format!("## {}. {}\n\n", i + 1, redact_text(&row.title)));
        md.push_str(&format!("- ID：{}\n", row.id));
        md.push_str(&format!(
            "- 状态：{} · 类型：{} · 严重度：{}\n",
            row.status.as_str(),
            row.kind.map(|k| k.as_str()).unwrap_or("—"),
            row.severity.map(|s| s.as_str()).unwrap_or("—"),
        ));
        if let Some(route) = row.route.as_deref() {
            md.push_str(&format!("- 路由：{}\n", redact_text(route)));
        }
        if let (Some(t), Some(i2)) = (row.target_type.as_deref(), row.target_id.as_deref()) {
            md.push_str(&format!("- 目标：{}/{}\n", redact_text(t), redact_text(i2)));
        }
        md.push_str(&format!("- 应用版本：{}\n", redact_text(&row.app_version)));
        md.push_str(&format!("- schema_version：{}\n", row.schema_version));
        md.push_str(&format!("- 记录时间：{}\n", rfc3339(row.created_at)));
        if let Some(fixed) = row.fixed_in_release.as_deref() {
            md.push_str(&format!("- 修复版本：{}\n", redact_text(fixed)));
        }
        md.push_str("\n### 问题描述\n\n");
        md.push_str(&redact_text(&row.description));
        md.push_str("\n\n### 期望\n\n");
        match row.expected.as_deref() {
            Some(e) => md.push_str(&redact_text(e)),
            None => md.push_str("（未填写）"),
        }
        md.push_str("\n\n### 引用\n\n");
        if row.refs.is_empty() {
            md.push_str("（无来源记录引用）\n");
        }
        for r in &row.refs {
            match r.summary.as_deref() {
                Some(s) => md.push_str(&format!("- {}：{}\n", r.external_id, redact_text(s))),
                None => md.push_str(&format!("- {}：\n", r.external_id)),
            }
        }
        md.push_str("\n### 判读运行\n\n");
        if row.runs.is_empty() {
            md.push_str("（无）\n");
        }
        for run in &row.runs {
            md.push_str(&format!(
                "- {} · {} · {}\n",
                run.run_id,
                run.runtime.as_deref().unwrap_or("—"),
                run.status.as_deref().unwrap_or("—"),
            ));
        }
        if explicit {
            md.push_str("\n### 正文（脱敏，经明确同意导出）\n\n");
            let own: Vec<&BodyInfo> = bodies
                .iter()
                .filter(|b| row.refs.iter().any(|r| r.external_id == b.external_id))
                .collect();
            if own.is_empty() {
                md.push_str("（引用的来源记录已不可用）\n");
            }
            for b in own {
                match b.body_masked.as_deref() {
                    Some(body) => md.push_str(&format!("- {}：{}\n", b.external_id, body)),
                    None => md.push_str(&format!("- {}：原文已按保留策略清理\n", b.external_id)),
                }
            }
        }
    }
    md
}

// ---------------------------------------------------------------------------
// 内部工具
// ---------------------------------------------------------------------------

/// trim 后校验长度；空 → `None`（可选字段归一化），超长报参数错误。
fn normalize_text(value: &str, field: &str, max_chars: usize) -> Result<Option<String>, SnapshotError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let chars = trimmed.chars().count();
    if chars > max_chars {
        return Err(SnapshotError::InvalidArgument(format!(
            "{field}过长：{chars} 字（上限 {max_chars}）"
        )));
    }
    Ok(Some(trimmed.to_string()))
}

fn normalize_refs(refs: &[String]) -> Result<Vec<String>, SnapshotError> {
    if refs.len() > 50 {
        return Err(SnapshotError::InvalidArgument(format!(
            "证据引用过多：{} 条（上限 50）",
            refs.len()
        )));
    }
    let mut out: Vec<String> = Vec::new();
    for r in refs {
        let trimmed = r.trim();
        if trimmed.is_empty() {
            return Err(SnapshotError::InvalidArgument("证据引用不能为空".to_string()));
        }
        if trimmed.chars().count() > 200 {
            return Err(SnapshotError::InvalidArgument("证据引用过长（上限 200 字）".to_string()));
        }
        if !out.iter().any(|x| x == trimmed) {
            out.push(trimmed.to_string());
        }
    }
    Ok(out)
}

/// IPC 契约 §3：标题 = 描述首行截 60 字（`title` 列非空约束）。
fn title_from_description(description: &str) -> String {
    let first_line = description.lines().next().unwrap_or("").trim();
    let mut title = String::new();
    for (i, ch) in first_line.chars().enumerate() {
        if i >= 60 {
            break;
        }
        title.push(ch);
    }
    if title.is_empty() {
        "未命名问题快照".to_string()
    } else {
        title
    }
}

/// `source_record` 的一行证据元数据（[`collect_refs`] 的查询结果）。
struct RecordMeta {
    id: String,
    source_id: Option<String>,
    kind: Option<String>,
    occurred_at: Option<i64>,
    sender_name: Option<String>,
    summary: Option<String>,
}

impl RecordMeta {
    /// 列顺序：id, source_id, kind, occurred_at, sender_name, summary。
    fn from_row(row: &Row) -> workcontext_store::rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            source_id: row.get(1)?,
            kind: row.get(2)?,
            occurred_at: row.get(3)?,
            sender_name: row.get(4)?,
            summary: row.get(5)?,
        })
    }
}

/// `agent_run` 的一行判读元数据（[`collect_runs`] 的查询结果；不含 `raw_output`）。
struct RunMeta {
    id: String,
    kind: Option<String>,
    runtime: Option<String>,
    model_id: Option<String>,
    prompt_version: Option<String>,
    status: Option<String>,
    created_at: Option<i64>,
}

impl RunMeta {
    /// 列顺序：id, kind, runtime, model_id, prompt_version, status, created_at。
    fn from_row(row: &Row) -> workcontext_store::rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            kind: row.get(1)?,
            runtime: row.get(2)?,
            model_id: row.get(3)?,
            prompt_version: row.get(4)?,
            status: row.get(5)?,
            created_at: row.get(6)?,
        })
    }
}

/// 引用 → (脱敏后的 SnapshotRef 列表, source_record 行 ID 列表)。
/// 引用不要求记录仍存在（保留期清理后引用仍是有效标识，summary 置空）。
fn collect_refs(
    conn: &Connection,
    refs: &[String],
) -> Result<(Vec<SnapshotRef>, Vec<String>), SnapshotError> {
    let mut out: Vec<SnapshotRef> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    for external_id in refs {
        let found = conn
            .query_row(
                "SELECT id, source_id, kind, occurred_at, sender_name, summary
                 FROM source_record WHERE external_id = ?1 ORDER BY rowid LIMIT 1",
                params![external_id],
                RecordMeta::from_row,
            )
            .optional()?;
        out.push(match found {
            Some(meta) => {
                ids.push(meta.id);
                SnapshotRef {
                    external_id: external_id.clone(),
                    source_id: meta.source_id,
                    kind: meta.kind,
                    occurred_at: meta.occurred_at,
                    sender_name: meta.sender_name.as_deref().map(redact_text),
                    summary: meta.summary.as_deref().map(redact_text),
                }
            }
            None => SnapshotRef {
                external_id: external_id.clone(),
                source_id: None,
                kind: None,
                occurred_at: None,
                sender_name: None,
                summary: None,
            },
        });
    }
    Ok((out, ids))
}

/// 每条证据记录关联的最近一次判读运行（只取元数据，绝不取 `raw_output`）。
/// 匹配规则：`agent_run.input_refs_json` 文本里含带引号的 `"source_record行ID"`
/// token（对数组或 ItemService 的对象形 `{"sourceRecordIds":[…]}` 都成立）。
fn collect_runs(conn: &Connection, record_ids: &[String]) -> Result<Vec<RunRef>, SnapshotError> {
    let mut runs: Vec<RunRef> = Vec::new();
    for record_id in record_ids {
        let pattern = format!("%\"{record_id}\"%");
        let found = conn
            .query_row(
                "SELECT id, kind, runtime, model_id, prompt_version, status, created_at
                 FROM agent_run WHERE input_refs_json LIKE ?1
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                params![pattern],
                RunMeta::from_row,
            )
            .optional()?;
        if let Some(meta) = found {
            if !runs.iter().any(|r| r.run_id == meta.id) {
                runs.push(RunRef {
                    run_id: meta.id,
                    kind: meta.kind,
                    runtime: meta.runtime,
                    model_id: meta.model_id,
                    prompt_version: meta.prompt_version,
                    status: meta.status,
                    created_at: meta.created_at,
                });
            }
        }
    }
    Ok(runs)
}

fn rfc3339(ms: i64) -> String {
    use chrono::{SecondsFormat, TimeZone, Utc};
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_else(|| ms.to_string())
}

// ---------------------------------------------------------------------------
// 脱敏（门槛 12：手机号/邮箱/token 不出现在日志与导出）
// ---------------------------------------------------------------------------

/// 去掉 URL 的 query：`?` 及其后到空白/非 ASCII 字节为止的内容整体丢弃。
///
/// 先于其它规则执行：query 里的 token、手机号随 query 一起消失，也不会与
/// 邮箱等规则的区间重叠。未命中时零拷贝。
fn strip_url_queries(input: &str) -> Cow<'_, str> {
    let lower = input.to_ascii_lowercase();
    let bytes = input.as_bytes();
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    for prefix in ["http://", "https://"] {
        let mut search = 0;
        while let Some(pos) = lower[search..].find(prefix) {
            let pos = search + pos;
            search = pos + prefix.len();
            // URL 只含 ASCII：遇到空白或非 ASCII 字节（如紧随其后的中文）即结束。
            let mut e = pos;
            while e < bytes.len() && bytes[e].is_ascii() && !bytes[e].is_ascii_whitespace() {
                e += 1;
            }
            if let Some(q) = input[pos..e].find('?') {
                cuts.push((pos + q, e));
            }
        }
    }
    if cuts.is_empty() {
        return Cow::Borrowed(input);
    }
    cuts.sort_unstable();
    let mut out = String::with_capacity(input.len());
    let mut pos = 0;
    for (s, e) in cuts {
        if s < pos {
            continue;
        }
        out.push_str(&input[pos..s]);
        pos = e;
    }
    out.push_str(&input[pos..]);
    Cow::Owned(out)
}

/// 文本脱敏（详细设计 §7.3 / §13.2）：
///
/// - URL 先去掉 `?` 及之后的 query；
/// - 大陆手机号（1[3-9] + 9 位）→ `138****1234`（保前 3 后 4）；
/// - 18 位身份证（17 位 + X/x）与 16–19 位数字串 → 仅保留后 4 位；
/// - 邮箱 → 本地部分只留首字符（保域名）；
/// - `Bearer <token>` 与 ≥32 位的 opaque token → `***`。
///
/// 同一段文本里的**每个**敏感项都会被处理（各规则按区间互斥认领）。全部
/// 规则只匹配 ASCII 边界，UTF-8 切片安全。人名打码（`张*`）只属于出域
/// 编码器（§13.2），不在此处猜测。
pub fn redact_text(input: &str) -> String {
    let stripped = strip_url_queries(input);
    let input: &str = &stripped;
    let bytes = input.as_bytes();
    let mut claimed: Vec<(usize, usize)> = Vec::new();
    let mut edits: Vec<(usize, usize, String)> = Vec::new();

    let overlaps = |claimed: &[(usize, usize)], s: usize, e: usize| {
        claimed.iter().any(|(a, b)| s < *b && *a < e)
    };
    let is_local = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'%' | b'+' | b'-');
    let is_domain = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-');
    let is_token = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-');

    // 1) 邮箱：保域名，本地部分打码。
    for (i, &b) in bytes.iter().enumerate() {
        if b != b'@' {
            continue;
        }
        let mut l = i;
        while l > 0 && is_local(bytes[l - 1]) {
            l -= 1;
        }
        let mut r = i + 1;
        while r < bytes.len() && is_domain(bytes[r]) {
            r += 1;
        }
        let local = &input[l..i];
        let domain = &input[i + 1..r];
        if local.is_empty()
            || domain.len() < 4
            || !domain.contains('.')
            || domain.starts_with('.')
            || domain.ends_with('.')
            || domain.starts_with('-')
        {
            continue;
        }
        let masked_local = if local.len() > 1 {
            format!("{}***", &local[..1])
        } else {
            "***".to_string()
        };
        claimed.push((l, r));
        edits.push((l, i, masked_local));
    }

    // 2) Bearer 凭据：token 本体替换为 ***。
    let lower = input.to_ascii_lowercase();
    let mut search = 0;
    while let Some(pos) = lower[search..].find("bearer") {
        let pos = search + pos;
        search = pos + "bearer".len();
        let mut t = pos + "bearer".len();
        while t < bytes.len() && (bytes[t] == b' ' || bytes[t] == b'\t') {
            t += 1;
        }
        let start = t;
        while t < bytes.len() && !bytes[t].is_ascii_whitespace() {
            t += 1;
        }
        if t - start >= 8 && !overlaps(&claimed, start, t) {
            claimed.push((start, t));
            edits.push((start, t, "***".to_string()));
        }
    }

    // 3) 数字串：身份证 / 手机号 / 银行卡。
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let len = i - start;
        if overlaps(&claimed, start, i) {
            continue;
        }
        if len == 17 && i < bytes.len() && (bytes[i] == b'X' || bytes[i] == b'x') {
            let tail4 = &input[start + 14..start + 18];
            claimed.push((start, start + 18));
            edits.push((start, start + 18, format!("{}{}", "*".repeat(14), tail4)));
            i += 1; // 消费校验位 X
            continue;
        }
        if len == 11 && bytes[start] == b'1' && (b'3'..=b'9').contains(&bytes[start + 1]) {
            let masked = format!("{}****{}", &input[start..start + 3], &input[start + 7..start + 11]);
            claimed.push((start, i));
            edits.push((start, i, masked));
            continue;
        }
        if (16..=19).contains(&len) {
            let masked = format!("{}{}", "*".repeat(len - 4), &input[i - 4..i]);
            claimed.push((start, i));
            edits.push((start, i, masked));
        }
    }

    // 4) ≥32 位 opaque token（hash/密钥）→ ***。
    let mut j = 0;
    while j < bytes.len() {
        if !is_token(bytes[j]) {
            j += 1;
            continue;
        }
        let start = j;
        while j < bytes.len() && is_token(bytes[j]) {
            j += 1;
        }
        if j - start >= 32 && !overlaps(&claimed, start, j) {
            claimed.push((start, j));
            edits.push((start, j, "***".to_string()));
        }
    }

    // 应用替换：按起点升序。`pos` 是上一条已应用编辑的终点，起点落在它之前
    // 的即重叠（claimed 机制下不应出现），防御性跳过。全部编辑都落在 ASCII
    // 边界，切片安全。
    edits.sort_by_key(|(s, _, _)| *s);
    let mut result = String::with_capacity(input.len() + 16);
    let mut pos = 0;
    for (s, e, rep) in edits {
        if s < pos {
            continue;
        }
        result.push_str(&input[pos..s]);
        result.push_str(&rep);
        pos = e;
    }
    result.push_str(&input[pos..]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_phone_keeps_head_and_tail() {
        let out = redact_text("有问题可联系值班电话 13800001234。");
        assert!(out.contains("138****1234"));
        assert!(!out.contains("13800001234"));
    }

    #[test]
    fn redact_leaves_longer_digit_runs_and_dates_alone() {
        // 14 位时间戳不是手机号/身份证/银行卡，应原样保留。
        assert_eq!(redact_text("批次 20261121142500 结束"), "批次 20261121142500 结束");
        // 12 位数字串（手机号多一位）不改写。
        assert_eq!(redact_text("编号 138000012345"), "编号 138000012345");
    }

    #[test]
    fn redact_id_card_and_bank_card_keep_last_four() {
        let id = redact_text("身份证 11010119900307867X");
        assert!(id.contains("**************867X"));
        assert!(!id.contains("11010119900307"));
        let bank = redact_text("卡号 6222020200112233334");
        assert!(bank.contains("***************3334"));
        assert!(!bank.contains("6222020200112233"));
    }

    #[test]
    fn redact_email_keeps_domain() {
        let out = redact_text("联系 zhangsan@example.com 或 a@b.cn");
        assert!(out.contains("z***@example.com"));
        assert!(!out.contains("zhangsan"));
        // 短本地部分同样只保留域名。
        assert!(out.contains("***@b.cn"));
    }

    #[test]
    fn redact_bearer_and_long_tokens() {
        let out = redact_text("Authorization: Bearer abcdef1234567890abcdef1234567890");
        assert!(!out.contains("abcdef1234567890"));
        assert!(out.contains("Bearer ***"));
        let hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let out = redact_text(&format!("sha256={hash}"));
        assert_eq!(out, "sha256=***");
    }

    #[test]
    fn redact_url_strips_query() {
        assert_eq!(
            redact_text("见 https://a.b/c?token=secret&page=2 结束"),
            "见 https://a.b/c 结束"
        );
    }

    /// 回归：一段文本里有多个敏感项时必须逐项脱敏（曾经因替换顺序写反，
    /// 只有位置最靠后的一项生效，前面的手机号/邮箱原样泄漏到导出）。
    #[test]
    fn redact_handles_every_sensitive_item_in_one_text() {
        let out = redact_text(
            "电话 13800001234，邮箱 zhangsan@example.com，备用 13900004321，卡号 6222020200112233334",
        );
        assert_eq!(
            out,
            "电话 138****1234，邮箱 z***@example.com，备用 139****4321，卡号 ***************3334"
        );
    }

    #[test]
    fn redact_url_query_is_dropped_even_when_it_holds_an_email() {
        // query 里含邮箱：整段 query 仍被丢弃，不会因与邮箱规则区间重叠而整条跳过。
        assert_eq!(
            redact_text("见 https://a.b/c?token=secret&email=zhangsan@example.com 结束"),
            "见 https://a.b/c 结束"
        );
        // 紧贴中文的 URL：到非 ASCII 字节为止，不吞掉后文。
        assert_eq!(redact_text("见https://a.b/c?x=1结束"), "见https://a.b/c结束");
    }

    #[test]
    fn redact_plain_chinese_untouched() {
        let text = "工单系统今晚 22:00-24:00 停机维护，你知悉就行。";
        assert_eq!(redact_text(text), text);
    }

    #[test]
    fn title_is_first_line_truncated_to_60_chars() {
        assert_eq!(title_from_description("首行标题\n第二行"), "首行标题");
        let long = "长".repeat(80);
        assert_eq!(title_from_description(&long).chars().count(), 60);
        assert_eq!(title_from_description("   \n 描述"), "未命名问题快照");
    }

    #[test]
    fn enums_parse_known_values_only() {
        assert_eq!(SnapshotKind::parse("spurious_item"), Some(SnapshotKind::SpuriousItem));
        assert_eq!(SnapshotKind::parse("random"), None);
        assert_eq!(Severity::parse("medium"), Some(Severity::Medium));
        assert_eq!(SnapshotStatus::parse("wontfix"), Some(SnapshotStatus::WontFix));
        assert_eq!(ExportFormat::parse("md"), Some(ExportFormat::Markdown));
        assert_eq!(ExportFormat::parse("csv"), None);
        assert_eq!(Redaction::parse("none"), Some(Redaction::Raw));
    }

    #[test]
    fn enums_serialize_to_ipc_contract_values() {
        // ui/IPC.md §3：status 取 "open" | "resolved" | "wontfix"（无下划线）。
        assert_eq!(serde_json::to_value(SnapshotStatus::WontFix).unwrap(), json!("wontfix"));
        assert_eq!(serde_json::to_value(SnapshotStatus::Open).unwrap(), json!("open"));
        assert_eq!(serde_json::to_value(Redaction::Raw).unwrap(), json!("none"));
        assert_eq!(serde_json::to_value(Redaction::Masked).unwrap(), json!("masked"));
        assert_eq!(
            serde_json::to_value(SnapshotKind::SpuriousItem).unwrap(),
            json!("spurious_item")
        );
        assert_eq!(serde_json::to_value(Severity::Medium).unwrap(), json!("medium"));
    }
}
