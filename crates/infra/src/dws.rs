//! 真实 DWS CLI 适配器（ADR-05 `CliDingTalkSource`；DWS 接口调研 §8 的落地；
//! 遗留事项 B7 / M6）。
//!
//! 边界（M6 门槛「白名单外不读」的字面实现）：
//!
//! 1. **命令白名单是封闭枚举**：[`DwsReadCommand`] 是构建 argv 的唯一入口，
//!    只有 `effect=read` 的三个命令（version / auth status / chat
//!    +chat-messages，调研 §4.1/§8.1），不存在表达写操作的代码路径；
//!    人控参数（profile / 目标 ID / 游标 token）一律过 [`safe_arg`] 校验
//!    （非空、不以 `-` 开头、无空白/控制字符），不可能被拼成旗标注入。
//! 2. **只请求白名单目标与批准时间窗**：`fetch` 按显式 `source.scope` 逐会话/
//!    逐人调用 `chat +chat-messages`（`--group` / `--open-dingtalk-id`）——CLI
//!    从未被要求返回白名单之外的会话数据；读取窗口必须落在调用方显式批准的
//!    [`ApprovedWindow`] 内（默认未批准 → 拒绝读取、零 CLI 调用）；引擎侧
//!    [`crate::sync::Scope::allows`] 再过滤一次（防御纵深）。
//! 3. **契约核对**：消息读取信封 `contractVersion != im.message-list.v1` 即
//!    拒收整轮（调研 U8）；`complete=false` / `truncated` / `partial` /
//!    `failures` 非空 → 整轮失败、游标不推进（§8.5，未获取 ≠ 未发生）。
//! 4. **错误信封映射**：stderr 的 `{"error":{category,…}}` → `category=auth`
//!    或 `reason=auth_refresh_failed` → [`SourceError::Auth`]（引擎置
//!    `auth_required`）；`retryable=true` → [`SourceError::RateLimited`]（带
//!    `retry_after_ms`）；其余 → [`SourceError::Other`]。**错误信息只含
//!    category/退出码/字节数**，绝不含子进程输出正文（日志无正文）。
//! 5. **不登录、不读真实消息**：本适配器不发起登录、不存凭据（登录态由用户
//!    自行 `dws auth login` 维护，本层只做 `dws auth status` 查询，不扩大为
//!    其它命令）；测试全部使用合成 CLI
//!    子进程（`fake_cli/` 替身二进制），不触真实 `dws`。
//!
//! 游标（不透明 JSON，引擎只存不析）：
//! `{"v":1,"window_start_ms":…,"window_end_ms":…,"remaining":["g:cid",…],
//!   "page_token":"…"}` —— `remaining` 非空表示**轮内续页**（窗口边界原样
//! 重复：`--page-token` 绑定当次查询条件，调研 §4.1）；空表示整轮完成，
//! 下轮窗口 = `window_end_ms − overlap`（§8.3，重复由唯一键吸收）。
//!
//! 已知缺口（记录于模块文档，接入授权后按调研 §7 验证计划补齐）：
//! - 消息读取契约**没有 @ 列表字段**（§4.3），`mentions` 恒为空——预筛
//!   `at_me` 依赖 app 层 `+at-me`/文本启发或 V0.2 事件流精确字段；
//! - `messageId` 唯一性范围未知（U1）：当前 `external_id = messageId`，
//!   试点若发现跨会话碰撞改取 `messageId + ':' + conversationId`（不改表）。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::subprocess::{self, Invocation, ProcOutcome, ProcResult, DEFAULT_STREAM_CAP};
use crate::sync::{
    ConversationType, Direction, DingTalkSource, PageParams, RawRecord, RecordKind, RecordPage,
    Scope, SourceError,
};
use crate::timefmt::{ceil_to_second, floor_to_second, format_datetime};

/// 消息读取结果契约版本（调研 §4.2；不匹配即拒收，U8）。
pub const MESSAGE_LIST_CONTRACT: &str = "im.message-list.v1";
/// 白名单命令面（全部 `effect=read`；封闭枚举 [`DwsReadCommand`] 的全集）。
pub const READ_COMMANDS: [&str; 3] = ["version", "auth status", "chat +chat-messages"];
/// 首轮回溯窗口：最近 24h（调研 §4.1：list-all 默认最近 1 天；回填深度 U3）。
pub const DEFAULT_INITIAL_LOOKBACK: Duration = Duration::from_secs(24 * 3600);
/// 单次子进程调用墙钟上限（调研 §6：全局 `--timeout` 默认 30s，外层放宽）。
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(90);
/// 单轮最多翻页数（§6.2 规则 6：单次同步总时长 10 min 的页粒度代理）。
pub const DEFAULT_MAX_PAGES_PER_ROUND: usize = 40;
/// 每页条数（`--limit`；调研 §4.1：默认 50，V0.1 取 50 保守节流）。
pub const DEFAULT_PAGE_LIMIT: u16 = 50;

// ---------------------------------------------------------------------------
// 白名单命令（构建 argv 的唯一入口）
// ---------------------------------------------------------------------------

