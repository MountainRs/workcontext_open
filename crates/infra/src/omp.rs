//! OMP CLI 判读 runtime（详细设计 §8 `CliAgentRuntime`；OMP 接口调研 §7 的
//! 落地版；遗留事项 B7）。
//!
//! - **argv 直传 + stdin 载荷**：参数数组不经 shell；`--system-prompt` 走 argv
//!   （短字符串），用户内容（判读正文）**只走 stdin**——不进 argv，避免进程
//!   列表泄露与长度限制（调研 §3）。
//! - **JSONL 边界**：stdout 是 JSONL 事件流（调研 §4），逐行解析、只取
//!   `session` / `message_end`(assistant) / `agent_end(isTerminal)`，忽略未知
//!   事件类型（前向兼容）；2 MB 硬上限（§7.4），超限判 failed。流式 delta
//!   （`message_update`）与终稿重复，一律忽略。
//! - **双层超时**：内层 `--max-time = timeout − inner_margin`（默认 5s，OMP
//!   优雅中止 exit 1 + `stopReason:"aborted"`）；外层墙钟 kill 兜底（调研 §5；
//!   默认 `timeout` 60s——实测中转 TTFT 14–19s，30s 偏紧）。
//! - **失败/超时/不可用三态分开**：`Rejected`（模型输出非法，§8.3 不自动重试）、
//!   `Timeout`（可重试）、`Unavailable`（exit 2 旗标失效/版本漂移/模型未批准
//!   →黄条，不静默降级为 mock，§8.1 选择逻辑）。
//! - **出域闸门单层化**：[`OmpCliRuntime::run_judge`] 内部执行
//!   [`crate::egress::EgressGate`]（策略/批准模型来自 [`OmpCliConfig`]，默认
//!   `none` + 无批准模型 → 一切判读 `Blocked`，正文不出工作台）；底层
//!   [`OmpCliRuntime::run_classify`] 只接受闸门产出的 [`EgressPayload`]（类型
//!   封闭），并校验 `approved_model` 与自身 `model_id` 一致。
//! - **日志无正文**：本模块不持久化任何原始 stdout/stderr/提示词；错误信息只
//!   含退出码、事件行号、字节规模、契约错误路径。
//!
//! 与 `mock.rs` 同口径：不引入 async 运行时与 core DTO 依赖，输出为已通过
//! `classify/v1` 契约校验的 `serde_json::Value`。

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use crate::classify_input::ClassifyInput;
use crate::contract::validate_classify_v1;
use crate::egress::{EgressBlockReason, EgressGate, EgressPayload, EgressPolicy};
use crate::subprocess::{self, Invocation, ProcResult, DEFAULT_STREAM_CAP};

/// runtime 名（写入 `agent_run.runtime`）。
pub const RUNTIME_NAME: &str = "cli";
/// 默认墙钟超时（调研 §5：TTFT 14–19s，60s 默认）。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// 内层 `--max-time` 的安全边距（调研 §5：内层 = timeout − 5s）。
pub const DEFAULT_INNER_MARGIN: Duration = Duration::from_secs(5);

/// 隔离旗标组（调研 §3/§7.1）：无工具、无会话落盘、无项目上下文发现。
const ISOLATION_FLAGS: [&str; 10] = [
    "--no-tools",
    "--no-session",
    "--no-extensions",
    "--no-skills",
    "--no-rules",
    "--no-context-files",
    "--no-lsp",
    "--no-title",
    "--thinking",
    "off",
    // 占位见 build_argv：-p/--mode json 在前，以上顺序稳定供测试断言。
];

/// OMP CLI 配置（持久化在 `config.json` 的 `agent`/`egress` 段）。
#[derive(Debug, Clone)]
pub struct OmpCliConfig {
    /// `omp` 可执行文件绝对路径。
    pub exe: PathBuf,
    /// 精确 `provider/model` id（写入 `agent_run.model_id`）。
    pub model_id: String,
    /// 外层墙钟超时（kill 兜底）。
    pub timeout: Duration,
    /// 内层 `--max-time` 边距；`--max-time = timeout − inner_margin`（下限 1s）。
    pub inner_margin: Duration,
    /// 子进程 `HOME`：方案 A（默认推荐）传用户真实家目录以复用凭据；
    /// 方案 B 传隔离目录（调研 §7.3，由 app 层决定，本层只透传）。
    pub home: Option<PathBuf>,
    /// 子进程工作目录（临时空目录，防项目内文件被发现；与旗标组双保险）。
    pub cwd: Option<PathBuf>,
    /// stdout/stderr 硬上限。
    pub stream_cap: usize,
    /// 期望的 omp 主版本（health 比对，防 `update.auto` 漂移；None 跳过比对）。
    pub expected_version_major: Option<u32>,
    /// 出域策略（默认 [`EgressPolicy::None`]：阻断一切正文）。
    pub egress_policy: EgressPolicy,
    /// 已批准模型（C2 授权产物；Redacted/Full 必需）。
    pub approved_model: Option<String>,
}

