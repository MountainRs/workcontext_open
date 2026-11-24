use crate::{archive, maintenance::worker, sources::{self, SettingsFile, SourceState}, AppState};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::HashSet, path::{Path, PathBuf}, sync::{atomic::{AtomicBool, AtomicU64, Ordering}, mpsc::{self, SyncSender}, Arc, Mutex}, thread, time::Duration};
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
use workcontext_app::{external_access, organizer as queue, organizer_archive as evidence};
use workcontext_store::rusqlite::{params, Connection};

#[path = "organizer_process.rs"] mod process;
#[path = "process_tree.rs"] pub(crate) mod process_tree;
const MODEL: &str = "workbuddy/deepseek-v4.1-flash";
const KEY: &str = "contextOrganizer";
const CHANGES: usize = 128;
const TICK: Duration = Duration::from_secs(10);
const CONSENT_COPY: &str = "允许工作台在运行期间向下列模型发送此账号待整理的桌面补录、Agent 写入原文，以及授权范围内的消息与附件。直接回复可附上已授权的原消息摘录。没有原生引用时，可附上同会话、同线程十分钟内紧邻的上一条已授权消息摘录，用于辨认当前消息所指事项。匹配项目或工单时会一并发送相关范围名称、记录标题、工单编号和交付内容。关闭后停止后续发送。";