/// 只读命令白名单（封闭枚举；新增变体必须同步 [`READ_COMMANDS`] 与测试）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DwsReadCommand {
    /// `dws version --format json`（U8：启动时记录 dws 版本）。
    Version,
    /// `dws auth status --profile <p> --format json`（probe 探测；本机 v1.0.62
    /// 实测**不支持** `--readonly`，调研 §3.1 的该旗标不采用）。
    AuthStatus { profile: String },
    /// `dws chat +chat-messages …`（唯一消息读取入口；`effect=read`）。
    ChatMessages {
        target: TargetRef,
        start: String,
        end: String,
        page_token: Option<String>,
        limit: u16,
        profile: String,
    },
}

impl DwsReadCommand {
    /// 命令名（白名单核对口径）。
    pub fn name(&self) -> &'static str {
        match self {
            DwsReadCommand::Version => "version",
            DwsReadCommand::AuthStatus { .. } => "auth status",
            DwsReadCommand::ChatMessages { .. } => "chat +chat-messages",
        }
    }

    /// argv（首元素是子命令名，程序本身由调用方提供）。
    pub fn argv(&self) -> Vec<String> {
        match self {
            DwsReadCommand::Version => vec!["version".into(), "--format".into(), "json".into()],
            DwsReadCommand::AuthStatus { profile } => vec![
                "auth".into(),
                "status".into(),
                "--profile".into(),
                profile.clone(),
                "--format".into(),
                "json".into(),
            ],
            DwsReadCommand::ChatMessages {
                target,
                start,
                end,
                page_token,
                limit,
                profile,
            } => {
                let mut args = vec![
                    "chat".into(),
                    "+chat-messages".into(),
                    target.flag().to_string(),
                    target.value().to_string(),
                    "--start".into(),
                    start.clone(),
                    "--end".into(),
                    end.clone(),
                    "--order".into(),
                    "asc".into(),
                    "--limit".into(),
                    limit.to_string(),
                    "--profile".into(),
                    profile.clone(),
                ];
                if let Some(token) = page_token {
                    args.push("--page-token".into());
                    args.push(token.clone());
                }
                args
            }
        }
    }

    /// 构造前校验全部人控参数（fail closed：不安全即 None，绝不拼 shell）。
    pub fn sanitized(self) -> Option<Self> {
        match &self {
            DwsReadCommand::Version => Some(self),
            DwsReadCommand::AuthStatus { profile } => safe_arg(profile).then_some(self),
            DwsReadCommand::ChatMessages {
                target,
                start,
                end,
                page_token,
                profile,
                ..
            } => {
                // 时间窗严格 `yyyy-MM-dd HH:mm:ss`（调研 §4.1；唯一含空格的
                // 合法值），字符集封闭，不可能是旗标。
                let time_ok = |t: &str| {
                    t.len() == 19
                        && t.as_bytes().get(4) == Some(&b'-')
                        && t.as_bytes().get(7) == Some(&b'-')
                        && t.as_bytes().get(10) == Some(&b' ')
                        && t.as_bytes().get(13) == Some(&b':')
                        && t.as_bytes().get(16) == Some(&b':')
                        && t.bytes().enumerate().all(|(i, b)| match i {
                            4 | 7 => b == b'-',
                            10 => b == b' ',
                            13 | 16 => b == b':',
                            _ => b.is_ascii_digit(),
                        })
                };
                let ok = safe_arg(target.value())
                    && safe_arg(profile)
                    && time_ok(start)
                    && time_ok(end)
                    && page_token.as_deref().is_none_or(safe_arg);
                ok.then_some(self)
            }
        }
    }
}

/// 人控参数校验：非空、不以 `-` 开头（防旗标注入）、无空白/控制字符。
fn safe_arg(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value.chars().all(|c| !c.is_whitespace() && !c.is_control())
}

/// 拉取目标（scope 维度的投影）：群会话或私聊对端。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TargetRef {
    /// `--group <openConversationId>`（scope.conversations）。
    Group(String),
    /// `--open-dingtalk-id <id>`（scope.people）。
    Person(String),
}

