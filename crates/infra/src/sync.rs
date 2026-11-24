//! B3 最小可用离线同步引擎（详细设计 §6、§4.4；遗留事项 B3）。
//!
//! 本里程碑**不连接真实 DWS**：来源全部是实现 [`DingTalkSource`] 的进程内
//! mock（[`MockSource`]），判读与网络均不涉及。真实适配器属 M6/B7，接入时
//! `fetch` 才需要 async 边界——分页、事务、游标语义全部在本引擎内，与 I/O
//! 形态无关，因此当前 trait 为同步接口（与 `mock.rs` 同一取舍）。
//!
//! 硬规则（§6.2）与落点：
//! 1. **游标只在整轮成功后推进**：每页一个事务（§8 `sync_source`），但
//!    `sync_checkpoint.cursor_json` 只在最后一页提交的同一事务里更新；中途
//!    失败/超时/截断 → 游标原地不动，下一轮从旧游标重拉，重复由
//!    `UNIQUE(source_id, external_id)` + `content_hash` 吸收。
//! 2. **不完整页不冒进**：`has_more=true` 却没有 `next_cursor`（截断、无安全
//!    续点）或游标原地踏步（`cursor_stalled`）→ 整轮按失败处理，可疑页的
//!    记录也不落库（截断页内容可能残缺，宁重拉不存疑）。
//! 3. **鉴权失败不冒进也不降级**：401/403 → `state='auth_required'`，不重试、
//!    不清游标、不累计失败；再次 `sync_source` 直接拒绝（不触来源），待
//!    [`SyncEngine::reset`]（用户重新授权后的手动入口）恢复。
//! 4. **连续超时降级**：可重试失败（超时/限流/其它来源错误/不完整页）连续
//!    达到 `max_consecutive_failures`（默认 5）→ `state='degraded'`，引擎拒绝
//!    自动重试；恢复同样走 [`SyncEngine::reset`]。
//! 5. **退避不睡眠**：延迟由 [`backoff_delay_ms`]（1/4/16/60/300s ±20% 抖动）
//!    纯函数给出，引擎只把 `consecutive_failures` 落库；定时唤醒属 app 层
//!    调度器，测试因此全程瞬时。
//! 6. **单轮上限**：`max_pages` / `max_duration`（默认 10 min），超限以
//!    `partial` 结束且游标不动，等下一轮从旧游标继续（重复由唯一键吸收）。
//! 7. **白名单先于落库**：`source.scope_json` 是显式会话/人白名单，来源返回
//!    但不在白名单内的记录直接丢弃（计数 `out_of_scope`，**不入库**——未经
//!    授权的数据不进工作库）；空白名单 = 全部拒绝（fail closed）。
//! 8. **判读元数据单独比对**（迁移 0002）：`RawRecord` 的 `mentions` /
//!    `reply_to_id` / `conversation_type` **不进入** `content_hash`（正文幂等
//!    键口径不变、已落库哈希稳定）；`content_hash` 相同而元数据变化时只更新
//!    这三列并计 `updated`，再次重拉收敛为 `unchanged`。
//! 9. **保留期随入库落地**（§14.3）：`retention_at = 首次入库时点 +
//!    rawBodyDays`（默认 30 天），`retention_sweep` 据此到期置空正文；重复
//!    同步不延期，已清理的正文不因重拉复活。
//!
//! 预筛（§6.3）与判读不在本模块：入库记录一律 `judge_state='pending'`，
//! `sync_run.kept_for_judgement = inserted`，等 Prefilter/判读层接入后收窄。

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

/// §6.2 退避基准序列（ms）：1s, 4s, 16s, 60s, 300s。
pub const BACKOFF_SCHEDULE_MS: [u64; 5] = [1_000, 4_000, 16_000, 60_000, 300_000];
/// 退避抖动幅度（±20%，§6.2）。
pub const JITTER_FRACTION: f64 = 0.2;
/// §6.1 `overlap_ms` 默认值：6h。宁可多拉，不可漏拉。
pub const OVERLAP_MS_6H: i64 = 6 * 60 * 60 * 1000;

// ---------------------------------------------------------------------------
// 来源侧类型
// ---------------------------------------------------------------------------

/// 数据源白名单（`source.scope_json` 的规范形）：会话（群/私聊）与人两个维度。
///
/// 记录只要命中任一维度即在授权范围内（见 [`Scope::allows`]）；两个集合都为
/// 空时**不放行任何记录**——白名单是显式授权边界，不是可选项。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    /// 会话/群 ID 白名单。
    #[serde(default)]
    pub conversations: BTreeSet<String>,
    /// 人 ID 白名单（私聊对端）。
    #[serde(default)]
    pub people: BTreeSet<String>,
}

impl Scope {
    /// 记录是否在白名单内：会话命中，或发送者命中（私聊场景）。
    pub fn allows(&self, record: &RawRecord) -> bool {
        record
            .conversation_id
            .as_deref()
            .is_some_and(|c| self.conversations.contains(c))
            || record
                .sender_id
                .as_deref()
                .is_some_and(|p| self.people.contains(p))
    }

    /// 序列化为 `source.scope_json`（键序确定的紧凑 JSON）。
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("Scope 序列化不会失败")
    }
}

/// `source_record.kind` 的取值（迁移 0001 的 CHECK 集合）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    ImMessage,
    WeeklyReport,
    File,
    ManualNote,
}

impl RecordKind {
    /// 数据库存储名（与迁移 0001 的 CHECK 一致）。
    pub fn as_str(&self) -> &'static str {
        match self {
            RecordKind::ImMessage => "im_message",
            RecordKind::WeeklyReport => "weekly_report",
            RecordKind::File => "file",
            RecordKind::ManualNote => "manual_note",
        }
    }
}

/// 消息方向（`source_record.direction` 的 CHECK 集合）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    In,
    Out,
    Na,
}

impl Direction {
    /// 数据库存储名（与迁移 0001 的 CHECK 一致）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Direction::In => "in",
            Direction::Out => "out",
            Direction::Na => "na",
        }
    }
}

/// 会话类型（迁移 0002 `source_record.conversation_type` 的 CHECK 集合；
/// V03 归属锚点①：`dm` 表示私聊对端、`group` 表示群会话）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationType {
    Dm,
    Group,
    Other,
}