#[derive(Serialize, Deserialize, Clone, Eq, PartialEq)]
struct Consent { workspace: PathBuf, profile: String, model: String, epoch: String, enabled: bool, #[serde(default)] adjacent: bool }

struct Observation { profile: String, checked_at: i64, error: Option<String> }

struct Control {
    wake: SyncSender<()>, stopping: AtomicBool, running: AtomicBool, generation: Arc<AtomicU64>,
    notice: Mutex<Option<Observation>>, thread: Mutex<Option<thread::JoinHandle<()>>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OrganizerStatus {
    profile: Option<String>, consent_profile: Option<String>, model: &'static str,
    enabled: bool, running: bool, blocked: Option<String>, error: Option<String>, queue: Option<queue::Status>,
    last_checked_at: Option<i64>,
    credentials_available: bool,
}

fn root(state: &SourceState) -> Result<&Path, String> { state.config_path.parent().ok_or_else(|| "工作区路径无效".into()) }
fn binding(cfg: &SettingsFile, root: &Path) -> Result<String, String> {
    if cfg.extra.contains_key("workspaceAccount") { external_access::workspace_profile(root).map_err(|e| e.to_string()) }
    else if cfg.extra.contains_key("externalAccess") { Err("外部接口授权缺少绑定账号，请先在设置中重新绑定".into()) }
    else { external_access::archive_profile(root).map_err(|_| "请先绑定工作区账号或完成归档同步".into()) }
}
fn consent(cfg: &SettingsFile) -> Option<Consent> { serde_json::from_value(cfg.extra.get(KEY)?.clone()).ok() }
fn prerequisites(cfg: &SettingsFile) -> Result<&str, String> {
    if cfg.judge.model_id.as_deref() != Some(MODEL) || cfg.egress.policy != "full" || cfg.egress.approved_model.as_deref() != Some(MODEL) || cfg.judge.mode != sources::config::JudgeMode::Cli {
        return Err(format!("请在判读配置中选择 CLI，并批准 {MODEL} 的 full 出域策略。"));
    }
    Ok(MODEL)
}
fn credentials_available() -> bool {
    std::env::var("WORKCONTEXT_REVIEW_API_KEY").is_ok_and(|k| !k.trim().is_empty())
}

fn model_command(script:&Path, available:bool) -> Result<std::process::Command, &'static str> {
    if !available { return Err("credentials_missing"); }
    let mut command = process::interpreter().map_err(|_| "model_unavailable")?;
    command.arg(script);
    Ok(command)
}
fn active(cfg: &SettingsFile, root: &Path) -> Result<Consent, String> {
    let approved = consent(cfg).filter(|c| c.enabled && !c.epoch.is_empty()).ok_or("未开启上下文整理")?;
    if approved.workspace != root.canonicalize().map_err(|e| e.to_string())? || approved.model != prerequisites(cfg)? || approved.profile != external_access::workspace_profile(root).map_err(|e| e.to_string())? {
        return Err("工作区、账号或模型已变更，请重新授权".into());
    }
    Ok(approved)
}

// Same acquisition order as source grant edits. Restore takes the database gate exclusively.
fn access<T>(app: &AppHandle, run: impl FnOnce(&SourceState, &Path, SettingsFile, &mut Connection) -> Result<T,String>) -> Result<T,String> {
    let source = app.state::<SourceState>();
    let state = app.state::<AppState>();
    let _config = source.config_lock.lock().map_err(|_| "配置锁不可用")?;
    let _gate = sources::lock_gate(&source.db_gate);
    let mut db = state.db.lock().map_err(|_| "工作库不可用")?;
    let cfg = sources::load_settings(&source).map_err(|e| e.message)?;
    run(&source, root(&source)?, cfg, &mut db)
}
fn show(e: impl std::fmt::Display) -> String { e.to_string() }

#[tauri::command]
pub(crate) fn organizer_status(app: AppHandle) -> Result<OrganizerStatus,String> {
    access(&app, |_, root, cfg, db| {
        let control = app.state::<Control>();
        let bound = binding(&cfg, root);
        let grant = consent(&cfg).filter(|g| g.enabled);
        let profile = bound.as_ref().ok().cloned();
        let blocked = bound.err().or_else(|| if grant.is_some() { active(&cfg, root).err() } else { prerequisites(&cfg).err() });
        let queue = profile.as_ref().map(|p| queue::status(db,p)).transpose().map_err(show)?;
        let notice = control.notice.lock().map_err(|_| "状态不可用")?;
        let observed = notice.as_ref().filter(|n| Some(&n.profile) == profile.as_ref());
        let error = observed.and_then(|n|n.error.clone());
        let last_checked_at = observed.map(|n|n.checked_at);
        Ok(OrganizerStatus {profile, consent_profile:grant.as_ref().map(|g|g.profile.clone()),model:MODEL,enabled:grant.is_some(),running:control.running.load(Ordering::Acquire),blocked,error,queue,last_checked_at,credentials_available:credentials_available()})
    })
}

#[tauri::command]
pub(crate) async fn organizer_set_consent(app: AppHandle, profile: String, model: String, enabled: bool) -> Result<OrganizerStatus,String> {
    worker(move || {
        let before = access(&app, |_,root,cfg,_| {
            if enabled && (prerequisites(&cfg)? != model || binding(&cfg,root)? != profile) {return Err("账号或模型已变更".into());}
            Ok((root.canonicalize().map_err(show)?, cfg.extra.get(KEY).cloned()))
        })?;
        if enabled {
            if !app.dialog().message(format!("{CONSENT_COPY}\n\n工作区：{}\n账号：{profile}\n模型：{model}\n网关：127.0.0.1:3425", before.0.display()))
                .title("授权自动整理工作上下文").buttons(MessageDialogButtons::OkCancel).blocking_show() { return Err("已取消授权".into()); }
        }
        access(&app, |state,root,mut cfg,db| {
            if root.canonicalize().map_err(show)? != before.0 || cfg.extra.get(KEY) != before.1.as_ref() { return Err("授权状态已变化，请刷新后重试".into()); }
            if enabled {
                if prerequisites(&cfg)? != model || binding(&cfg,root)? != profile { return Err("绑定账号或模型已变更".into()); }
                cfg.extra.entry("workspaceAccount").or_insert_with(||json!({"profile":profile,"sourceId":null}));
                let grant = Consent {workspace:before.0.clone(),profile:profile.clone(),model,epoch:sources::random_id(db).map_err(|e|e.message)?,enabled:true,adjacent:true};
                cfg.extra.insert(KEY.into(),serde_json::to_value(grant).map_err(show)?);
            } else {
                if consent(&cfg).is_some_and(|c| c.profile != profile) {return Err("授权账号已变更，请刷新后重试".into());}
                cfg.extra.remove(KEY);
            }
            sources::config::save(&state.config_path,&cfg).map_err(show)?;
            let control = app.state::<Control>();
            control.generation.fetch_add(1, Ordering::AcqRel);
            let _ = control.wake.try_send(());
            Ok(())
        })?;
        organizer_status(app)
    }).await
}
fn request(app: AppHandle, profile: String, model: String, retry: bool) -> Result<OrganizerStatus,String> {
    access(&app, |_,root,cfg,db| {
        let grant = active(&cfg,root)?;
        if grant.profile != profile || grant.model != model { return Err("账号或模型已变更".into()); }
        if retry { queue::retry(db,&profile,sources::now_ms()).map_err(show)?; }
        let _ = app.state::<Control>().wake.try_send(());
        Ok(())
    })?;
    organizer_status(app)
}
#[tauri::command]
pub(crate) fn organizer_run(app: AppHandle, profile:String, model:String) -> Result<OrganizerStatus,String> {request(app,profile,model,false)}
#[tauri::command]
pub(crate) fn organizer_retry(app: AppHandle, profile:String, model:String) -> Result<OrganizerStatus,String> {request(app,profile,model,true)}

fn usable(text: &str, limit: usize) -> bool { !text.trim().is_empty() && !text.contains('\0') && text.chars().count() <= limit }
#[derive(Serialize)]
struct NamedScope { id: String, name: String }

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelSource<'a> {
    origin_ref: &'a str,
    body: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<ReplyContext<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous: Option<ReplyContext<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReplyContext<'a> { origin_ref: &'a str, body: &'a str }

#[derive(Serialize)]
struct Payload<'a> { source: ModelSource<'a>, scopes: Vec<NamedScope>, candidates: Vec<queue::Candidate> }