impl TargetRef {
    pub fn flag(&self) -> &'static str {
        match self {
            TargetRef::Group(_) => "--group",
            TargetRef::Person(_) => "--open-dingtalk-id",
        }
    }

    pub fn value(&self) -> &str {
        match self {
            TargetRef::Group(v) | TargetRef::Person(v) => v,
        }
    }

    /// 游标 `remaining` 序列化形。
    pub fn tag(&self) -> String {
        match self {
            TargetRef::Group(v) => format!("g:{v}"),
            TargetRef::Person(v) => format!("p:{v}"),
        }
    }

    fn from_tag(tag: &str) -> Option<Self> {
        match tag.split_once(':') {
            Some(("g", v)) if !v.is_empty() => Some(TargetRef::Group(v.to_string())),
            Some(("p", v)) if !v.is_empty() => Some(TargetRef::Person(v.to_string())),
            _ => None,
        }
    }

    fn conversation_type(&self) -> ConversationType {
        match self {
            TargetRef::Group(_) => ConversationType::Group,
            TargetRef::Person(_) => ConversationType::Dm,
        }
    }

    /// 记录是否属于本目标的授权拉取范围（会话一致，或本人/对端在私聊里）。
    fn owns(&self, record: &RawRecord, me_user_id: Option<&str>) -> bool {
        match self {
            TargetRef::Group(cid) => record.conversation_id.as_deref() == Some(cid),
            TargetRef::Person(pid) => {
                record.sender_id.as_deref() == Some(pid)
                    || me_user_id.is_some_and(|me| record.sender_id.as_deref() == Some(me))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 独立探测入口（不建 source 即可探版本/授权）
// ---------------------------------------------------------------------------

/// 子进程传输选项（独立探测入口用；`CliDingTalkSource` 内部复用）。
#[derive(Debug, Clone)]
pub struct DwsTransportOptions {
    /// `DWS_CONFIG_DIR` 隔离（可选；默认与用户 `~/.dws` 共享登录态，调研 §8.6）。
    pub config_dir: Option<PathBuf>,
    /// 子进程 HOME；清空环境后仍需定位当前用户的 DWS 登录配置。
    pub home: Option<PathBuf>,
    pub call_timeout: Duration,
    pub stream_cap: usize,
}

impl Default for DwsTransportOptions {
    fn default() -> Self {
        Self {
            config_dir: None,
            home: std::env::home_dir(),
            call_timeout: DEFAULT_CALL_TIMEOUT,
            stream_cap: DEFAULT_STREAM_CAP,
        }
    }
}

/// `dws auth status` 的快照字段（调研 §3.1；本机 v1.0.62 实测至少返回
/// `authenticated` / `token_valid`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DwsAuthStatus {
    pub authenticated: bool,
    pub token_valid: bool,
    pub refresh_token_valid: bool,
}

impl DwsAuthStatus {
    /// 是否需要重新授权（引擎 `auth_required` 的判定口径）。
    ///
    /// 以 `authenticated` 为准：官方语义是「access **或** refresh 任一有效」，
    /// 因此 `authenticated=false` 才是权威的重新授权信号；`token_valid=false`
    /// 只表示 access 过期，CLI 会在业务命令里自动刷新并最多重放一次（调研
    /// §3.1），不构成 `auth_required`。字段缺失时不臆断（见 [`auth_status`]）。
    pub fn requires_auth(&self) -> bool {
        !self.authenticated
    }
}

/// 独立探测/拉取失败（结构化，无子进程输出正文）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DwsCliError {
    #[error("dws 启动失败：{0}")]
    Spawn(String),
    #[error("dws 调用超时")]
    Timeout,
    #[error("dws 需要授权（category=auth）")]
    Auth,
    #[error("dws 限流（category={category}，retry_after_ms={retry_after_ms:?}）")]
    RateLimited {
        category: String,
        retry_after_ms: Option<u64>,
    },
    #[error("dws 错误（category={category}，exit={exit:?}）")]
    Failed { category: String, exit: Option<i32> },
    #[error("dws 输出不可解析：{0}")]
    BadEnvelope(String),
}

/// `dws version --format json` → 版本串（如 `v1.0.62`）。
pub fn cli_version(exe: &Path, opts: &DwsTransportOptions) -> Result<String, DwsCliError> {
    let proc = transport(exe, &DwsReadCommand::Version, opts)?;
    let ProcOutcome { exit, .. } = proc;
    if exit != Some(0) {
        return Err(map_failure(&proc));
    }
    let value: Value = serde_json::from_slice(&proc.stdout)
        .map_err(|_| DwsCliError::BadEnvelope("version 非 JSON".into()))?;
    value["version"]
        .as_str()
        .map(|v| v.to_string())
        .ok_or_else(|| DwsCliError::BadEnvelope("version 缺 version 字段".into()))
}

/// `dws auth status --profile <p> --format json` → 授权快照。
///
/// 只读查询，不因字段缺失而改用其它命令：`authenticated` 必须存在；
/// `token_valid` 缺省取 `authenticated`；`refresh_token_valid` 缺省为 false
/// （仅作展示，不参与 [`DwsAuthStatus::requires_auth`] 判定）。
pub fn auth_status(
    exe: &Path,
    profile: &str,
    opts: &DwsTransportOptions,
) -> Result<DwsAuthStatus, DwsCliError> {
    let command = DwsReadCommand::AuthStatus {
        profile: profile.to_string(),
    };
    if command.clone().sanitized().is_none() {
        return Err(DwsCliError::BadEnvelope("profile 参数不合法".into()));
    }
    let proc = transport(exe, &command, opts)?;
    if proc.exit != Some(0) {
        return Err(map_failure(&proc));
    }
    let value: Value = serde_json::from_slice(&proc.stdout)
        .map_err(|_| DwsCliError::BadEnvelope("auth status 非 JSON".into()))?;
    let Some(authenticated) = value["authenticated"].as_bool() else {
        return Err(DwsCliError::BadEnvelope("auth status 缺 authenticated".into()));
    };
    Ok(DwsAuthStatus {
        authenticated,
        token_valid: value["token_valid"].as_bool().unwrap_or(authenticated),
        refresh_token_valid: value["refresh_token_valid"].as_bool().unwrap_or(false),
    })
}

// ---------------------------------------------------------------------------
// 来源适配器
// ---------------------------------------------------------------------------

/// 调用方**显式批准**的读取时间窗（UTC ms；`[start_ms, end_ms)`）。
///
/// 这是白名单的时间维度：真实读取只发生在批准范围内。`end_ms = None` 为
/// 开放上界（批准后持续同步新消息，直到撤权——撤权即写入 `end_ms`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApprovedWindow {
    /// 批准起点（含）：此前的消息一律不读。
    pub start_ms: i64,
    /// 批准终点（不含）；`None` = 开放上界。
    pub end_ms: Option<i64>,
}

