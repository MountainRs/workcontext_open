//! Tauri command 层：事项用例（today / item）的 IPC 包装。
//!
//! 只做三件事：取 `AppState` 的连接、注入时钟（`now`）、把
//! [`workcontext_app::items::ItemsError`] 归一为 `ui/IPC.md` §0.2 的
//! `AppError`（camelCase）。业务逻辑全部在 `workcontext_app::items`。
//!
//! 入参形状与 `ui/src/lib/api.ts` 实发一致：`item_get` 收平铺 `{ id }`；
//! `items_query` / `item_correct` / `item_close` / `item_set_followup` 收
//! 嵌套 `{ args: {…} }`（Tauri 按参数名 `args` 取键）。

use workcontext_app::items as app_items;
use workcontext_app::items::{
    CloseArgs, CorrectionArgs, ItemDetail, ItemsError, ItemsQueryArgs, ItemPage, SetFollowupArgs,
    TodayView,
};
use workcontext_store::rusqlite::Connection;

use crate::AppState;

/// `ui/IPC.md` §0.2 错误模型。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl From<ItemsError> for AppError {
    fn from(e: ItemsError) -> Self {
        AppError {
            code: e.code().to_string(),
            message: e.to_string(),
            retryable: e.retryable(),
            hint: e.hint(),
        }
    }
}

pub type IpcResult<T> = Result<T, AppError>;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// 连接锁中毒：不 panic 穿透到 IPC，按 INTERNAL 返回。
fn poisoned() -> AppError {
    AppError {
        code: "INTERNAL".to_string(),
        message: "本地数据库连接状态异常，请重启应用。".to_string(),
        retryable: false,
        hint: None,
    }
}

fn archive_profile(state: &AppState) -> Option<String> {
    crate::ownership::bound_profile(state.db_path.parent()?).ok()
}

fn with_conn<T>(state: &AppState, f: impl FnOnce(&Connection) -> Result<T, ItemsError>) -> IpcResult<T> {
    let conn = state.db.lock().map_err(|_| poisoned())?;
    f(&conn).map_err(AppError::from)
}

fn with_conn_mut<T>(
    state: &AppState,
    f: impl FnOnce(&mut Connection) -> Result<T, ItemsError>,
) -> IpcResult<T> {
    let mut conn = state.db.lock().map_err(|_| poisoned())?;
    f(&mut conn).map_err(AppError::from)
}

/// `today_list`：无入参；now 由本层注入。
#[tauri::command]
pub fn today_list(state: tauri::State<AppState>) -> IpcResult<TodayView> {
    with_conn(&state, |conn| app_items::today_list_for_profile(conn, now_ms(), archive_profile(&state).as_deref()))
}

/// `items_query`：入参 `{ args: ItemsQuery }`。
#[tauri::command]
pub fn items_query(state: tauri::State<AppState>, args: ItemsQueryArgs) -> IpcResult<ItemPage> {
    with_conn(&state, |conn| app_items::items_query_for_profile(conn, &args, archive_profile(&state).as_deref()))
}

/// `item_get`：入参 `{ id }`。
#[tauri::command]
pub fn item_get(state: tauri::State<AppState>, id: String) -> IpcResult<ItemDetail> {
    with_conn(&state, |conn| app_items::item_get_for_profile(conn, &id, archive_profile(&state).as_deref()))
}

/// `item_correct`：入参 `{ args: CorrectionArgs }`。
#[tauri::command]
pub fn item_correct(
    state: tauri::State<AppState>,
    args: CorrectionArgs,
) -> IpcResult<ItemDetail> {
    with_conn_mut(&state, |conn| {
        app_items::ensure_item_visible(conn, &args.id, archive_profile(&state).as_deref())?;
        app_items::item_correct(conn, &args, now_ms())
    })
}

/// `item_close`：入参 `{ args: CloseArgs }`。
#[tauri::command]
pub fn item_close(state: tauri::State<AppState>, args: CloseArgs) -> IpcResult<ItemDetail> {
    with_conn_mut(&state, |conn| {
        app_items::ensure_item_visible(conn, &args.id, archive_profile(&state).as_deref())?;
        app_items::item_close(conn, &args, now_ms())
    })
}

/// `item_set_followup`：入参 `{ args: SetFollowupArgs }`。
#[tauri::command]
pub fn item_set_followup(
    state: tauri::State<AppState>,
    args: SetFollowupArgs,
) -> IpcResult<ItemDetail> {
    with_conn_mut(&state, |conn| {
        app_items::ensure_item_visible(conn, &args.id, archive_profile(&state).as_deref())?;
        app_items::item_set_followup(conn, &args, now_ms())
    })
}