impl OmpCliConfig {
    /// 最小构造：其余参数取默认（timeout 60s / margin 5s / cap 2MB / egress 阻断）。
    pub fn new(exe: PathBuf, model_id: impl Into<String>) -> Self {
        Self {
            exe,
            model_id: model_id.into(),
            timeout: DEFAULT_TIMEOUT,
            inner_margin: DEFAULT_INNER_MARGIN,
            home: None,
            cwd: None,
            stream_cap: DEFAULT_STREAM_CAP,
            expected_version_major: None,
            egress_policy: EgressPolicy::None,
            approved_model: None,
        }
    }

    fn gate(&self) -> EgressGate {
        EgressGate::new(self.egress_policy, self.approved_model.clone())
    }
}

/// 判读成功：已通过 `classify/v1` 契约校验的输出 + 计费/观测字段
/// （调研 §4 取数路径：最后一条 assistant `message_end`）。
#[derive(Debug, Clone, PartialEq)]
pub struct ClassifyOk {
    pub output: Value,
    pub model_id: String,
    pub session_ref: Option<String>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub cost_usd: Option<f64>,
    pub latency_ms: Option<f64>,
}

/// Observed CLI telemetry, including non-success outcomes. Missing counters
/// remain `None`; neither exit status nor wall time implies token usage.
#[derive(Debug, Default)]
pub struct CallUsage {
    pub process_started: bool,
    pub reported_model: Option<String>,
    pub session_ref: Option<String>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub cost_usd: Option<f64>,
    pub latency_ms: Option<f64>,
}

/// 一次判读的结局（app 层一层映射到 `agent_run.status`）。
#[derive(Debug, Clone, PartialEq)]
pub enum CliAgentOutcome {
    /// `status='ok'`。
    Ok(ClassifyOk),
    /// `status='rejected'`：模型输出非法 JSON 或不符 classify/v1（不自动重试）。
    Rejected(RejectCode),
    /// `status='failed'`：空回复/流损坏/超限/异常退出。
    Failed(FailCode),
    /// `status='timeout'`：可重试（§8.3 由用户手动批处理）。
    Timeout(TimeoutKind),
    /// 出域闸门阻断：不触模型，调用方记 `egress_log` + `judge_state='blocked'`。
    Blocked {
        policy: EgressPolicy,
        reason: EgressBlockReason,
    },
    /// runtime 不可用：黄条提示，不静默降级、不重试。
    Unavailable(UnavailCode),
    /// `--max-cost` 护栏触发（exit 3）：failed + 黄条。
    CostGuard,
}

/// 拒绝码（`agent_run.error_code`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectCode {
    /// 模型文本不是合法 JSON（`AGENT_BAD_JSON`）。
    BadJson,
    /// 不符 classify/v1 契约（`AGENT_SCHEMA_V1`）。
    Schema,
}

/// 失败码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailCode {
    /// exit 0 但 assistant 内容为空（§7.2）。
    EmptyContent,
    /// stdout 超过硬上限（§7.4）。
    OutputTooLarge,
    /// JSONL 中出现非 JSON 行（流损坏；只记行号，不记内容）。
    BadStream { line: usize },
    /// 其余非零退出（stderr 不入日志，只记码）。
    Exit { code: i32 },
}

/// 超时种类（调研 §5：`aborted ∨ 外部 kill ∨ 无 agent_end 收尾`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutKind {
    /// 内层 `--max-time` 优雅中止：exit 1 + `stopReason:"aborted"`。
    InnerAborted,
    /// 外层墙钟 kill（或子进程被信号杀死）。
    OuterKill,
    /// exit 0 但无 `agent_end(isTerminal)` 收尾。
    NoTerminalEvent,
}