impl ApprovedWindow {
    /// 本轮请求窗口与批准范围取交集；无交集返回 `None`。
    fn clamp(&self, start_ms: i64, end_ms: i64) -> Option<(i64, i64)> {
        let start = start_ms.max(self.start_ms);
        let end = self.end_ms.map_or(end_ms, |approved| end_ms.min(approved));
        (start < end).then_some((start, end))
    }

    /// 时刻是否落在批准范围内（记录侧的时间过滤，防御纵深）。
    fn contains(&self, at_ms: i64) -> bool {
        at_ms >= self.start_ms && self.end_ms.is_none_or(|end| at_ms < end)
    }
}

/// DWS CLI 适配器配置（持久化在 `config.json` 的 `sources.<id>`）。
#[derive(Debug, Clone)]
pub struct DwsCliConfig {
    /// `dws` 可执行文件绝对路径。
    pub exe: PathBuf,
    /// `corpId:userId`（多账号必须显式固定，调研 §3.1）。
    pub profile: String,
    /// 本人 userId（方向判定；缺省从 profile 冒号后段推导）。
    pub me_user_id: Option<String>,
    /// 调用方显式批准的读取时间窗。**默认 `None` = fail closed**：`fetch`
    /// 拒绝读取并返回明确错误（不发起任何 CLI 调用）；有值时每个请求窗口
    /// 与之取交集，记录再按时间过滤一次。
    pub approved_window: Option<ApprovedWindow>,
    pub initial_lookback: Duration,
    pub call_timeout: Duration,
    pub max_pages_per_round: usize,
    pub page_limit: u16,
    pub config_dir: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub stream_cap: usize,
    /// 时间窗时区偏移（ms；CLI 按本地时间解释 `yyyy-MM-dd HH:mm:ss`，
    /// app 层从系统时区注入，默认 0=UTC）。
    pub tz_offset_ms: i64,
}

impl DwsCliConfig {
    pub fn new(exe: PathBuf, profile: impl Into<String>) -> Self {
        Self {
            exe,
            profile: profile.into(),
            me_user_id: None,
            approved_window: None,
            initial_lookback: DEFAULT_INITIAL_LOOKBACK,
            call_timeout: DEFAULT_CALL_TIMEOUT,
            max_pages_per_round: DEFAULT_MAX_PAGES_PER_ROUND,
            page_limit: DEFAULT_PAGE_LIMIT,
            config_dir: None,
            home: std::env::home_dir(),
            stream_cap: DEFAULT_STREAM_CAP,
            tz_offset_ms: 0,
        }
    }

    fn transport_options(&self) -> DwsTransportOptions {
        DwsTransportOptions {
            config_dir: self.config_dir.clone(),
            home: self.home.clone(),
            call_timeout: self.call_timeout,
            stream_cap: self.stream_cap,
        }
    }

    /// 本人 userId（显式配置优先，缺省取 profile 冒号后段）。
    fn me(&self) -> Option<String> {
        self.me_user_id
            .clone()
            .or_else(|| self.profile.split(':').nth(1).map(|s| s.to_string()))
    }
}

/// 引擎游标（不透明 JSON 的实现细节；引擎只存不析）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct DwsCursor {
    v: u8,
    window_start_ms: i64,
    window_end_ms: i64,
    #[serde(default)]
    remaining: Vec<String>,
    #[serde(default)]
    page_token: Option<String>,
}

/// 真实 DWS CLI 来源（`DingTalkSource` 实现；同步阻塞，app 层放工作线程）。
pub struct CliDingTalkSource {
    id: String,
    scope: Scope,
    config: DwsCliConfig,
    now: Box<dyn Fn() -> i64 + Send + Sync>,
}

