//! 迭代池快照 IPC 命令（ui/IPC.md §3 `snapshot_create` / `snapshot_list` /
//! `snapshot_export`）。诊断导出（`diagnostics_export`）归维护模块，不在此处。
//!
//! 约定：读写在主线程命令内短持数据库锁完成；导出先在锁内生成内容、
//! **释放锁之后**才把原生保存对话框与写盘交给后台线程
//! （[`crate::export::save`] 要求 worker 线程 + 不持库锁）。
//! 取消保存不是错误，按契约返回 `cancelled`。
//! 命令以中文字符串拒绝时，UI 归一化为 `IPC_ERROR` 并显示原文。

use crate::AppState;
use workcontext_app::snapshots as svc;

/// `snapshot_create` 出参（契约 §3：`{ id, title, createdAt }`）。
/// Tauri `generate_handler!` 要求命令返回类型至少 crate 可见。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SnapshotCreated {
    id: String,
    title: String,
    created_at: i64,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn err(e: svc::SnapshotError) -> String {
    match e {
        svc::SnapshotError::InvalidArgument(m) | svc::SnapshotError::NotFound(m) => m,
        other => other.to_string(),
    }
}

/// 取数据库连接。锁中毒（此前有命令持锁时 panic）不 panic、也不静默复用
/// 可能处于半写状态的连接，而是作为可显示的错误返回。
fn db<'a>(
    state: &'a tauri::State<'a, AppState>,
) -> Result<std::sync::MutexGuard<'a, workcontext_store::rusqlite::Connection>, String> {
    state
        .db
        .lock()
        .map_err(|_| "本地数据库连接暂不可用，请重启应用后重试".to_string())
}

/// 记录问题快照：标题由描述首行生成，默认 `redaction="masked"`，
/// 不接收也不保存整段聊天正文；目标是事项时自动附带证据链引用。
// 参数与 ui/IPC.md §3 的平铺入参一一对应（UI 实发 `{ ...args }`，不嵌套），
// 无法打包成结构体，属 lint 的合理例外。
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub fn snapshot_create(
    state: tauri::State<'_, AppState>,
    route: String,
    target_type: Option<String>,
    target_id: Option<String>,
    description: String,
    expected: Option<String>,
    kind: Option<String>,
    severity: Option<String>,
) -> Result<SnapshotCreated, String> {
    let kind = match kind.as_deref() {
        None | Some("") => svc::SnapshotKind::Other,
        Some(s) => svc::SnapshotKind::parse(s).ok_or_else(|| format!("未知的问题类型：{s}"))?,
    };
    let severity = match severity.as_deref() {
        None | Some("") => None,
        Some(s) => Some(svc::Severity::parse(s).ok_or_else(|| format!("未知的严重度：{s}"))?),
    };
    let conn = db(&state)?;
    let row = svc::create(
        &conn,
        &svc::NewSnapshot {
            route: Some(route),
            target_type,
            target_id,
            description,
            expected,
            kind,
            severity,
            record_refs: Vec::new(),
        },
        &svc::CreateContext {
            now_ms: now_ms(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
        },
    )
    .map_err(err)?;
    Ok(SnapshotCreated {
        id: row.id,
        title: row.title,
        created_at: row.created_at,
    })
}

/// 迭代池列表；`status` 可选过滤（open/resolved/wontfix）。
#[tauri::command]
pub fn snapshot_list(
    state: tauri::State<'_, AppState>,
    status: Option<String>,
) -> Result<Vec<svc::SnapshotRow>, String> {
    let status = match status.as_deref() {
        None | Some("") => None,
        Some(s) => Some(svc::SnapshotStatus::parse(s).ok_or_else(|| format!("未知的快照状态：{s}"))?),
    };
    let conn = db(&state)?;
    svc::list(&conn, status).map_err(err)
}

/// 导出快照（MD/JSON）：内容在锁内生成并整体脱敏；正文仅在
/// `includeBody=true`（UI 明确勾选）时以脱敏形式附带；
/// 保存对话框与写盘在后台线程完成，取消返回 `cancelled`。
#[tauri::command]
pub async fn snapshot_export(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    ids: Vec<String>,
    format: String,
    include_body: bool,
) -> Result<crate::export::ExportResult, String> {
    let format =
        svc::ExportFormat::parse(&format).ok_or_else(|| "format 只支持 md 或 json".to_string())?;
    let include_body = if include_body {
        svc::IncludeBody::Explicit
    } else {
        svc::IncludeBody::No
    };
    let file = {
        // 锁只覆盖只读查询，生成内容后立即释放。
        let conn = db(&state)?;
        svc::export(&conn, &ids, format, include_body, now_ms()).map_err(err)?
    };
    let app_data = state
        .db_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| "无法定位应用数据目录".to_string())?;
    save_in_background(app, app_data, file.file_name, format.extension(), file.content.into_bytes())
        .await
}

/// 后台线程弹原生保存对话框并持久化（`export::save`：worker 线程专用、
/// 不得持数据库锁；此处调用时锁已释放）。
async fn save_in_background(
    app: tauri::AppHandle,
    app_data: std::path::PathBuf,
    filename: String,
    ext: &'static str,
    bytes: Vec<u8>,
) -> Result<crate::export::ExportResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        crate::export::save(&app, &app_data, &filename, ext, &bytes)
    })
    .await
    .map_err(|e| format!("导出任务执行失败：{e}"))?
}
