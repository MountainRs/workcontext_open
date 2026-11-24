//! 会话清单的后台自动整理：归档新增消息后，在后台发起一轮判读。
//!
//! - **授权**：`config.json` 的 `ownershipAutoProfile` 必须逐字等于归档当前绑定的
//!   账号。单次同意（`ownership_refresh`）、归档自动判读等开关都不构成授权，也
//!   不互相升级。
//! - **触发**：应用启动、归档同步成功、授权变化都会唤醒调度器，另有每分钟一次的
//!   兜底检查；与界面所在页面无关，归档同步进行中不发起。
//! - **去重**：`ownership/auto.json` 记录最近一轮成功整理覆盖的归档水位（最大
//!   消息 rowid）。只有归档新增消息才推进水位；没有新消息的同步
//!   和对已有消息的改写都不触发下一轮。
//! - **退避**：失败或未正常结束的一轮按 1、5、15、60 分钟退避，重启后继续生效，
//!   任意一轮成功后清零。
//! - **单飞**：手动与自动共用一个运行名额。关闭授权会终止进行中的自动整理，不
//!   影响手动发起的一轮。

use super::{credentials, run, OwnershipWorker, RunState};
use crate::{archive, sources};
use serde::{Deserialize, Serialize};
use sources::SettingsFile;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Manager};
use workcontext_store::rusqlite::{Connection, OpenFlags};

/// `config.json` 中保存授权账号的键。
const CONSENT: &str = "ownershipAutoProfile";
/// 调度状态文件，位于应用数据目录的 `ownership` 下。
const STATE_FILE: &str = "auto.json";
/// 两次兜底检查的间隔；唤醒会立即触发评估。
const TICK: Duration = Duration::from_secs(60);
/// 第 1–4 次及以后连续失败后的等待时长（毫秒）。
const BACKOFF_MS: [i64; 4] = [60_000, 300_000, 900_000, 3_600_000];
const MAX_BACKOFF_MS: i64 = BACKOFF_MS[BACKOFF_MS.len() - 1];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trigger {
    Manual,
    Auto,
}

/// 归档水位：只随归档新增消息前进，改写已有消息时不变。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Watermark {
    rowid: i64,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Schedule {
    profile: String,
    /// 最近一轮成功整理覆盖的归档水位。
    reviewed: Option<Watermark>,
    failures: u32,
    retry_at: Option<i64>,
    /// 已发起但尚未记录结果的一轮所针对的归档水位；进程被结束时留在文件里。
    pending: Option<Watermark>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Done,
    Failed,
    Cancelled,
}

/// 此刻不发起自动整理的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Blocked {
    /// 上一轮没有留下结果。
    Interrupted,
    /// 归档自上一轮成功整理后没有新增消息。
    Reviewed,
    /// 上一轮失败，仍在等待。
    Backoff,
}

impl Schedule {
    /// 只接受属于该账号的记录；文件缺失、损坏或属于其他账号时从零开始。
    fn load(path: &Path, profile: &str) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Schedule>(&bytes).ok())
            .filter(|saved| saved.profile == profile)
            .unwrap_or_else(|| Self { profile: profile.to_owned(), ..Self::default() })
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let bytes = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())
    }

    fn blocked(&self, archived: &Watermark, now: i64) -> Option<Blocked> {
        if self.pending.is_some() {
            Some(Blocked::Interrupted)
        } else if self.reviewed == Some(*archived) {
            Some(Blocked::Reviewed)
        } else if self.retry_at.is_some_and(|at| at > now && at - now <= MAX_BACKOFF_MS) {
            // 时钟被拨回后，比最长退避还远的重试时刻不再生效。
            Some(Blocked::Backoff)
        } else {
            None
        }
    }

    fn fail(&mut self, now: i64) {
        self.pending = None;
        self.failures = self.failures.saturating_add(1);
        self.retry_at = Some(now.saturating_add(backoff(self.failures)));
    }

    fn settle(&mut self, archived: Option<Watermark>, outcome: Outcome, now: i64) {
        match outcome {
            Outcome::Done => {
                self.pending = None;
                // 没有水位的一轮（同步未完成时的手动整理）不冒充已整理的归档。
                if archived.is_some() {
                    self.reviewed = archived;
                }
                self.failures = 0;
                self.retry_at = None;
            }
            Outcome::Failed => self.fail(now),
            Outcome::Cancelled => self.pending = None,
        }
    }
}

fn backoff(failures: u32) -> i64 {
    BACKOFF_MS[(failures.max(1) as usize - 1).min(BACKOFF_MS.len() - 1)]
}

/// 一轮运行的记账凭据：结束时据此更新调度状态。
struct Ticket {
    path: PathBuf,
    schedule: Schedule,
    archived: Option<Watermark>,
}

