use crate::sources::{self, SourceState};
use serde::Serialize;
use std::{collections::BTreeSet, path::Path};
use tauri::State;
use workcontext_app::external_access::{self as access, Binding, Grant};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccessView {
    profiles: BTreeSet<String>,
    profile: Option<String>,
    enabled: bool,
    read_archive: bool,
    archive_profile: Option<String>,
}
fn root(state: &SourceState) -> Result<&Path, String> {
    state.config_path.parent().ok_or_else(|| "配置路径无效".into())
}
#[tauri::command]
pub(crate) fn external_access_status(state: State<'_, SourceState>) -> Result<AccessView, String> {
    let _guard = state.config_lock.lock().map_err(|_| "配置锁不可用")?;
    let cfg = sources::load_settings(&state).map_err(|e| e.message)?;
    let root = root(&state)?;
    let archive_profile = access::archive_profile(root).ok();
    let mut profiles: BTreeSet<String> = cfg.sources.values().map(|s| &s.profile).filter(|p| access::valid_profile(p)).cloned().collect();
    profiles.extend(archive_profile.iter().cloned());
    let profile = access::authorize(root).ok();
    profiles.extend(profile.iter().cloned());
    let read_archive = profile.as_ref().is_some_and(|p| Some(p) == archive_profile.as_ref()) && cfg.extra.get("externalAccess").and_then(|v| v.get("readArchive")).and_then(|v| v.as_bool()) == Some(true);
    Ok(AccessView { enabled: profile.is_some(), profiles, profile, read_archive, archive_profile })
}
#[tauri::command]
pub(crate) fn external_access_set(state: State<'_, SourceState>, enabled: bool, profile: Option<String>, read_archive: Option<bool>) -> Result<(), String> {
    set(&state, enabled, profile, read_archive.unwrap_or(false))
}
pub(crate) fn set(state: &SourceState, enabled: bool, profile: Option<String>, read_archive: bool) -> Result<(), String> {
    let _guard = state.config_lock.lock().map_err(|_| "配置锁不可用")?;
    let mut cfg = sources::load_settings(state).map_err(|e| e.message)?;
    let mut rebound = false;
    if enabled {
        let root = root(state)?;
        let profile = profile.filter(|p| access::valid_profile(p)).ok_or("请选择有效绑定账号")?;
        let source_id = cfg.sources.iter().find(|(_, entry)| entry.profile == profile).map(|(id, _)| id.clone());
        if (source_id.is_none() || read_archive) && access::archive_profile(root).as_deref() != Ok(profile.as_str()) {
            return Err("归档账号未绑定或已变更".into());
        }
        let workspace = root.canonicalize().map_err(|e| e.to_string())?;
        let binding = Binding { profile: profile.clone(), source_id };
        let next = serde_json::to_value(binding).map_err(|e| e.to_string())?;
        rebound = cfg.extra.get("workspaceAccount") != Some(&next);
        cfg.extra.insert("workspaceAccount".into(), next);
        cfg.extra.insert("externalAccess".into(), serde_json::to_value(Grant { workspace, profile, read_archive }).map_err(|e| e.to_string())?);
    } else {
        cfg.extra.remove("externalAccess");
    }
    sources::config::save(&state.config_path, &cfg).map_err(|e| e.to_string())?;
    if rebound { state.invalidate_organizer(); }
    Ok(())
}
