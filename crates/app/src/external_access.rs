//! Offline policy for the local interface. Consent is outside the restorable work database.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::{Path, PathBuf}};
use workcontext_store::rusqlite::{Connection, OpenFlags};

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum AccessError {
    #[error("本地外部接口已关闭")]
    Disabled,
    #[error("本机工作区或账号绑定已变更，请在桌面重新授权")]
    Binding,
    #[error("未获准读取此账号的原始归档")]
    Archive,
    #[error("本机配置不可用")]
    Config,
}
impl AccessError {
    pub fn code(&self) -> &'static str {
        match self { Self::Disabled => "INTERFACE_DISABLED", Self::Binding => "BINDING_MISMATCH", Self::Archive => "SOURCE_DENIED", Self::Config => "CONFIG_UNAVAILABLE" }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Binding { pub profile: String, pub source_id: Option<String> }
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Grant { pub workspace: PathBuf, pub profile: String, pub read_archive: bool }
#[derive(Deserialize)]
struct Source { profile: String }
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct Settings {
    schema: u32,
    sources: BTreeMap<String, Source>,
    workspace_account: Option<Binding>,
    external_access: Option<Grant>,
}
fn load(root: &Path) -> Result<Settings, AccessError> {
    let bytes = match fs::read(root.join("config.json")) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(AccessError::Disabled),
        Err(_) => return Err(AccessError::Config),
    };
    let cfg: Settings = serde_json::from_slice(&bytes).map_err(|_| AccessError::Config)?;
    if cfg.schema > 1 { return Err(AccessError::Config); }
    Ok(cfg)
}
pub fn valid_profile(profile: &str) -> bool {
    let Some((corp, user)) = profile.split_once(':') else { return false };
    [corp, user].iter().all(|id| !id.is_empty() && id.len() <= 128 && !id.starts_with('-') &&
        id.bytes().all(|b| (0x21..=0x7e).contains(&b) && b != b':'))
}
pub fn archive_profile(root: &Path) -> Result<String, AccessError> {
    let db = Connection::open_with_flags(root.join("archive/archive.sqlite3"), OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|_| AccessError::Archive)?;
    let profile: String = db.query_row("SELECT value FROM archive_state WHERE key='profile'", [], |r| r.get(0)).map_err(|_| AccessError::Archive)?;
    valid_profile(&profile).then_some(profile).ok_or(AccessError::Archive)
}
fn bound_profile(root: &Path, cfg: &Settings) -> Result<String, AccessError> {
    let bound = cfg.workspace_account.as_ref().ok_or(AccessError::Binding)?;
    if !valid_profile(&bound.profile) {
        return Err(AccessError::Binding);
    }
    if let Some(id) = &bound.source_id {
        if !cfg.sources.get(id).is_some_and(|s| s.profile == bound.profile) { return Err(AccessError::Binding); }
    } else if let Ok(current) = archive_profile(root) {
        if current != bound.profile { return Err(AccessError::Binding); }
    }
    Ok(bound.profile.clone())
}
pub fn workspace_profile(root: &Path) -> Result<String, AccessError> {
    bound_profile(root, &load(root)?)
}
fn checked(root: &Path) -> Result<Grant, AccessError> {
    let mut cfg = load(root)?;
    let grant = cfg.external_access.take().ok_or(AccessError::Disabled)?;
    if grant.workspace != root.canonicalize().map_err(|_| AccessError::Binding)? || grant.profile != bound_profile(root, &cfg)? {
        return Err(AccessError::Binding);
    }
    Ok(grant)
}
pub fn authorize(root: &Path) -> Result<String, AccessError> { Ok(checked(root)?.profile) }
pub fn authorize_archive(root: &Path, profile: &str) -> Result<(), AccessError> {
    let grant = checked(root)?;
    if grant.profile == profile && grant.read_archive && archive_profile(root)? == profile { Ok(()) } else { Err(AccessError::Archive) }
}
