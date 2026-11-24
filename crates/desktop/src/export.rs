use serde::Serialize;
use std::{fs, io::Write, path::Path};
use tauri_plugin_dialog::DialogExt;

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub(crate) enum ExportResult {
    Cancelled,
    Saved { path: String },
}

/// On a worker thread only. Supply already-redacted bytes; hold no database lock here.
pub(crate) fn save(app: &tauri::AppHandle, app_data: &Path, filename: &str, ext: &str, bytes: &[u8]) -> Result<ExportResult, String> {
    let Some(selected) = app.dialog().file().set_file_name(filename).add_filter(ext, &[ext]).blocking_save_file() else {
        return Ok(ExportResult::Cancelled);
    };
    let selected = selected.into_path().map_err(|_| "请选择本地文件")?;
    let parent = selected.parent().and_then(|p| fs::canonicalize(p).ok()).ok_or("导出目录不可访问")?;
    let data_root = fs::canonicalize(app_data).map_err(|_| "无法验证应用数据目录")?;
    if parent.starts_with(data_root) || !selected.extension().is_some_and(|s| s.eq_ignore_ascii_case(ext)) {
        return Err(format!("请选择应用数据目录以外的 .{ext} 文件"));
    }
    let mut temp = tempfile::NamedTempFile::new_in(&parent).map_err(|_| "不能在所选目录创建文件")?;
    temp.write_all(bytes).and_then(|_| temp.as_file().sync_all()).map_err(|_| "写入导出文件失败")?;
    temp.persist(&selected).map_err(|_| "无法保存导出文件")?;
    Ok(ExportResult::Saved { path: selected.to_string_lossy().into_owned() })
}
