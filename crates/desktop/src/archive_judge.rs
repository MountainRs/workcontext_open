//! Bounded judge for the local DingTalk archive. Register `sources::archive_judge::archive_judge_now`.
//!
//! After a successful archive commit, call `archive_judge_after_sync`. It honors
//! the archive worker's persisted opt-in bound to the model approved for `full`.
//! The manual command does not bypass model approval. No archive writes.
//!
//! IPC: `invoke("archive_judge_now")` waits for a round (at most 512 messages
//! scanned / 4 model attempts). Returns `status`, `scanned`, `skippedUnchanged`,
//! `skippedIneligible`, `more`, and `calls`. Inspect per-call status and error
//! codes even when the command resolves. Token counts are nullable, never estimates.

use std::sync::{Mutex, TryLockError};
use std::time::Duration;

use serde::Serialize;
use workcontext_app::judge::{
    apply_classified_records, archive_can_judge, classify_omp_outcome, ArchiveMatch, JudgeError, RuntimeOutcome,
};
use workcontext_core::classify::{ConversationType, Direction, RecordContext, RecordKind};
use workcontext_core::{MeContext, PolicyValidator};
use workcontext_infra::classify_input::{
    input_sha256, ArchiveExcerpt, ArchiveExcerptKind, ClassifyInput, ConversationRef, InputRecord,
    PersonRef, SourceRef,
};
use workcontext_infra::omp::{CliAgentOutcome, OmpCliRuntime};
use workcontext_infra::timefmt::format_rfc3339_ms;
use workcontext_store::rusqlite::{params, Connection, OpenFlags, OptionalExtension, Row};

use super::{config::JudgeMode, egress, judge, load_settings, now_ms, with_conn, AppError, SettingsFile, SourceState};

const SOURCE_ID: &str = "archive-dingtalk";
const CALL_LIMIT: usize = 4;
const SCAN_LIMIT: usize = 512;
const MESSAGE_CHARS: usize = 1600;
const NEARBY_CHARS: usize = 360;
const ATTACHMENT_CHARS: usize = 600;
const NEARBY_MS: i64 = 30 * 60 * 1000;
static ARCHIVE_JOB: Mutex<()> = Mutex::new(());

// Owned by this module. The archive is read-only and a work DB backup retains
// both the scan position and the pre-call journal together with the items.
const JOURNAL_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS archive_judge_cursor (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    last_occurred_at INTEGER NOT NULL,
    last_rowid INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS archive_judge_call (
    id INTEGER PRIMARY KEY,
    model_id TEXT NOT NULL,
    input_sha256 TEXT NOT NULL,
    source_record_id TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    cli_status TEXT,
    cli_error TEXT,
    process_started INTEGER,
    reported_model TEXT,
    session_ref TEXT,
    tokens_in INTEGER,
    tokens_out INTEGER,
    cost_usd REAL,
    latency_ms REAL,
    apply_status TEXT NOT NULL DEFAULT 'pending',
    apply_error TEXT,
    agent_run_id TEXT REFERENCES agent_run(id) ON DELETE SET NULL
);
CREATE INDEX IF NOT EXISTS ix_archive_judge_call_input
    ON archive_judge_call (model_id, input_sha256, id DESC);";

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveJudgeResult {
    status: &'static str,
    scanned: usize,
    skipped_unchanged: usize,
    skipped_ineligible: usize,
    more: bool,
    calls: Vec<ArchiveCall>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ArchiveCall {
    call_id: i64,
    agent_run_id: Option<String>,
    status: &'static str,
    error_code: Option<String>,
    item_id: Option<String>,
    tokens_in: Option<u64>,
    tokens_out: Option<u64>,
}

impl ArchiveCall {
    fn stops_round(&self) -> bool {
        matches!(self.error_code.as_deref(), Some(
            "AGENT_MODEL_IDENTITY_UNVERIFIED" | "AGENT_MODEL_NOT_APPROVED"
            | "AGENT_CONFIG" | "AGENT_UNREACHABLE" | "AGENT_COST_GUARD"
        ))
    }
}

/// Awaits one bounded round. `more` means the current sweep has more work;
/// subsequent sweeps also revisit late attachment extracts and edits.
#[tauri::command]
pub async fn archive_judge_now(app: tauri::AppHandle) -> Result<ArchiveJudgeResult, AppError> {
    dispatch(app, false).await
}

/// Rust hook. Call only after the archive worker completed successfully.
pub async fn archive_judge_after_sync(app: tauri::AppHandle) -> Result<ArchiveJudgeResult, AppError> {
    dispatch(app, true).await
}

async fn dispatch(app: tauri::AppHandle, after_sync: bool) -> Result<ArchiveJudgeResult, AppError> {
    use tauri::Manager;
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.try_state::<SourceState>()
            .ok_or_else(|| AppError::internal("归档判读未注册 SourceState"))?;
        run(&state, after_sync)
    })
    .await
    .map_err(|_| AppError::internal("归档判读工作线程异常退出"))?
}

