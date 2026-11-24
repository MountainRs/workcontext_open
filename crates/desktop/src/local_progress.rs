use serde::Serialize;
use tauri::Manager;
use workcontext_store::rusqlite::{named_params, Connection, OpenFlags, OptionalExtension};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Evidence {
    conversation: String,
    request: String,
    requested_at: i64,
    replied_at: Option<i64>,
    reply: Option<String>,
    files: u32,
    extracted: u32,
    outgoing: bool,
    attachment_excerpt: Option<String>,
    cid: String,
    mid: String,
    reply_mid: Option<String>,
}

#[derive(Serialize)]
struct Document {
    name: String,
    observation: String,
    units: u32,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    synced_at: Option<i64>,
    evidence: Vec<Evidence>,
    document_status: Option<String>,
    documents: Vec<Document>,
}

#[tauri::command]
pub async fn local_progress(app: tauri::AppHandle) -> Result<Progress, String> {
    let path = app.path().app_data_dir().map_err(|e| e.to_string())?.join("archive/archive.sqlite3");
    if !path.try_exists().map_err(|e| e.to_string())? { return Ok(Progress::default()); }
    tauri::async_runtime::spawn_blocking(move || -> Result<Progress, String> {
        let mut db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| e.to_string())?;
        let tx = db.transaction().map_err(|e| e.to_string())?;
        let state = |key: &str| tx.query_row("SELECT value FROM archive_state WHERE key=?1", [key], |r| r.get::<_,String>(0)).optional().map_err(|e| e.to_string());
        let stamp = match state("messages_ready_ms")? {
            Some(stamp) => Some(stamp),
            None => state("last_success_ms")?,
        };
        let Some(stamp) = stamp else { return Ok(Progress::default()); };
        let until: i64 = stamp.parse().map_err(|_| "归档水位无效")?;
        let profile = state("profile")?.ok_or("归档缺少账号绑定")?;
        let (_, me) = profile.split_once(':').ok_or("归档账号无效")?;
        if state("self_open_id")?.filter(|id| !id.is_empty()).is_none() {
            return Err("缺少 DWS 本人身份映射，不能判定消息归属。请重新同步。".into());
        }
        let now = workcontext_store::now_ms();
        let since = now - 7 * 86_400_000;
        let mut query = tx.prepare(include_str!("local_progress.sql")).map_err(|e| e.to_string())?;
        let evidence = query.query_map(named_params!{":since":since, ":until":until.min(now), ":me":me}, |r| Ok(Evidence {
            conversation:r.get(0)?, request:r.get(1)?, requested_at:r.get(2)?, replied_at:r.get(3)?, reply:r.get(4)?, files:r.get(5)?, extracted:r.get(6)?,
            outgoing:r.get(7)?, attachment_excerpt:r.get(8)?,
            cid:r.get(9)?, mid:r.get(10)?, reply_mid:r.get(11)?,
        })).map_err(|e| e.to_string())?.collect::<Result<Vec<_>,_>>().map_err(|e| e.to_string())?;
        let document_status = state("document_status")?;
        let mut documents = Vec::new();
        if state("documents_ready_ms")?.is_some() {
            let mut q = tx.prepare("SELECT name,observation,units FROM recent_document_evidence WHERE modified_ms BETWEEN ?1 AND ?2 ORDER BY modified_ms DESC").map_err(|e| e.to_string())?;
            documents = q.query_map([since, now], |r| Ok(Document { name:r.get(0)?, observation:r.get(1)?, units:r.get(2)? }))
                .map_err(|e| e.to_string())?.collect::<Result<Vec<_>,_>>().map_err(|e| e.to_string())?;
        }
        Ok(Progress { synced_at: Some(until), evidence, document_status, documents })
    }).await.map_err(|e| e.to_string())?
}
