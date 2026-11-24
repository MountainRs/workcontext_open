//! Desktop-only commands for manual records and confirmed scopes.
use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
use workcontext_app::{context as app, items::{self, ItemsError}, ownership_flow};
use workcontext_store::rusqlite::Connection;

use crate::{maintenance::worker, AppState};

const LOCAL_PROFILE: &str = "@local";

pub(super) fn profile(state: &AppState, local_only: Option<bool>) -> Result<String, String> {
    if local_only == Some(true) { return Ok(LOCAL_PROFILE.into()); }
    Ok(bound_profile(state).unwrap_or_else(|_| LOCAL_PROFILE.into()))
}

fn bound_profile(state: &AppState) -> Result<String, String> {
    let root = state.db_path.parent().ok_or("工作库路径无效")?;
    crate::ownership::bound_profile(root).map_err(str::to_owned)
}

fn run<T>(state: &AppState, local_only: Option<bool>, action: impl FnOnce(&mut Connection, &str) -> Result<T, ItemsError>) -> Result<T, String> {
    let mut db = state.db.lock().map_err(|_| "本地工作库不可用")?;
    action(&mut db, &profile(state, local_only)?).map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn context_local_available(state: State<AppState>) -> Result<bool, String> {
    if profile(&state, None)? == LOCAL_PROFILE { return Ok(false); }
    let db = state.db.lock().map_err(|_| "本地工作库不可用")?;
    db.query_row("SELECT EXISTS(SELECT 1 FROM context_record WHERE profile=?1) OR EXISTS(SELECT 1 FROM work_thread WHERE profile=?1)",
        [LOCAL_PROFILE], |r| r.get(0)).map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn context_scopes(state: State<AppState>, local_only: Option<bool>) -> Result<Vec<app::Scope>, String> {
    run(&state, local_only, |db, account| app::scopes(db, account))
}

pub(crate) fn scope_diff(old: Option<&app::Scope>, title: &str, goal: &str, background: &str) -> String {
    let prior = old.map(|s| format!("范围 ID：{}，版本 {}\n名称：{:?}\n目标：{:?}\n背景：{:?}", s.id, s.version, s.title, s.goal, s.background))
        .unwrap_or_else(|| "尚未建档".into());
    format!("当前值\n{prior}\n\n拟议值\n名称：{title:?}\n目标：{goal:?}\n背景：{background:?}")
}

#[tauri::command]
pub(crate) async fn context_scope_save(app: AppHandle, args: app::SaveScopeArgs, local_only: Option<bool>) -> Result<app::Scope, String> {
    worker(move || {
        let state = app.state::<AppState>();
        let account = profile(&state, local_only)?;
        let kind = match args.kind.as_str() {
            "project" => "项目",
            "domain" => "领域",
            _ => return Err("范围类型无效".into()),
        };
        let action = if args.id.is_some() { "修改" } else { "新建" };
        let old = if let Some(id) = &args.id {
            let db = state.db.lock().map_err(|_| "本地工作库不可用")?;
            let scope = app::require_scope(&db, &account, id).map_err(|e| e.to_string())?;
            if Some(scope.version) != args.version || scope.kind != args.kind { return Err("范围已变更，请刷新后重试".into()); }
            Some(scope)
        } else { None };
        let diff = scope_diff(old.as_ref(), args.title.trim(), &args.goal, &args.background);
        let message = format!("工作库：{}\n账号：{account}\n来源：桌面手动编辑\n\n{diff}", state.db_path.display());
        let approved = app.dialog().message(message)
            .title(format!("确认{action}{kind}"))
            .buttons(MessageDialogButtons::OkCancel).blocking_show();
        if !approved { return Err("已取消，本次更改未保存".into()); }
        let mut db = state.db.lock().map_err(|_| "本地工作库不可用")?;
        if profile(&state, local_only)? != account { return Err("归档账号已切换，请重新核对".into()); }
        if let Some(previous) = &old {
            let current = app::require_scope(&db, &account, &previous.id).map_err(|e| e.to_string())?;
            if current != *previous { return Err("范围已变更，请重新核对".into()); }
        }
        app::save_scope(&mut db, &account, &args).map_err(|e| e.to_string())
    }).await
}

#[tauri::command]
pub(crate) fn context_records(state: State<AppState>, args: app::RecordsArgs, local_only: Option<bool>) -> Result<app::RecordPage, String> {
    run(&state, local_only, |db, account| app::records(db, account, &args))
}

#[tauri::command]
pub(crate) fn context_record_add(state: State<AppState>, args: app::AddRecordArgs, local_only: Option<bool>) -> Result<app::RecordDetail, String> {
    run(&state, local_only, |db, account| app::add_record(db, account, &args))
}

#[tauri::command]
pub(crate) fn context_record_get(state: State<AppState>, id: String, local_only: Option<bool>, include_source: Option<bool>) -> Result<app::RecordDetail, String> {
    run(&state, local_only, |db, account| if include_source == Some(true) { app::get_record_with_source(db, account, &id) } else { app::get_record(db, account, &id) })
}

#[tauri::command]
pub(crate) fn context_record_correct(state: State<AppState>, args: app::CorrectRecordArgs, local_only: Option<bool>) -> Result<app::RecordDetail, String> {
    run(&state, local_only, |db, account| app::correct_record(db, account, &args))
}

#[tauri::command]
pub(crate) fn context_scope_items(state: State<AppState>, id: String, local_only: Option<bool>) -> Result<Vec<items::ScopeItem>, String> {
    run(&state, local_only, |db, account| items::context_scope_items(db, account, &id))
}

#[tauri::command]
pub(crate) fn context_item_scope_set(state: State<AppState>, id: String, version: String, scope_id: Option<String>) -> Result<(), String> {
    set_item_scope(&state, &id, &version, scope_id.as_deref())
}

fn set_item_scope(state: &AppState, id: &str, version: &str, scope_id: Option<&str>) -> Result<(), String> {
    let mut db = state.db.lock().map_err(|_| "本地工作库不可用")?;
    let account = bound_profile(state)?;
    ownership_flow::context_item_scope_set(&mut db, &account, id, version, scope_id).map_err(|e| e.to_string())
}

#[cfg(test)]
#[path = "context_binding_tests.rs"]
mod binding_tests;