impl ConversationType {
    /// 数据库存储名（与迁移 0002 的 CHECK 一致）。
    pub fn as_str(&self) -> &'static str {
        match self {
            ConversationType::Dm => "dm",
            ConversationType::Group => "group",
            ConversationType::Other => "other",
        }
    }
}

/// 来源返回的一条原始记录（落库为 `source_record` 的一行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRecord {
    pub external_id: String,
    pub kind: RecordKind,
    /// UTC ms。
    pub occurred_at: i64,
    pub conversation_id: Option<String>,
    pub sender_id: Option<String>,
    pub sender_name: Option<String>,
    pub direction: Direction,
    pub summary: Option<String>,
    pub body: Option<String>,
    pub perm_scope: Option<String>,
    /// 被 @ 的 user_id 列表（迁移 0002 `mentions_json`；预筛 `at_me` 的依据）。
    pub mentions: Vec<String>,
    /// 同源被引用消息的 external_id（迁移 0002；「引用我」与续链的显式依据）。
    pub reply_to_id: Option<String>,
    /// 会话类型（迁移 0002；V03 归属锚点①），缺省视为无元数据。
    pub conversation_type: Option<ConversationType>,
 }

impl RawRecord {
    /// 测试/夹具用的便捷构造：一条 IM 消息，其余字段留默认。
    pub fn im(
        external_id: &str,
        occurred_at: i64,
        conversation_id: &str,
        sender_id: &str,
        direction: Direction,
        body: &str,
    ) -> Self {
        Self {
            external_id: external_id.to_string(),
            kind: RecordKind::ImMessage,
            occurred_at,
            conversation_id: Some(conversation_id.to_string()),
            sender_id: Some(sender_id.to_string()),
            sender_name: None,
            direction,
            summary: None,
            body: Some(body.to_string()),
            perm_scope: None,
            mentions: Vec::new(),
            reply_to_id: None,
            conversation_type: None,
        }
    }

    /// `mentions_json` 存储形：紧凑 JSON 数组（迁移 0002 的 `NOT NULL DEFAULT '[]'`）。
    pub fn mentions_json(&self) -> String {
        serde_json::to_string(&self.mentions).expect("字符串数组序列化不会失败")
    }

    /// 判读元数据三元组是否与库中存储一致（列序：mentions/reply/type）。
    fn meta_matches(
        &self,
        stored_mentions: &str,
        stored_reply: Option<&str>,
        stored_conv: Option<&str>,
    ) -> bool {
        stored_mentions == self.mentions_json()
            && stored_reply == self.reply_to_id.as_deref()
            && stored_conv == self.conversation_type.map(|c| c.as_str())
    }

    /// `body_sha256`：正文原文（非 NFC）的 SHA-256 小写十六进制。
    pub fn body_sha256(&self) -> Option<String> {
        self.body.as_deref().map(|b| sha256_hex(b.as_bytes()))
    }

    /// §4.4 幂等键原料：字段按固定顺序 NFC 规范化后以 `\u{1f}` 拼接再哈希。
    ///
    /// 规则确定且可独立复算（与 `classify_input::canonical_json` 同一精神），
    /// 同一记录重拉必得同一 `content_hash` → 计入 `unchanged` 而非重复入库。
    /// 判读元数据（mentions/reply_to/conversation_type）**有意不参与**本哈希：
    /// 已落库哈希保持稳定，元数据变化由 [`RawRecord::meta_matches`] 单独比对，
    /// 在 upsert 内走独立更新路径。
    pub fn content_hash(&self) -> String {
        let nfc = |s: Option<&str>| s.unwrap_or("").nfc().collect::<String>();
        let mut buf = String::new();
        let fields: [String; 8] = [
            self.external_id.nfc().collect(),
            self.kind.as_str().to_string(),
            self.occurred_at.to_string(),
            nfc(self.conversation_id.as_deref()),
            nfc(self.sender_id.as_deref()),
            self.direction.as_str().to_string(),
            nfc(self.summary.as_deref()),
            nfc(self.body.as_deref()),
        ];
        for (i, f) in fields.iter().enumerate() {
            if i > 0 {
                buf.push('\u{1f}');
            }
            buf.push_str(f);
        }
        sha256_hex(buf.as_bytes())
    }
}

/// 一页拉取结果（§6.1 `RecordPage`）。
///
/// `next_cursor` 对工作台是**不透明 JSON**：引擎原样保存/回传，绝不解析或
/// 猜测性构造。`snapshot_at` 为来源侧快照时间，成功轮写进
/// `sync_checkpoint.last_success_at`。
#[derive(Debug, Clone, PartialEq)]
pub struct RecordPage {
    pub records: Vec<RawRecord>,
    pub next_cursor: Option<Value>,
    pub has_more: bool,
    pub snapshot_at: i64,
}

/// 拉取页参数（§6.1 `Page`）：页大小 + 重叠窗口（默认 6h）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageParams {
    pub size: u16,
    pub overlap_ms: i64,
}

/// 来源侧错误分类（§6.2 规则 3/4/5 的输入）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SourceError {
    /// 401/403：鉴权失败 → `auth_required`，不重试、不清游标、不累计失败。
    #[error("鉴权失败（HTTP {status}）：{message}")]
    Auth { status: u16, message: String },
    /// 超时：可重试失败，计入连续失败。
    #[error("请求超时：{message}")]
    Timeout { message: String },
    /// 429：尊重 `retry_after_ms`（上限 10 min）；引擎不睡眠，延迟由调度方执行。
    #[error("限流（HTTP 429）：{message}（Retry-After {retry_after_ms:?}ms）")]
    RateLimited {
        message: String,
        retry_after_ms: Option<u64>,
    },
    /// 其余来源错误：按可重试失败处理。
    #[error("来源错误：{message}")]
    Other { message: String },
}

impl SourceError {
    /// 是否鉴权类错误（决定 `auth_required` 路径）。
    fn is_auth(&self) -> bool {
        matches!(self, SourceError::Auth { .. })
    }

    /// 落库错误码（`sync_checkpoint.last_error_code` / `sync_run.error_code`）。
    fn code(&self) -> &'static str {
        match self {
            SourceError::Auth { .. } => "auth",
            SourceError::Timeout { .. } => "timeout",
            SourceError::RateLimited { .. } => "rate_limited",
            SourceError::Other { .. } => "source_error",
        }
    }
}