fn busy(message: &str) -> AppError {
    AppError {
        code: "ARCHIVE_JUDGE_BUSY".into(), message: message.into(), detail: None,
        retryable: true, hint: None,
    }
}

fn validate_archive_approval(cfg: &SettingsFile) -> Result<&str, AppError> {
    judge::validate_preconditions(cfg)?;
    if cfg.judge.mode != JudgeMode::Cli || cfg.egress.policy != "full" {
        return Err(AppError::invalid_argument("归档判读需要 cli 模式和持久 full 出域批准。"));
    }
    let model = cfg.judge.model_id.as_deref().unwrap_or_default();
    // OMP documents provider/model as exact; bare names, patterns, roles and
    // thinking suffixes are deliberately not accepted here.
    let segment = |s: &str| {
        s.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    if model.len() > 128 || !model.contains('/') || !model.split('/').all(segment)
        || cfg.egress.approved_model.as_deref() != Some(model)
    {
        return Err(AppError::invalid_argument("归档判读需要逐字一致的精确 provider/model 持久批准。"));
    }
    Ok(model)
}

fn run(state: &SourceState, after_sync: bool) -> Result<ArchiveJudgeResult, AppError> {
    let cfg = load_settings(state)?;
    let root = state.config_path.with_file_name("archive");
    if after_sync && !crate::archive::auto_approved(&cfg) {
        return Ok(ArchiveJudgeResult { status: "disabled", ..Default::default() });
    }
    let approved_model = validate_archive_approval(&cfg)?;
    let _job = match ARCHIVE_JOB.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::Poisoned(err)) => err.into_inner(),
        Err(TryLockError::WouldBlock) => return Err(busy("已有归档判读正在运行。")),
    };
    let mut archive = Connection::open_with_flags(root.join("archive.sqlite3"), OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(AppError::from_db)?;
    archive.busy_timeout(Duration::from_secs(5)).map_err(AppError::from_db)?;
    archive.execute_batch("PRAGMA trusted_schema = OFF;").map_err(AppError::from_db)?;
    let cutoff = archive_cutoff(&archive, &cfg.me.user_id)?;
    let me = MeContext { user_id: cfg.me.user_id.trim().to_owned(), display_name: cfg.me.display_name.clone() };
    with_conn(state, |conn| {
        conn.execute_batch(JOURNAL_SCHEMA).map_err(AppError::from_db)?;
        let has_time: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('archive_judge_cursor') WHERE name='last_occurred_at')",
            [], |row| row.get(0),
        ).map_err(AppError::from_db)?;
        if !has_time {
            let tx = conn.transaction().map_err(AppError::from_db)?;
            tx.execute_batch("ALTER TABLE archive_judge_cursor ADD COLUMN last_occurred_at INTEGER NOT NULL DEFAULT 0;
                              DELETE FROM archive_judge_cursor;").map_err(AppError::from_db)?;
            tx.commit().map_err(AppError::from_db)?;
        }
        conn.execute(
            "UPDATE archive_judge_call SET apply_status='interrupted',apply_error='no_final_receipt',
             finished_at=?1 WHERE apply_status='pending' AND started_at<?2",
            params![now_ms(), now_ms()-600_000],
        ).map_err(AppError::from_db)?;
        let now = now_ms();
        let window = now.saturating_sub(7 * 86_400_000)..cutoff.min(now);
        scan(&mut archive, conn, window, &me, CALL_LIMIT, |conn, anchor| {
            let sha = input_sha256(&anchor.input);
            if already_attempted(conn, approved_model, &sha, !after_sync).map_err(AppError::from_db)? {
                return Ok(None);
            }
            // Config writers take config_lock before db_gate. Never wait for
            // config_lock while holding db_gate; restart instead on contention.
            let _auth = match state.config_lock.try_lock() {
                Ok(guard) => guard,
                Err(TryLockError::Poisoned(err)) => err.into_inner(),
                Err(TryLockError::WouldBlock) => return Err(busy("出域配置正在修改，归档判读已停止。")),
            };
            let latest = load_settings(state)?;
            let model = validate_archive_approval(&latest)?;
            if model != approved_model || latest.me.user_id.trim() != me.user_id
                || latest.me.display_name != me.display_name
                || (after_sync && !crate::archive::auto_approved(&latest))
            {
                return Err(AppError::invalid_argument("归档判读配置已变更，本轮已停止。"));
            }
            let runtime = judge::omp_runtime(&latest)?;
            let days = latest.retention.raw_body_days_bounded();
            call_and_apply(conn, &me, anchor, &sha, &runtime, i64::from(days), |input| {
                runtime.run_judge_measured(input)
            }).map(Some)
        })
    })
}

