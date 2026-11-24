//! 子进程执行底座（详细设计 §12.6；DWS/OMP 调研的公共落点）。
//!
//! 所有真实 CLI 适配器（[`crate::dws`]、[`crate::omp`]）都经由本模块 spawn，
//! 统一保证四条边界：
//!
//! 1. **参数数组直传，永不拼 shell 字符串**（§12.6；Windows `cmd /c` 引号规则
//!    是注入与 bug 的常见来源）。调用方只能传预先校验过的 `String` 参数。
//! 2. **最小环境**：`env_clear()` 后只回填最小 `PATH`、可选 `HOME`、临时目录
//!    与 Windows 系统变量——不继承任何企业环境变量（可能含凭据，§12.6）。
//! 3. **时限与硬上限**：墙钟超时 kill 子进程；stdout/stderr 读到 EOF 但只保留
//!    前 `cap` 字节（超出置 truncated 标记，由上层按 `failed` 处理，调研 §7.4）。
//! 4. **Windows 无窗**：`CREATE_NO_WINDOW`（§12.2，业务 crate 不出现该 cfg，
//!    平台细节收敛在本适配层）。
//!
//! 本模块**不做任何日志**；返回的 stderr 字节只供上层提取结构化字段
//! （如 DWS 错误信封的 `category`），错误信息组装永远不含子进程输出正文。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 默认 stdout/stderr 硬上限：2 MB（调研 §7.4：JSONL 回显放大约 4×，256 KB
/// 余量不足；流式按行解析 + 2 MB 兜底，超限判 failed）。
pub const DEFAULT_STREAM_CAP: usize = 2 * 1024 * 1024;

/// 一次子进程调用。
#[derive(Debug, Clone)]
pub struct Invocation {
    /// 可执行文件路径（绝对路径，避免依赖子进程的 PATH 解析）。
    pub program: PathBuf,
    /// 参数数组（已通过调用方校验；无 shell 参与）。
    pub args: Vec<String>,
    /// 写入 stdin 的字节（写完即关闭；`None` 表示立即关闭 stdin）。
    pub stdin: Option<Vec<u8>>,
    /// 墙钟时限；[`Duration::ZERO`] 表示不限时（仅诊断用途，适配器恒传有限值）。
    pub timeout: Duration,
    /// stdout/stderr 保留上限（字节）。
    pub cap: usize,
    /// 回填给子进程的 `HOME`（OMP 复用用户凭据 / DWS 共享登录态时传真实家目录；
    /// 隔离方案传专用目录）。
    pub home: Option<PathBuf>,
    /// 子进程工作目录（OMP 传临时空目录，防项目内 `AGENTS.md` 被发现）。
    pub cwd: Option<PathBuf>,
    /// 额外回填的环境变量（如 `DWS_CONFIG_DIR`）。
    pub extra_env: Vec<(String, String)>,
}

/// 执行结果。
#[derive(Debug)]
pub enum ProcResult {
    /// 子进程已运行并终结。
    Ran(ProcOutcome),
    /// 无法启动（程序不存在/无执行权限等）。message 只含 io 错误摘要，无输出正文。
    Spawn(String),
}

