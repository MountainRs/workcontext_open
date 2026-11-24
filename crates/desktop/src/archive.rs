use crate::sources;
use serde::Serialize;
use crate::organizer::process_tree::ProcessTree;
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};
use tauri::{AppHandle, Manager};
use workcontext_store::rusqlite::{Connection, OpenFlags, OptionalExtension};

#[derive(Default)]
pub struct ArchiveWorker {
    busy: AtomicBool,
    stopping: AtomicBool,
    child: Mutex<Option<ProcessTree>>,
    last_error: Mutex<Option<String>>,
}
pub(crate) fn syncing(app: &AppHandle) -> bool {
    app.state::<ArchiveWorker>().busy.load(Ordering::Acquire)
}
fn root(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("archive"))
}
fn approved(cfg: &sources::SettingsFile) -> Option<&str> {
    cfg.judge.model_id.as_deref().filter(|model| {
        matches!(cfg.judge.mode, sources::config::JudgeMode::Cli)
            && cfg.egress.policy == "full"
            && cfg.egress.approved_model.as_deref() == Some(*model)
    })
}
pub(crate) fn auto_approved(cfg: &sources::SettingsFile) -> bool {
        cfg.extra
            .get("archiveJudgeOnSync")
            .and_then(|v| v.as_bool())
            == Some(true)
            && approved(cfg).is_some_and(|m| {
                cfg.extra.get("archiveAutoModel").and_then(|v| v.as_str()) == Some(m)
            })
}
fn automatic(app: &AppHandle) -> bool {
    sources::load_settings(&app.state::<sources::SourceState>()).is_ok_and(|cfg| auto_approved(&cfg))
}
#[tauri::command]
pub fn archive_auto_set(app: AppHandle, enabled: bool) -> Result<(), String> {
    let state = app.state::<sources::SourceState>();
    let _guard = state.config_lock.lock().map_err(|_| "配置锁不可用")?;
    let mut cfg = sources::load_settings(&state).map_err(|e| e.message)?;
    if enabled {
        let m = approved(&cfg)
            .ok_or("请先批准确切 CLI 模型的 full 出域策略")?
            .to_owned();
        cfg.extra.insert("archiveAutoModel".into(), m.into());
    } else {
        cfg.extra.remove("archiveAutoModel");
    }
    cfg.extra
        .insert("archiveJudgeOnSync".into(), enabled.into());
    sources::config::save(&state.config_path, &cfg).map_err(|e| e.to_string())
}

const CAPTURE_ACCOUNT: &str = "periodicArchiveProfile";
const SYNC_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

fn capture_account(app: &AppHandle) -> Option<String> {
    let state = app.state::<sources::SourceState>();
    let cfg = sources::load_settings(&state).ok()?;
    let approved = cfg.extra.get(CAPTURE_ACCOUNT)?.as_str()?;
    let bound = crate::ownership::bound_profile(state.db_path.parent()?).ok()?;
    (bound == approved).then_some(bound)
}

#[tauri::command]
pub fn archive_capture_set(app: AppHandle, profile: String, enabled: bool) -> Result<(), String> {
    let state = app.state::<sources::SourceState>();
    let _guard = state.config_lock.lock().map_err(|_| "配置锁不可用")?;
    let mut cfg = sources::load_settings(&state).map_err(|e| e.message)?;
    if enabled {
        let bound = crate::ownership::bound_profile(state.db_path.parent().ok_or("无工作目录")?)?;
        if bound != profile { return Err("归档账号已变化，请刷新后重试".into()); }
        cfg.extra.insert(CAPTURE_ACCOUNT.into(), profile.into());
    } else {
        cfg.extra.remove(CAPTURE_ACCOUNT);
    }
    sources::config::save(&state.config_path, &cfg).map_err(|e| e.to_string())
}

pub(crate) fn start_capture_scheduler(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last_attempt = None::<std::time::Instant>;
        while !app.state::<ArchiveWorker>().stopping.load(Ordering::Acquire) {
            if let Some(profile) = capture_account(&app) {
                if last_attempt.is_none_or(|last| last.elapsed() >= SYNC_INTERVAL)
                    && launch(app.clone(), Some(profile)).is_ok() {
                    last_attempt = Some(std::time::Instant::now());
                }
            } else {
                last_attempt = None;
            }
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
    });
}