fn payload(db: &Connection, root: &Path, grant: &Consent, job: &mut queue::Job, now: i64) -> Result<Option<Vec<u8>>,String> {
    if !queue::live(db,&grant.profile,job,now).map_err(show)? {return Err("claim_changed".into());}
    job.reply_excerpt = None;
    job.reply_to = None;
    job.reply_version = None;
    job.reply_expires_at = None;
    if job.archive {
        job.original.clear();
        let Some(ev) = evidence::latest(db,root,&grant.profile,&job.origin_ref,now).map_err(|e|e.code().to_owned())? else {return Ok(None)};
        if ev.seq != job.source_version {return Err("source_changed".into());}
        if let Some(parent) = ev.reply_parent.filter(|p| usable(&p.source_ref,2048) && usable(&p.excerpt,4096)) {
            job.reply_to = Some(parent.source_ref);
            job.reply_version = Some(parent.seq);
            job.reply_expires_at = Some(parent.expires_at);
            job.reply_excerpt = Some(workcontext_infra::egress::redact_text(&parent.excerpt));
        }
        let mut combined = ev.body.unwrap_or_default();
        for excerpt in ev.attachments { combined.push_str("\n\n附件摘录：\n"); combined.push_str(&excerpt); }
        job.original = combined;
    } else {
        let same: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM context_input WHERE profile=?1 AND id=?2 AND (CASE WHEN producer IS NULL THEN original ELSE json_extract(original,'$.result') END)=?3 AND origin_ref IS NULL AND source_version=?4)",
            params![grant.profile,job.input_id,job.original,job.source_version], |r|r.get(0)).map_err(show)?;
        if !same {return Err("source_changed".into());}
    }
    let masked = workcontext_infra::egress::redact_text(&job.original);
    let body = masked.as_str();
    if body.trim().is_empty() {return Ok(None);}
    if !usable(body,65536) || !usable(&job.origin_ref,2048) {return Err("input_limit".into());}
    let ids = queue::candidate_archive_ids(db, &grant.profile, job).map_err(show)?;
    job.authorized_inputs = if ids.is_empty() { Vec::new() } else {
        evidence::authorized_subset(db,root,&grant.profile,now,&ids).unwrap_or_default()
    };
    let mut candidates = queue::candidates(db,&grant.profile,job).map_err(show)?;
    let mut ids = HashSet::new();
    let ambiguous: HashSet<_> = candidates.iter().filter(|c| !ids.insert(c.id.as_str())).map(|c| c.id.clone()).collect();
    candidates.retain(|c| usable(&c.title,1024) && usable(&c.delivery,1024) && usable(&c.id,256) && !ambiguous.contains(&c.id)
        && workcontext_infra::egress::redact_text(&c.delivery) == c.delivery
        && c.anchors.iter().all(|a| a == &job.origin_ref || job.reply_to.as_ref() == Some(a) || (body.contains(a) && workcontext_infra::egress::redact_text(a) == *a)));
    for candidate in &mut candidates { candidate.title = workcontext_infra::egress::redact_text(&candidate.title); }
    let referenced: Vec<&str> = candidates.iter().filter_map(|c| c.scope_id.as_deref()).collect();
    let referenced = serde_json::to_string(&referenced).map_err(show)?;
    let mut statement = db.prepare_cached("SELECT id,title FROM work_thread WHERE profile=?1 AND (instr(?2,title)>0 OR instr(?4,title)>0 OR id IN (SELECT value FROM json_each(?3))) ORDER BY title,id LIMIT 64").map_err(show)?;
    let scopes = statement.query_map(params![grant.profile,body,referenced,job.reply_excerpt], |r| Ok(NamedScope {id:r.get(0)?,name:workcontext_infra::egress::redact_text(&r.get::<_,String>(1)?)})).map_err(show)?
        .collect::<Result<Vec<_>,_>>().map_err(show)?;
    candidates.retain(|c| c.scope_id.as_ref().is_none_or(|id| scopes.iter().any(|s| &s.id == id)));
    let reply_to = job.reply_to.as_deref().zip(job.reply_excerpt.as_deref()).map(|(origin_ref,body)|ReplyContext {origin_ref,body});
    let (reply_to,previous) = if job.reply_version.is_some_and(|v| v<0) { (None,reply_to) } else { (reply_to,None) };
    let bytes = serde_json::to_vec(&Payload { source:ModelSource { origin_ref: &job.origin_ref, body, reply_to, previous }, scopes, candidates }).map_err(show)?;
    if bytes.len() > 524288 {return Err("input_limit".into());}
    Ok(Some(bytes))
}