fn archive_cutoff(conn: &Connection, me: &str) -> Result<i64, AppError> {
    let (profile, unfinished, success): (Option<String>, bool, Option<String>) = conn.query_row(
        "SELECT (SELECT value FROM archive_state WHERE key='profile'), \
                EXISTS(SELECT 1 FROM archive_state WHERE key='message_window'), \
                (SELECT value FROM archive_state WHERE key='last_success_ms')",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).map_err(AppError::from_db)?;
    if !profile.as_deref().and_then(|p| p.split_once(':'))
        .is_some_and(|(corp, owner)| !corp.is_empty() && owner == me.trim())
    {
        return Err(AppError::invalid_argument("归档绑定账号与配置的本人身份不一致。"));
    }
    if unfinished {
        return Err(AppError::invalid_argument("归档同步窗口尚未完成，未发起判读。"));
    }
    success.and_then(|raw| raw.parse::<i64>().ok()).filter(|&ms| ms > 0)
        .ok_or_else(|| AppError::invalid_argument("没有成功完成的归档同步。"))
}

struct Message {
    rowid: i64,
    cid: String,
    mid: String,
    body: String,
    sender: Option<String>,
    occurred_at: i64,
    reply_to: Option<String>,
    conversation_type: ConversationType,
    mentions: Vec<String>,
}

fn message(row: &Row<'_>) -> workcontext_store::rusqlite::Result<Message> {
    let single_chat: Option<bool> = row.get(7)?;
    let kind: Option<String> = row.get(8)?;
    Ok(Message {
        mentions: row.get::<_,Option<String>>(9)?.and_then(|s|serde_json::from_str(&s).ok()).unwrap_or_default(),
        rowid: row.get(0)?, cid: row.get(1)?, mid: row.get(2)?, body: row.get(3)?,
        sender: row.get(4)?, occurred_at: row.get(5)?, reply_to: row.get(6)?,
        conversation_type: match (single_chat, kind.as_deref()) {
            (Some(true), _) | (None, Some("dm" | "single")) => ConversationType::Dm,
            (Some(false), _) | (None, Some("group")) => ConversationType::Group,
            _ => ConversationType::Other,
        },
    })
}

