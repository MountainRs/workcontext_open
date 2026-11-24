//! 事项用例服务（详细设计 §9、§10.1；wire 契约 `ui/IPC.md` §2）。
//!
//! 覆盖 IPC 命令：`today_list` / `items_query` / `item_get` / `item_correct` /
//! `item_close` / `item_set_followup`。视图 DTO 一律 camelCase Serialize、
//! 时间 UTC 毫秒；入参 DTO camelCase Deserialize，由 desktop command 层
//! （`crates/desktop/src/items.rs`）直接反序列化后调用本模块。
//!
//! 写路径全部单事务（BEGIN IMMEDIATE）：人工动作写 `work_item` +
//! `field_override`（只锁实际改动的字段，§9）+ `work_event(actor=human)`。
//! 人工操作可越过字段锁（§3.3 末行守卫：锁只拦 Agent）；但生命周期转换仍
//! 走 core 状态机守卫，收尾证据过 §3.3 白名单（V05）。
//!
//! 证据时效（freshness）：以 `sync_checkpoint` 的 `last_success_at` 为准，
//! `lastSuccessAt=null → unknown`——UI 对「等别人」组隐藏卡片，防止把
//! 「未查到回复」当成「对方未回复」。事项内证据时效由 `lastEvidenceAt`
//! 原样透出，不替 UI 下结论。

use serde::{Deserialize, Serialize};
use serde_json::json;
use workcontext_core::{
    close_verdict, evaluate_transition, validate_correction, Attention, CloseVerdict,
    Correction, DbFollowupType, ItemField, Lifecycle, LockedField,
    ResolutionEvidence, ResolutionEvidenceType, TransitionOutcome, TransitionRequest,
};
use workcontext_store::rusqlite::{self, params, Connection, OptionalExtension};

#[path = "item_catalog.rs"]
mod catalog;
pub use catalog::{context_scope_items, item_get_for_profile, DeliveryDetail, RelatedDelivery, ScopeItem};
pub(crate) use catalog::CATALOG;

// ========================= 错误模型（IPC §0.2） =========================

/// 用例层错误；`code()` 对齐 `ui/IPC.md` 已约定的后端错误码。
#[derive(Debug, thiserror::Error)]
pub enum ItemsError {
    #[error("本地数据库暂时被占用或读写失败，请稍后重试")]
    Db(#[from] rusqlite::Error),
    #[error("这条事项已不存在，可能已被恢复或清理")]
    NotFound,
    #[error("{message}")]
    InvalidArgument { message: String },
    #[error("{message}")]
    ValidationRejected { message: String, hint: Option<String> },
}

impl ItemsError {
    /// 稳定错误码（`ui/IPC.md` §0.2 表）。
    pub fn code(&self) -> &'static str {
        match self {
            ItemsError::Db(e) => {
                if e.to_string().contains("locked") || e.to_string().contains("busy") {
                    "DB_BUSY"
                } else {
                    "INTERNAL"
                }
            }
            ItemsError::NotFound => "NOT_FOUND",
            ItemsError::InvalidArgument { .. } => "INVALID_ARGUMENT",
            ItemsError::ValidationRejected { .. } => "VALIDATION_REJECTED",
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(self, ItemsError::Db(_))
    }