/// runtime 不可用码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnavailCode {
    /// 启动失败（`AGENT_UNREACHABLE`）。
    Spawn,
    /// exit 2：旗标失效/用法错误（多半版本漂移），需人工处理。
    ConfigExit,
    /// 载荷的批准模型与 runtime 模型不一致（闸门与 runtime 双保险）。
    ModelNotApproved,
    /// Archive result did not attest the exact approved provider/model.
    ModelIdentityUnverified,
}

impl CliAgentOutcome {
    /// `agent_run.status` 取值（迁移 0001 CHECK 集合）。
    ///
    /// 注意 [`CliAgentOutcome::Blocked`]：模型未被调用，按 §7.3 **不应落
    /// `agent_run` 行**——调用方记 `egress_log(decision='blocked')` 并置
    /// `source_record.judge_state='blocked'`；此处的 `"rejected"` 只是为
    /// CHECK 集合提供一个合法占位，调用方应先按变体匹配处理 Blocked。
    pub fn status(&self) -> &'static str {
        match self {
            CliAgentOutcome::Ok(_) => "ok",
            CliAgentOutcome::Rejected(_) | CliAgentOutcome::Blocked { .. } => "rejected",
            CliAgentOutcome::Failed(_)
            | CliAgentOutcome::Unavailable(_)
            | CliAgentOutcome::CostGuard => "failed",
            CliAgentOutcome::Timeout(_) => "timeout",
        }
    }

    /// `agent_run.error_code`（None = ok）。
    pub fn error_code(&self) -> Option<String> {
        match self {
            CliAgentOutcome::Ok(_) => None,
            CliAgentOutcome::Rejected(code) => Some(
                match code {
                    RejectCode::BadJson => "AGENT_BAD_JSON",
                    RejectCode::Schema => "AGENT_SCHEMA_V1",
                }
                .to_string(),
            ),
            CliAgentOutcome::Failed(code) => Some(
                match code {
                    FailCode::EmptyContent => "AGENT_EMPTY".to_string(),
                    FailCode::OutputTooLarge => "AGENT_OUTPUT_TOO_LARGE".to_string(),
                    FailCode::BadStream { line } => format!("AGENT_BAD_STREAM_L{line}"),
                    FailCode::Exit { code } => format!("AGENT_EXIT_{code}"),
                },
            ),
            CliAgentOutcome::Timeout(kind) => Some(
                match kind {
                    TimeoutKind::InnerAborted => "AGENT_TIMEOUT_INNER",
                    TimeoutKind::OuterKill => "AGENT_TIMEOUT_OUTER",
                    TimeoutKind::NoTerminalEvent => "AGENT_TIMEOUT_NO_TERMINAL",
                }
                .to_string(),
            ),
            CliAgentOutcome::Blocked { reason, .. } => Some(reason.code().to_string()),
            CliAgentOutcome::Unavailable(code) => Some(
                match code {
                    UnavailCode::Spawn => "AGENT_UNREACHABLE",
                    UnavailCode::ConfigExit => "AGENT_CONFIG",
                    UnavailCode::ModelNotApproved => "AGENT_MODEL_NOT_APPROVED",
                    UnavailCode::ModelIdentityUnverified => "AGENT_MODEL_IDENTITY_UNVERIFIED",
                }
                .to_string(),
            ),
            CliAgentOutcome::CostGuard => Some("AGENT_COST_GUARD".to_string()),
        }
    }

    /// 是否值得进「待重试」（§8.3：只有 timeout 手动批处理）。
    pub fn retryable(&self) -> bool {
        matches!(self, CliAgentOutcome::Timeout(_))
    }
}

/// health 探测结局（`omp --version`）。
#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeHealth {
    /// 可用；`version` 如 `"omp/19.1.0"`。
    Ok { version: String },
    Unavailable { code: RuntimeHealthCode },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeHealthCode {
    /// 启动失败（`AGENT_UNREACHABLE`）。
    Spawn,
    /// 探测超时。
    Timeout,
    /// 输出不可解析。
    BadOutput,
    /// 主版本与配置不符（`update.auto` 漂移，提示钉版本）。
    VersionMismatch { found: u32, expected_major: u32 },
}

/// OMP CLI 判读 runtime。
#[derive(Debug, Clone)]
pub struct OmpCliRuntime {
    config: OmpCliConfig,
}

