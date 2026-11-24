//! 会话事项清单：读取本机判读报告、人工流转，以及按账号授权的后台自动整理。
//!
//! - `ownership_refresh` 只授权当次发送；`ownership_set_auto` 把授权绑定到归档当前
//!   绑定的账号并写入 `config.json`。两者互不升级：单次同意不落授权，归档自动
//!   判读等其他开关也不被当作自动整理的授权。
//! - 后台整理由 [`auto`] 调度，只在归档新增消息后发起，与界面所在页面无关；
//!   手动与自动共用同一段时间内只允许一轮运行的名额。
//! - 每一轮都把授权账号传给判读脚本，脚本在采集前和保存前各核对一次；账号不
//!   一致时不发送、不写入报告。

use crate::{archive, sources, AppState};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{BufRead, BufReader, ErrorKind},
    path::Path,
    process::{Child, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex, MutexGuard,
    },
    thread,
    time::{Duration, Instant},
};
use tauri::{AppHandle, Manager};
use workcontext_app::ownership_flow::{self as flow, Candidate, EvidenceDetail, FlowCard, Receipt, SourceKey, SourceSummary, State};
use workcontext_store::rusqlite::{Connection, OpenFlags};

#[path = "ownership_auto.rs"]
pub(crate) mod auto;

/// 一轮判读允许的最长时间；超时的进程被结束，不占住唯一的运行名额。
const RUN_LIMIT: Duration = Duration::from_secs(60 * 60);
/// 查看判读进程是否退出的间隔。
const POLL: Duration = Duration::from_millis(250);
const STOPPED: &str = "自动整理已关闭，本轮已停止";
const TIMED_OUT: &str = "会话判读超过 60 分钟未结束，已停止";

/// 判读作业的状态与唤醒调度器的通道。
pub(crate) struct OwnershipWorker {
    run: Mutex<RunState>,
    signals: Mutex<mpsc::Receiver<()>>,
    alert: mpsc::Sender<()>,
}

impl Default for OwnershipWorker {
    fn default() -> Self {
        let (alert, signals) = mpsc::channel();
        Self { run: Mutex::new(RunState::default()), signals: Mutex::new(signals), alert }
    }
}

impl OwnershipWorker {
    /// 请求调度器立即重新评估；连续请求合并为一次。
    pub(crate) fn wake(&self) {
        let _ = self.alert.send(());
    }

    /// 等待下一次唤醒，最多 `limit`。
    pub(crate) fn wait(&self, limit: Duration) {
        let signals = self.signals.lock().unwrap_or_else(|e| e.into_inner());
        let _ = signals.recv_timeout(limit);
        while signals.try_recv().is_ok() {}
    }

    fn lock(&self) -> MutexGuard<'_, RunState> {
        self.run.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 终止由自动整理发起的当前一轮；手动发起的一轮不受影响。
    fn stop_auto(&self) {
        let mut run = self.lock();
        run.error = None;
        if let Some(cancel) = run.cancel.as_ref().filter(|_| run.auto) {
            cancel.store(true, Ordering::Release);
        }
    }
}

#[derive(Default)]
struct RunState {
    running: bool,
    /// 本轮由后台自动整理发起。
    auto: bool,
    progress: Option<String>,
    /// 保留失败账号，换绑归档后不显示上一账号的错误。
    error: Option<(String, String)>,
    /// 本轮的终止标记，也用来丢弃上一轮进程迟到的进度行。
    cancel: Option<Arc<AtomicBool>>,
}

impl RunState {
    fn started(auto: bool, cancel: Arc<AtomicBool>) -> Self {
        Self {
            running: true,
            auto,
            cancel: Some(cancel),
            progress: Some(if auto { "正在启动自动整理" } else { "正在启动会话判读" }.into()),
            error: None,
        }
    }
}

#[derive(Deserialize)]
struct ReportEvidence {
    #[serde(flatten)]
    key: SourceKey,
    #[serde(default)]
    time: String,
    #[serde(default)]
    conversation: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    excerpt: String,
}

