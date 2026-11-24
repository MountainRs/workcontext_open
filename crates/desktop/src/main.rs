//! Tauri 桌面入口。M1 职责：在真实 `app_data_dir` 建库并在重启后复用，
//! 启动/运行错误对用户可见（对话框 + stderr），不静默吞掉。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod archive;
mod background;
mod context;
mod workspace_read;
mod export;
mod external_access;
mod external_suggestions;
mod items;
mod local_progress;
mod maintenance;
mod organizer;
mod ownership;
mod skills;
mod snapshots;
mod sources;

use std::{fs, path::PathBuf, sync::Mutex};

use tauri::Manager;
use workcontext_store::rusqlite::Connection;

/// 全局状态：持有建库连接与路径，供 commands 查询诊断信息。
pub(crate) struct AppState {
    pub(crate) db: Mutex<Connection>,
    pub(crate) db_path: PathBuf,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DbInfo {
    db_path: String,
    schema_version: u32,
}

/// 前端可调用：确认数据库落在真实数据目录且迁移到位（重启复用的验证入口）。
#[tauri::command]
fn db_info(state: tauri::State<AppState>) -> Result<DbInfo, String> {
    let version = workcontext_store::schema_version(&state.db.lock().unwrap())
        .map_err(|e| format!("读取 schema 版本失败: {e}"))?;
    Ok(DbInfo {
        db_path: state.db_path.display().to_string(),
        schema_version: version,
    })
}

/// 启动失败对用户可见：stderr 日志 + 模态错误框，然后以非零码退出。
fn fatal(app: &tauri::AppHandle, context: &str, error: &dyn std::fmt::Display) {
    let message = format!("{context}: {error}");
    eprintln!("[workcontext] {message}");
    use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
    app.dialog()
        .message(message)
        .title("WorkContext 启动失败")
        .kind(MessageDialogKind::Error)
        .show(|_| std::process::exit(1));
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
}

#[cfg(windows)]
fn apply_caption_color(app: &tauri::AppHandle) {
    use windows::Win32::{
        Foundation::COLORREF,
        Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CAPTION_COLOR},
        UI::{
            Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW},
            WindowsAndMessaging::{SystemParametersInfoW, SPI_GETHIGHCONTRAST},
        },
    };

    let Some(config) = app.config().app.windows.iter().find(|window| window.label == "main") else {
        return;
    };
    let Some(color) = config.background_color else { return };
    let Some(hwnd) = app.get_webview_window(&config.label).and_then(|window| window.hwnd().ok()) else {
        return;
    };
    let mut contrast = HIGHCONTRASTW {
        cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    // SAFETY: SPI_GETHIGHCONTRAST writes one HIGHCONTRASTW of the supplied size.
    let queried = unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            contrast.cbSize,
            Some(std::ptr::from_mut(&mut contrast).cast()),
            Default::default(),
        )
    };
    if queried.is_err() || contrast.dwFlags.contains(HCF_HIGHCONTRASTON) {
        return;
    }
    let caption = COLORREF(u32::from_le_bytes([color.0, color.1, color.2, 0]));
    // SAFETY: DWM reads one COLORREF during the call. Older Windows keeps its system caption.
    let _ = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR,
            std::ptr::from_ref(&caption).cast(),
            std::mem::size_of::<COLORREF>() as u32,
        )
    };
}

fn when_ready<F>(handler: F) -> impl Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool
where
    F: Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool,
{
    move |invoke| {
        // A startup-error dialog may still be open when the WebView sends its first requests.
        if invoke.message.webview_ref().try_state::<AppState>().is_none() {
            invoke.resolver.reject("工作库尚未就绪，请查看启动提示");
            return true;
        }
        handler(invoke)
    }
}