impl Ticket {
    /// 先把「已发起」写盘再启动进程，进程被结束时才能据此退避。
    fn open(path: PathBuf, mut schedule: Schedule, archived: Option<Watermark>, strict: bool) -> Result<Self, String> {
        schedule.pending = archived;
        if strict {
            // 自动整理写不下记账就不发起；手动整理不受记账文件影响。
            schedule.save(&path)?;
        } else {
            let _ = schedule.save(&path);
        }
        Ok(Self { path, schedule, archived })
    }

    fn settle(&mut self, failed: bool, cancelled: bool) {
        let outcome = match (failed, cancelled) {
            (false, _) => Outcome::Done,
            (true, true) => Outcome::Cancelled,
            (true, false) => Outcome::Failed,
        };
        self.schedule.settle(self.archived, outcome, workcontext_store::now_ms());
        let _ = self.schedule.save(&self.path);
    }
}

fn state_path(root: &Path) -> PathBuf {
    root.join("ownership").join(STATE_FILE)
}

/// 已授权自动整理的账号。
fn authorized(cfg: &SettingsFile) -> Option<&str> {
    cfg.extra.get(CONSENT)?.as_str().filter(|profile| sources::is_profile(profile))
}

fn authorized_by(state: &sources::SourceState, profile: &str) -> bool {
    sources::load_settings(state).is_ok_and(|cfg| authorized(&cfg) == Some(profile))
}

/// 只读取当前账号已获授权且空闲时的退避记录，不查询归档水位。
pub(super) fn view_status(root: &Path, state: &sources::SourceState, profile: Option<&str>, running: bool) -> (bool, Option<i64>) {
    let Some(profile) = profile.filter(|p| authorized_by(state, p)) else { return (false, None) };
    let retry_at = if running { None } else {
        let schedule = Schedule::load(&state_path(root), profile);
        schedule.retry_at.filter(|_| schedule.pending.is_none())
    };
    (true, retry_at)
}

/// 写入或清除授权。开启只对归档当前绑定的账号成立且要求凭据可用；关闭不受
/// 账号与运行条件限制，并总是清除已保存的授权。
pub(super) fn set_auto(
    state: &sources::SourceState,
    bound: Result<&str, &'static str>,
    profile: &str,
    enabled: bool,
    ready: Result<(), &'static str>,
) -> Result<(), String> {
    if !sources::is_profile(profile) {
        return Err("账号标识无效".into());
    }
    let mut cfg = sources::load_settings(state).map_err(|e| e.message)?;
    if enabled {
        if bound != Ok(profile) {
            return Err(match bound {
                Err(reason) => reason.to_owned(),
                Ok(_) => "归档当前绑定的账号与所选账号不一致，未开启自动整理".into(),
            });
        }
        ready.map_err(str::to_owned)?;
        cfg.extra.insert(CONSENT.into(), profile.into());
    } else {
        cfg.extra.remove(CONSENT);
    }
    sources::config::save(&state.config_path, &cfg).map_err(|e| e.to_string())
}

/// 归档水位。没有消息、同步窗口未完成或无法读取时为 `None`。
fn watermark(root: &Path, profile: &str) -> Option<Watermark> {
    let db = Connection::open_with_flags(root.join("archive/archive.sqlite3"), OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let rowid = db.query_row(
        "SELECT (SELECT max(rowid) FROM messages) FROM archive_state \
         WHERE key='profile' AND value=?1 AND NOT EXISTS \
         (SELECT 1 FROM archive_state WHERE key='message_window')",
        [profile],
        |row| row.get::<_, Option<i64>>(0),
    ).ok()??;
    Some(Watermark { rowid })
}

/// 本次评估可以整理的账号与归档水位：账号已授权、凭据可用、归档有完整的新内容。
fn candidate(root: &Path, cfg: &SettingsFile, ready: bool) -> Option<(String, Watermark)> {
    let consent = authorized(cfg)?;
    if !ready {
        return None;
    }
    watermark(root, consent).map(|archived| (consent.to_owned(), archived))
}

/// 在唯一的运行名额上发起一轮。`prepare` 在持有名额时执行：它拒绝或决定不发起
/// 时名额保持空闲，返回记账凭据后才启动进程。
fn start(
    worker: &OwnershipWorker,
    trigger: Trigger,
    prepare: impl FnOnce() -> Result<Option<Ticket>, String>,
    job: impl FnOnce(Arc<AtomicBool>) -> Result<(), String> + Send + 'static,
    finish: impl FnOnce(Ticket, Result<(), String>, &AtomicBool) + Send + 'static,
) -> Result<bool, String> {
    let cancel = Arc::new(AtomicBool::new(false));
    let mut run = worker.lock();
    if run.running {
        return Err("已有会话判读正在运行".into());
    }
    let Some(ticket) = prepare()? else {
        return Ok(false);
    };
    let passed = Arc::clone(&cancel);
    thread::Builder::new()
        .name("ownership-review".into())
        .spawn(move || {
            // 作业线程异常退出也要交还名额。
            let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(Arc::clone(&passed)))) {
                Ok(result) => result,
                Err(_) => Err("会话判读异常终止".into()),
            };
            finish(ticket, result, &passed);
        })
        .map_err(|e| e.to_string())?;
    *run = RunState::started(matches!(trigger, Trigger::Auto), cancel);
    Ok(true)
}

