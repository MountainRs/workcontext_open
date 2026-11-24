use crate::{sources::SourceState, AppState};
use serde::Serialize;
use std::path::Path;
use tauri::State;
use workcontext_app::{context, external_access::{self, AccessError}, skills::{self, Entry, Hit}};

fn profile(root: &Path) -> Result<Option<String>, String> {
    match external_access::workspace_profile(root) {
        Ok(profile) => Ok(Some(profile)),
        Err(AccessError::Disabled | AccessError::Binding) => Ok(external_access::archive_profile(root).ok()),
        Err(err) => Err(err.to_string()),
    }
}

#[derive(Serialize)]
pub(crate) struct Registry {
    profile: Option<String>,
    scopes: Vec<context::Scope>,
    items: Vec<DesktopHit>,
}
#[derive(Serialize)]
struct DesktopHit { #[serde(flatten)] hit: Hit, global: bool }

#[tauri::command]
pub(crate) fn skills_list(state: State<AppState>, source: State<SourceState>) -> Result<Registry, String> {
    let root = state.db_path.parent().ok_or("工作库路径无效")?;
    let _guard = source.config_lock.lock().map_err(|_| "配置锁不可用")?;
    let profile = profile(root)?;
    let db = state.db.lock().map_err(|_| "本地工作库不可用")?;
    let scopes = match &profile { Some(p) => context::scopes(&db, p).map_err(|e| e.to_string())?, None => Vec::new() };
    let hits = skills::find(&db, root, profile.as_deref(), "", None, 0, usize::MAX).map_err(|e| e.to_string())?;
    let items = hits.items.into_iter().map(|hit| DesktopHit { global: hit.global, hit }).collect();
    Ok(Registry { profile, scopes, items })
}

#[tauri::command]
pub(crate) fn skills_register(state: State<AppState>, source: State<SourceState>, profile: Option<String>, entry: Entry) -> Result<(), String> {
    let root = state.db_path.parent().ok_or("工作库路径无效")?;
    let _guard = source.config_lock.lock().map_err(|_| "配置锁不可用")?;
    if profile.is_some() && profile != self::profile(root)? { return Err("账号已变更，请刷新后重新登记".into()); }
    let db = state.db.lock().map_err(|_| "本地工作库不可用")?;
    skills::register(&db, root, profile.as_deref(), entry).map_err(|e| e.to_string())
}