impl CliDingTalkSource {
    /// 构造不可失败：不做子进程探测（CLI 缺失在首次 probe/fetch 以错误暴露）。
    /// `now` 供测试注入固定时钟（窗口边界确定性）。
    pub fn new(id: &str, scope: Scope, config: DwsCliConfig) -> Self {
        Self {
            id: id.to_string(),
            scope,
            config,
            now: Box::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0)
            }),
        }
    }

    /// 注入时钟（测试）。
    pub fn with_clock(mut self, now: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.now = Box::new(now);
        self
    }

    fn probe_impl(&self) -> Result<(), SourceError> {
        match auth_status(&self.config.exe, &self.config.profile, &self.config.transport_options())
        {
            Ok(status) if status.requires_auth() => Err(SourceError::Auth {
                status: 401,
                message: "dws auth_status 要求重新授权".into(),
            }),
            Ok(_) => Ok(()),
            Err(DwsCliError::Auth) => Err(SourceError::Auth {
                status: 401,
                message: "dws auth (category=auth)".into(),
            }),
            Err(other) => Err(source_error(other)),
        }
    }

    fn fetch_impl(
        &self,
        scope: &Scope,
        cursor: Option<&Value>,
        page: &PageParams,
    ) -> Result<RecordPage, SourceError> {
        let now_ms = (self.now)();
        let end_ms = floor_to_second(now_ms);
        let all_targets = {
            let mut targets: Vec<TargetRef> = scope
                .conversations
                .iter()
                .map(|cid| TargetRef::Group(cid.clone()))
                .chain(scope.people.iter().map(|pid| TargetRef::Person(pid.clone())))
                .collect();
            targets.sort();
            targets
        };

        // 空白名单：fail closed——不发起任何子进程调用。
        if all_targets.is_empty() {
            return Ok(RecordPage {
                records: Vec::new(),
                next_cursor: None,
                has_more: false,
                snapshot_at: now_ms,
            });
        }

        let lookback = self.config.initial_lookback.as_millis() as i64;
        let (req_start_ms, req_end_ms, remaining, mut page_token) = match cursor {
            Some(value) => {
                let parsed: DwsCursor = serde_json::from_value(value.clone()).map_err(|_| {
                    SourceError::Other {
                        message: "dws 游标形状不识别（拒绝猜测）".into(),
                    }
                })?;
                if parsed.v != 1 {
                    return Err(SourceError::Other {
                        message: "dws 游标版本不识别".into(),
                    });
                }
                if parsed.remaining.is_empty() {
                    (
                        floor_to_second(parsed.window_end_ms - page.overlap_ms),
                        end_ms,
                        all_targets,
                        None,
                    )
                } else {
                    // 轮内续页：窗口原样重复（page-token 绑定查询条件）。
                    let targets: Vec<TargetRef> = parsed
                        .remaining
                        .iter()
                        .map(|tag| TargetRef::from_tag(tag))
                        .collect::<Option<Vec<_>>>()
                        .ok_or_else(|| SourceError::Other {
                            message: "dws 游标 remaining 不合法".into(),
                        })?;
                    if !targets.iter().all(|t| all_targets.contains(t)) {
                        return Err(SourceError::Other {
                            message: "dws 游标目标超出白名单（拒绝）".into(),
                        });
                    }
                    (
                        parsed.window_start_ms,
                        parsed.window_end_ms,
                        targets,
                        parsed.page_token,
                    )
                }
            }
            None => (
                floor_to_second(end_ms - lookback),
                end_ms,
                all_targets,
                None,
            ),
        };

        // 批准时间窗（fail closed）：未经调用方显式批准起止 → 拒绝读取，
        // 不发起任何 CLI 调用。
        let Some(approved) = self.config.approved_window else {
            return Err(SourceError::Other {
                message: "dws 未授权读取时间窗（需调用方显式批准起止）".into(),
            });
        };
        // 请求窗口与批准范围取交集。CLI 边界只支持整秒：起点向上取整、终点
        // 向下取整——请求范围只会落在批准范围之内，绝不会多读。
        let clamped = approved
            .clamp(req_start_ms, req_end_ms)
            .map(|(start, end)| (ceil_to_second(start), floor_to_second(end)))
            .filter(|(start, end)| start < end);
        let Some((start_ms, end_ms)) = clamped else {
            // 无交集：授权范围外不读。空终页让引擎正常推进（游标记本轮请求
            // 窗口，下轮按 overlap 重开后再取交集）。
            return Ok(RecordPage {
                records: Vec::new(),
                next_cursor: Some(
                    serde_json::to_value(DwsCursor {
                        v: 1,
                        window_start_ms: req_start_ms,
                        window_end_ms: req_end_ms,
                        remaining: Vec::new(),
                        page_token: None,
                    })
                    .expect("游标序列化不会失败"),
                ),
                has_more: false,
                snapshot_at: now_ms,
            });
        };
        let start = format_datetime(start_ms, self.config.tz_offset_ms);
        let end = format_datetime(end_ms, self.config.tz_offset_ms);
        let me = self.config.me();
        let me_user_id = me.as_deref();
        let opts = self.config.transport_options();

        let mut records: Vec<RawRecord> = Vec::new();
        let mut pages_used = 0usize;
        let mut index = 0usize;
        while index < remaining.len() {
            let target = remaining[index].clone();
            loop {
                if pages_used >= self.config.max_pages_per_round {
                    // 单轮页数上限 → partial：当前目标起的剩余目标 + 续页 token。
                    let rest: Vec<String> =
                        remaining[index..].iter().map(|t| t.tag()).collect();
                    return Ok(RecordPage {
                        records,
                        next_cursor: Some(
                            serde_json::to_value(DwsCursor {
                                v: 1,
                                window_start_ms: start_ms,
                                window_end_ms: end_ms,
                                remaining: rest,
                                page_token: page_token.clone(),
                            })
                            .expect("游标序列化不会失败"),
                        ),
                        has_more: true,
                        snapshot_at: now_ms,
                    });
                }
                let command = DwsReadCommand::ChatMessages {
                    target: target.clone(),
                    start: start.clone(),
                    end: end.clone(),
                    page_token: page_token.clone(),
                    limit: self.config.page_limit,
                    profile: self.config.profile.clone(),
                };
                let Some(command) = command.sanitized() else {
                    return Err(SourceError::Other {
                        message: "dws 参数校验失败（目标/游标不合法）".into(),
                    });
                };
                pages_used += 1;
                let proc = transport(&self.config.exe, &command, &opts).map_err(source_error)?;
                let envelope = parse_envelope(&proc, self.config.stream_cap)?;
                for (conversation_id, message) in envelope.messages {
                    let record = self
                        .record_from_message(&target, conversation_id.as_deref(), &message)
                        .map_err(|reason| SourceError::Other {
                            message: format!("dws 信封字段缺失：{reason}"),
                        })?;
                    // 防御纵深：只保留本目标授权范围内、且落在批准时间窗内的记录。
                    if target.owns(&record, me_user_id) && approved.contains(record.occurred_at) {
                        records.push(record);
                    }
                }
                match envelope.continuation {
                    Some(token) => page_token = Some(token),
                    None => {
                        page_token = None;
                        break;
                    }
                }
            }
            index += 1;
        }

        Ok(RecordPage {
            records,
            next_cursor: Some(
                serde_json::to_value(DwsCursor {
                    v: 1,
                    window_start_ms: start_ms,
                    window_end_ms: end_ms,
                    remaining: Vec::new(),
                    page_token: None,
                })
                .expect("游标序列化不会失败"),
            ),
            has_more: false,
            snapshot_at: now_ms,
        })
    }

    /// 信封 message 字段 → `RawRecord`（字段名按读取契约 §4.3，不可换名猜测）。
    fn record_from_message(
        &self,
        target: &TargetRef,
        conversation_id: Option<&str>,
        message: &Value,
    ) -> Result<RawRecord, &'static str> {
        let external_id = message["messageId"]
            .as_str()
            .ok_or("messageId（external_id）")?;
        let time = &message["createTime"];
        let occurred_at = time.as_i64().or_else(|| {
            chrono::NaiveDateTime::parse_from_str(time.as_str()?, "%Y-%m-%d %H:%M:%S")
                .ok()?.and_utc().timestamp_millis().checked_sub(self.config.tz_offset_ms)
        }).ok_or("createTime")?;
        let sender_id = message["senderId"].as_str();
        let direction = match (self.config.me(), sender_id) {
            (Some(me), Some(sender)) if me == sender => Direction::Out,
            (_, Some(_)) => Direction::In,
            (_, None) => Direction::Na,
        };
        // 引用链：读取侧 quotedMessage.messageId / 事件侧 quoted_message.message_id。
        let reply_to_id = ["messageId", "message_id"]
            .iter()
            .find_map(|key| message["quotedMessage"][key].as_str().map(|s| s.to_string()));
        Ok(RawRecord {
            external_id: external_id.to_string(),
            kind: RecordKind::ImMessage,
            occurred_at,
            conversation_id: message["conversationId"]
                .as_str()
                .map(|s| s.to_string())
                .or_else(|| conversation_id.map(|s| s.to_string())),
            sender_id: sender_id.map(|s| s.to_string()),
            sender_name: message["sender"].as_str().map(|s| s.to_string()),
            direction,
            summary: None,
            body: message["text"].as_str().map(|s| s.to_string()),
            perm_scope: None,
            // 读取契约无 @ 列表字段（模块文档「已知缺口」）：预筛 at_me 不依赖它。
            mentions: Vec::new(),
            reply_to_id,
            conversation_type: Some(target.conversation_type()),
        })
    }
}