/// DWS 拉取接口（§6.1；当前仅进程内 mock 实现，见模块文档）。
pub trait DingTalkSource: Send + Sync {
    /// 与 `source.id` 一致的来源标识。
    fn id(&self) -> &str;
    /// 白名单（`source.scope_json`），引擎在落库前按它过滤。
    fn scope(&self) -> &Scope;
    /// 授权有效性探测。同步循环不调用它——`fetch` 的 [`SourceError::Auth`]
    /// 才是权威信号；本方法供 UI/启动流程展示「需要重新授权」。
    fn probe(&self) -> Result<(), SourceError>;
    /// 从 `cursor`（上次整轮成功保存的不透明 JSON，首轮为 `None`）拉下一页。
    /// `scope` 是该来源的显式白名单（与 `source.scope_json` 一致），
    /// **在 fetch 前传入**：实现必须用它约束拉取范围（真实适配器可下推为
    /// 服务端过滤）；引擎仍会在落库前强制丢弃范围外记录（防御纵深，见
    /// 模块文档规则 7）。
    fn fetch(
        &self,
        scope: &Scope,
        cursor: Option<&Value>,
        page: &PageParams,
    ) -> Result<RecordPage, SourceError>;
}

// ---------------------------------------------------------------------------
// 检查点 / 结果类型
// ---------------------------------------------------------------------------

/// `sync_checkpoint.state` 的状态机取值（迁移 0001 的 CHECK 集合）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointState {
    Idle,
    Syncing,
    Degraded,
    AuthRequired,
    Disabled,
}

impl CheckpointState {
    /// 数据库存储名。
    pub fn as_str(&self) -> &'static str {
        match self {
            CheckpointState::Idle => "idle",
            CheckpointState::Syncing => "syncing",
            CheckpointState::Degraded => "degraded",
            CheckpointState::AuthRequired => "auth_required",
            CheckpointState::Disabled => "disabled",
        }
    }

    /// 从数据库值还原；`None` 表示不在 CHECK 集合内（库被外力改写）。
    fn from_db(s: &str) -> Option<Self> {
        Some(match s {
            "idle" => CheckpointState::Idle,
            "syncing" => CheckpointState::Syncing,
            "degraded" => CheckpointState::Degraded,
            "auth_required" => CheckpointState::AuthRequired,
            "disabled" => CheckpointState::Disabled,
            _ => return None,
        })
    }
}

/// 一轮同步的检查点快照（`sync_checkpoint` 一行的只读视图）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub source_id: String,
    pub cursor_json: Option<String>,
    pub cursor_at: Option<i64>,
    pub last_attempt_at: Option<i64>,
    pub last_success_at: Option<i64>,
    pub consecutive_failures: u32,
    pub state: CheckpointState,
    pub last_error_code: Option<String>,
    pub last_error_msg: Option<String>,
}

/// 一轮同步的结局。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStatus {
    /// 整轮完成：所有页入库且最终事务提交，游标已推进。
    Ok,
    /// 达到单轮上限（页数/时长）：游标未动，下一轮从旧游标继续。
    Partial,
    /// 可重试失败或引擎拦截的不完整页/游标停滞；见 `SyncOutcome::error`。
    Failed,
    /// 鉴权失败（或上轮遗留 `auth_required` 直接拒绝）：游标不动、不重试。
    AuthRequired,
    /// 已降级，自动同步被拒绝（未触来源）；待 [`SyncEngine::reset`]。
    Degraded,
    /// 来源被停用（`state='disabled'`，app 层 `source_set_enabled` 写入）。
    Disabled,
}

/// 失败详情（`sync_checkpoint.last_error_*` / `sync_run.error_*` 的内存形）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncErrorInfo {
    pub code: String,
    pub message: String,
}

/// 一轮同步的可观测结果（同时持久化在 `sync_run`，本结构便于断言与回传 UI）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOutcome {
    pub source_id: String,
    /// 本轮 `sync_run.id`；拒绝路径（Disabled/Degraded/AuthRequired 遗留）没有 run。
    pub run_id: Option<String>,
    pub status: SyncStatus,
    /// 完成的页数。
    pub pages: usize,
    /// 来源返回的记录总数（含白名单外）。
    pub fetched: usize,
    /// 白名单外被丢弃、未落库的条数。
    pub out_of_scope: usize,
    pub inserted: usize,
    /// 已存在且 `content_hash` 相同（§4.4「相同则跳过」）。
    pub unchanged: usize,
    /// 已存在但内容变化（§4.4「不同则更新」）。`sync_run` 无对应列（迁移
    /// 0001 已定），更新体现在 `source_record` 行本身。
    pub updated: usize,
    /// 游标是否推进（仅 [`SyncStatus::Ok`]）。
    pub cursor_advanced: bool,
    pub error: Option<SyncErrorInfo>,
}

/// 引擎自身的失败（来源失败是 [`SyncOutcome`] 的正常业务结果，不在此列）。
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("数据库错误：{0}")]
    Db(#[from] rusqlite::Error),
    #[error("来源未注册：{0}")]
    UnknownSource(String),
    #[error("检查点游标损坏（cursor_json 非合法 JSON）：{0}")]
    CorruptCursor(String),
    #[error("检查点状态非法（不在 CHECK 集合内）：{0}")]
    CorruptState(String),
}

// ---------------------------------------------------------------------------
// 退避（纯函数，引擎不睡眠）
// ---------------------------------------------------------------------------

/// 第 `consecutive_failures` 次失败后的下次自动重试延迟（§6.2 规则 3）。
///
/// - 序列取 [`BACKOFF_SCHEDULE_MS`]，失败次数超出后钳制在 300s；
/// - `jitter_unit ∈ [0.0, 1.0]` 是抖动抽样的归一化值：`0` → 下界（−20%）、
///   `0.5` → 无抖动、`1` → 上界（+20%），越界按最近端钳制；
/// - 引擎不睡眠：调度方取本值后自行定时唤醒（429 场景优先取
///   `Retry-After`，上限 10 min）。
pub fn backoff_delay_ms(consecutive_failures: u32, jitter_unit: f64) -> u64 {
    let idx = (consecutive_failures.saturating_sub(1) as usize).min(BACKOFF_SCHEDULE_MS.len() - 1);
    let base = BACKOFF_SCHEDULE_MS[idx] as f64;
    let unit = jitter_unit.clamp(0.0, 1.0);
    let factor = 1.0 + (unit * 2.0 - 1.0) * JITTER_FRACTION;
    (base * factor).round() as u64
}

// ---------------------------------------------------------------------------
// 引擎
// ---------------------------------------------------------------------------