fn main() {
    tauri::Builder::default()
        .manage(archive::ArchiveWorker::default())
        .manage(ownership::OwnershipWorker::default())
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            background::show(app);
        }))
        .plugin(tauri_plugin_window_state::Builder::default()
            .with_state_flags(tauri_plugin_window_state::StateFlags::all() - tauri_plugin_window_state::StateFlags::VISIBLE)
            .build())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let handle = app.handle().clone();
            #[cfg(windows)]
            apply_caption_color(&handle);
            let initialized = (|| -> Result<_, Box<dyn std::error::Error>> {
                let data_dir = app.path().app_data_dir()?;
                fs::create_dir_all(&data_dir)?;
                let db_path = data_dir.join(workcontext_core::DB_FILENAME);
                let meta = workcontext_store::AppMeta {
                    app_version: env!("CARGO_PKG_VERSION").to_owned(),
                    platform: std::env::consts::OS.to_owned(),
                };
                let mut conn = workcontext_store::open_with_backup(&db_path, &data_dir.join("backups"), &meta)?;
                workcontext_store::retention_sweep(&conn, workcontext_store::now_ms())?;
                if let Err(error) = ownership::index_current_report(&mut conn, &data_dir) {
                    eprintln!("[workcontext] 会话报告未入库：{error}");
                }
                conn.execute(
                    "INSERT INTO app_release(version, platform, first_seen_at)
                     VALUES (?1, ?2, CAST(unixepoch('subsec')*1000 AS INTEGER))
                     ON CONFLICT(version) DO NOTHING",
                    (env!("CARGO_PKG_VERSION"), std::env::consts::OS),
                )?;
                Ok((conn, db_path))
            })();
            let (conn, db_path) = match initialized {
                Ok(pair) => pair,
                Err(error) => {
                    fatal(&handle, "初始化本地数据库失败", &error);
                    return Ok(());
                }
            };
            eprintln!(
                "[workcontext] database ready at {} (schema {})",
                db_path.display(),
                workcontext_store::schema_version(&conn).unwrap_or(0)
            );
            let source_state = sources::init(&handle)?;
            app.manage(source_state);
            app.manage(AppState {
                db: Mutex::new(conn),
                db_path,
            });
            background::install(&handle)?;
            archive::start_capture_scheduler(handle.clone());
            organizer::start(handle.clone());
            ownership::auto::start_scheduler(&handle);
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" && window.app_handle().tray_by_id("workcontext").is_some() && window.hide().is_ok() {
                    api.prevent_close();
                }
            }
        })
        .invoke_handler(when_ready(tauri::generate_handler![
            db_info,
            skills::skills_list, skills::skills_register,
            context::context_local_available,
            context::context_scopes, context::context_scope_save, context::context_scope_items,
            workspace_read::workspace_overview, workspace_read::workspace_search,
            workspace_read::workspace_archive_search, workspace_read::workspace_source,
            context::context_records, context::context_record_get, context::context_record_add,
            context::context_record_correct,
            context::context_item_scope_set,
            external_access::external_access_status, external_access::external_access_set,
            external_suggestions::external_suggestions_list, external_suggestions::external_suggestion_review,
            local_progress::local_progress,
            organizer::organizer_status, organizer::organizer_set_consent, organizer::organizer_run, organizer::organizer_retry,
            archive::archive_sync_now, archive::archive_status, archive::archive_auto_set,
            archive::archive_capture_set,
            ownership::ownership_view, ownership::ownership_refresh, ownership::ownership_set_auto,
            ownership::ownership_move, ownership::ownership_undo, ownership::ownership_set_conclusion, ownership::ownership_set_project,
            sources::archive_judge::archive_judge_now,
            items::today_list, items::items_query, items::item_get, items::item_correct,
            items::item_close, items::item_set_followup,
            snapshots::snapshot_create, snapshots::snapshot_list, snapshots::snapshot_export,
            maintenance::backup_now, maintenance::backup_list, maintenance::restore_from,
            maintenance::diagnostics, maintenance::diagnostics_export,
            sources::source_list, sources::source_register, sources::source_set_enabled,
            sources::sync_status, sources::sync_now, sources::sync_reset,
            sources::judge_now, sources::judge_retry, sources::agent_health,
            sources::egress_get, sources::egress_set,
            sources::runtime_config_get, sources::runtime_configure,
        ]))
        .build(tauri::generate_context!())
        .expect("error while building WorkContext")
        .run(|app, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                organizer::stop(app);
                archive::stop(app);
            }
        });
}