impl DingTalkSource for CliDingTalkSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn scope(&self) -> &Scope {
        &self.scope
    }

    fn probe(&self) -> Result<(), SourceError> {
        self.probe_impl()
    }

    fn fetch(
        &self,
        scope: &Scope,
        cursor: Option<&Value>,
        page: &PageParams,
    ) -> Result<RecordPage, SourceError> {
        self.fetch_impl(scope, cursor, page)
    }
}

// ---------------------------------------------------------------------------
// 子进程传输与信封解析
// ---------------------------------------------------------------------------

/// 执行一条白名单命令。
fn transport(
    exe: &Path,
    command: &DwsReadCommand,
    opts: &DwsTransportOptions,
) -> Result<ProcOutcome, DwsCliError> {
    let mut extra_env: Vec<(String, String)> = Vec::new();
    if let Some(dir) = &opts.config_dir {
        extra_env.push(("DWS_CONFIG_DIR".to_string(), dir.to_string_lossy().into_owned()));
    }
    let result = subprocess::run(Invocation {
        program: exe.to_path_buf(),
        args: command.argv(),
        stdin: None,
        timeout: opts.call_timeout,
        cap: opts.stream_cap,
        home: opts.home.clone(),
        cwd: None,
        extra_env,
    });
    match result {
        ProcResult::Spawn(reason) => Err(DwsCliError::Spawn(reason)),
        ProcResult::Ran(proc) => {
            if proc.timed_out {
                Err(DwsCliError::Timeout)
            } else {
                Ok(proc)
            }
        }
    }
}

/// stderr 错误信封 → 分类错误（信息只含 category/退出码，无正文）。
fn map_failure(proc: &ProcOutcome) -> DwsCliError {
    if let Ok(value) = serde_json::from_slice::<Value>(&proc.stderr) {
        let error = &value["error"];
        let category = error["category"].as_str().unwrap_or("").to_string();
        let reason = error["reason"].as_str().unwrap_or("");
        let retryable = error["retryable"].as_bool();
        let retry_after_ms = error["retry_after_seconds"]
            .as_f64()
            .map(|s| (s * 1000.0) as u64);
        if category == "auth" || reason == "auth_refresh_failed" {
            return DwsCliError::Auth;
        }
        if retryable == Some(true) {
            return DwsCliError::RateLimited {
                category,
                retry_after_ms,
            };
        }
        if !category.is_empty() {
            return DwsCliError::Failed {
                category,
                exit: proc.exit,
            };
        }
    }
    DwsCliError::Failed {
        category: format!("stderr_unparsed_{}B", proc.stderr.len()),
        exit: proc.exit,
    }
}