/// 正文保留期默认值：30 天（§13.1 `retention.rawBodyDays`、§14.3）。
pub const DEFAULT_RAW_BODY_DAYS: u32 = 30;

/// 一天的毫秒数（保留期换算）。
const MS_PER_DAY: i64 = 86_400_000;

/// 引擎可调参数（§6.2 规则 5/6 的落点）。
#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// 单页请求的记录数上限（§6.2 循环 `size=100`）。
    pub page_size: u16,
    /// 传给来源的重叠窗口（§6.1，默认 6h）。
    pub overlap_ms: i64,
    /// 单轮页数上限：超过以 `partial` 结束且游标不动。
    pub max_pages: usize,
    /// 单轮时长上限（默认 10 min）：至少完成一页后才判定。
    pub max_duration: Duration,
    /// 连续失败达到该值 → `degraded`（§6.2 规则 3：默认 5）。
    pub max_consecutive_failures: u32,
    /// 采集正文的保留天数（`config.retention.rawBodyDays`，默认
    /// [`DEFAULT_RAW_BODY_DAYS`]）：入库时写 `retention_at = 首次入库时点 +
    /// rawBodyDays`，供 `retention_sweep` 到期置空正文。重复同步不延期。
    pub raw_body_days: u32,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            page_size: 100,
            overlap_ms: OVERLAP_MS_6H,
            max_pages: 600,
            max_duration: Duration::from_secs(600),
            max_consecutive_failures: 5,
            raw_body_days: DEFAULT_RAW_BODY_DAYS,
        }
    }
}

/// 离线同步引擎：单连接串行执行（§6.2 规则 2 的单源串行；多源并行调度属
/// app 层，本引擎一次只碰一个来源，连接无需共享）。
pub struct SyncEngine {
    db: Mutex<Connection>,
    sources: Mutex<BTreeMap<String, Arc<dyn DingTalkSource>>>,
    config: SyncConfig,
}

impl SyncEngine {
    /// 绑定工作库连接与参数；来源随后逐个 [`SyncEngine::register`]。
    pub fn new(db: Connection, config: SyncConfig) -> Self {
        Self {
            db: Mutex::new(db),
            sources: Mutex::new(BTreeMap::new()),
            config,
        }
    }