fn scan(
    archive: &mut Connection,
    conn: &mut Connection,
    window: std::ops::Range<i64>,
    me: &MeContext,
    limit: usize,
    mut judge: impl FnMut(&mut Connection, &Anchor) -> Result<Option<ArchiveCall>, AppError>,
) -> Result<ArchiveJudgeResult, AppError> {
    let cursor: (i64, i64) = conn.query_row(
        "SELECT last_occurred_at,last_rowid FROM archive_judge_cursor WHERE singleton=1",
        [], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional().map_err(AppError::from_db)?.unwrap_or((0, 0));
    let mut result = ArchiveJudgeResult { status: "ok", ..Default::default() };
    let mut last = cursor;
    // Fetch the bounded page before taking any model latency. Each anchor's
    // excerpts are selected from a short read snapshot, never across a model call.
    let mut stmt = archive.prepare(
        "SELECT m.rowid,m.cid,m.mid,COALESCE(substr(m.body,1,?1),''),m.sender_id,m.occurred_at,m.reply_to, \
         c.single_chat,c.conversation_type,m.mentions_json FROM messages m \
         LEFT JOIN conversations c ON c.cid=m.cid WHERE m.occurred_at>=?6 AND m.occurred_at<?2 \
         AND (m.occurred_at,m.rowid)>(?3,?4) \
         ORDER BY m.occurred_at,m.rowid LIMIT ?5",
    ).map_err(AppError::from_db)?;
    let messages = stmt.query_map(params![MESSAGE_CHARS, window.end, cursor.0, cursor.1, SCAN_LIMIT + 1, window.start], message)
        .map_err(AppError::from_db)?.collect::<Result<Vec<_>, _>>().map_err(AppError::from_db)?;
    drop(stmt);
    for msg in messages {
        if result.scanned >= SCAN_LIMIT || result.calls.len() >= limit {
            result.more = true;
            break;
        }
        result.scanned += 1;
        last = (msg.occurred_at, msg.rowid);
        if let Some(requires_existing) = action(&msg, me) {
            let anchor = {
                let tx = archive.transaction().map_err(AppError::from_db)?;
                let anchor = anchor(&tx, &msg, me, window.start, requires_existing).map_err(AppError::from_db)?;
                tx.commit().map_err(AppError::from_db)?;
                anchor
            };
            let eligible = match archive_can_judge(conn, &anchor.match_keys(), msg.occurred_at) {
                Ok(eligible) => eligible,
                Err(JudgeError::ArchiveAmbiguous) => false,
                Err(e) => return Err(AppError::internal(e)),
            };
            if !eligible {
                result.skipped_ineligible += 1;
                continue;
            }
            let call = judge(conn, &anchor)?;
            // A subsequent failure must not lose prior model-call progress.
            save_cursor(conn, last).map_err(AppError::from_db)?;
            if let Some(call) = call {
                let stop = call.stops_round();
                result.calls.push(call);
                if stop {
                    result.status = "stopped";
                    result.more = true;
                    break;
                }
            } else {
                result.skipped_unchanged += 1;
            }
        }
    }
    save_cursor(conn, if result.more { last } else { (0, 0) }).map_err(AppError::from_db)?;
    Ok(result)
}

fn save_cursor(conn: &Connection, cursor: (i64, i64)) -> workcontext_store::rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO archive_judge_cursor (singleton,last_occurred_at,last_rowid) VALUES (1,?1,?2) \
         ON CONFLICT(singleton) DO UPDATE SET last_occurred_at=excluded.last_occurred_at, last_rowid=excluded.last_rowid",
        params![cursor.0, cursor.1],
    )?;
    Ok(())
}