fn checked<T>(app:&AppHandle, expected:&Consent, f:impl FnOnce(&Connection,&Path)->Result<T,String>) -> Result<T,String> {
    access(app,|_,root,cfg,db| {
        if active(&cfg,root)? != *expected {return Err("consent_changed".into());}
        let tx = db.transaction().map_err(show)?;
        let snapshot = f(&tx,root)?;
        tx.commit().map_err(show)?;
        Ok(snapshot)
    })
}
fn run_job(app:&AppHandle, grant:&Consent, job:&mut queue::Job, generation:u64) -> Result<(),String> {
    let control = app.state::<Control>();
    // Resolve the interpreter before sampling the data that will actually leave the process.
    let script = archive::worker_script(app,"context_organize.py")?;
    let command = model_command(&script,credentials_available());
    let snapshot = checked(app,grant,|db,root|payload(db,root,grant,job,sources::now_ms()))?;
    let parent_version = job.reply_version;
    let decision = if let Some(bytes) = snapshot.as_deref() {
        process::call(command?,bytes,&control,generation).map_err(str::to_owned)?
    } else {queue::Decision {records:vec![]}};
    access(app,|_,root,cfg,db| {
        if control.stopping.load(Ordering::Acquire) || control.generation.load(Ordering::Acquire) != generation || active(&cfg,root)? != *grant {
            return Err("consent_changed".into());
        }
        let tx = db.transaction_with_behavior(workcontext_store::rusqlite::TransactionBehavior::Immediate).map_err(show)?;
        let now = sources::now_ms();
        verify_snapshot(snapshot.as_deref(),payload(&tx,root,grant,job,now)?.as_deref())?;
        if job.reply_version != parent_version { return Err("source_changed".into()); }
        if !queue::apply_in(&tx,&grant.profile,job,&decision,now).map_err(show)? {return Err("claim_changed".into());}
        tx.commit().map_err(show)
    })
}
fn verify_snapshot(before:Option<&[u8]>, after:Option<&[u8]>) -> Result<(),String> {
    if before == after {Ok(())} else {Err("source_changed".into())}
}