/// 一次已运行子进程的可观测结果。
#[derive(Debug)]
pub struct ProcOutcome {
    /// 退出码；`None` = 被信号杀死（外部超时 kill 等）。
    pub exit: Option<i32>,
    /// 是否因墙钟超时被本模块 kill。
    pub timed_out: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

/// 执行一次子进程调用（阻塞至终结或超时 kill）。
pub fn run(inv: Invocation) -> ProcResult {
    let mut cmd = Command::new(&inv.program);
    cmd.args(&inv.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .env("PATH", minimal_path(&inv.program))
        .env("TMPDIR", std::env::temp_dir());
    for (key, value) in &inv.extra_env {
        cmd.env(key, value);
    }
    if let Some(home) = &inv.home {
        cmd.env("HOME", home);
    }
    if let Some(cwd) = &inv.cwd {
        cmd.current_dir(cwd);
    }
    inherit_windows_basics(&mut cmd);
    spawn_hidden(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => return ProcResult::Spawn(format!("{err}")),
    };

    // stdin：独立线程写入后关闭（超过管道缓冲区的输入不会死锁）。
    let stdin_handle = child
        .stdin
        .take()
        .map(|mut pipe| {
            let data = inv.stdin.clone().unwrap_or_default();
            std::thread::spawn(move || {
                let _ = pipe.write_all(&data);
                // 关闭即 EOF；drop 时自动关闭。
            })
        });
    // stdout/stderr：独立线程读到 EOF，只保留前 cap 字节（继续排空管道，
    // 子进程永远不会因管道写满而阻塞）。
    let cap = inv.cap.max(1);
    let out_handle = child.stdout.take().map(|pipe| {
        std::thread::spawn(move || read_capped(pipe, cap))
    });
    let err_handle = child.stderr.take().map(|pipe| {
        std::thread::spawn(move || read_capped(pipe, cap))
    });

    // 墙钟等待：轮询 try_wait，超时 kill（print 模式一次性调用，无长连接可泄漏；
    // `dws event` 长连接订阅不在 V0.1 范围，见调研 §5 的 SIGTERM 约束）。
    let deadline = (inv.timeout != Duration::ZERO).then(|| Instant::now() + inv.timeout);
    let mut timed_out = false;
    let exit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit = status.code();
                break;
            }
            Ok(None) => {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    timed_out = true;
                    let _ = child.kill();
                    let _ = child.wait();
                    exit = None;
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            // try_wait 失败（子进程被外部收尸等）视为异常终结：exit=None。
            Err(_) => {
                exit = None;
                break;
            }
        }
    }

    if let Some(handle) = stdin_handle {
        let _ = handle.join();
    }
    let stdout = out_handle.map(|h| h.join().unwrap_or_default()).unwrap_or_default();
    let stderr = err_handle.map(|h| h.join().unwrap_or_default()).unwrap_or_default();

    ProcResult::Ran(ProcOutcome {
        exit,
        timed_out,
        stdout: stdout.0,
        stderr: stderr.0,
        stdout_truncated: stdout.1,
        stderr_truncated: stderr.1,
    })
}

/// 读到 EOF，保留前 `cap` 字节；返回 (数据, 是否超出上限)。
fn read_capped<R: Read>(mut pipe: R, cap: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let room = cap.saturating_sub(kept.len());
                if n > room {
                    truncated = true;
                }
                kept.extend_from_slice(&chunk[..n.min(room)]);
            }
            Err(_) => break,
        }
    }
    (kept, truncated)
}

/// 最小 `PATH`：可执行文件所在目录 + 系统基础目录。子进程按绝对路径启动，
/// PATH 只为其内部可能的辅助调用兜底。
fn minimal_path(program: &Path) -> String {
    let mut entries: Vec<String> = Vec::new();
    if let Some(dir) = program.parent() {
        if !dir.as_os_str().is_empty() {
            entries.push(dir.to_string_lossy().into_owned());
        }
    }
    if cfg!(windows) {
        entries.push(r"C:\Windows\System32".into());
        entries.push(r"C:\Windows".into());
    } else {
        entries.push("/usr/bin".into());
        entries.push("/bin".into());
    }
    entries.join(if cfg!(windows) { ";" } else { ":" })
}

/// Windows 子进程需要的基本环境变量（无它们时常见运行时直接失败）。
#[cfg(windows)]
fn inherit_windows_basics(cmd: &mut Command) {
    for key in ["SystemRoot", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
}

#[cfg(not(windows))]
fn inherit_windows_basics(_cmd: &mut Command) {}

/// §12.2：Windows 上 `CREATE_NO_WINDOW`，避免托盘/后台场景弹出控制台窗。
#[cfg(windows)]
fn spawn_hidden(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn spawn_hidden(_cmd: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn missing_program_is_spawn_error() {
        let result = run(Invocation {
            program: PathBuf::from("/nonexistent/wctx-probe-missing"),
            args: vec![],
            stdin: None,
            timeout: Duration::from_secs(5),
            cap: 1024,
            home: None,
            cwd: None,
            extra_env: vec![],
        });
        assert!(matches!(result, ProcResult::Spawn(_)));
    }
}