    pub fn hint(&self) -> Option<String> {
        match self {
            ItemsError::ValidationRejected { hint, .. } => hint.clone(),
            ItemsError::NotFound => Some("返回全部事项后刷新列表。".to_string()),
            _ => None,
        }
    }
}

// ========================= 共享行加载（judge.rs 复用） =========================

/// `work_item` 行的强类型视图（列名即字段名）。
pub(crate) struct ItemRow {
    pub id: String,
    pub title: String,
    pub lifecycle: Lifecycle,
    pub attention: Attention,
    pub next_action: Option<String>,
    pub followup_type: DbFollowupType,
    pub followup_at: Option<i64>,
    pub followup_note: Option<String>,
    pub thread_id: Option<String>,
    pub origin_source_record_id: Option<String>,
    pub confidence: Option<i64>,
    pub awaiting_confirmation: bool,
    pub closed_at: Option<i64>,
    pub close_reason: Option<String>,
    pub resolution_evidence: Option<String>,
    pub last_evidence_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl ItemRow {
    /// 供 `PolicyValidator` 使用的既有事项视图。
    pub(crate) fn to_existing(&self) -> workcontext_core::ExistingItem {
        workcontext_core::ExistingItem {
            id: self.id.clone(),
            lifecycle: self.lifecycle,
            attention: self.attention,
            title: self.title.clone(),
            next_action: self.next_action.clone(),
            followup_type: self.followup_type,
            followup_at: self.followup_at,
            awaiting_confirmation: self.awaiting_confirmation,
            resolution_evidence: self.resolution_evidence.clone(),
            last_evidence_at: self.last_evidence_at,
            locked_fields: Vec::new(),
        }
    }
}

/// snake_case 文本 → 枚举（Lifecycle / Attention / DbFollowupType …）。
pub(crate) fn enum_from_str<T: serde::de::DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

const ITEM_COLUMNS: &str = "id, title, lifecycle, attention, next_action, followup_type, \
     followup_at, followup_note, thread_id, origin_source_record_id, dedupe_key, confidence, \
     awaiting_confirmation, closed_at, close_reason, resolution_evidence, last_evidence_at, \
     created_at, updated_at";

pub(crate) fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<ItemRow> {
    // 第 11 列 dedupe_key 仅作 V10 唯一键，不进入内存行。
    let _dedupe_key: String = row.get(10)?;
    Ok(ItemRow {
        id: row.get(0)?,
        title: row.get(1)?,
        lifecycle: enum_from_str(&row.get::<_, String>(2)?).expect("库内 lifecycle 受 CHECK 约束"),
        attention: enum_from_str(&row.get::<_, String>(3)?).expect("库内 attention 受 CHECK 约束"),
        next_action: row.get(4)?,
        followup_type: enum_from_str(&row.get::<_, String>(5)?)
            .expect("库内 followup_type 受 CHECK 约束"),
        followup_at: row.get(6)?,
        followup_note: row.get(7)?,
        thread_id: row.get(8)?,
        origin_source_record_id: row.get(9)?,
        confidence: row.get(11)?,
        awaiting_confirmation: row.get::<_, i64>(12)? != 0,
        closed_at: row.get(13)?,
        close_reason: row.get(14)?,
        resolution_evidence: row.get(15)?,
        last_evidence_at: row.get(16)?,
        created_at: row.get(17)?,
        updated_at: row.get(18)?,
    })
}

/// 按 id 读单条事项；不存在返回 `None`。
pub(crate) fn load_item(conn: &Connection, id: &str) -> rusqlite::Result<Option<ItemRow>> {
    conn.query_row(
        &format!("SELECT {ITEM_COLUMNS} FROM work_item WHERE id = ?1"),
        params![id],
        row_to_item,
    )
    .optional()
}

/// 该事项当前生效的人工锁（`locked=1`；§3.1：存在即锁定）。
pub(crate) fn load_locks(conn: &Connection, item_id: &str) -> rusqlite::Result<Vec<LockedField>> {
    let mut stmt = conn.prepare(
        "SELECT field, value_json, locked FROM field_override \
         WHERE target_type = 'work_item' AND target_id = ?1 AND locked = 1",
    )?;
    let locks = stmt
        .query_map(params![item_id], |row| {
            let field: String = row.get(0)?;
            Ok(ItemField::parse(&field).map(|f| {
                LockedField {
                    field: f,
                    value_json: row.get(1).unwrap_or(None),
                    locked: row.get::<_, i64>(2).unwrap_or(1) != 0,
                }
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // 未知字段（前向兼容旧库）不参与锁判定，直接忽略。
    Ok(locks.into_iter().flatten().collect())
}

// ========================= wire DTO（ui/IPC.md §2） =========================

/// 同步新鲜度（IPC §2 `Freshness`）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Freshness {
    pub state: &'static str, // fresh | stale | expired | unknown
    pub last_success_at: Option<i64>,
    pub window_start: Option<i64>,
    pub window_end: Option<i64>,
}

/// 今日卡片（IPC §2 `TodayItem`）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TodayItem {
    pub id: String,
    pub title: String,
    pub attention: &'static str,
    pub next_action: String,
    pub why_here: String,
    pub last_evidence_at: Option<i64>,
    pub source_ref: SourceRefDto,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceRefDto {
    pub id: String,
    pub kind: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TodayView {
    pub groups: TodayGroups,
    pub freshness: Freshness,
}

#[derive(Debug, Clone, Serialize)]
pub struct TodayGroups {
    pub mine: Vec<TodayItem>,
    pub waiting: Vec<TodayItem>,
    pub confirm: Vec<TodayItem>,
}

/// `items_query` 入参（IPC §2 `ItemsQuery`）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemsQueryArgs {
    /// all | me | waiting | confirm | closed。
    pub filter: Option<String>,
    pub query: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemSummary {
    pub id: String,
    pub title: String,
    pub lifecycle: String,
    pub attention: String,
    pub next_action: Option<String>,
    pub source_label: Option<String>,
    pub last_evidence_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub confirmation: String,
    pub historical: bool,
    pub project: Option<String>,
    pub awaiting_confirmation: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemPage {
    pub items: Vec<ItemSummary>,
    pub total: i64,
    pub freshness: Freshness,
}

/// `item_get` 出参（IPC §2 `ItemDetail`）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemDetail {
    pub item: ItemDetailCore,
    pub delivery: Option<DeliveryDetail>,
    pub timeline: Vec<EventDto>,
    pub evidence: Vec<EvidenceDto>,
    pub corrections: Vec<CorrectionDto>,
    pub source_freshness: Vec<SourceFreshnessDto>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemDetailCore {
    pub id: String,
    pub title: String,
    pub confirmation: String,
    pub project: Option<String>,
    pub lifecycle: String,
    pub attention: String,
    pub next_action: Option<String>,
    pub followup_type: String,
    pub followup_at: Option<i64>,
    pub followup_note: Option<String>,
    pub thread_id: Option<String>,
    pub confidence: Option<i64>,
    pub awaiting_confirmation: bool,
    pub closed_at: Option<i64>,
    pub close_reason: Option<String>,
    pub resolution_evidence: Option<String>,
    pub last_evidence_at: Option<i64>,
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub locked_fields: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventDto {
    pub id: String,
    pub event_type: String,
    pub actor: String,
    pub at: i64,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceDto {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub occurred_at: i64,
    pub sender_name: Option<String>,
    pub summary: Option<String>,
    pub raw_state: String,
    pub link_type: String,
    pub state: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CorrectionDto {
    pub at: i64,
    pub actor: String,
    pub reason: String,
    pub note: Option<String>,
    pub fields: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceFreshnessDto {
    pub source_id: String,
    pub last_success_at: Option<i64>,
}

/// `item_correct` 入参（IPC §2 `CorrectionArgs`）。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CorrectionArgs {
    pub id: String,
    pub patch: CorrectionPatch,
    /// not_mine | closed | plain_info | wrong_link | other。
    pub reason: String,
    pub note: Option<String>,
}

/// `null` 与「字段缺席」语义不同（threadId: null = 解除关联）。
fn deserialize_double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: serde::de::DeserializeOwned,
    D: serde::de::Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CorrectionPatch {
    pub belongs_to_me: Option<bool>,
    pub lifecycle: Option<String>,
    pub attention: Option<String>,
    pub next_action: Option<String>,
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub thread_id: Option<Option<String>>,
}

/// `item_close` 入参（IPC §2 `CloseArgs`）。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseArgs {
    pub id: String,
    pub evidence_type: String,
    pub note: Option<String>,
}

/// `item_set_followup` 入参（IPC §2 `SetFollowupArgs`）。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetFollowupArgs {
    pub id: String,
    /// none | event | deadline | manual_time（serde 小写口径即 wire 口径）。
    #[serde(rename = "type")]
    pub followup_type: String,
    pub at: Option<i64>,
    pub note: Option<String>,
}

// ========================= today_list =========================

/// `today_list`（IPC：入参可省略；now 由调用方注入）。
pub fn today_list(conn: &Connection, now_ms: i64) -> Result<TodayView, ItemsError> {
    today_list_for_profile(conn, now_ms, None)
}

// A reserved flow identity never changes owners, even when its first move is undone.
const VISIBLE_ITEM: &str = "NOT EXISTS (SELECT 1 FROM ownership_card f WHERE f.id = work_item.id AND f.profile IS NOT ?1)";

pub fn ensure_item_visible(conn: &Connection, id: &str, profile: Option<&str>) -> Result<(), ItemsError> {
    conn.query_row(
        &format!("SELECT 1 FROM work_item WHERE id = ?2 AND {VISIBLE_ITEM}"),
        params![profile, id], |_| Ok(()),
    ).optional()?.ok_or(ItemsError::NotFound)
}

pub fn today_list_for_profile(conn: &Connection, now_ms: i64, profile: Option<&str>) -> Result<TodayView, ItemsError> {
    let freshness = freshness_of(conn, now_ms)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {ITEM_COLUMNS} FROM work_item WHERE lifecycle IN ('active','candidate') AND {VISIBLE_ITEM}"
    ))?;
    let rows: Vec<ItemRow> = stmt
        .query_map([profile], row_to_item)?
        .collect::<rusqlite::Result<_>>()?;
    let mut mine: Vec<TodayItem> = Vec::new();
    let mut waiting: Vec<(Option<i64>, TodayItem)> = Vec::new();
    let mut confirm: Vec<TodayItem> = Vec::new();

    for row in rows {
        // 缺可执行下一步的事项不出卡片（§11.3：nextAction 必填非空），
        // 不生成占位动作；它仍在 items_query / item_get 里可见、可补。
        let Some(next_action) = row.next_action.as_deref().map(str::trim).filter(|a| !a.is_empty()) else {
            continue;
        };
        let group = workcontext_core::today_group(
            row.lifecycle,
            row.attention,
            row.awaiting_confirmation,
        );
        let attention = row.attention;
        let followup_at = row.followup_at;
        let card = TodayItem {
            why_here: why_here(conn, &row),
            id: row.id.clone(),
            title: row.title.clone(),
            attention: attention.as_str(),
            next_action: next_action.to_string(),
            last_evidence_at: row.last_evidence_at,
            source_ref: source_ref_of(conn, row.origin_source_record_id.as_deref()),
        };
        match group {
            workcontext_core::TodayGroup::Me => mine.push(card),
            workcontext_core::TodayGroup::WaitingOther => waiting.push((followup_at, card)),
            workcontext_core::TodayGroup::Confirm => confirm.push(card),
            workcontext_core::TodayGroup::Hidden => {}
        }
    }
    // 排序：mine/confirm 按最近证据倒序（§4.5）；waiting 到期跟进优先
    // （IFNULL(followup_at, +inf) 升序），其后按最近证据倒序。
    mine.sort_by(|a, b| b.last_evidence_at.cmp(&a.last_evidence_at).then_with(|| a.id.cmp(&b.id)));
    waiting.sort_by(|a, b| {
        let key =
            |w: &(Option<i64>, TodayItem)| (w.0.unwrap_or(i64::MAX), -(w.1.last_evidence_at.unwrap_or(0)));
        key(a).cmp(&key(b))
    });
    confirm.sort_by(|a, b| b.last_evidence_at.cmp(&a.last_evidence_at).then_with(|| a.id.cmp(&b.id)));
    let waiting: Vec<TodayItem> = waiting.into_iter().map(|(_, card)| card).collect();

    Ok(TodayView {
        groups: TodayGroups { mine, waiting, confirm },
        freshness,
    })
}

/// `whyHere`（≤80 字）：只取证据链上最近一条证据的记录摘要原文；没有摘要
/// 即为空串（UI 显示「未提供判读理由」）。不按注意力状态生成套话。
fn why_here(conn: &Connection, item: &ItemRow) -> String {
    let summary: Option<String> = conn
        .query_row(
            "SELECT sr.summary FROM link_evidence le \
             JOIN source_record sr ON sr.id = le.source_record_id \
             WHERE le.target_type = 'work_item' AND le.target_id = ?1 \
             AND le.link_type IN ('origin','reference') AND le.state <> 'rejected' \
             ORDER BY sr.occurred_at DESC LIMIT 1",
            params![item.id],
            |row| row.get(0),
        )
        .optional()
        .unwrap_or(None)
        .or_else(|| {
            conn.query_row(
                "SELECT summary FROM source_record WHERE id = ?1",
                params![item.origin_source_record_id],
                |row| row.get(0),
            )
            .optional()
            .unwrap_or(None)
        })
        .filter(|s: &String| !s.trim().is_empty());
    summary.map(|s| s.chars().take(80).collect()).unwrap_or_default()
}

fn source_ref_of(conn: &Connection, record_id: Option<&str>) -> SourceRefDto {
    let fallback = SourceRefDto {
        id: String::new(),
        kind: "manual_note".to_string(),
        label: "本地记录".to_string(),
    };
    let Some(record_id) = record_id else { return fallback };
    let row = conn
        .query_row(
            "SELECT sr.id, sr.kind, sr.sender_name, s.display_name \
             FROM source_record sr JOIN source s ON s.id = sr.source_id WHERE sr.id = ?1",
            params![record_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional();
    match row {
        Ok(Some((id, kind, sender, source_name))) => {
            let label = match sender {
                Some(s) if !s.trim().is_empty() => format!("{source_name} · {s}"),
                _ => source_name,
            };
            SourceRefDto { id, kind, label }
        }
        _ => fallback,
    }
}

/// 新鲜度（§4.5「附新鲜度」= `MAX(sync_checkpoint.last_success_at)`）：
/// - `unknown`：从未成功同步（未同步 ≠ 离线，UI 据此隐藏「等别人」卡片）；
/// - `expired`：同步已停（任一来源 `degraded`/`auth_required`）或最后成功 ≥48h；
/// - `stale`：最后成功 ≥6h；其余 `fresh`。
fn freshness_of(conn: &Connection, now_ms: i64) -> Result<Freshness, ItemsError> {
    let (last, ws, we): (Option<i64>, Option<i64>, Option<i64>) = conn.query_row(
        "SELECT MAX(last_success_at), MIN(window_start), MAX(window_end) \
         FROM sync_checkpoint WHERE last_success_at IS NOT NULL",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let stopped: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sync_checkpoint WHERE state IN ('degraded','auth_required')",
        [],
        |row| row.get(0),
    )?;
    let state = match last {
        None => "unknown",
        Some(ls) => {
            let age = now_ms.saturating_sub(ls).max(0);
            if stopped > 0 || age >= 48 * 3_600_000 {
                "expired"
            } else if age >= 6 * 3_600_000 {
                "stale"
            } else {
                "fresh"
            }
        }
    };
    Ok(Freshness { state, last_success_at: last, window_start: ws, window_end: we })
}

// ========================= items_query =========================

/// `items_query`（IPC §2）。`query` 由本函数做 FTS/LIKE 转义，调用方原样传入。
pub fn items_query(conn: &Connection, args: &ItemsQueryArgs) -> Result<ItemPage, ItemsError> {
    items_query_for_profile(conn, args, None)
}

pub fn items_query_for_profile(conn: &Connection, args: &ItemsQueryArgs, profile: Option<&str>) -> Result<ItemPage, ItemsError> {
    catalog::query(conn, args, profile)
}

fn unix_now(conn: &Connection) -> Result<i64, ItemsError> {
    Ok(conn.query_row(
        "SELECT CAST(strftime('%s','now') AS INTEGER) * 1000",
        [],
        |row| row.get(0),
    )?)
}

// ========================= item_get =========================

/// `item_get`（IPC §2）：时间线 + 证据链 + 纠错史 + 来源新鲜度。
pub fn item_get(conn: &Connection, id: &str) -> Result<ItemDetail, ItemsError> {
    let profile: Option<String> = conn.query_row(
        "SELECT f.profile FROM work_item w LEFT JOIN ownership_card f ON f.id=w.id WHERE w.id=?1",
        [id], |r| r.get(0),
    ).optional()?.ok_or(ItemsError::NotFound)?;
    catalog::get(conn, id, profile.as_deref())
}

fn formal_detail(conn: &Connection, id: &str, confirmation: String, project: Option<String>) -> Result<ItemDetail, ItemsError> {
    let row = load_item(conn, id)?.ok_or(ItemsError::NotFound)?;
    let locks = load_locks(conn, id)?;

    let mut timeline_stmt = conn.prepare(
        "SELECT id, event_type, actor, at, note FROM work_event \
         WHERE target_type = 'work_item' AND target_id = ?1 ORDER BY at DESC, rowid DESC",
    )?;
    let timeline: Vec<EventDto> = timeline_stmt
        .query_map(params![id], |row| {
            Ok(EventDto {
                id: row.get(0)?,
                event_type: row.get(1)?,
                actor: row.get(2)?,
                at: row.get(3)?,
                note: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut evidence_stmt = conn.prepare(
        "SELECT sr.id, sr.kind, sr.sender_name, \
                CASE WHEN sr.source_id <> 'archive-dingtalk' THEN sr.summary \
                     WHEN sr.raw_state = 'available' AND sr.retention_at > unixepoch('subsec') * 1000 \
                     THEN substr(sr.body, 1, 160) END, sr.occurred_at, \
                CASE WHEN sr.source_id = 'archive-dingtalk' AND sr.raw_state = 'available' \
                     AND COALESCE(sr.retention_at, 0) <= unixepoch('subsec') * 1000 \
                     THEN 'redacted' ELSE sr.raw_state END, \
                le.link_type, le.state, s.display_name \
         FROM link_evidence le \
         JOIN source_record sr ON sr.id = le.source_record_id \
         JOIN source s ON s.id = sr.source_id \
         WHERE le.target_type = 'work_item' AND le.target_id = ?1 AND le.state <> 'rejected' \
         ORDER BY sr.occurred_at ASC",
    )?;
    let evidence: Vec<EvidenceDto> = evidence_stmt
        .query_map(params![id], |row| {
            let sender: Option<String> = row.get(2)?;
            let source_name: String = row.get(8)?;
            let label = match sender.as_deref() {
                Some(s) if !s.trim().is_empty() => format!("{source_name} · {s}"),
                _ => source_name,
            };
            Ok(EvidenceDto {
                id: row.get(0)?,
                kind: row.get(1)?,
                label,
                occurred_at: row.get(4)?,
                sender_name: sender,
                summary: row.get(3)?,
                raw_state: row.get(5)?,
                link_type: row.get(6)?,
                state: row.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    // 纠错史：corrected 事件 note=原因枚举，after_json 记 detail 与锁定字段。
    let mut corr_stmt = conn.prepare(
        "SELECT at, actor, note, after_json FROM work_event \
         WHERE target_type = 'work_item' AND target_id = ?1 AND event_type = 'corrected' \
         ORDER BY at DESC, rowid DESC",
    )?;
    let corrections: Vec<CorrectionDto> = corr_stmt
        .query_map(params![id], |row| {
            let at: i64 = row.get(0)?;
            let actor: String = row.get(1)?;
            let reason: Option<String> = row.get(2)?;
            let after: Option<String> = row.get(3)?;
            Ok((at, actor, reason, after))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|(at, actor, reason, after)| {
            let after: Option<serde_json::Value> =
                after.as_deref().and_then(|s| serde_json::from_str(s).ok());
            Some(CorrectionDto {
                at,
                actor,
                reason: reason?,
                note: after
                    .as_ref()
                    .and_then(|a| a.get("detail"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                fields: after
                    .as_ref()
                    .and_then(|a| a.get("fields"))
                    .and_then(serde_json::Value::as_array)
                    .map(|fs| {
                        fs.iter()
                            .filter_map(|f| f.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect();

    // 来源新鲜度：该事项证据涉及的来源各自的最后成功同步。
    let mut fresh_stmt = conn.prepare(
        "SELECT DISTINCT s.id, sc.last_success_at FROM source s \
         JOIN source_record sr ON sr.source_id = s.id \
         JOIN link_evidence le ON le.source_record_id = sr.id \
         LEFT JOIN sync_checkpoint sc ON sc.source_id = s.id \
         WHERE le.target_type = 'work_item' AND le.target_id = ?1",
    )?;
    let source_freshness: Vec<SourceFreshnessDto> = fresh_stmt
        .query_map(params![id], |row| {
            Ok(SourceFreshnessDto { source_id: row.get(0)?, last_success_at: row.get(1)? })
        })?
        .collect::<rusqlite::Result<_>>()?;

    Ok(ItemDetail {
        delivery: None,
        item: ItemDetailCore {
            id: row.id,
            title: row.title,
            confirmation,
            project,
            lifecycle: row.lifecycle.as_str().to_string(),
            attention: row.attention.as_str().to_string(),
            next_action: row.next_action,
            followup_type: row.followup_type.as_str().to_string(),
            followup_at: row.followup_at,
            followup_note: row.followup_note,
            thread_id: row.thread_id,
            confidence: row.confidence,
            awaiting_confirmation: row.awaiting_confirmation,
            closed_at: row.closed_at,
            close_reason: row.close_reason,
            resolution_evidence: row.resolution_evidence,
            last_evidence_at: row.last_evidence_at,
            created_at: Some(row.created_at),
            updated_at: Some(row.updated_at),
            locked_fields: locks.iter().map(|l| l.field.as_str().to_string()).collect(),
        },
        timeline,
        evidence,
        corrections,
        source_freshness,
    })
}

// ========================= 写用例（单事务） =========================

/// `work_event.id`：同一毫秒对同一目标重复操作也不得主键冲突（追加计数）。
pub(crate) fn next_event_id(conn: &Connection, prefix: &str) -> rusqlite::Result<String> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM work_event WHERE id LIKE ?1 || '-%'",
        params![prefix],
        |row| row.get(0),
    )?;
    Ok(format!("{prefix}-{n:04}"))
}

fn insert_event(
    conn: &Connection,
    target_id: &str,
    event_type: &str,
    actor: &str,
    at: i64,
    note: Option<&str>,
    after_json: Option<String>,
) -> rusqlite::Result<()> {
    let id = next_event_id(conn, &format!("we-{at:x}-{event_type}-{target_id}"))?;
    conn.execute(
        "INSERT INTO work_event (id, target_type, target_id, event_type, actor, at, note, after_json) \
         VALUES (?,?,?,?,?,?,?,?)",
        params![id, "work_item", target_id, event_type, actor, at, note, after_json],
    )?;
    Ok(())
}

const CORRECTION_REASONS: &[&str] = &["not_mine", "closed", "plain_info", "wrong_link", "other"];

/// `item_correct`（IPC §2 / 详细设计 §9）：只锁真正表态的字段，人工越过
/// Agent 的字段锁，但生命周期转换仍走 core 守卫。
pub fn item_correct(
    conn: &mut Connection,
    args: &CorrectionArgs,
    now_ms: i64,
) -> Result<ItemDetail, ItemsError> {
    if !CORRECTION_REASONS.contains(&args.reason.as_str()) {
        return Err(ItemsError::InvalidArgument {
            message: format!("未知的纠错原因 “{}”，未保存。", args.reason),
        });
    }
    let patch = &args.patch;

    let mut lifecycle: Option<Lifecycle> = patch.lifecycle.as_deref().and_then(enum_from_str);
    let mut attention: Option<Attention> = patch.attention.as_deref().and_then(enum_from_str);
    if patch.lifecycle.is_some() && lifecycle.is_none() {
        return Err(ItemsError::InvalidArgument {
            message: "lifecycle 取值不合法，未保存。".to_string(),
        });
    }
    if patch.attention.is_some() && attention.is_none() {
        return Err(ItemsError::InvalidArgument {
            message: "attention 取值不合法，未保存。".to_string(),
        });
    }
    // 「不是我的事」归一化（A11 / PRD R06）：UI 只发 `{belongsToMe:false}`；
    // 服务端落 ignored + no_next 并立即移出今日页，锁 belongs_to_me 与 lifecycle。
    // 显式给出与「不是我的事」矛盾的生命周期（仍 active/candidate）即拒绝。
    let not_mine = patch.belongs_to_me == Some(false);
    let mut normalized_no_next = false;
    if not_mine {
        match lifecycle {
            None => lifecycle = Some(Lifecycle::Ignored),
            Some(Lifecycle::Active | Lifecycle::Candidate) => {
                return Err(ItemsError::InvalidArgument {
                    message: "「不是我的事」不能同时保留为待办事项，未保存。".to_string(),
                })
            }
            Some(Lifecycle::Ignored | Lifecycle::Closed) => {}
        }
        if attention.is_none() {
            attention = Some(Attention::NoNext);
            normalized_no_next = true;
        }
    }

    let reason_text = match args.note.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(note) => format!("{}：{}", args.reason, note),
        None => args.reason.clone(),
    };
    // 锁集合只含用户真正表态的维度：归一化得到的 no_next 不额外锁 attention。
    let lock_attention = if normalized_no_next { None } else { attention };
    let core_correction = Correction {
        belongs_to_me: patch.belongs_to_me,
        lifecycle,
        thread_id: patch.thread_id.clone(),
        attention: lock_attention,
        reason: reason_text.clone(),
    };
    validate_correction(&core_correction).map_err(|e| ItemsError::InvalidArgument {
        message: format!("纠错未保存：{e}"),
    })?;
    // core 校验已保证至少改动一个语义字段；next_action 属扩展锁，可单独出现。

    let tx = conn.transaction()?;
    let row = crate::items::load_item(&tx, &args.id)?.ok_or(ItemsError::NotFound)?;
    let locks = load_locks(&tx, &args.id)?;

    // 生命周期转换：人工触发，仍受 §3.3 守卫（如 candidate→closed 需证据，此处拒绝）。
    if let Some(target) = core_correction.lifecycle {
        if target != row.lifecycle {
            let req = TransitionRequest {
                from: row.lifecycle,
                to: target,
                actor: workcontext_core::Actor::Human,
                reason: Some(reason_text.as_str()),
                new_source_record_id: None,
                resolution: None,
                anchor_satisfied: true,
                locked_fields: &locks,
            };
            if let TransitionOutcome::Denied(denial) = evaluate_transition(&req) {
                return Err(ItemsError::ValidationRejected {
                    message: format!("状态转换被守卫拒绝（{denial:?}），未保存。"),
                    hint: Some("检查目标状态是否需要证据或新记录。".to_string()),
                });
            }
        }
    }

    let mut locked_fields: Vec<ItemField> = workcontext_core::correction_locks(&core_correction);
    // Unreserved legacy items are visible locally and may only use legacy threads.
    let mut corrected_scope: Option<&str> = None;
    if let Some(Some(tid)) = &core_correction.thread_id {
        let thread = tx.query_row("SELECT profile IS NOT NULL FROM work_thread WHERE id=?1 AND
            (profile IS NULL OR (kind='project' AND profile=(SELECT profile FROM ownership_card WHERE id=?2)))",
            params![tid,args.id], |row| row.get::<_, bool>(0)).optional()?;
        if thread.is_none() {
            return Err(ItemsError::InvalidArgument {
                message: format!("要关联的工作脉络 {tid} 不存在，未保存。"),
            });
        }
        if thread == Some(true) { corrected_scope = Some(tid.as_str()); }
        locked_fields.push(ItemField::ThreadId);
    }

    // 应用字段（被人工锁定的旧值可被人工再次覆盖：人工优先）。
    let mut sets: Vec<(String, String)> = vec![
        ("updated_at".to_string(), now_ms.to_string()),
    ];
    if let Some(target) = core_correction.lifecycle {
        sets.push(("lifecycle".to_string(), format!("'{}'", target.as_str())));
        if target == Lifecycle::Ignored {
            sets.push(("close_reason".to_string(), sql_str(&reason_text)));
            sets.push(("closed_at".to_string(), now_ms.to_string()));
            sets.push(("awaiting_confirmation".to_string(), "0".to_string()));
        }
    }
    if let Some(att) = attention {
        sets.push(("attention".to_string(), format!("'{}'", att.as_str())));
    }
    if let Some(thread) = &core_correction.thread_id {
        let legacy_thread = thread.as_deref().filter(|_| corrected_scope.is_none());
        let v = legacy_thread.map(sql_str).unwrap_or_else(|| "NULL".to_string());
        sets.push(("thread_id".to_string(), v));
        tx.execute("UPDATE ownership_card SET context_scope_id=?3,
            project_override=coalesce((SELECT title FROM work_thread WHERE id=?1 AND profile IS NULL),'')
            WHERE id=?2", params![legacy_thread,args.id,corrected_scope])?;
    }
    if let Some(next_action) = patch.next_action.as_deref() {
        if !next_action.trim().is_empty() {
            sets.push(("next_action".to_string(), sql_str(next_action.trim())));
            locked_fields.push(ItemField::NextAction);
        }
    }
    let assignments = sets
        .iter()
        .map(|(k, v)| format!("{k} = {v}"))
        .collect::<Vec<_>>()
        .join(", ");
    tx.execute(&format!("UPDATE work_item SET {assignments} WHERE id = ?1"), params![args.id])?;

    // 落锁：只写本次真正表态的字段（§9），UPSERT 覆盖旧值。
    for field in &locked_fields {
        let value_json = match field {
            ItemField::BelongsToMe => patch.belongs_to_me.map(|b| b.to_string()),
            ItemField::Lifecycle => core_correction.lifecycle.map(|l| l.to_value_json()),
            ItemField::Attention => core_correction
                .attention
                .map(|a| format!("\"{}\"", a.as_str())),
            ItemField::ThreadId => core_correction
                .thread_id
                .as_ref()
                .map(|t| t.as_deref().map(sql_json_str).unwrap_or_else(|| "null".to_string())),
            ItemField::NextAction => patch
                .next_action
                .as_deref()
                .map(|n| sql_json_str(n.trim())),
            _ => None,
        };
        tx.execute(
            "INSERT INTO field_override (target_type, target_id, field, value_json, locked, set_at, set_by, note) \
             VALUES ('work_item', ?1, ?2, ?3, 1, ?4, 'human', ?5) \
             ON CONFLICT(target_type, target_id, field) DO UPDATE SET \
               value_json = excluded.value_json, locked = 1, set_at = excluded.set_at, \
               note = excluded.note",
            params![args.id, field.as_str(), value_json, now_ms, reason_text],
        )?;
    }

    insert_event(
        &tx,
        &args.id,
        "corrected",
        "human",
        now_ms,
        Some(&args.reason),
        Some(json!({
            "detail": args.note.as_deref().unwrap_or(""),
            "fields": locked_fields.iter().map(|f| f.as_str()).collect::<Vec<_>>(),
        })
        .to_string()),
    )?;
    tx.commit()?;

    item_get(conn, &args.id)
}

/// `item_close`（IPC §2）：人工收尾；`ack_only` / `artifact_created` 按 V05 拒绝。
pub fn item_close(
    conn: &mut Connection,
    args: &CloseArgs,
    now_ms: i64,
) -> Result<ItemDetail, ItemsError> {
    let evidence_type = ResolutionEvidenceType::parse(&args.evidence_type).ok_or_else(|| {
        ItemsError::InvalidArgument {
            message: format!("未知的收尾证据类型 “{}”。", args.evidence_type),
        }
    })?;
    if matches!(close_verdict(evidence_type), CloseVerdict::Forbid) {
        return Err(ItemsError::ValidationRejected {
            message: "「好的」「收到」或「文件已生成」不构成收尾证据，未改变事项状态。".to_string(),
            hint: Some("选择明确的完成证据，或保持等待。".to_string()),
        });
    }

    let tx = conn.transaction()?;
    let row = load_item(&tx, &args.id)?.ok_or(ItemsError::NotFound)?;
    let locks = load_locks(&tx, &args.id)?;
    let note = args.note.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let close_reason = note.map(str::to_string).unwrap_or_else(|| match evidence_type {
        ResolutionEvidenceType::HumanConfirmed => "人工确认收尾".to_string(),
        ResolutionEvidenceType::ArtifactAccepted => "交付物已被接收方确认".to_string(),
        ResolutionEvidenceType::ExplicitComplete => "对方明确表示已完成".to_string(),
        _ => "人工确认收尾".to_string(),
    });
    let evidence = ResolutionEvidence {
        evidence_type,
        evidence_ids: Vec::new(),
        note: Some(close_reason.clone()),
    };
    let req = TransitionRequest {
        from: row.lifecycle,
        to: Lifecycle::Closed,
        actor: workcontext_core::Actor::Human,
        reason: Some(close_reason.as_str()),
        new_source_record_id: None,
        resolution: Some(&evidence),
        anchor_satisfied: true,
        locked_fields: &locks,
    };
    if let TransitionOutcome::Denied(denial) = evaluate_transition(&req) {
        return Err(ItemsError::ValidationRejected {
            message: format!("收尾被守卫拒绝（{denial:?}），未改变事项状态。"),
            hint: Some("该事项当前状态可能不允许直接收尾。".to_string()),
        });
    }

    tx.execute(
        "UPDATE work_item SET lifecycle = 'closed', closed_at = ?2, close_reason = ?3, \
         resolution_evidence = ?4, awaiting_confirmation = 0, updated_at = ?2 WHERE id = ?1",
        params![args.id, now_ms, close_reason, evidence_type.as_str()],
    )?;
    insert_event(&tx, &args.id, "closed", "human", now_ms, Some(&close_reason), None)?;
    tx.commit()?;

    item_get(conn, &args.id)
}

/// `item_set_followup`（IPC §2）：deadline/manual_time 必须带时间，event 必须带说明。
pub fn item_set_followup(
    conn: &mut Connection,
    args: &SetFollowupArgs,
    now_ms: i64,
) -> Result<ItemDetail, ItemsError> {
    let ftype: DbFollowupType = enum_from_str(&args.followup_type).ok_or_else(|| {
        ItemsError::InvalidArgument {
            message: format!("未知的跟进类型 “{}”。", args.followup_type),
        }
    })?;
    let at = args.at.filter(|a| *a > 0);
    let note = args.note.as_deref().map(str::trim).filter(|s| !s.is_empty());
    match ftype {
        DbFollowupType::None => {}
        DbFollowupType::Deadline | DbFollowupType::ManualTime => {
            if at.is_none() {
                return Err(ItemsError::InvalidArgument {
                    message: "人工跟进时间缺少年月日，未保存。".to_string(),
                });
            }
        }
        DbFollowupType::Event => {
            if note.is_none() {
                return Err(ItemsError::InvalidArgument {
                    message: "事件型跟进缺少说明（等什么），未保存。".to_string(),
                });
            }
        }
    }

    let tx = conn.transaction()?;
    load_item(&tx, &args.id)?.ok_or(ItemsError::NotFound)?;
    let note_text = match ftype {
        DbFollowupType::None => "清除人工跟进".to_string(),
        DbFollowupType::Event => format!("设置事件型跟进：{}", note.unwrap_or_default()),
        DbFollowupType::Deadline => "设置截止时间跟进".to_string(),
        DbFollowupType::ManualTime => "设置人工提醒时间".to_string(),
    };
    tx.execute(
        "UPDATE work_item SET followup_type = ?2, followup_at = ?3, followup_note = ?4, \
         updated_at = ?5 WHERE id = ?1",
        params![args.id, ftype.as_str(), at, note, now_ms],
    )?;
    insert_event(&tx, &args.id, "updated", "human", now_ms, Some(&note_text), None)?;
    tx.commit()?;

    item_get(conn, &args.id)
}

// ========================= SQL 文本工具 =========================

/// 单引号字面量（内部单引号翻倍）。
fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// JSON 字符串字面量（含引号）。
fn sql_json_str(s: &str) -> String {
    serde_json::to_string(s).expect("JSON 字符串序列化不会失败")
}