// Some(false): independent action. Some(true): progress that may only update an existing item.
fn action(msg: &Message, me: &MeContext) -> Option<bool> {
    if msg.cid.is_empty() || msg.mid.is_empty() || msg.occurred_at <= 0
        || msg.sender.as_deref().is_none_or(str::is_empty)
    {
        return None;
    }
    let body = msg.body.trim();
    let outgoing = msg.sender.as_deref() == Some(me.user_id.as_str());
    let addressed = msg.mentions.contains(&me.user_id) || msg.conversation_type == ConversationType::Dm
        || me.display_name.as_deref().is_some_and(|name| !name.trim().is_empty() && body.contains(name));
    let lower = body.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|word| lower.contains(word));
    let action = has(&["提交", "发给", "发我", "确认", "审核", "审批", "跟进", "更新", "交付", "处理",
        "安排", "整理", "准备", "修复", "完成", "反馈", "补充", "提供", "同步", "submit", "review",
        "send ", "confirm", "update", "deliver", "share ", "prepare", "fix "]);
    let request = has(&["请", "麻烦", "辛苦", "需要你", "能否", "帮我", "你跟进", "你负责",
        "由你", "你来", "please ", "could you ", "can you "]);
    let commitment = has(&["我来", "我会", "我负责", "我今天", "我明天", "我这边", "i will ", "i'll "]);
    let deadline = has(&["截止", "之前", "最晚", "下班前", "今天内", "明天前", "到期", "deadline", "by eod", "due "]);
    let result = completion_statement(body).is_some() || has(&["已完成", "已提交", "已发", "已修复", "已处理", "已更新", "收到", "我看过", "done", "sent", "approved"]);
    if action && ((outgoing && (commitment || request)) || (addressed && (request || deadline))
        || (request && deadline)) {
        Some(false)
    } else if msg.reply_to.is_some() && result {
        Some(true)
    } else {
        None
    }
}

// Deliberately recognize whole, affirmative statements, not substrings: a
// question, quotation, condition, negation or delivery notice is not closure.
fn completion_statement(body: &str) -> Option<&'static str> {
    let statement = body.trim().trim_end_matches(['。', '.', '!', '！']);
    if matches!(statement, "已完成" | "我已完成" | "任务已完成" | "已经完成")
        || statement.eq_ignore_ascii_case("done") || statement.eq_ignore_ascii_case("completed") {
        Some("explicit_complete")
    } else if matches!(statement, "验收通过" | "已验收通过" | "确认验收通过")
        || statement.eq_ignore_ascii_case("accepted") {
        Some("artifact_accepted")
    } else {
        None
    }
}

struct Anchor {
    input: ClassifyInput,
    context: RecordContext,
    direction: &'static str,
    reply_to: Option<String>,
    raw_mid: String,
    raw_reply: Option<String>,
    requires_existing: bool,
    reply_from_counterpart: bool,
}

impl Anchor {
    fn match_keys(&self) -> ArchiveMatch<'_> {
        ArchiveMatch {
            conversation_id: &self.input.conversation.id,
            message: (&self.context.id, &self.raw_mid),
            reply: self.reply_to.as_deref().zip(self.raw_reply.as_deref()),
            requires_existing: self.requires_existing,
        }
    }
}

fn external_id(cid: &str, mid: &str) -> String {
    format!("archive:{}:{cid}:{mid}", cid.len())
}

