use serde_json::Value;
use tauri::State;
use workcontext_app::{external_archive, external_read};
use crate::{context::profile, AppState};

#[tauri::command]
pub(crate) fn workspace_overview(state: State<AppState>, local_only: Option<bool>) -> Result<Value,String> {
    let db = state.db.lock().map_err(|_| "本地工作库不可用")?;
    external_read::desktop_overview(&db,&profile(&state,local_only)?).map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn workspace_search(state: State<AppState>, query: String, cursor: Option<String>, local_only: Option<bool>) -> Result<Value,String> {
    let db = state.db.lock().map_err(|_| "本地工作库不可用")?;
    external_read::desktop_search(&db,&profile(&state,local_only)?,&query,cursor).map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn workspace_archive_search(state: State<AppState>, query: String, cursor: Option<String>) -> Result<Value,String> {
    if query.trim().is_empty() { return Err("请输入检索词".into()); }
    let root = state.db_path.parent().ok_or("工作库路径无效")?;
    let db = state.db.lock().map_err(|_| "本地工作库不可用")?;
    let account = crate::ownership::bound_profile(root)?;
    let args = external_archive::Search {keyword:Some(query), cursor, limit:20, ..Default::default()};
    external_archive::desktop_search(&db,root,&account,&args,crate::sources::now_ms()).map_err(archive_error)
}

#[tauri::command]
pub(crate) fn workspace_source(state: State<AppState>, reference: String) -> Result<Value,String> {
    let root = state.db_path.parent().ok_or("工作库路径无效")?;
    let db = state.db.lock().map_err(|_| "本地工作库不可用")?;
    let account = crate::ownership::bound_profile(root)?;
    external_archive::desktop_source(&db,root,&account,&reference,crate::sources::now_ms()).map_err(archive_error)
}

fn archive_error(error: external_archive::ArchiveError) -> String {
    match error {
        external_archive::ArchiveError::Denied => "当前账号没有可读取的归档范围，请检查数据来源授权。",
        external_archive::ArchiveError::Unavailable => "归档库不可用，请在数据来源检查同步状态。",
        external_archive::ArchiveError::Invalid => "检索词、分页或来源引用无效。",
    }.into()
}