fn launch(
    app: &AppHandle,
    trigger: Trigger,
    root: &Path,
    profile: &str,
    prepare: impl FnOnce() -> Result<Option<Ticket>, String>,
) -> Result<bool, String> {
    let worker = app.state::<OwnershipWorker>();
    let (job_handle, finish_handle) = (app.clone(), app.clone());
    let (job_root, job_profile) = (root.to_path_buf(), profile.to_owned());
    start(
        &worker,
        trigger,
        prepare,
        move |cancel| run(&job_handle, &job_root, &job_profile, &cancel),
        move |ticket, result, cancelled| finish(&finish_handle, ticket, result, cancelled),
    )
}

/// 作业结束后先落盘调度状态，再交还名额并唤醒调度器。
fn finish(app: &AppHandle, mut ticket: Ticket, result: Result<(), String>, cancel: &AtomicBool) {
    let worker = app.state::<OwnershipWorker>();
    let mut run = worker.lock();
    let cancelled = cancel.load(Ordering::Acquire);
    ticket.settle(result.is_err(), cancelled);
    let error = result.err().filter(|_| !cancelled).map(|message| (ticket.schedule.profile, message));
    *run = RunState { error, ..Default::default() };
    drop(run);
    worker.wake();
}

/// 手动整理不要求归档有新消息，也不受记账文件影响。
pub(super) fn manual(app: &AppHandle, root: &Path, profile: &str) -> Result<(), String> {
    launch(app, Trigger::Manual, root, profile, move || {
        let path = state_path(root);
        let schedule = Schedule::load(&path, profile);
        Ticket::open(path, schedule, watermark(root, profile), false).map(Some)
    })
    .map(|_| ())
}

fn start_auto(app: &AppHandle, root: &Path, profile: &str, archived: Watermark) -> Result<bool, String> {
    if archive::syncing(app) {
        return Ok(false);
    }
    launch(app, Trigger::Auto, root, profile, move || {
        let path = state_path(root);
        let mut schedule = Schedule::load(&path, profile);
        let now = workcontext_store::now_ms();
        match schedule.blocked(&archived, now) {
            Some(Blocked::Interrupted) => {
                schedule.fail(now);
                schedule.save(&path)?;
            }
            Some(Blocked::Reviewed | Blocked::Backoff) => {}
            None => return Ticket::open(path, schedule, Some(archived), true).map(Some),
        }
        Ok(None)
    })
}

/// 评估一次是否发起自动整理。没有授权或归档没有新增消息时什么也不做。
fn evaluate(app: &AppHandle) {
    // 同步进行中归档只写入了一部分；同步结束后会再次唤醒。
    if archive::syncing(app) {
        return;
    }
    let Ok(root) = app.path().app_data_dir() else {
        return;
    };
    let config = app.state::<sources::SourceState>();
    // 持锁直到运行名额登记完成，撤权方随后能找到并停止这轮作业。
    let Ok(_guard) = config.config_lock.lock() else {
        return;
    };
    let Ok(cfg) = sources::load_settings(&config) else {
        return;
    };
    let Some((profile, archived)) = candidate(&root, &cfg, credentials().is_ok()) else {
        return;
    };
    let _ = start_auto(app, &root, &profile, archived);
}

/// 归档同步成功结束后由同步线程调用。
pub(crate) fn archive_synced(app: &AppHandle) {
    app.state::<OwnershipWorker>().wake();
}

/// 应用运行期间常驻的调度线程；与界面所在页面无关。
pub(crate) fn start_scheduler(app: &AppHandle) {
    let app = app.clone();
    let started = thread::Builder::new()
        .name("ownership-auto".into())
        .spawn(move || {
            loop {
                evaluate(&app);
                app.state::<OwnershipWorker>().wait(TICK);
            }
        });
    if let Err(error) = started {
        eprintln!("[workcontext] 无法创建会话自动整理调度线程: {error}");
    }
}

#[cfg(test)]
#[path = "ownership_auto_tests.rs"]
mod tests;
