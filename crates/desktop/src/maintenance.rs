use crate::{export, AppState};
use serde::Serialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use workcontext_store as store;

fn meta() -> store::AppMeta {
    store::AppMeta { app_version: env!("CARGO_PKG_VERSION").to_owned(), platform: std::env::consts::OS.to_owned() }
}
fn app_data(state: &AppState) -> &Path { state.db_path.parent().expect("absolute app data path") }
fn backups(state: &AppState) -> PathBuf { app_data(state).join("backups") }

// Paths from the renderer are untrusted. Only a single backup directory under
// the managed root may be selected. Never follow a symlink outside that root.
fn backup_directory(state: &AppState, selected: &str) -> Result<PathBuf, String> {
    let root = backups(state).canonicalize().map_err(|_| "没有可恢复的备份")?;
    let dir = Path::new(selected).canonicalize().map_err(|_| "找不到所选备份")?;
    if dir.parent() != Some(root.as_path()) || !dir.is_dir() { return Err("备份不在本应用的备份目录内".into()); }
    Ok(dir)
}

fn view(entry: &store::BackupEntry) -> Value {
    json!({"path":entry.dir,"createdAt":entry.manifest.created_at,"sizeBytes":entry.manifest.size_bytes,
        "schemaVersion":entry.manifest.schema_version,"appVersion":entry.manifest.app_version,"sha256Verified":null})
}

#[tauri::command]
pub(crate) async fn backup_now(app: AppHandle) -> Result<Value, String> {
    worker(move || {
        let state = app.state::<AppState>();
        let db = state.db.lock().map_err(|_| "数据库锁不可用")?;
        let root = backups(&state);
        let entry = store::backup_now(&db,&root,store::BackupKind::Manual,store::now_ms(),&meta()).map_err(|e| e.to_string())?;
        store::cleanup_backups(&root,store::DEFAULT_KEEP_ROUTINE).map_err(|e| e.to_string())?;
        Ok(view(&entry))
    }).await
}

#[tauri::command]
pub(crate) async fn backup_list(app: AppHandle) -> Result<Vec<Value>, String> {
    worker(move || {
        let state = app.state::<AppState>();
        Ok(store::backup_list(&backups(&state)).iter().map(view).collect())
    }).await
}

#[derive(Serialize)]
#[serde(tag="status", rename_all="camelCase")]
pub(crate) enum RestoreResult { Cancelled, Restored { restarted: bool, #[serde(rename="preRestoreBackupPath")] pre_restore_backup_path: PathBuf } }

#[tauri::command]
pub(crate) async fn restore_from(app: AppHandle, path: String, confirm_token: String) -> Result<RestoreResult,String> {
    if confirm_token != "恢复" { return Err("请输入「恢复」确认操作".into()); }
    worker(move || {
        let state = app.state::<AppState>();
        let dir = backup_directory(&state,&path)?;
        store::verify_backup(&dir).map_err(|e| e.to_string())?;
        let approved = app.dialog().message(format!("恢复备份 {}？当前数据将先备份，然后替换为所选备份。", dir.display()))
            .title("恢复 WorkContext 本地数据库").kind(MessageDialogKind::Warning).buttons(MessageDialogButtons::OkCancel).blocking_show();
        if !approved { return Ok(RestoreResult::Cancelled); }
        let sources = app.state::<crate::sources::SourceState>();
        let _exclusive = sources.db_gate.write().map_err(|_| "无法暂停来源任务")?;
        crate::organizer::invalidate(&app);
        let mut db = state.db.lock().map_err(|_| "数据库锁不可用")?;
        let out = store::restore_and_reconcile(&mut db, &state.db_path, &backups(&state), &dir, &meta(), |db| {
            crate::sources::reconcile_after_restore(&sources, db)
                .map_err(|_| "备份已恢复，但授权状态尚未核对；请勿启用同步并检查本地配置")?;
            store::retention_sweep(db, store::now_ms()).map_err(|_| "备份已恢复，但过期正文清理失败")?;
            Ok::<(), &str>(())
        }).map_err(|e| e.to_string())??;
        Ok(RestoreResult::Restored{restarted:false, pre_restore_backup_path:out.pre_restore_backup.dir})
    }).await
}

pub(crate) async fn worker<T:Send+'static>(task:impl FnOnce()->Result<T,String> + Send+'static)->Result<T,String>{
    tauri::async_runtime::spawn_blocking(task).await.map_err(|_| "本地维护操作异常终止".to_owned())?
}

#[tauri::command]
pub(crate) async fn diagnostics(app: AppHandle) -> Result<store::Diagnostics,String> {
    worker(move || {
        let state = app.state::<AppState>();
        let db = state.db.lock().map_err(|_| "数据库锁不可用")?;
        Ok(store::collect_diagnostics(&db,&state.db_path,&backups(&state),&meta(),store::now_ms(),7))
    }).await
}

#[tauri::command]
pub(crate) async fn diagnostics_export(app: AppHandle) -> Result<export::ExportResult,String> {
    worker(move || {
        let state = app.state::<AppState>();
        let bytes = {
            let db = state.db.lock().map_err(|_| "数据库锁不可用")?;
            let summary = store::collect_diagnostics(&db,&state.db_path,&backups(&state),&meta(),store::now_ms(),7);
            // 导出包不携带源名称、标识、绝对路径、数据库错误字符串或原始日志。
            let safe = json!({"appVersion":env!("CARGO_PKG_VERSION"),"platform":std::env::consts::OS,
                "generatedAt":summary.storage.generated_at,"schemaVersion":summary.app.schema_version,
                "integrity":summary.db.integrity,"counts":summary.db.counts,"journalMode":summary.storage.journal_mode,
                "judge":{"ok":summary.judge.ok,"rejected":summary.judge.rejected,"failed":summary.judge.failed,"timeout":summary.judge.timeout},"sourceCount":summary.sources.len(),
                "lastBackupAt":summary.backups.last.map(|b| b.created_at)});
            serde_json::to_vec_pretty(&safe).map_err(|_| "诊断摘要序列化失败")?
        };
        export::save(&app,app_data(&state),"workcontext-diagnostics.json","json",&bytes)
    }).await
}