impl OmpCliRuntime {
    pub fn new(config: OmpCliConfig) -> Self {
        Self { config }
    }

    pub fn runtime_name(&self) -> &'static str {
        RUNTIME_NAME
    }

    pub fn model_id(&self) -> &str {
        &self.config.model_id
    }

    /// 判读入口（app 层唯一入口）：内部执行出域闸门。
    ///
    /// - 阻断（默认 `policy=none`）→ [`CliAgentOutcome::Blocked`]，**不 spawn**；
    /// - 放行 → 组装 §7.2 提示词并调 [`OmpCliRuntime::run_classify`]。
    pub fn run_judge(&self, input: &ClassifyInput) -> CliAgentOutcome {
        self.run_judge_measured(input).0
    }

    pub fn run_judge_measured(&self, input: &ClassifyInput) -> (CliAgentOutcome, CallUsage) {
        match self.config.gate().check(input) {
            crate::egress::EgressDecision::Blocked(reason) => (
                CliAgentOutcome::Blocked { policy: self.config.egress_policy, reason },
                CallUsage::default(),
            ),
            crate::egress::EgressDecision::Allowed(payload) => {
                self.run_classify_measured(&payload, input.archive_context.is_some())
            }
        }
    }

    /// 底层执行：只接受闸门产出的载荷（类型封闭）；校验批准模型一致。
    pub fn run_classify(&self, payload: &EgressPayload) -> CliAgentOutcome {
        self.run_classify_measured(payload, false).0
    }

    fn run_classify_measured(
        &self, payload: &EgressPayload, archive: bool,
    ) -> (CliAgentOutcome, CallUsage) {
        if payload.approved_model() != self.config.model_id {
            return (CliAgentOutcome::Unavailable(UnavailCode::ModelNotApproved), CallUsage::default());
        }
        let mut args = self.classify_argv(payload.system());
        if archive {
            args.push("--offline".into());
        }
        let result = subprocess::run(Invocation {
            program: self.config.exe.clone(),
            args,
            stdin: Some(payload.user().as_bytes().to_vec()),
            timeout: self.config.timeout,
            cap: self.config.stream_cap,
            home: self.config.home.clone(),
            cwd: self.config.cwd.clone(),
            extra_env: vec![],
        });
        let ProcResult::Ran(proc) = result else {
            return (CliAgentOutcome::Unavailable(UnavailCode::Spawn), CallUsage::default());
        };
        self.map_measured(&proc, archive)
    }

    /// 健康探测：`omp --version`（无模型调用、无凭据依赖）。
    pub fn health(&self) -> RuntimeHealth {
        let result = subprocess::run(Invocation {
            program: self.config.exe.clone(),
            args: vec!["--version".to_string()],
            stdin: None,
            timeout: Duration::from_secs(10),
            cap: 4 * 1024,
            home: self.config.home.clone(),
            cwd: self.config.cwd.clone(),
            extra_env: vec![],
        });
        let ProcResult::Ran(proc) = result else {
            return RuntimeHealth::Unavailable {
                code: RuntimeHealthCode::Spawn,
            };
        };
        if proc.timed_out {
            return RuntimeHealth::Unavailable {
                code: RuntimeHealthCode::Timeout,
            };
        }
        if proc.exit != Some(0) || proc.stdout_truncated {
            return RuntimeHealth::Unavailable {
                code: RuntimeHealthCode::BadOutput,
            };
        }
        let Some(version) = parse_version(&proc.stdout) else {
            return RuntimeHealth::Unavailable {
                code: RuntimeHealthCode::BadOutput,
            };
        };
        if let (Some(expected), Some(found)) =
            (self.config.expected_version_major, major_of(&version))
        {
            if found != expected {
                return RuntimeHealth::Unavailable {
                    code: RuntimeHealthCode::VersionMismatch {
                        found,
                        expected_major: expected,
                    },
                };
            }
        }
        RuntimeHealth::Ok { version }
    }

    /// argv 模板（调研 §7.1；`--cwd` 用进程级 cwd 实现等价隔离，不用未验证旗标）。
    fn classify_argv(&self, system: &str) -> Vec<String> {
        let inner_secs = self
            .config
            .timeout
            .saturating_sub(self.config.inner_margin)
            .as_secs()
            .max(1);
        let args: Vec<String> = vec![
            "-p".into(),
            "--mode".into(),
            "json".into(),
            "--no-tools".into(),
            "--no-session".into(),
            "--no-extensions".into(),
            "--no-skills".into(),
            "--no-rules".into(),
            "--no-context-files".into(),
            "--no-lsp".into(),
            "--no-title".into(),
            "--thinking".into(),
            "off".into(),
            "--max-time".into(),
            inner_secs.to_string(),
            "--model".into(),
            self.config.model_id.clone(),
            "--system-prompt".into(),
            system.to_string(),
        ];
        debug_assert_eq!(&args[3..13], &ISOLATION_FLAGS[..]);
        args
    }

    /// 退出状态 → 结局映射（调研 §7.2；所有分支不携带任何输出正文）。
    fn map_measured(
        &self, proc: &subprocess::ProcOutcome, archive: bool,
    ) -> (CliAgentOutcome, CallUsage) {
        let (extract, bad_line) = extract_jsonl(&proc.stdout);
        let mut usage = CallUsage { process_started: true, ..CallUsage::default() };
        usage.session_ref.clone_from(&extract.session_ref);
        if let Some(a) = &extract.assistant {
            if let Some(provider) = &a.provider {
                if !a.model_id.is_empty() {
                    usage.reported_model = Some(format!("{provider}/{}", a.model_id));
                }
            }
            usage.tokens_in = a.tokens_in;
            usage.tokens_out = a.tokens_out;
            usage.cost_usd = a.cost_usd;
            usage.latency_ms = a.duration_ms;
        }
        let outcome = self.map_outcome(proc, extract, bad_line);
        let outcome = if archive
            && (matches!(outcome, CliAgentOutcome::Ok(_)) || usage.reported_model.is_some())
            && usage.reported_model.as_deref() != Some(self.model_id())
        {
            CliAgentOutcome::Unavailable(UnavailCode::ModelIdentityUnverified)
        } else {
            outcome
        };
        (outcome, usage)
    }

    fn map_outcome(
        &self, proc: &subprocess::ProcOutcome, extract: JsonlExtract, bad_line: Option<usize>,
    ) -> CliAgentOutcome {
        if proc.timed_out {
            return CliAgentOutcome::Timeout(TimeoutKind::OuterKill);
        }
        match proc.exit {
            Some(2) => return CliAgentOutcome::Unavailable(UnavailCode::ConfigExit),
            Some(3) => return CliAgentOutcome::CostGuard,
            _ => {}
        }
        if proc.stdout_truncated {
            return CliAgentOutcome::Failed(FailCode::OutputTooLarge);
        }
        if let Some(line) = bad_line {
            return CliAgentOutcome::Failed(FailCode::BadStream { line });
        }
        match proc.exit {
            None => CliAgentOutcome::Timeout(TimeoutKind::OuterKill),
            Some(1) => match extract.assistant.as_ref().map(|a| a.stop_reason.as_str()) {
                Some("aborted") => CliAgentOutcome::Timeout(TimeoutKind::InnerAborted),
                _ => CliAgentOutcome::Failed(FailCode::Exit { code: 1 }),
            },
            Some(0) => {
                if !extract.terminal {
                    return CliAgentOutcome::Timeout(TimeoutKind::NoTerminalEvent);
                }
                let Some(assistant) = extract.assistant else {
                    return CliAgentOutcome::Failed(FailCode::EmptyContent);
                };
                if assistant.text.is_empty() {
                    return CliAgentOutcome::Failed(FailCode::EmptyContent);
                }
                let parsed: Result<Value, _> = serde_json::from_str(&assistant.text);
                match parsed {
                    Err(_) => CliAgentOutcome::Rejected(RejectCode::BadJson),
                    Ok(output) => match validate_classify_v1(&output) {
                        Err(_) => CliAgentOutcome::Rejected(RejectCode::Schema),
                        Ok(()) => CliAgentOutcome::Ok(ClassifyOk {
                            output,
                            model_id: assistant.model_id.clone(),
                            session_ref: extract.session_ref.clone(),
                            tokens_in: assistant.tokens_in,
                            tokens_out: assistant.tokens_out,
                            cost_usd: assistant.cost_usd,
                            latency_ms: assistant.duration_ms,
                        }),
                    },
                }
            }
            Some(code) => CliAgentOutcome::Failed(FailCode::Exit { code }),
        }
    }
}