fn anchor(
    conn: &Connection, msg: &Message, me: &MeContext, window_start: i64, requires_existing: bool,
) -> workcontext_store::rusqlite::Result<Anchor> {
    let mut excerpts = Vec::with_capacity(5);
    let mut reply_to = None;
    let mut reply_from_counterpart = false;
    if let Some(mid) = msg.reply_to.as_ref().filter(|mid| *mid != &msg.mid) {
        let reply: Option<(Option<String>, Option<String>)> = conn.query_row(
            "SELECT substr(body,1,?1),sender_id FROM messages WHERE cid=?2 AND mid=?3 AND occurred_at BETWEEN ?5 AND ?4",
            params![NEARBY_CHARS, msg.cid, mid, msg.occurred_at, window_start],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        if let Some((body, sender)) = reply {
            reply_from_counterpart = msg.conversation_type == ConversationType::Dm
                && sender.as_deref().is_some_and(|id| !id.is_empty() && Some(id) != msg.sender.as_deref());
            reply_to = Some(external_id(&msg.cid, mid));
            if let Some(body) = body.filter(|body| !body.trim().is_empty()) {
                excerpts.push(ArchiveExcerpt { kind: ArchiveExcerptKind::Reply, body });
            }
        }
    }
    let mut earlier = conn.prepare_cached(
        "SELECT substr(body,1,?1) FROM messages WHERE cid=?2 AND occurred_at>=?3 \
         AND (occurred_at,mid)<(?4,?5) AND mid<>COALESCE(?6,'') AND trim(body)<>'' \
         ORDER BY occurred_at DESC,mid DESC LIMIT 2",
    )?;
    let after = window_start.max(msg.occurred_at.saturating_sub(NEARBY_MS));
    for row in earlier.query_map(params![NEARBY_CHARS, msg.cid, after, msg.occurred_at, msg.mid, msg.reply_to], |r| r.get(0))? {
        excerpts.push(ArchiveExcerpt { kind: ArchiveExcerptKind::Earlier, body: row? });
    }
    let mut attachments = conn.prepare_cached(
        "SELECT substr(a.text_content,1,?1) FROM attachments a JOIN messages m USING(cid,mid) \
         WHERE a.cid=?2 AND m.occurred_at BETWEEN ?9 AND ?8 \
         AND (a.mid=?3 OR a.mid=?4 OR (?5 AND m.sender_id=?6 AND m.occurred_at BETWEEN ?7 AND ?8)) \
         AND a.status='downloaded' AND a.extraction_status='extracted' AND trim(a.text_content)<>'' \
         ORDER BY (a.mid=?3) DESC, (a.mid=?4) DESC, m.occurred_at DESC, a.id LIMIT 2",
    )?;
    let refers_to_file = ["附件","表格","文档","这份","截图","文件","报表","清单"].iter().any(|word| msg.body.contains(word));
    let reply_mid = msg.reply_to.as_deref().filter(|_| reply_to.is_some());
    let attachment_params = params![ATTACHMENT_CHARS, msg.cid, msg.mid, reply_mid, refers_to_file,
        msg.sender, msg.occurred_at.saturating_sub(600_000), msg.occurred_at, window_start];
    for row in attachments.query_map(attachment_params, |r| r.get(0))? {
        excerpts.push(ArchiveExcerpt { kind: ArchiveExcerptKind::Attachment, body: row? });
    }
    let id = external_id(&msg.cid, &msg.mid);
    let (direction, direction_tag) = if msg.sender.as_deref() == Some(me.user_id.as_str()) {
        (Direction::Out, "out")
    } else {
        (Direction::In, "in")
    };
    let conversation_type = match msg.conversation_type {
        ConversationType::Dm => "dm", ConversationType::Group => "group", ConversationType::Other => "other",
    };
    let record = InputRecord {
        id: id.clone(), direction: direction_tag.into(), sender_id: msg.sender.clone().unwrap_or_default(),
        sender_name: String::new(), mentions: msg.mentions.clone(), reply_to: reply_to.clone(),
        occurred_at: format_rfc3339_ms(msg.occurred_at), body: msg.body.clone(),
    };
    let mut input = ClassifyInput::new(
        SourceRef { id: SOURCE_ID.into(), kind: "import".into() },
        ConversationRef { id: msg.cid.clone(), conversation_type: conversation_type.into(), display_name: String::new() },
        PersonRef { user_id: me.user_id.clone(), display_name: me.display_name.clone().unwrap_or_default() },
        Vec::new(), vec![record],
    );
    input.archive_context = Some(excerpts);
    Ok(Anchor {
        input, direction: direction_tag, reply_to,
        raw_mid: msg.mid.clone(), raw_reply: msg.reply_to.clone(), requires_existing, reply_from_counterpart,
        context: RecordContext {
            id: id.clone(), source_id: SOURCE_ID.into(), external_id: id, kind: RecordKind::ImMessage,
            conversation_id: Some(msg.cid.clone()), conversation_type: msg.conversation_type, direction,
            sender_id: msg.sender.clone(), mentions: msg.mentions.clone(), occurred_at: msg.occurred_at,
            ingested_at: now_ms(), summary: None, body: Some(msg.body.clone()),
        },
    })
}

fn already_attempted(conn: &Connection, model: &str, sha: &str, retry: bool) -> workcontext_store::rusqlite::Result<bool> {
    let latest: Option<(Option<String>, String)> = conn.query_row(
        "SELECT cli_status, apply_status FROM archive_judge_call WHERE model_id=?1 AND input_sha256=?2 ORDER BY id DESC LIMIT 1",
        params![model, sha], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?;
    Ok(latest.is_some_and(|(cli, applied)| {
        !(retry && applied != "ok" && matches!(cli.as_deref(), Some("failed" | "timeout")))
    }))
}

fn store_anchor(conn: &Connection, anchor: &Anchor, sha: &str, retention_days: i64) -> workcontext_store::rusqlite::Result<()> {
    let c = &anchor.context;
    conn.execute(
        "INSERT INTO source (id,kind,display_name,scope_json,enabled,created_at) \
         VALUES (?1,'import','钉钉会话归档','{}',0,?2) ON CONFLICT(id) DO NOTHING",
        params![SOURCE_ID, c.ingested_at],
    )?;
    conn.execute(
        "INSERT INTO source_record (id,source_id,external_id,kind,occurred_at,ingested_at,conversation_id, \
         sender_id,direction,body,content_hash,judge_state,mentions_json,reply_to_id,conversation_type,retention_at) \
         VALUES (?1,?2,?1,'im_message',?3,?4,?5,?6,?7,?8,?9,'kept',?10,?11,?12,?13) \
         ON CONFLICT(source_id,external_id) DO UPDATE SET occurred_at=excluded.occurred_at, \
         sender_id=excluded.sender_id,direction=excluded.direction, \
         body=CASE WHEN source_record.raw_state='available' THEN excluded.body ELSE source_record.body END, \
         content_hash=excluded.content_hash,mentions_json=excluded.mentions_json, \
         reply_to_id=excluded.reply_to_id,conversation_type=excluded.conversation_type",
        params![c.id, SOURCE_ID, c.occurred_at, c.ingested_at, c.conversation_id, c.sender_id,
            anchor.direction, c.body, sha, serde_json::to_string(&c.mentions).expect("IDs serialize"),
            anchor.reply_to, anchor.input.conversation.conversation_type, c.ingested_at + retention_days * 86_400_000],
    )?;
    Ok(())
}

// A known source ID establishes provenance, not that the message actually
// says what the model claims. Only an explicit reply in a matched task can
// propose closure. The shared validator still owns the final state transition.
fn archive_outcome(anchor: &Anchor, outcome: CliAgentOutcome) -> RuntimeOutcome {
    let translated = classify_omp_outcome(&anchor.input, outcome);
    if let RuntimeOutcome::Ok(output) = &translated {
        if let Some(kind @ ("explicit_complete" | "artifact_accepted")) =
            output.pointer("/resolution/evidence_type").and_then(|v| v.as_str())
        {
            let body = anchor.context.body.as_deref().unwrap_or_default();
            if !anchor.requires_existing || anchor.reply_to.is_none()
                || body.chars().count() >= MESSAGE_CHARS || completion_statement(body) != Some(kind)
                || (kind == "artifact_accepted" && !anchor.reply_from_counterpart)
            {
                return RuntimeOutcome::Rejected { code: "ARCHIVE_RESOLUTION_UNSUPPORTED".into() };
            }
        }
    }
    translated
}

fn call_and_apply(
    conn: &mut Connection,
    me: &MeContext,
    anchor: &Anchor,
    sha: &str,
    runtime: &OmpCliRuntime,
    retention_days: i64,
    invoke: impl FnOnce(&ClassifyInput) -> (CliAgentOutcome, workcontext_infra::omp::CallUsage),
) -> Result<ArchiveCall, AppError> {
    let model = runtime.model_id();
    let ids = std::slice::from_ref(&anchor.context.id);
    let call_id = {
        let tx = conn.transaction().map_err(AppError::from_db)?;
        store_anchor(&tx, anchor, sha, retention_days).map_err(AppError::from_db)?;
        let id = tx.query_row(
            "INSERT INTO archive_judge_call (model_id,input_sha256,source_record_id,started_at) \
             VALUES (?1,?2,?3,?4) RETURNING id", params![model, sha, ids[0], now_ms()], |row| row.get(0),
        ).map_err(AppError::from_db)?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS archive_judge_context (
            call_id INTEGER PRIMARY KEY REFERENCES archive_judge_call(id),
            excerpts INTEGER NOT NULL, attachments INTEGER NOT NULL)").map_err(AppError::from_db)?;
        let excerpts = anchor.input.archive_context.as_deref().unwrap_or_default();
        let attachments = excerpts.iter().filter(|e| e.kind == ArchiveExcerptKind::Attachment).count();
        tx.execute("INSERT INTO archive_judge_context VALUES (?1,?2,?3)",params![id,excerpts.len(),attachments]).map_err(AppError::from_db)?;
        tx.commit().map_err(AppError::from_db)?;
        id
    };
    let (outcome, usage) = invoke(&anchor.input);
    let cli_status = outcome.status();
    let cli_error = outcome.error_code();
    let now = now_ms();
    conn.execute(
        "UPDATE archive_judge_call SET finished_at=?2,cli_status=?3,cli_error=?4,process_started=?5, \
         reported_model=?6,session_ref=?7,tokens_in=?8,tokens_out=?9,cost_usd=?10,latency_ms=?11 WHERE id=?1",
        params![call_id, now, cli_status, cli_error, usage.process_started, usage.reported_model,
            usage.session_ref, usage.tokens_in.and_then(|n| i64::try_from(n).ok()),
            usage.tokens_out.and_then(|n| i64::try_from(n).ok()), usage.cost_usd, usage.latency_ms],
    ).map_err(AppError::from_db)?;
    let blocked = matches!(outcome, CliAgentOutcome::Blocked { .. });
    let outcome = archive_outcome(anchor, outcome);
    let apply_rejection = match &outcome {
        RuntimeOutcome::Rejected { code } => Some(code.clone()),
        _ => None,
    };
    let result = apply_classified_records(
        conn, runtime, &PolicyValidator::new(), me, &anchor.input, std::slice::from_ref(&anchor.context),
        ids, outcome, now, Some(&anchor.match_keys()),
    );
    let (status, agent_run_id, item_id, error_code) = match result {
        Ok(report) => {
            let run_id = (!report.run_id.is_empty() && report.skipped_reason != Some("idempotent_hit"))
                .then_some(report.run_id);
            let error = cli_error.or(apply_rejection).or(report.rejection_rule).or_else(|| {
                (report.skipped_reason == Some("runtime_failure")).then(|| "AGENT_OUTPUT_REJECTED".into())
            });
            (report.status, run_id, report.item_id, error)
        }
        Err(JudgeError::ArchiveStale | JudgeError::ArchiveDetached) => {
            ("skipped", None, None, Some("ARCHIVE_JUDGE_EVIDENCE_CHANGED".into()))
        }
        Err(JudgeError::ArchiveAmbiguous) => {
            ("skipped", None, None, Some("ARCHIVE_JUDGE_AMBIGUOUS".into()))
        }
        Err(_) => ("apply_failed", None, None, Some("ARCHIVE_JUDGE_APPLY_FAILED".into())),
    };
    let tx = conn.transaction().map_err(AppError::from_db)?;
    tx.execute(
        "UPDATE archive_judge_call SET apply_status=?2, agent_run_id=?3, apply_error=?4 WHERE id=?1",
        params![call_id, status, agent_run_id, error_code],
    ).map_err(AppError::from_db)?;
    if let Some(run_id) = &agent_run_id {
        tx.execute(
            "UPDATE agent_run SET session_ref=?2, latency_ms=?3 WHERE id=?1",
            params![run_id, usage.session_ref, usage.latency_ms.map(|ms| ms.round() as i64)],
        ).map_err(AppError::from_db)?;
    }
    if blocked {
        egress::log_blocked(&tx, now, "full", Some(model), ids, agent_run_id.as_deref()).map_err(AppError::from_db)?;
    } else if let Some(run_id) = agent_run_id.as_deref().filter(|_| usage.process_started) {
        egress::log_allowed(&tx, now, "full", Some(model), ids, run_id).map_err(AppError::from_db)?;
    }
    tx.commit().map_err(AppError::from_db)?;
    Ok(ArchiveCall { call_id, agent_run_id, status, error_code, item_id,
        tokens_in: usage.tokens_in, tokens_out: usage.tokens_out })
}

#[cfg(test)]
#[path = "archive_judge_tests.rs"]
mod tests;