/// 独立入口错误 → 引擎 `SourceError`。
fn source_error(error: DwsCliError) -> SourceError {
    match error {
        DwsCliError::Spawn(message) => SourceError::Other { message },
        DwsCliError::Timeout => SourceError::Timeout {
            message: "dws 调用超时（外层 kill）".into(),
        },
        DwsCliError::Auth => SourceError::Auth {
            status: 401,
            message: "dws auth (category=auth)".into(),
        },
        DwsCliError::RateLimited {
            category,
            retry_after_ms,
        } => SourceError::RateLimited {
            message: format!("dws retryable (category={category})"),
            retry_after_ms,
        },
        DwsCliError::Failed { category, exit } => SourceError::Other {
            message: format!("dws error (category={category}, exit={exit:?})"),
        },
        DwsCliError::BadEnvelope(reason) => SourceError::Other {
            message: format!("dws 信封不可解析：{reason}"),
        },
    }
}

/// 信封解析产物：扁平消息列表 + 续页 token。
struct ParsedEnvelope {
    messages: Vec<(Option<String>, Value)>,
    continuation: Option<String>,
}

/// 校验契约与完整性台账（§4.2），抽取消息与续页 token。
fn parse_envelope(proc: &ProcOutcome, _cap: usize) -> Result<ParsedEnvelope, SourceError> {
    if proc.exit != Some(0) {
        return Err(source_error(map_failure(proc)));
    }
    let value: Value = serde_json::from_slice(&proc.stdout).map_err(|_| SourceError::Other {
        message: format!("dws stdout 非常 JSON（{}B）", proc.stdout.len()),
    })?;
    let contract = value["contractVersion"].as_str().unwrap_or("");
    if contract != MESSAGE_LIST_CONTRACT {
        return Err(SourceError::Other {
            message: format!("dws contractVersion={contract:?}≠{MESSAGE_LIST_CONTRACT:?}，拒收"),
        });
    }
    // 完整性台账（§4.2）：截断 / partial / failures ≠ ∅ → 整轮失败、游标不推进
    // （未获取 ≠ 未发生）。`complete=false` 本身不是失败——它配合游标表示
    // 「还有后续页」，终止判定见下方。
    let truncated = ["truncated", "truncatedByPageLimit", "truncatedByResultLimit", "partial"]
        .iter()
        .any(|key| value[*key].as_bool() == Some(true));
    let failed = value["failures"].as_array().map_or(0, |f| f.len());
    if truncated || failed > 0 {
        return Err(SourceError::Other {
            message: format!("dws 台账不完整（truncated={truncated}，failures={failed}）"),
        });
    }
    // 消息定位：list-all 合并形 result.conversationMessagesList[].messages[]
    // 或信封顶层 messages[]（§4.1/§4.2 两种文档化形状）。
    let mut messages: Vec<(Option<String>, Value)> = Vec::new();
    if let Some(rooms) = value["result"]["conversationMessagesList"].as_array() {
        for room in rooms {
            let conversation = room["conversationId"].as_str().map(|s| s.to_string());
            if let Some(items) = room["messages"].as_array() {
                for message in items {
                    messages.push((conversation.clone(), message.clone()));
                }
            }
        }
    } else if let Some(items) = value["messages"].as_array() {
        for message in items {
            messages.push((None, message.clone()));
        }
    } else {
        return Err(SourceError::Other {
            message: "dws 信封缺 messages/conversationMessagesList".into(),
        });
    }
    // 终止判定（§4.2）：响应仍带游标（或 hasMore=true）→ 续页；不再返回游标
    // 或 complete=true → 完成。空页 + 游标非空 = 继续；空页 + 游标为空 = 正常完成。
    let token = ["nextCursor", "nextPageToken"]
        .iter()
        .find_map(|key| value[*key].as_str())
        .or_else(|| value["nextPage"]["token"].as_str());
    let has_more = value["hasMore"].as_bool() == Some(true);
    let continuation = match token {
        Some(token) if safe_arg(token) => Some(token.to_string()),
        Some(_) => {
            // 游标值不可安全回传（旗标注入风险）：拒绝而不是猜测。
            return Err(SourceError::Other {
                message: "dws 续页游标不合法（拒绝回传）".into(),
            });
        }
        None if has_more => {
            // hasMore 但无安全续点：截断不冒进（M3 门槛）。
            return Err(SourceError::Other {
                message: "dws hasMore 但无可续游标（截断不冒进）".into(),
            });
        }
        None => {
            if value["complete"].as_bool() == Some(false) {
                return Err(SourceError::Other {
                    message: "dws complete=false 且无续点（不冒认完整）".into(),
                });
            }
            None
        }
    };
    Ok(ParsedEnvelope {
        messages,
        continuation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitelist_is_closed_and_read_only() {
        // 全部可表达命令 ∈ 白名单，argv 无 shell 拼接、无写操作。
        let commands = vec![
            DwsReadCommand::Version,
            DwsReadCommand::AuthStatus {
                profile: "corp:u1".into(),
            },
            DwsReadCommand::ChatMessages {
                target: TargetRef::Group("cidA".into()),
                start: "2026-11-21 00:00:00".into(),
                end: "2026-11-22 00:00:00".into(),
                page_token: None,
                limit: 50,
                profile: "corp:u1".into(),
            },
        ];
        for command in commands {
            assert!(READ_COMMANDS.contains(&command.name()));
            let argv = command.argv();
            // 参数数组直传：无 shell 元字符；旗标后的值不得被解析成另一个旗标。
            assert!(argv.iter().all(|a| !a.contains(';') && !a.contains('|') && !a.contains('`')));
            for pair in argv.windows(2) {
                if pair[0].starts_with("--") {
                    assert!(!pair[1].starts_with("--"), "值被解析成旗标：{pair:?}");
                }
            }
        }
    }

    #[test]
    fn unsafe_args_are_rejected() {
        let bad = DwsReadCommand::AuthStatus {
            profile: "--evil".into(),
        };
        assert!(bad.clone().sanitized().is_none());
        let bad = DwsReadCommand::AuthStatus {
            profile: "a b".into(),
        };
        assert!(bad.sanitized().is_none());
        assert!(!safe_arg(""));
        assert!(!safe_arg("--flag"));
        assert!(safe_arg("corp:u1"));
        // 时间窗参数必须严格 `yyyy-MM-dd HH:mm:ss`。
        let bad_time = DwsReadCommand::ChatMessages {
            target: TargetRef::Group("cidA".into()),
            start: "2026-11-21".into(),
            end: "2026-11-22 00:00:00".into(),
            page_token: None,
            limit: 50,
            profile: "corp:u1".into(),
        };
        assert!(bad_time.sanitized().is_none());
    }

    #[test]
    fn target_tags_roundtrip_and_order() {
        let t = TargetRef::Group("cidA".into());
        assert_eq!(TargetRef::from_tag(&t.tag()), Some(t));
        let t = TargetRef::Person("uid9".into());
        assert_eq!(TargetRef::from_tag(&t.tag()), Some(t));
        assert_eq!(TargetRef::from_tag("x:1"), None);
        // Group < Person，同类型按字典序。
        let mut sorted = [TargetRef::Person("z".into()), TargetRef::Group("b".into()), TargetRef::Group("a".into())];
        sorted.sort();
        assert_eq!(sorted[0], TargetRef::Group("a".into()));
        assert_eq!(sorted[2], TargetRef::Person("z".into()));
    }

    #[test]
    fn envelope_rejects_foreign_contract_and_incomplete() {
        let mut proc = ProcOutcome {
            exit: Some(0),
            timed_out: false,
            stdout: br#"{"contractVersion":"im.message-list.v2","complete":true,"messages":[]}"#.to_vec(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        };
        assert!(parse_envelope(&proc, 1024).is_err());
        proc.stdout = br#"{"contractVersion":"im.message-list.v1","complete":false}"#.to_vec();
        assert!(parse_envelope(&proc, 1024).is_err());
        proc.stdout =
            br#"{"contractVersion":"im.message-list.v1","complete":true,"truncated":true}"#.to_vec();
        assert!(parse_envelope(&proc, 1024).is_err());
        proc.stdout = br#"{"contractVersion":"im.message-list.v1","complete":true,"hasMore":true}"#.to_vec();
        // hasMore 无续点 → 拒绝。
        assert!(parse_envelope(&proc, 1024).is_err());
        proc.stdout = br#"{"contractVersion":"im.message-list.v1","complete":true,"messages":[{"messageId":"m1","createTime":1}]}"#.to_vec();
        let parsed = parse_envelope(&proc, 1024).expect("完整信封");
        assert_eq!(parsed.messages.len(), 1);
        assert!(parsed.continuation.is_none());
    }

    #[test]
    fn auth_error_envelope_maps_to_auth() {
        let proc = ProcOutcome {
            exit: Some(3),
            timed_out: false,
            stdout: Vec::new(),
            stderr: br#"{"error":{"code":3,"category":"auth","reason":"auth_refresh_failed","retryable":false}}"#.to_vec(),
            stdout_truncated: false,
            stderr_truncated: false,
        };
        assert_eq!(map_failure(&proc), DwsCliError::Auth);
        let proc = ProcOutcome {
            exit: Some(1),
            timed_out: false,
            stdout: Vec::new(),
            stderr: br#"{"error":{"category":"api","retryable":true,"retry_after_seconds":2}}"#.to_vec(),
            stdout_truncated: false,
            stderr_truncated: false,
        };
        assert_eq!(
            map_failure(&proc),
            DwsCliError::RateLimited {
                category: "api".into(),
                retry_after_ms: Some(2000),
            }
        );
    }

    #[test]
    fn error_messages_never_carry_output_bodies() {
        let proc = ProcOutcome {
            exit: Some(9),
            timed_out: false,
            stdout: Vec::new(),
            stderr: b"RAW SECRET stderr".to_vec(),
            stdout_truncated: false,
            stderr_truncated: false,
        };
        let err = source_error(map_failure(&proc));
        let SourceError::Other { message } = err else {
            panic!("应归 Other");
        };
        assert!(!message.contains("RAW"), "不得携带 stderr 正文：{message}");
        assert!(message.contains("stderr_unparsed_17B"));
    }
}