/// JSONL 抽取结果（只保留结构化字段）。
struct JsonlExtract {
    session_ref: Option<String>,
    assistant: Option<AssistantFinal>,
    terminal: bool,
}

struct AssistantFinal {
    text: String,
    model_id: String,
    provider: Option<String>,
    stop_reason: String,
    tokens_in: Option<u64>,
    tokens_out: Option<u64>,
    cost_usd: Option<f64>,
    duration_ms: Option<f64>,
}

/// 逐行解析 JSONL；非 JSON 行（非空）→ Err(行号)。未知事件类型忽略。
#[cfg(test)]
fn parse_jsonl(stdout: &[u8]) -> Result<JsonlExtract, usize> {
    let (extract, bad_line) = extract_jsonl(stdout);
    bad_line.map_or(Ok(extract), Err)
}

fn extract_jsonl(stdout: &[u8]) -> (JsonlExtract, Option<usize>) {
    let mut extract = JsonlExtract {
        session_ref: None,
        assistant: None,
        terminal: false,
    };
    for (idx, line) in stdout.split(|&b| b == b'\n').enumerate() {
        if line.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            return (extract, Some(idx + 1));
        };
        match value["type"].as_str() {
            Some("session") => {
                if extract.session_ref.is_none() {
                    if let Some(id) = value["id"].as_str() {
                        extract.session_ref = Some(id.to_string());
                    }
                }
            }
            Some("message_end") => {
                let message = &value["message"];
                if message["role"].as_str() == Some("assistant") {
                    extract.assistant = Some(assistant_final(message));
                }
            }
            Some("agent_end") if value["isTerminal"].as_bool() == Some(true) => {
                extract.terminal = true;
            }
            _ => {}
        }
    }
    (extract, None)
}