fn intake(db: &mut Connection, root: &Path, profile: &str, now: i64) -> Result<(bool, Option<String>), String> {
    let unavailable = |e: workcontext_app::external_archive::ArchiveError| {
        (false, Some(format!("归档增量暂不可读（{}），已保留读取位置。", e.code())))
    };
    let key = match evidence::grant_key(db,root,profile,now) {
        Ok(key) => key,
        Err(error) => return Ok(unavailable(error)),
    };
    queue::reset_if_grant_changed(db,profile,&key).map_err(show)?;
    let after = queue::checkpoint(db,profile).map_err(show)?;
    match evidence::changes_for_grant(db,root,profile,after,CHANGES,now,&key) {
        Ok(page) if page.checkpoint != after => match evidence::grant_key(db,root,profile,now) {
            Ok(current) if current == key => Ok((queue::ingest(db,profile,after,&page).map_err(show)?,None)),
            Ok(_) => Ok((false,Some("来源授权已变更，稍后按新范围重新读取。".into()))),
            Err(error) => Ok(unavailable(error)),
        },
        Ok(_) => Ok((false,None)),
        Err(error) => Ok(unavailable(error)),
    }
}

fn round(app:&AppHandle) -> Result<bool,String> {
    let control = app.state::<Control>();
    let claim = access(app,|_,root,cfg,db| {
        if control.stopping.load(Ordering::Acquire) {return Ok(None);}
        let Ok(grant) = active(&cfg,root) else {return Ok(None)};
        let now = sources::now_ms();
        let (advanced,warning) = intake(db,root,&grant.profile,now)?;
        *control.notice.lock().map_err(|_|"状态不可用")? = Some(Observation {profile:grant.profile.clone(),checked_at:now,error:warning});
        let job = queue::claim(db,&grant.profile,now).map_err(show)?;
        Ok(Some((grant,job,advanced,control.generation.load(Ordering::Acquire))))
    })?;
    let Some((grant,job,advanced,generation)) = claim else {return Ok(false)};
    let Some(mut job) = job else {return Ok(advanced)};
    control.running.store(true,Ordering::Release);
    let result = run_job(app,&grant,&mut job,generation);
    control.running.store(false,Ordering::Release);
    if let Err(reason) = result {
        access(app,|_,_,_,db| {
            if control.generation.load(Ordering::Acquire) == generation {
                queue::fail(db,&grant.profile,&job,sources::now_ms(),&reason).map_err(show)?;
            }
            Ok(())
        })?;
        return Ok(advanced);
    }
    Ok(true)
}

pub(crate) fn start(app:AppHandle) {
    let (wake, receiver) = mpsc::sync_channel(1);
    let generation = Arc::clone(&app.state::<SourceState>().organizer_generation);
    if !app.manage(Control {wake,stopping:AtomicBool::new(false),running:AtomicBool::new(false),generation,notice:Mutex::default(),thread:Mutex::default()}) {return;}
    let state = app.state::<Control>();
    let mut thread_slot = state.thread.lock().unwrap_or_else(|e|e.into_inner());
    if state.stopping.load(Ordering::Acquire) { return; }
    let handle = app.clone();
    let thread = thread::spawn(move || {
        let control = handle.state::<Control>();
        while !control.stopping.load(Ordering::Acquire) {
            match round(&handle) {
                Ok(true) => continue,
                Ok(false) => (),
                Err(_) => {
                    if let Ok(Some(profile)) = access(&handle,|_,root,cfg,_| Ok(binding(&cfg,root).ok())) {
                        if let Ok(mut msg) = control.notice.lock() {
                            *msg=Some(Observation {profile,checked_at:sources::now_ms(),error:Some("整理队列暂不可用，稍后自动重试。".into())});
                        }
                    }
                }
            }
            let _ = receiver.recv_timeout(TICK);
        }
    });
    *thread_slot = Some(thread);
}
// Called under the exclusive restore gate, before the database is replaced.
pub(crate) fn invalidate(app:&AppHandle) {
    if let Some(control) = app.try_state::<Control>() {
        control.generation.fetch_add(1,Ordering::AcqRel);
        let _ = control.wake.try_send(());
    }
}

pub(crate) fn stop(app:&AppHandle) {
    if let Some(control) = app.try_state::<Control>() {
        control.stopping.store(true,Ordering::Release);
        let _ = control.wake.try_send(());
        let thread = control.thread.lock().unwrap_or_else(|e|e.into_inner()).take();
        if let Some(thread) = thread {let _ = thread.join();}
    }
}

#[cfg(test)]
#[path = "organizer_tests.rs"]
mod tests;