#[derive(Deserialize)]
struct ReportItem {
    work: String,
    project: Option<String>,
    evidence: Vec<ReportEvidence>,
    anchors: Option<Vec<SourceKey>>,
}

#[derive(Deserialize)]
enum Bucket {
    本人处理,
    本人需核实,
    归属待定,
    等待他人,
}

#[derive(Deserialize)]
struct ReportProof {
    #[serde(default)]
    lines: Vec<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Card {
    item_index: usize,
    bucket: Bucket,
    work: String,
    action: String,
    when: String,
    source_summary: SourceSummary,
    proof: Option<ReportProof>,
}

#[derive(Deserialize)]
struct Snapshot {
    profile: String,
    cutoff: String,
    #[serde(default)]
    items: Vec<ReportItem>,
    cards: Option<Vec<Card>>,
}

#[derive(Serialize)]
struct Report {
    cutoff: String,
    cards: Option<Vec<FlowCard>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OwnershipView {
    report: Option<Report>,
    running: bool,
    progress: Option<String>,
    error: Option<String>,
    can_run: bool,
    /// 归档当前绑定的账号；尚未绑定时为 null。
    profile: Option<String>,
    /// 该账号是否已明确开启后台自动整理。
    auto_enabled: bool,
    /// 调度已记录的重试时刻（Unix 毫秒）。
    auto_retry_at: Option<i64>,
}

pub(crate) fn bound_profile(root: &Path) -> Result<String, &'static str> {
    let db = Connection::open_with_flags(root.join("archive/archive.sqlite3"), OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| "无法读取归档账号，请先完成一次归档同步")?;
    let profile: String = db.query_row("SELECT value FROM archive_state WHERE key='profile'", [], |row| row.get(0))
        .map_err(|_| "尚未绑定归档账号，请先完成一次归档同步")?;
    if !sources::is_profile(&profile) {
        return Err("归档账号标识无效");
    }
    Ok(profile)
}

fn credentials() -> Result<(), &'static str> {
    if std::env::var("WORKCONTEXT_REVIEW_API_KEY").is_ok_and(|key| !key.trim().is_empty()) {
        Ok(())
    } else {
        Err("请设置 WORKCONTEXT_REVIEW_API_KEY 并重启桌面端")
    }
}

fn latest(root: &Path, profile: &str) -> Result<Option<Snapshot>, &'static str> {
    let file = match File::open(root.join("ownership/latest.json")) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("无法读取上轮判读结果"),
    };
    let report: Snapshot = serde_json::from_reader(BufReader::new(file)).map_err(|_| "上轮判读结果格式无效")?;
    Ok((report.profile == profile).then_some(report))
}

fn board(db: &mut Connection, root: &Path, profile: &str) -> Result<Option<Report>, String> {
    project_board(db, profile, latest(root, profile)?)
}

fn project_board(db: &mut Connection, profile: &str, snapshot: Option<Snapshot>) -> Result<Option<Report>, String> {
    let has_snapshot = snapshot.is_some();
    let (cutoff, cards, mut items) = snapshot
        .map(|s| (s.cutoff, s.cards, s.items))
        .unwrap_or_default();
    let has_cards = cards.is_some();
    let candidates = cards.unwrap_or_default().into_iter().map(|card| {
        let item = items.get_mut(card.item_index).ok_or("事项与来源索引不匹配，请重新整理清单")?;
        let sources = std::mem::take(&mut item.evidence);
        if sources.is_empty() {
            return Err("事项缺少可关联的来源，请重新整理清单");
        }
        // Older card reports index their responsibility proof into the original evidence order.
        let anchors = item.anchors.take().unwrap_or_else(|| {
            card.proof.as_ref().and_then(|proof| proof.lines.iter().map(|n| {
                sources.get(n.checked_sub(1)?).map(|source| source.key.clone())
            }).collect::<Option<Vec<_>>>()).unwrap_or_default()
        });
        let mut evidence = Vec::with_capacity(sources.len());
        let mut evidence_details = Vec::new();
        for source in sources {
            evidence.push(source.key);
            if !source.excerpt.trim().is_empty() {
                evidence_details.push(EvidenceDetail { time: source.time, conversation: source.conversation,
                    kind: source.kind, excerpt: source.excerpt });
            }
        }
        let (state, needs_verification) = match card.bucket {
            Bucket::本人处理 => (State::Mine, false),
            Bucket::本人需核实 => (State::Mine, true),
            Bucket::归属待定 => (State::Triage, false),
            Bucket::等待他人 => (State::Waiting, false),
        };
        Ok(Candidate {
            project: item.project.take(),
            state, needs_verification, evidence, anchors, evidence_details, source_work: std::mem::take(&mut item.work),
            work: card.work, action: card.action, when: card.when, source_summary: card.source_summary,
        })
    }).collect::<Result<Vec<_>, _>>()?;
    let cards = flow::view(db, profile, &candidates).map_err(|e| e.to_string())?;
    if !has_snapshot && cards.is_empty() {
        return Ok(None);
    }
    Ok(Some(Report { cutoff, cards: (has_cards || !cards.is_empty()).then_some(cards) }))
}