fn assistant_final(message: &Value) -> AssistantFinal {
    let mut text = String::new();
    if let Some(parts) = message["content"].as_array() {
        for part in parts {
            if part["type"].as_str() == Some("text") {
                if let Some(chunk) = part["text"].as_str() {
                    text.push_str(chunk);
                }
            }
        }
    }
    let usage = &message["usage"];
    AssistantFinal {
        text,
        model_id: message["model"].as_str().unwrap_or_default().to_string(),
        provider: message["provider"].as_str().filter(|p| !p.is_empty()).map(str::to_string),
        stop_reason: message["stopReason"].as_str().unwrap_or_default().to_string(),
        tokens_in: usage["input"].as_u64(),
        tokens_out: usage["output"].as_u64(),
        cost_usd: usage["cost"]["total"].as_f64(),
        duration_ms: message["duration"].as_f64(),
    }
}

/// `omp/19.1.0` → `19`；无数字前缀返回 None。
fn major_of(version: &str) -> Option<u32> {
    version.split('/').nth(1)?.split('.').next()?.parse().ok()
}

fn parse_version(stdout: &[u8]) -> Option<String> {
    let first = std::str::from_utf8(stdout).ok()?.lines().next()?;
    let first = first.trim();
    (first.starts_with("omp/") && major_of(first).is_some()).then(|| first.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_template_isolation_flags_and_inner_time() {
        let mut config = OmpCliConfig::new("/bin/true".into(), "p/m");
        config.timeout = Duration::from_secs(12);
        let runtime = OmpCliRuntime::new(config);
        let argv = runtime.classify_argv("SYS");
        assert_eq!(argv[0], "-p");
        assert_eq!(&argv[1..3], &["--mode", "json"]);
        for flag in ["--no-tools", "--no-session", "--no-extensions", "--no-skills",
                     "--no-rules", "--no-context-files", "--no-lsp", "--no-title"] {
            assert!(argv.contains(&flag.to_string()), "缺隔离旗标 {flag}");
        }
        let max_time = argv.iter().position(|a| a == "--max-time").unwrap();
        assert_eq!(argv[max_time + 1], "7", "内层 = 12s − 5s");
        let model = argv.iter().position(|a| a == "--model").unwrap();
        assert_eq!(argv[model + 1], "p/m");
        let sys = argv.iter().position(|a| a == "--system-prompt").unwrap();
        assert_eq!(argv[sys + 1], "SYS");
    }

    #[test]
    fn inner_time_floor_is_one_second() {
        let mut config = OmpCliConfig::new("/bin/true".into(), "p/m");
        config.timeout = Duration::from_secs(3);
        let runtime = OmpCliRuntime::new(config);
        let argv = runtime.classify_argv("S");
        let i = argv.iter().position(|a| a == "--max-time").unwrap();
        assert_eq!(argv[i + 1], "1");
    }

    #[test]
    fn jsonl_extract_takes_last_assistant_message_end() {
        let jsonl = concat!(
            "{\"type\":\"session\",\"id\":\"sid-1\"}\n",
            "{\"type\":\"message_end\",\"message\":{\"role\":\"user\",\"content\":[]}}\n",
            "{\"type\":\"message_update\",\"assistantMessageEvent\":{\"delta\":\"x\"}}\n",
            "{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"a\"}],\"model\":\"m1\"}}\n",
            "{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"b\"},{\"type\":\"other\"}],\"model\":\"m2\",\"usage\":{\"input\":5,\"output\":2,\"cost\":{\"total\":0.1}},\"stopReason\":\"stop\",\"duration\":88.5}}\n",
            "{\"type\":\"agent_end\",\"isTerminal\":true}\n",
        );
        let extract = parse_jsonl(jsonl.as_bytes()).expect("良构 JSONL");
        assert_eq!(extract.session_ref.as_deref(), Some("sid-1"));
        assert!(extract.terminal);
        let a = extract.assistant.expect("有 assistant");
        assert_eq!(a.text, "b");
        assert_eq!(a.model_id, "m2");
        assert_eq!(a.tokens_in, Some(5));
        assert_eq!(a.cost_usd, Some(0.1));
        assert_eq!(a.duration_ms, Some(88.5));
    }

    #[test]
    fn jsonl_bad_line_reports_line_number_only() {
        let jsonl = "{\"type\":\"agent_start\"}\nnot-json\n";
        assert_eq!(parse_jsonl(jsonl.as_bytes()).err(), Some(2));
    }

    #[test]
    fn archive_call_keeps_reported_tokens_on_failure_and_does_not_adopt_another_model() {
        let runtime = OmpCliRuntime::new(OmpCliConfig::new("unused-cli".into(), "p/vendor/m"));
        let mut proc = subprocess::ProcOutcome {
            exit: Some(1),
            timed_out: false,
            stdout: br#"{"type":"message_end","message":{"role":"assistant","provider":"p","model":"vendor/m","usage":{"input":19},"stopReason":"aborted"}}"#.to_vec(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        };
        let (outcome, usage) = runtime.map_measured(&proc, true);
        assert_eq!(outcome, CliAgentOutcome::Timeout(TimeoutKind::InnerAborted));
        assert_eq!(usage.tokens_in, Some(19));
        assert_eq!(usage.tokens_out, None);
        proc.stdout = br#"{"type":"message_end","message":{"role":"assistant","provider":"q","model":"vendor/m","usage":{"input":19}}}"#.to_vec();
        let (outcome, usage) = runtime.map_measured(&proc, true);
        assert_eq!(outcome, CliAgentOutcome::Unavailable(UnavailCode::ModelIdentityUnverified));
        assert_eq!(usage.reported_model.as_deref(), Some("q/vendor/m"));
    }

    #[test]
    fn version_parsing() {
        assert_eq!(parse_version(b"omp/19.1.0\n"), Some("omp/19.1.0".to_string()));
        assert_eq!(parse_version(b"v1.2\n"), None);
        assert_eq!(major_of("omp/19.1.0"), Some(19));
    }

    #[test]
    fn outcome_status_and_codes() {
        assert_eq!(CliAgentOutcome::Ok(ClassifyOk {
            output: Value::Null,
            model_id: "m".into(),
            session_ref: None,
            tokens_in: None,
            tokens_out: None,
            cost_usd: None,
            latency_ms: None,
        }).status(), "ok");
        assert_eq!(CliAgentOutcome::Rejected(RejectCode::BadJson).error_code().as_deref(), Some("AGENT_BAD_JSON"));
        assert_eq!(CliAgentOutcome::Timeout(TimeoutKind::InnerAborted).status(), "timeout");
        assert!(CliAgentOutcome::Timeout(TimeoutKind::OuterKill).retryable());
        assert!(!CliAgentOutcome::Rejected(RejectCode::Schema).retryable());
        assert_eq!(
            CliAgentOutcome::Failed(FailCode::BadStream { line: 7 }).error_code().as_deref(),
            Some("AGENT_BAD_STREAM_L7")
        );
    }
}