pub(crate) fn stop(app: &AppHandle) {
    let Some(worker) = app.try_state::<ArchiveWorker>() else { return };
    worker.stopping.store(true, Ordering::Release);
    let mut slot = worker.child.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(child) = slot.as_mut() { child.stop(); }
}
#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveStatus {
    running: bool,
    scope: &'static str,
    last_success_ms: Option<i64>,
    conversations: i64,
    messages: i64,
    attachments: i64,
    downloaded: i64,
    failed: i64,
    error: Option<String>,
    auto_judge: bool,
    profile: Option<String>,
    periodic_capture: bool,
}
#[tauri::command]
pub fn archive_status(app: AppHandle) -> Result<ArchiveStatus, String> {
    let root = root(&app)?;
    let worker = app.state::<ArchiveWorker>();
    let mut status = ArchiveStatus {
        scope: "当前账号最近 168 小时内有消息的群聊与私聊；后续重叠增量，含窗口内可下载附件",
        running: worker.busy.load(Ordering::Acquire),
        error: worker
            .last_error
            .lock()
            .map_err(|_| "作业状态不可用")?
            .clone(),
        auto_judge: automatic(&app),
        profile: crate::ownership::bound_profile(&app.path().app_data_dir().map_err(|e| e.to_string())?).ok(),
        periodic_capture: capture_account(&app).is_some(),
        ..Default::default()
    };
    let path = root.join("archive.sqlite3");
    if !path.exists() {
        return Ok(status);
    }
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| e.to_string())?;
    let exists = |name: &str| {
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [name],
            |r| r.get::<_, bool>(0),
        )
        .map_err(|e| e.to_string())
    };
    let count = |sql: &str| {
        db.query_row(sql, [], |r| r.get::<_, i64>(0))
            .map_err(|e| e.to_string())
    };
    if exists("conversations")? {
        status.conversations = count("SELECT count(*) FROM conversations")?;
    }
    if exists("messages")? {
        status.messages = count("SELECT count(*) FROM messages")?;
    }
    if exists("attachments")? {
        status.attachments = count("SELECT count(*) FROM attachments")?;
        status.downloaded = count("SELECT count(*) FROM attachments WHERE status='downloaded'")?;
        status.failed = count("SELECT count(*) FROM attachments WHERE status='failed'")?;
        let reasons: Option<String> = db
            .query_row(
                "SELECT group_concat(reason || ' ×' || n, '；') FROM (
                SELECT coalesce(failure_reason,extraction_error,'unknown') reason,count(*) n
                FROM attachments WHERE status='failed' OR extraction_status='failed'
                GROUP BY reason ORDER BY n DESC LIMIT 5)",
                [],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        if let Some(reasons) = reasons {
            status
                .error
                .get_or_insert_with(String::new)
                .push_str(&format!(" 附件：{reasons}"));
        }
    }
    if exists("archive_state")? {
        let state = |k: &str| {
            db.query_row(
                "SELECT CAST(value AS TEXT) FROM archive_state WHERE key=?1",
                [k],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| e.to_string())
        };
        status.last_success_ms = state("last_success_ms")?.and_then(|v| v.parse().ok());
        if let Some(error) = state("error")?.filter(|v| !v.is_empty()) {
            status.error = Some(error)
        }
    }
    Ok(status)
}
pub(crate) fn archive_python() -> Result<Command, String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs = Vec::new();
    #[cfg(debug_assertions)]
    dirs.push(Path::new(env!("CARGO_MANIFEST_DIR")).join(if cfg!(windows) {
        "../../.venv/Scripts"
    } else {
        "../../.venv/bin"
    }));
    dirs.extend(std::env::split_paths(&path).filter(|p| p.is_absolute()));
    if cfg!(windows) {
        if let Some(home) = std::env::var_os("USERPROFILE") {
            dirs.push(PathBuf::from(home).join(".local").join("bin"));
        }
        if let Some(win) = std::env::var_os("SystemRoot") {
            dirs.push(PathBuf::from(win)); // classic py.exe launcher
        }
    }
    let names: &[&str] = if cfg!(windows) { &["python.exe", "python3.exe", "py.exe"] } else { &["python3"] };
    for dir in dirs.iter().filter(|p| p.is_absolute()) {
        for &name in names {
            // WindowsApps' python.exe can open the Store instead of starting a worker.
            if cfg!(windows) && name != "py.exe" && dir.file_name().and_then(|n| n.to_str())
                .is_some_and(|n| n.eq_ignore_ascii_case("WindowsApps")) {
                continue;
            }
            let exe = dir.join(name);
            if !exe.is_file() {
                continue;
            }
            let make_command = || {
                let mut cmd = Command::new(&exe);
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
                }
                if name == "py.exe" { cmd.arg("-3"); }
                cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
                cmd
            };
            if make_command().args(["-I", "-c", "import sqlite3,sys; sys.exit(sys.version_info < (3,8))"])
                .status().is_ok_and(|s| s.success()) {
                return Ok(make_command());
            }
        }
    }
    Err("未找到带 sqlite3 的 Python 3.8+；请将 Python 或其虚拟环境加入 PATH 后重启桌面端".into())
}
pub(crate) fn worker_script(app: &AppHandle, filename: &str) -> Result<PathBuf, String> {
    if cfg!(debug_assertions) {
        Ok(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts").join(filename))
    } else {
        app.path().resource_dir().map(|dir| dir.join("archive-worker").join(filename))
            .map_err(|_| "无法定位桌面应用资源".into())
    }
}
fn run(app: &AppHandle, root: &Path, profile: Option<&str>) -> Result<(), String> {
    let dws = sources::trusted_exe(sources::DWS_EXE).map_err(|e| e.message)?;
    let script = worker_script(app, "archive_sync.py")?;
    let mut cmd = archive_python()?;
    cmd
        .arg("-E")
        .arg("-s")
        .arg(script)
        .arg("--root")
        .arg(root)
        .arg("--dws")
        .arg(dws);
    if let Some(expected) = profile {
        cmd.args(["--expected-profile", expected]);
    }
    let worker = app.state::<ArchiveWorker>();
    {
        let mut slot = worker.child.lock().map_err(|_| "归档进程锁不可用")?;
        if worker.stopping.load(Ordering::Acquire) { return Err("归档已停止".into()); }
        *slot = Some(ProcessTree::spawn(&mut cmd).map_err(|e| format!("无法启动归档器（需要 Python 3）：{e}"))?);
    }
    let code = loop {
        {
            let mut slot = worker.child.lock().map_err(|_| "归档进程锁不可用")?;
            let child = slot.as_mut().ok_or("归档进程已停止")?;
            if let Some(exit) = child.try_wait().map_err(|e| e.to_string())? {
                slot.take();
                break exit;
            }
            if profile.is_some_and(|p| capture_account(app).as_deref() != Some(p)) {
                child.stop();
                slot.take();
                return Err("此账号的后台归档授权已关闭".into());
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    };
    if code.success() {
        Ok(())
    } else {
        Err(format!("归档器退出码 {:?}；查看归档失败记录", code.code()))
    }
}
#[tauri::command]
pub fn archive_sync_now(app: AppHandle) -> Result<(), String> {
    launch(app, None)
}

fn launch(app: AppHandle, profile: Option<String>) -> Result<(), String> {
    let root = root(&app)?;
    let worker = app.state::<ArchiveWorker>();
    if worker.busy.swap(true, Ordering::AcqRel) {
        return Err("归档同步正在运行".into());
    }
    *worker.last_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
    std::thread::spawn(move || {
        let worker = app.state::<ArchiveWorker>();
        let result = run(&app, &root, profile.as_deref());
        let synced = result.is_ok();
        let should_judge = synced && !worker.stopping.load(Ordering::Acquire) && automatic(&app);
        *worker.last_error.lock().unwrap_or_else(|e| e.into_inner()) = result.err();
        if should_judge {
            start_automatic_judge(&app);
        }
        worker.busy.store(false, Ordering::Release);
        if synced {
            crate::ownership::auto::archive_synced(&app);
        }
    });
    Ok(())
}
fn start_automatic_judge(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(error) =
            sources::archive_judge::archive_judge_after_sync(app.clone()).await
        {
            *app.state::<ArchiveWorker>()
                .last_error
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(format!("自动判读：{error:?}"));
        }
    });
}