    /// 注册来源：写入/刷新 `source` 行（`kind='dws_im'`，白名单取
    /// `source.scope()`）并确保 `sync_checkpoint` 存在。幂等：重复注册只更新
    /// `display_name` / `scope_json`，不覆盖 `enabled`/`authorized_at` 等用户
    /// 决策字段。
    pub fn register(
        &self,
        source: Arc<dyn DingTalkSource>,
        display_name: &str,
        now_ms: i64,
    ) -> Result<(), SyncError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO source(id, kind, display_name, scope_json, enabled, authorized_at, created_at)
             VALUES(?1, 'dws_im', ?2, ?3, 1, ?4, ?4)
             ON CONFLICT(id) DO UPDATE SET display_name = excluded.display_name,
                                          scope_json = excluded.scope_json",
            params![source.id(), display_name, source.scope().to_json(), now_ms],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO sync_checkpoint(source_id, state, consecutive_failures, updated_at)
             VALUES(?1, 'idle', 0, ?2)",
            params![source.id(), now_ms],
        )?;
        tx.commit()?;
        self.lock_sources().insert(source.id().to_string(), source);
        Ok(())
    }

    /// 同步单个来源（一轮）。拒绝路径（disabled/degraded/auth_required 遗留）
    /// 不触来源、不写库，直接以对应 [`SyncStatus`] 返回。
    pub fn sync_source(&self, source_id: &str, now_ms: i64) -> Result<SyncOutcome, SyncError> {
        let source = self
            .lock_sources()
            .get(source_id)
            .map(Arc::clone)
            .ok_or_else(|| SyncError::UnknownSource(source_id.to_string()))?;
        let mut conn = self.lock();

        let cp = self.load_checkpoint(&conn, source_id, now_ms)?;
        if let Some(status) = match cp.state {
            CheckpointState::Disabled => Some(SyncStatus::Disabled),
            CheckpointState::Degraded => Some(SyncStatus::Degraded),
            CheckpointState::AuthRequired => Some(SyncStatus::AuthRequired),
            CheckpointState::Idle | CheckpointState::Syncing => None,
        } {
            return Ok(refused(source_id, status));
        }

        let started = Instant::now();
        let run_id = {
            let tx = conn.transaction()?;
            // 接管上一轮崩溃残留的 'running' 行：单连接串行保证此刻没有并发轮。
            tx.execute(
                "UPDATE sync_run SET state='failed', finished_at=?2, error_code='abandoned',
                     error_msg='被下一轮同步接管（上一轮未正常收尾）'
                 WHERE source_id=?1 AND state='running'",
                params![source_id, now_ms],
            )?;
            // run id 由 SQLite 生成 128 位随机十六进制：不依赖进程内存计数器，
            // 跨重启/跨进程/同库多引擎都不会撞主键。
            let run_id: String =
                tx.query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))?;
            tx.execute(
                "INSERT INTO sync_run(id, source_id, started_at, state)
                 VALUES(?1, ?2, ?3, 'running')",
                params![run_id, source_id, now_ms],
            )?;
            tx.execute(
                "UPDATE sync_checkpoint SET state='syncing', last_attempt_at=?2, updated_at=?2
                 WHERE source_id=?1",
                params![source_id, now_ms],
            )?;
            tx.commit()?;
            run_id
        };
        let start_cursor: Option<Value> = match cp.cursor_json.as_deref() {
            Some(raw) => Some(serde_json::from_str(raw).map_err(|e| {
                SyncError::CorruptCursor(format!("{source_id}: {e}"))
            })?),
            None => None,
        };

        let page_params = PageParams {
            size: self.config.page_size,
            overlap_ms: self.config.overlap_ms,
        };
        let mut cursor = start_cursor;
        let mut st = Counters::default();
        let mut pages = 0usize;
        // 结局三选一：整轮成功 / 达到单轮上限 / 失败。
        let mut completed: Option<RecordPage> = None;
        let mut partial = false;
        let mut failure: Option<Failure> = None;

        while failure.is_none() && completed.is_none() {
            if pages >= self.config.max_pages {
                partial = true;
                break;
            }
            if pages > 0 && started.elapsed() >= self.config.max_duration {
                partial = true;
                break;
            }
            let passed = cursor.clone();
            let page = match source.fetch(source.scope(), passed.as_ref(), &page_params) {
                Ok(page) => page,
                Err(err) => {
                    failure = Some(Failure {
                        info: SyncErrorInfo {
                            code: err.code().to_string(),
                            message: err.to_string(),
                        },
                        is_auth: err.is_auth(),
                    });
                    break;
                }
            };
            pages += 1;
            st.fetched += page.records.len();

            // 形状校验先于入库（模块文档规则 2）：截断/停滞页的记录不落库。
            if page.has_more {
                match &page.next_cursor {
                    None => {
                        failure = Some(Failure {
                            info: SyncErrorInfo {
                                code: "incomplete_page".into(),
                                message: format!(
                                    "第 {pages} 页 has_more=true 但无 next_cursor，无安全续点，不冒进"
                                ),
                            },
                            is_auth: false,
                        });
                        break;
                    }
                    Some(next) if Some(next) == passed.as_ref() => {
                        failure = Some(Failure {
                            info: SyncErrorInfo {
                                code: "cursor_stalled".into(),
                                message: format!(
                                    "第 {pages} 页游标原地踏步（{next}），来源无进展，不冒进"
                                ),
                            },
                            is_auth: false,
                        });
                        break;
                    }
                    Some(_) => {}
                }
            }

            // 单页一事务：白名单过滤 + 幂等 upsert，整页成功或整页回滚。
            let tx = conn.transaction()?;
            for record in &page.records {
                if !source.scope().allows(record) {
                    st.out_of_scope += 1;
                    continue;
                }
                upsert_record(&tx, source_id, record, now_ms, self.config.raw_body_days, &mut st)?;
            }
            tx.commit()?;

            if page.has_more {
                cursor = page.next_cursor;
            } else {
                completed = Some(page);
            }
        }

        let ctx = RunCtx { source_id, run_id: &run_id, now_ms };
        match (completed, partial, failure) {
            (Some(page), _, _) => {
                self.finish_ok(&mut conn, &ctx, &cp, &page, &st)?;
                Ok(SyncOutcome {
                    source_id: source_id.to_string(),
                    run_id: Some(run_id),
                    status: SyncStatus::Ok,
                    pages,
                    fetched: st.fetched,
                    out_of_scope: st.out_of_scope,
                    inserted: st.inserted,
                    unchanged: st.unchanged,
                    updated: st.updated,
                    cursor_advanced: true,
                    error: None,
                })
            }
            (None, true, None) => {
                self.finish_partial(&mut conn, &ctx, &st)?;
                Ok(SyncOutcome {
                    source_id: source_id.to_string(),
                    run_id: Some(run_id),
                    status: SyncStatus::Partial,
                    pages,
                    fetched: st.fetched,
                    out_of_scope: st.out_of_scope,
                    inserted: st.inserted,
                    unchanged: st.unchanged,
                    updated: st.updated,
                    cursor_advanced: false,
                    error: None,
                })
            }
            (None, _, Some(f)) => {
                let status = if f.is_auth {
                    SyncStatus::AuthRequired
                } else {
                    SyncStatus::Failed
                };
                self.finish_failure(&mut conn, &ctx, cp.consecutive_failures, &f, &st)?;
                Ok(SyncOutcome {
                    source_id: source_id.to_string(),
                    run_id: Some(run_id),
                    status,
                    pages,
                    fetched: st.fetched,
                    out_of_scope: st.out_of_scope,
                    inserted: st.inserted,
                    unchanged: st.unchanged,
                    updated: st.updated,
                    cursor_advanced: false,
                    error: Some(f.info),
                })
            }
            (None, false, None) => unreachable!("循环只以三种结局之一退出"),
        }
    }

    /// 按注册序同步全部来源；各来源独立成败，互不阻断（§6.2 规则 2 的串行
    /// 近似——单连接顺序执行，多源并行由 app 层各起一个引擎实例）。
    pub fn sync_all(&self, now_ms: i64) -> Result<Vec<SyncOutcome>, SyncError> {
        let ids: Vec<String> = self.lock_sources().keys().cloned().collect();
        let mut outcomes = Vec::with_capacity(ids.len());
        for id in ids {
            outcomes.push(self.sync_source(&id, now_ms)?);
        }
        Ok(outcomes)
    }

    /// 手动恢复：`degraded` / `auth_required` → `idle`，清零连续失败与错误
    /// 信息（用户重新授权或点「手动重试」后的入口）；游标与最后成功时间
    /// 原样保留。`idle` 时为幂等 no-op。
    pub fn reset(&self, source_id: &str, now_ms: i64) -> Result<(), SyncError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE sync_checkpoint
             SET state='idle', consecutive_failures=0, last_error_code=NULL, last_error_msg=NULL,
                 updated_at=?2
             WHERE source_id=?1 AND state IN ('degraded','auth_required')",
            params![source_id, now_ms],
        )?;
        Ok(())
    }

    /// 读取检查点（未注册时 `None`）。
    pub fn checkpoint(&self, source_id: &str) -> Result<Option<Checkpoint>, SyncError> {
        let conn = self.lock();
        read_checkpoint(&conn, source_id)
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_sources(&self) -> MutexGuard<'_, BTreeMap<String, Arc<dyn DingTalkSource>>> {
        self.sources
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 读取检查点；`sync_checkpoint` 行缺失时补一行 `idle`（来源行由
    /// [`SyncEngine::register`] 保证存在，外键成立）。
    fn load_checkpoint(
        &self,
        conn: &Connection,
        source_id: &str,
        now_ms: i64,
    ) -> Result<Checkpoint, SyncError> {
        if read_checkpoint(conn, source_id)?.is_none() {
            conn.execute(
                "INSERT OR IGNORE INTO sync_checkpoint(source_id, state, consecutive_failures, updated_at)
                 VALUES(?1, 'idle', 0, ?2)",
                params![source_id, now_ms],
            )?;
        }
        read_checkpoint(conn, source_id)?.ok_or_else(|| SyncError::UnknownSource(source_id.into()))
    }

    /// §6.2：最终事务——游标推进、失败清零、run 收尾、`work_event` 记账。
    fn finish_ok(
        &self,
        conn: &mut Connection,
        ctx: &RunCtx<'_>,
        cp: &Checkpoint,
        last_page: &RecordPage,
        st: &Counters,
    ) -> Result<(), SyncError> {
        let cursor_json: Option<String> =
            last_page.next_cursor.as_ref().map(|v| v.to_string());
        let snapshot_at = last_page.snapshot_at;
        let note = format!(
            "synced: fetched={} out_of_scope={} inserted={} unchanged={} updated={}",
            st.fetched, st.out_of_scope, st.inserted, st.unchanged, st.updated
        );
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE sync_checkpoint
             SET cursor_json=?2, cursor_at=?3, window_start=?4, window_end=?3,
                 last_success_at=?3, consecutive_failures=0, state='idle',
                 last_error_code=NULL, last_error_msg=NULL, updated_at=?5
             WHERE source_id=?1",
            params![ctx.source_id, cursor_json, snapshot_at, cp.cursor_at, ctx.now_ms],
        )?;
        tx.execute(
            "UPDATE sync_run SET state='ok', finished_at=?2, fetched=?3, inserted=?4,
                 unchanged=?5, kept_for_judgement=?6
             WHERE id=?1",
            params![ctx.run_id, ctx.now_ms, st.fetched, st.inserted, st.unchanged, st.inserted],
        )?;
        tx.execute(
            "INSERT INTO work_event(id, target_type, target_id, event_type, actor, at, note)
             VALUES(?1, 'source', ?2, 'synced', 'system', ?3, ?4)",
            params![format!("{}:synced", ctx.run_id), ctx.source_id, ctx.now_ms, note],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 达到单轮上限：已入库的页保留，游标不动（下一轮重拉，唯一键吸收重复），
    /// checkpoint 离开 `syncing`，run 记 `partial`。
    fn finish_partial(
        &self,
        conn: &mut Connection,
        ctx: &RunCtx<'_>,
        st: &Counters,
    ) -> Result<(), SyncError> {
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE sync_checkpoint SET state='idle', updated_at=?2 WHERE source_id=?1",
            params![ctx.source_id, ctx.now_ms],
        )?;
        tx.execute(
            "UPDATE sync_run SET state='partial', finished_at=?2, fetched=?3, inserted=?4,
                 unchanged=?5, kept_for_judgement=?6
             WHERE id=?1",
            params![ctx.run_id, ctx.now_ms, st.fetched, st.inserted, st.unchanged, st.inserted],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 失败收尾：游标/`last_success_at` 一律不动（§6.2 规则 1/4），只更新
    /// 状态、失败计数与错误信息。
    fn finish_failure(
        &self,
        conn: &mut Connection,
        ctx: &RunCtx<'_>,
        prev_failures: u32,
        f: &Failure,
        st: &Counters,
    ) -> Result<(), SyncError> {
        let new_failures = if f.is_auth {
            prev_failures // 鉴权失败不累计降级计数（§6.2 规则 4）
        } else {
            prev_failures + 1
        };
        let new_state = if f.is_auth {
            "auth_required"
        } else if new_failures >= self.config.max_consecutive_failures {
            "degraded"
        } else {
            "idle"
        };
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE sync_checkpoint
             SET state=?2, consecutive_failures=?3, last_error_code=?4, last_error_msg=?5,
                 updated_at=?6
             WHERE source_id=?1",
            params![
                ctx.source_id,
                new_state,
                new_failures,
                f.info.code,
                f.info.message,
                ctx.now_ms
            ],
        )?;
        tx.execute(
            "UPDATE sync_run SET state='failed', finished_at=?2, fetched=?3, inserted=?4,
                 unchanged=?5, kept_for_judgement=?6, error_code=?7, error_msg=?8
             WHERE id=?1",
            params![
                ctx.run_id,
                ctx.now_ms,
                st.fetched,
                st.inserted,
                st.unchanged,
                st.inserted,
                f.info.code,
                f.info.message
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
}

/// 一轮运行的固定上下文（三个收尾事务共用，避免长参数列）。
struct RunCtx<'a> {
    source_id: &'a str,
    run_id: &'a str,
    now_ms: i64,
}

/// 单轮计数器（落库列见 `sync_run`；`updated` 仅内存结果，见 [`SyncOutcome`]）。
#[derive(Debug, Default, Clone, Copy)]
struct Counters {
    fetched: usize,
    out_of_scope: usize,
    inserted: usize,
    unchanged: usize,
    updated: usize,
}

/// 失败结论：错误信息 + 是否鉴权类（决定 `auth_required` 与失败计数路径）。
struct Failure {
    info: SyncErrorInfo,
    is_auth: bool,
}

fn refused(source_id: &str, status: SyncStatus) -> SyncOutcome {
    debug_assert!(matches!(
        status,
        SyncStatus::Disabled | SyncStatus::Degraded | SyncStatus::AuthRequired
    ));
    SyncOutcome {
        source_id: source_id.to_string(),
        run_id: None,
        status,
        pages: 0,
        fetched: 0,
        out_of_scope: 0,
        inserted: 0,
        unchanged: 0,
        updated: 0,
        cursor_advanced: false,
        error: None,
    }
}

fn read_checkpoint(conn: &Connection, source_id: &str) -> Result<Option<Checkpoint>, SyncError> {
    let row = conn.query_row(
        "SELECT cursor_json, cursor_at, last_attempt_at, last_success_at,
                consecutive_failures, state, last_error_code, last_error_msg
         FROM sync_checkpoint WHERE source_id = ?1",
        params![source_id],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        },
    );
    match row {
        Ok((
            cursor_json,
            cursor_at,
            last_attempt_at,
            last_success_at,
            failures,
            state,
            last_error_code,
            last_error_msg,
        )) => {
            let state = CheckpointState::from_db(&state).ok_or_else(|| {
                SyncError::CorruptState(format!("{source_id}: {state}"))
            })?;
            let consecutive_failures = u32::try_from(failures.max(0))
                .map_err(|_| SyncError::CorruptState(format!("{source_id}: {failures}")))?;
            Ok(Some(Checkpoint {
                source_id: source_id.to_string(),
                cursor_json,
                cursor_at,
                last_attempt_at,
                last_success_at,
                consecutive_failures,
                state,
                last_error_code,
                last_error_msg,
            }))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 单页事务内的幂等 upsert（§4.4）：无则插入、同 `content_hash` 跳过、
/// 内容变化则整行更新并计 `updated`。
///
/// 判读元数据（mentions/reply_to/conversation_type，迁移 0002）**不进入**
/// `content_hash`：正文幂等键保持「仅内容」口径（已落库的哈希值不变）；
/// 元数据单独比对——`content_hash` 相同但元数据变化时，只更新三列并计
/// `updated`（同记录重拉一次后收敛为 `unchanged`）。
///
/// **保留期**（§14.3）：入库即写 `retention_at = 首次入库时点 + rawBodyDays`，
/// 供 `retention_sweep` 到期置空正文。重复同步**不延期**：`content_hash`
/// 相同时不触碰；内容变化时只 `COALESCE` 回填本字段引入前入库的旧行（按其
/// 自身 `ingested_at` 计算）。正文已被清理（`raw_state <> 'available'`）的
/// 记录重拉**不复活正文**——只刷新非正文列与去重键，使后续重拉收敛为
/// `unchanged`，`body` / `raw_state` / `retention_at` 原样保留。
fn upsert_record(
    tx: &Transaction<'_>,
    source_id: &str,
    record: &RawRecord,
    now_ms: i64,
    raw_body_days: u32,
    st: &mut Counters,
) -> Result<(), rusqlite::Error> {
    let content_hash = record.content_hash();
    let retention_ms = i64::from(raw_body_days) * MS_PER_DAY;
    let existing = tx
        .query_row(
            "SELECT content_hash, mentions_json, reply_to_id, conversation_type,
                    raw_state, ingested_at, retention_at
             FROM source_record
             WHERE source_id = ?1 AND external_id = ?2",
            params![source_id, record.external_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                ))
            },
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    match existing {
        None => {
            tx.execute(
                "INSERT INTO source_record(
                     id, source_id, external_id, kind, occurred_at, ingested_at,
                     conversation_id, sender_id, sender_name, direction,
                     summary, body, body_sha256, content_hash, perm_scope,
                     mentions_json, reply_to_id, conversation_type,
                     raw_state, judge_state, retention_at)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                        ?16, ?17, ?18, 'available', 'pending', ?19)",
                params![
                    format!("{source_id}:{}", record.external_id),
                    source_id,
                    record.external_id,
                    record.kind.as_str(),
                    record.occurred_at,
                    now_ms,
                    record.conversation_id,
                    record.sender_id,
                    record.sender_name,
                    record.direction.as_str(),
                    record.summary,
                    record.body,
                    record.body_sha256(),
                    content_hash,
                    record.perm_scope,
                    record.mentions_json(),
                    record.reply_to_id,
                    record.conversation_type.map(|c| c.as_str()),
                    now_ms + retention_ms,
                ],
            )?;
            st.inserted += 1;
        }
        Some(existing) => {
            let (stored_hash, stored_mentions, stored_reply, stored_conv) =
                (existing.0, existing.1, existing.2, existing.3);
            let (raw_state, ingested_at, retention_at) = (existing.4, existing.5, existing.6);
            if stored_hash == content_hash {
                if record.meta_matches(&stored_mentions, stored_reply.as_deref(), stored_conv.as_deref()) {
                    // 旧数据回填：期限缺失（本字段引入前入库）时按首次入库时点补齐；
                    // 已有期限不动（重复同步不延期）。内容未变，仍计 unchanged。
                    if retention_at.is_none() {
                        tx.execute(
                            "UPDATE source_record
                             SET retention_at=?3
                             WHERE source_id=?1 AND external_id=?2 AND retention_at IS NULL",
                            params![source_id, record.external_id, ingested_at + retention_ms],
                        )?;
                    }
                    st.unchanged += 1;
                } else {
                    tx.execute(
                        "UPDATE source_record
                         SET mentions_json=?3, reply_to_id=?4, conversation_type=?5
                         WHERE source_id=?1 AND external_id=?2",
                        params![
                            source_id,
                            record.external_id,
                            record.mentions_json(),
                            record.reply_to_id,
                            record.conversation_type.map(|c| c.as_str()),
                        ],
                    )?;
                    st.updated += 1;
                }
            } else if raw_state == "available" {
                tx.execute(
                    "UPDATE source_record
                     SET kind=?3, occurred_at=?4, conversation_id=?5, sender_id=?6, sender_name=?7,
                         direction=?8, summary=?9, body=?10, body_sha256=?11, content_hash=?12,
                         perm_scope=?13, mentions_json=?14, reply_to_id=?15, conversation_type=?16,
                         retention_at=COALESCE(retention_at, ?17)
                     WHERE source_id=?1 AND external_id=?2",
                    params![
                        source_id,
                        record.external_id,
                        record.kind.as_str(),
                        record.occurred_at,
                        record.conversation_id,
                        record.sender_id,
                        record.sender_name,
                        record.direction.as_str(),
                        record.summary,
                        record.body,
                        record.body_sha256(),
                        content_hash,
                        record.perm_scope,
                        record.mentions_json(),
                        record.reply_to_id,
                        record.conversation_type.map(|c| c.as_str()),
                        ingested_at + retention_ms,
                    ],
                )?;
                st.updated += 1;
            } else {
                // 正文已被保留期清理（或本就不可得）：不复活正文，只刷新非正文列
                // 与去重键，`body`/`raw_state`/`retention_at` 原样保留。
                tx.execute(
                    "UPDATE source_record
                     SET kind=?3, occurred_at=?4, conversation_id=?5, sender_id=?6, sender_name=?7,
                         direction=?8, summary=?9, body_sha256=?10, content_hash=?11,
                         perm_scope=?12, mentions_json=?13, reply_to_id=?14, conversation_type=?15
                     WHERE source_id=?1 AND external_id=?2",
                    params![
                        source_id,
                        record.external_id,
                        record.kind.as_str(),
                        record.occurred_at,
                        record.conversation_id,
                        record.sender_id,
                        record.sender_name,
                        record.direction.as_str(),
                        record.summary,
                        record.body_sha256(),
                        content_hash,
                        record.perm_scope,
                        record.mentions_json(),
                        record.reply_to_id,
                        record.conversation_type.map(|c| c.as_str()),
                    ],
                )?;
                st.updated += 1;
            }
        }
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

// ---------------------------------------------------------------------------
// 脚本化 mock 来源（测试与 A13 失联演练；不触网）
// ---------------------------------------------------------------------------

/// [`MockSource`] 的一个脚本步骤：返回一页，或抛一个错误。
#[derive(Debug, Clone)]
pub enum MockStep {
    Page(RecordPage),
    Error(SourceError),
}

/// 脚本化 mock 来源：按 FIFO 消耗 [`MockStep`]，耗尽后返回空的终页
/// （`has_more=false`）。记录每次 `fetch` 收到的游标（[`MockSource::fetch_log`]），
/// 供测试断言「游标没动、下一轮从旧游标重拉」。
///
/// 脚本在来源被 [`SyncEngine::register`] 之后仍可续写（`&self` API），模拟
/// 断线期间来源侧新增消息的回填场景。
#[derive(Debug)]
pub struct MockSource {
    id: String,
    scope: Scope,
    script: Mutex<VecDeque<MockStep>>,
    fetch_log: Mutex<Vec<Option<String>>>,
    /// 每次 `fetch` 收到的白名单 JSON（断言「scope 在 fetch 前传递」用）。
    scope_log: Mutex<Vec<String>>,
}

impl MockSource {
    /// 新建空脚本来源。
    pub fn new(id: &str, scope: Scope) -> Self {
        Self {
            id: id.to_string(),
            scope,
            script: Mutex::new(VecDeque::new()),
            fetch_log: Mutex::new(Vec::new()),
            scope_log: Mutex::new(Vec::new()),
        }
    }

    /// 追加一页。
    pub fn push_page(&self, page: RecordPage) -> &Self {
        self.script.lock().unwrap().push_back(MockStep::Page(page));
        self
    }

    /// 追加一个错误。
    pub fn push_error(&self, error: SourceError) -> &Self {
        self.script.lock().unwrap().push_back(MockStep::Error(error));
        self
    }

    /// 未消耗的脚本步数（断言「拒绝路径没触来源」用）。
    pub fn remaining(&self) -> usize {
        self.script.lock().unwrap().len()
    }

    /// 每次 `fetch` 收到的游标（`to_string` 形，`None` = 首轮无游标）。
    pub fn fetch_log(&self) -> Vec<Option<String>> {
        self.fetch_log.lock().unwrap().clone()
    }

    /// 每次 `fetch` 收到的白名单 JSON（`Scope::to_json` 形）。
    pub fn scope_log(&self) -> Vec<String> {
        self.scope_log.lock().unwrap().clone()
    }
}

impl DingTalkSource for MockSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn scope(&self) -> &Scope {
        &self.scope
    }

    fn probe(&self) -> Result<(), SourceError> {
        match self.script.lock().unwrap().front() {
            Some(MockStep::Error(e @ SourceError::Auth { .. })) => Err(e.clone()),
            _ => Ok(()),
        }
    }

    fn fetch(
        &self,
        scope: &Scope,
        cursor: Option<&Value>,
        _page: &PageParams,
    ) -> Result<RecordPage, SourceError> {
        self.fetch_log
            .lock()
            .unwrap()
            .push(cursor.map(|c| c.to_string()));
        self.scope_log.lock().unwrap().push(scope.to_json());
        match self.script.lock().unwrap().pop_front() {
            Some(MockStep::Page(page)) => Ok(page),
            Some(MockStep::Error(e)) => Err(e),
            None => Ok(RecordPage {
                records: Vec::new(),
                next_cursor: None,
                has_more: false,
                snapshot_at: 0,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_schedule_follows_design_with_jitter_bounds() {
        // 序列 1/4/16/60/300s，超界钳制在 300s（§6.2 规则 3）。
        assert_eq!(backoff_delay_ms(1, 0.5), 1_000);
        assert_eq!(backoff_delay_ms(2, 0.5), 4_000);
        assert_eq!(backoff_delay_ms(3, 0.5), 16_000);
        assert_eq!(backoff_delay_ms(4, 0.5), 60_000);
        assert_eq!(backoff_delay_ms(5, 0.5), 300_000);
        assert_eq!(backoff_delay_ms(50, 0.5), 300_000);
        // ±20% 抖动：0 → 下界、1 → 上界、越界钳制。
        assert_eq!(backoff_delay_ms(1, 0.0), 800);
        assert_eq!(backoff_delay_ms(1, 1.0), 1_200);
        assert_eq!(backoff_delay_ms(1, -3.0), 800);
        assert_eq!(backoff_delay_ms(1, 9.0), 1_200);
    }

    #[test]
    fn content_hash_is_stable_and_nfc_insensitive() {
        // 同一记录（正文组合字符 vs 预组合）→ 同一 content_hash（§4.4 NFC）。
        let decomposed = RawRecord::im("m1", 1, "cid", "u1", Direction::In, "cafe\u{301}");
        let composed = RawRecord::im("m1", 1, "cid", "u1", Direction::In, "café");
        assert_eq!(decomposed.content_hash(), composed.content_hash());
        // 任一字段变化 → 哈希变化。
        let mut changed = composed.clone();
        changed.body = Some("其它内容".into());
        assert_ne!(changed.content_hash(), composed.content_hash());
    }

    #[test]
    fn metadata_stays_out_of_content_hash_and_compares_alone() {
        // 元数据不进 content_hash：正文口径一致的两条记录哈希相同。
        let base = RawRecord::im("m1", 1, "cid", "u1", Direction::In, "正文");
        let mut meta = base.clone();
        meta.mentions = vec!["u0".into()];
        meta.reply_to_id = Some("m0".into());
        meta.conversation_type = Some(ConversationType::Group);
        assert_eq!(base.content_hash(), meta.content_hash());
        // 存储形与比对。
        assert_eq!(meta.mentions_json(), r#"["u0"]"#);
        assert!(meta.meta_matches(r#"["u0"]"#, Some("m0"), Some("group")));
        assert!(!meta.meta_matches("[]", Some("m0"), Some("group")));
        assert!(!meta.meta_matches(r#"["u0"]"#, None, Some("group")));
        assert!(!meta.meta_matches(r#"["u0"]"#, Some("m0"), None));
        assert!(base.meta_matches("[]", None, None));
    }

    #[test]
    fn scope_fail_closed_and_hits_either_dimension() {
        let mut scope = Scope::default();
        assert!(!scope.allows(&RawRecord::im("m", 1, "any", "any", Direction::In, "x")));
        scope.conversations.insert("cid".into());
        scope.people.insert("p1".into());
        assert!(scope.allows(&RawRecord::im("m", 1, "cid", "other", Direction::In, "x")));
        assert!(scope.allows(&RawRecord::im("m", 1, "dm-x", "p1", Direction::In, "x")));
        assert!(!scope.allows(&RawRecord::im("m", 1, "other", "other", Direction::In, "x")));
    }
}
