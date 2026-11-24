use crate::{context::scope_diff, maintenance::worker, sources::SourceState, AppState};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
use workcontext_app::{external_access, external_suggestions as proposals};

fn account(state: &AppState) -> Result<String, String> {
    let root = state.db_path.parent().ok_or("工作库路径无效")?;
    external_access::workspace_profile(root).map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn external_suggestions_list(state: State<AppState>) -> Result<Vec<proposals::Pending>, String> {
    let db = state.db.lock().map_err(|_| "本地工作库不可用")?;
    proposals::pending(&db, &account(&state)?).map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) async fn external_suggestion_review(app: AppHandle, id: String) -> Result<proposals::Receipt, String> {
    worker(move || {
        let state = app.state::<AppState>();
        let profile = account(&state)?;
        let shown = {
            let db = state.db.lock().map_err(|_| "本地工作库不可用")?;
            proposals::review(&db, &profile, &id).map_err(|e| e.to_string())?
        };
        let approved = if shown.status() == "pending" && !shown.is_stale() {
            let a = shown.proposal();
            let kind = if a.kind == "project" { "项目" } else { "领域" };
            let operation = if a.scope_id.is_some() { "修改" } else { "新建" };
            let diff = scope_diff(shown.original(), &a.title, &a.goal, &a.background);
            let message = format!("工作库：{}\n账号：{profile}\n建议 ID：{id}\n提交者：{:?}\n来源：{:?}\n理由：{:?}\n\n{diff}\n\n确定将保存变更，取消将拒绝本条建议。",
                state.db_path.display(), a.producer, a.source, a.reason);
            app.dialog().message(message).title(format!("确认{operation}{kind}"))
                .buttons(MessageDialogButtons::OkCancel).blocking_show()
        } else { false };
        let settings = app.state::<SourceState>();
        let _binding = settings.config_lock.lock().map_err(|_| "配置锁不可用")?;
        let mut db = state.db.lock().map_err(|_| "本地工作库不可用")?;
        proposals::decide(&mut db, &account(&state)?, shown, approved).map_err(|e| e.to_string())
    }).await
}