/// Index the latest report even when the first page opened is the catalog or a deep link.
pub(crate) fn index_current_report(db: &mut Connection, root: &Path) -> Result<(), String> {
    if let Ok(profile) = bound_profile(root) {
        board(db, root, &profile)?;
    }
    Ok(())
}

#[tauri::command]
pub(crate) fn ownership_view(app: AppHandle) -> Result<OwnershipView, String> {
    let root = app.path().app_data_dir().map_err(|_| "无法定位应用数据目录")?;
    let profile = bound_profile(&root);
    let worker = app.state::<OwnershipWorker>();
    let (running, progress, worker_error) = {
        let run = worker.lock();
        let error = run.error.as_ref().filter(|(owner, _)| profile.as_deref() == Ok(owner.as_str()));
        (run.running, run.progress.clone(), error.map(|(_, message)| message.clone()))
    };
    let config = app.state::<sources::SourceState>();
    let ready = profile.as_ref().map(|_| ()).map_err(|e| *e).and_then(|_| credentials());
    let (auto_enabled, auto_retry_at) = auto::view_status(&root, &config, profile.as_deref().ok(), running);
    let state = app.state::<AppState>();
    let mut db = state.db.lock().map_err(|_| "事项数据库不可用")?;
    let report = match &profile {
        Ok(profile) => board(&mut db, &root, profile)?,
        Err(_) => None,
    };
    let error = worker_error.or_else(|| ready.err().map(str::to_owned));
    Ok(OwnershipView {
        report, running, progress, error, can_run: ready.is_ok() && !running, auto_enabled, auto_retry_at, profile: profile.ok(),
    })
}

/// 写入授权与终止进行中的自动整理在同一把配置锁内完成；调度器发起前也在
/// 这把锁内重新核对授权，关闭不会漏掉正在开始的一轮。
#[tauri::command]
pub(crate) fn ownership_set_auto(app: AppHandle, profile: String, enabled: bool) -> Result<(), String> {
    let root = app.path().app_data_dir().map_err(|_| "无法定位应用数据目录")?;
    let bound = bound_profile(&root);
    let config = app.state::<sources::SourceState>();
    let worker = app.state::<OwnershipWorker>();
    let _guard = config.config_lock.lock().map_err(|_| "配置锁不可用")?;
    auto::set_auto(&config, bound.as_deref().map_err(|e| *e), &profile, enabled, credentials())?;
    if !enabled {
        worker.stop_auto();
    }
    drop(_guard);
    worker.wake();
    Ok(())
}

/// 本次同意只适用于界面显示的账号。
fn agreed(bound: Result<&str, &'static str>, profile: &str) -> Result<(), String> {
    if bound.map_err(str::to_owned)? != profile {
        return Err("归档当前绑定的账号与本次同意的账号不一致，未启动整理".into());
    }
    Ok(())
}

#[tauri::command]
pub(crate) fn ownership_refresh(app: AppHandle, profile: String) -> Result<(), String> {
    credentials()?;
    let root = app.path().app_data_dir().map_err(|_| "无法定位应用数据目录")?;
    agreed(bound_profile(&root).as_deref().map_err(|e| *e), &profile)?;
    auto::manual(&app, &root, &profile)
}

#[tauri::command]
pub(crate) fn ownership_move(app: AppHandle, id: String, version: String, target: State, action: Option<String>) -> Result<Receipt, String> {
    let root = app.path().app_data_dir().map_err(|_| "无法定位应用数据目录")?;
    let state = app.state::<AppState>();
    let mut db = state.db.lock().map_err(|_| "事项数据库不可用")?;
    let profile = bound_profile(&root)?;
    board(&mut db, &root, &profile)?;
    flow::move_card(&mut db, &profile, &id, &version, target, action.as_deref(), workcontext_store::now_ms())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn ownership_set_conclusion(app: AppHandle, id: String, version: String, note: String, resolved: bool) -> Result<(), String> {
    let root = app.path().app_data_dir().map_err(|_| "无法定位应用数据目录")?;
    let state = app.state::<AppState>();
    let mut db = state.db.lock().map_err(|_| "事项数据库不可用")?;
    let profile = bound_profile(&root)?;
    board(&mut db, &root, &profile)?;
    flow::set_conclusion(&mut db, &profile, &id, &version, &note, resolved, workcontext_store::now_ms())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn ownership_set_project(app: AppHandle, id: String, version: String, project: Option<String>) -> Result<(), String> {
    let root = app.path().app_data_dir().map_err(|_| "无法定位应用数据目录")?;
    let state = app.state::<AppState>();
    let db = state.db.lock().map_err(|_| "事项数据库不可用")?;
    flow::set_project(&db, &bound_profile(&root)?, &id, &version, project.as_deref()).map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn ownership_undo(app: AppHandle, token: String) -> Result<(), String> {
    let root = app.path().app_data_dir().map_err(|_| "无法定位应用数据目录")?;
    let state = app.state::<AppState>();
    let mut db = state.db.lock().map_err(|_| "事项数据库不可用")?;
    let profile = bound_profile(&root)?;
    flow::undo(&mut db, &profile, &token, workcontext_store::now_ms()).map_err(|e| e.to_string())
}

fn public_progress(line: &str) -> bool {
    if matches!(line, "正在核验账号" | "正在采集近30天会话" | "正在判读会话" | "正在保存结果") {
        return true;
    }
    line.strip_prefix("正在判读会话 ").and_then(|s| s.split_once('/')).is_some_and(|(n, total)| {
        [n, total].iter().all(|v| !v.is_empty() && v.len() <= 9 && v.bytes().all(|c| c.is_ascii_digit()))
    })
}

/// 读取判读进程的公开进度直到它退出；收到终止请求或超过时限时结束该进程。
fn supervise(child: &mut Child, cancel: &AtomicBool, limit: Duration, mut progress: impl FnMut(String) + Send + 'static) -> Result<ExitStatus, String> {
    let pipe = child.stdout.take().ok_or("无法读取会话判读进度")?;
    thread::spawn(move || {
        for line in BufReader::new(pipe).lines() {
            let Ok(line) = line else { break };
            if public_progress(&line) {
                progress(line);
            }
        }
    });
    let started = Instant::now();
    loop {
        let stopped = if cancel.load(Ordering::Acquire) {
            Some(STOPPED)
        } else if started.elapsed() >= limit {
            Some(TIMED_OUT)
        } else {
            None
        };
        if let Some(reason) = stopped {
            let _ = child.kill();
            let _ = child.wait();
            return Err(reason.to_owned());
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => thread::sleep(POLL),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("无法取得判读进程状态".to_owned());
            }
        }
    }
}

fn run(app: &AppHandle, root: &Path, profile: &str, cancel: &Arc<AtomicBool>) -> Result<(), String> {
    let script = archive::worker_script(app, "ownership_review.py")?;
    if !script.is_file() { return Err("找不到会话判读脚本".into()); }
    let dws = sources::trusted_exe(sources::DWS_EXE).map_err(|_| "找不到受信任的 DWS 程序")?;
    // 脚本只处理这一个账号；归档在此期间换绑账号时它不发送、不写入。
    let mut child = archive::archive_python()?.args(["-E", "-s", "-u", "-X", "utf8"]).arg(script)
        .arg("--root").arg(root).arg("--dws").arg(dws).arg("--profile").arg(profile)
        .arg("--desktop-session").stdin(Stdio::piped())
        .stdout(Stdio::piped()).spawn().map_err(|_| "无法启动会话判读进程")?;
    let handle = app.clone();
    let mine = Arc::clone(cancel);
    let status = supervise(&mut child, cancel, RUN_LIMIT, move |line| {
        let worker = handle.state::<OwnershipWorker>();
        let mut run = worker.lock();
        // 上一轮进程迟到的进度行不进入新一轮的状态。
        if run.cancel.as_ref().is_some_and(|current| Arc::ptr_eq(current, &mine)) {
            run.progress = Some(line);
        }
    })?;
    if !status.success() {
        return Err(format!("会话判读失败（退出码 {}）", status.code().map_or_else(|| "未知".into(), |n| n.to_string())));
    }
    let state = app.state::<AppState>();
    let mut db = state.db.lock().map_err(|_| "事项数据库不可用")?;
    if bound_profile(root)? != profile { return Err("归档账号已切换".into()); }
    let snapshot = latest(root, profile)?.ok_or("未找到与归档账号匹配的判读结果")?;
    project_board(&mut db, profile, Some(snapshot))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn board_keeps_a_handoff_after_snapshot_replacement_and_restart() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("workcontext.sqlite3");
        let directory = root.path().join("ownership");
        std::fs::create_dir(&directory).unwrap();
        let snapshot = directory.join("latest.json");
        let mut old = json!({
            "profile":"org:user", "cutoff":"2026-11-23 15:00",
            "items":[{"work":"核对样图", "evidence":[{"cid":"chat", "mid":"message"}]}],
            "cards":[{"bucket":"本人需核实", "work":"核对样图", "action":"检查三张样图的工序。",
                "when":"", "itemIndex":0, "sourceSummary":{"time":"11 月 23 日","place":"","people":"项目组","event":"项目组发来了三张样图。"}}]
        });
        std::fs::write(&snapshot, old.to_string()).unwrap();
        let mut db = workcontext_store::open(&path).unwrap();
        let first = serde_json::to_value(board(&mut db, root.path(), "org:user").unwrap()).unwrap();
        let card = &first["cards"][0];
        assert_eq!(card["state"], "mine");
        assert_eq!(card["needsVerification"], true);
        let id = card["id"].as_str().unwrap();
        let handoff = "等项目负责人确认第三张样图。";
        flow::move_card(&mut db, "org:user", id, card["version"].as_str().unwrap(), State::Waiting, Some(handoff), 100).unwrap();
        drop(db);
        let mut db = workcontext_store::open(&path).unwrap();
        old["cards"][0]["work"] = json!("评审样图的三处工序");
        std::fs::write(&snapshot, old.to_string()).unwrap();
        let refreshed = serde_json::to_value(board(&mut db, root.path(), "org:user").unwrap()).unwrap();
        let cards = refreshed["cards"].as_array().unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0]["id"], id);
        assert_eq!(cards[0]["state"], "waiting");
        assert_eq!(cards[0]["needsVerification"], false);
        assert_eq!(cards[0]["action"], handoff);
        std::fs::remove_file(snapshot).unwrap();
        let retained = serde_json::to_value(board(&mut db, root.path(), "org:user").unwrap()).unwrap();
        assert_eq!(retained["cards"][0]["id"], id);
        assert_eq!(retained["cards"][0]["needsVerification"], false);
        assert!(board(&mut db, root.path(), "org:other").unwrap().is_none());
    }
}
