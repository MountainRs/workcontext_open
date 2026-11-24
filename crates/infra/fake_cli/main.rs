//! 合成 CLI 替身（wctx-fake-cli）：DWS / OMP 适配器的 subprocess 验收测试用。
//!
//! - **不触真实 `dws`/`omp`**：全部行为由剧本 JSON 驱动；无剧本的命令以
//!   结构化错误信封退出（fail loud，不装成功）。
//! - 角色按首参数自动判别：`version|auth|chat` → dws；其余（`-p`/`--version`）
//!   → omp。
//! - 每次调用把 `{argv, stdin}` 追加写入日志（JSONL），供测试断言 argv
//!   白名单/隔离旗标/stdin 载荷。
//! - 输出形态严格模仿调研实测：dws 失败写 stderr 的
//!   `{"error":{category,…}}`；omp `--mode json` 输出 JSONL 事件流。

use std::io::{Read, Write};
use std::path::PathBuf;

use serde_json::{json, Value};

/// 适配器以 `env_clear()` 启动子进程，剧本/日志路径不能靠环境变量传递：
/// 约定落在可执行文件旁（`<exe>.script.json` / `<exe>.log`），测试把替身
/// 拷进各自的临时目录即得隔离；显式环境变量仅用于手工调试。
fn sidecar(suffix: &str, env_key: &str) -> PathBuf {
    if let Some(path) = std::env::var_os(env_key) {
        return PathBuf::from(path);
    }
    let mut path = std::env::current_exe().expect("替身自身路径");
    path.set_extension("");
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{suffix}"));
    path.set_file_name(name);
    path
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // 子进程启动即读干 stdin（print 模式的 prompt 经此进入；dws 调用无 stdin）。
    let mut stdin = String::new();
    let _ = std::io::stdin().read_to_string(&mut stdin);
    let script: Value = {
        let raw = std::fs::read_to_string(sidecar("script.json", "WCTX_FAKE_SCRIPT"))
            .expect("读取剧本文件");
        serde_json::from_str(&raw).expect("剧本必须是合法 JSON")
    };
    log_invocation(&args, &stdin);
    match args.first().map(String::as_str) {
        Some("version") | Some("auth") | Some("chat") => run_dws(&args, &script),
        _ => run_omp(&args, &script, &stdin),
    }
}

fn log_invocation(args: &[String], stdin: &str) {
    let entry = json!({ "argv": args, "stdin": stdin });
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(sidecar("log", "WCTX_FAKE_LOG"))
    {
        let _ = writeln!(file, "{entry}");
    }
}

fn read_log() -> Vec<Value> {
    let Ok(raw) = std::fs::read_to_string(sidecar("log", "WCTX_FAKE_LOG")) else {
        return Vec::new();
    };
    raw.lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
}

// ---------------------------------------------------------------------------
// dws 替身
// ---------------------------------------------------------------------------

fn run_dws(args: &[String], script: &Value) -> ! {
    let step = match args.first().map(String::as_str) {
        Some("version") => script["dws"]["version"].clone(),
        Some("auth") => script["dws"]["auth_status"].clone(),
        Some("chat") => {
            // chat +chat-messages --group <v> | --open-dingtalk-id <v> …
            let target = args
                .windows(2)
                .find(|w| w[0] == "--group" || w[0] == "--open-dingtalk-id")
                .map(|w| w[1].clone())
                .unwrap_or_else(|| "<no-target>".to_string());
            let queue = script["dws"]["chat"].get(&target).and_then(Value::as_array);
            // FIFO：同 target 的第 n 次调用取剧本第 n 条（已调用次数从日志推得）。
            let prior = read_log()
                .iter()
                .filter(|entry| {
                    entry["argv"]
                        .as_array()
                        .is_some_and(|argv| argv.contains(&json!(target)))
                })
                .count()
                .saturating_sub(1);
            match queue {
                Some(list) if prior < list.len() => list[prior].clone(),
                _ => error_exit(
                    3,
                    "validation",
                    &format!("fake scenario exhausted for target {target}"),
                ),
            }
        }
        _ => error_exit(3, "validation", "fake dws: unknown subcommand"),
    };
    if step.is_null() {
        error_exit(3, "validation", "fake dws: no scenario for subcommand");
    }
    // 两种条目形态：`{"stdout":…,"stderr":…,"exit":…,"sleep_ms":…}`，或直接给出
    // 成功信封本身（exit 0）。
    let framed = ["stdout", "stderr", "exit", "sleep_ms"]
        .iter()
        .any(|key| step.get(key).is_some());
    let (stdout, stderr, exit) = if framed {
        if let Some(ms) = step["sleep_ms"].as_u64() {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        (
            render(&step["stdout"]),
            render(&step["stderr"]),
            step["exit"].as_i64().unwrap_or(0) as i32,
        )
    } else {
        (render(&step), String::new(), 0)
    };
    // 退出前显式 flush：`process::exit` 不保证冲洗无换行的缓冲。
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(stdout.as_bytes());
    let _ = out.flush();
    let _ = std::io::stderr().write_all(stderr.as_bytes());
    let _ = std::io::stderr().flush();
    std::process::exit(exit);
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => format!("{other}"),
    }
}

fn error_exit(code: i32, category: &str, message: &str) -> ! {
    let envelope = json!({
        "error": {"code": code, "category": category, "message": message, "retryable": false}
    });
    eprint!("{envelope}");
    std::process::exit(code);
}

// ---------------------------------------------------------------------------
// omp 替身
// ---------------------------------------------------------------------------

const DEFAULT_REPLY: &str = r#"{"schema_version":"classify/v1","source_ids":["s1"],"classification":"followup","belongs_to_me":"yes","needs_action":true,"suggested_title":"确认复测结果","next_action":null,"current_turn":"other","followup":{"type":"none","at":null,"basis":null},"evidence":["s1"],"rationale":"对方明确请本人协调复测","confidence":78,"uncertainties":[],"related_hint":null}"#;

fn run_omp(args: &[String], script: &Value, stdin: &str) -> ! {
    if args.first().map(String::as_str) == Some("--version") {
        let version = script["omp"]["version"].as_str().unwrap_or("omp/19.1.0");
        println!("{version}");
        std::process::exit(0);
    }
    let mode = script["omp"]["mode"].as_str().unwrap_or("ok");
    let model = script["omp"]["model"].as_str().unwrap_or("fake/model");
    let reply = script["omp"]["reply"].as_str().map(String::from);
    let emit = |value: Value| println!("{value}");

    match mode {
        "exit1" => {
            eprintln!("Model \"x/y\" not found");
            std::process::exit(1);
        }
        "exit2" => {
            eprintln!("Error: unknown flag: --json");
            std::process::exit(2);
        }
        "exit3" => {
            eprintln!("cost limit exceeded");
            std::process::exit(3);
        }
        "hang" => {
            std::thread::sleep(std::time::Duration::from_secs(30));
            std::process::exit(0);
        }
        "huge" => {
            let pad = "x".repeat(1_200_000);
            for seq in 0..3 {
                emit(json!({"type": "noise", "seq": seq, "pad": pad}));
            }
            std::process::exit(0);
        }
        _ => {}
    }

    let (text, stop_reason, exit) = match mode {
        "abort" => (String::new(), "aborted", 1),
        "badjson" => (reply.unwrap_or_else(|| "这不是JSON{{{".into()), "stop", 0),
        "schemabad" => (
            reply.unwrap_or_else(|| r#"{"schema_version":"classify/v1"}"#.into()),
            "stop",
            0,
        ),
        "mirror" => (stdin.to_string(), "stop", 0),
        _ => (reply.unwrap_or_else(|| DEFAULT_REPLY.to_string()), "stop", 0),
    };

    emit(json!({"type": "session", "version": 3,
        "id": "f9c0ffee-0000-4000-8000-000000000001", "cwd": "/tmp"}));
    emit(json!({"type": "agent_start"}));
    emit(json!({"type": "turn_start"}));
    // user 提示回显（真实 omp 的放大效应；适配器只取 assistant message_end）。
    let echo: String = stdin.chars().take(120).collect();
    emit(json!({"type": "message_end", "message": {"role": "user",
        "content": [{"type": "text", "text": echo}]}}));
    let content = if text.is_empty() {
        json!([])
    } else {
        json!([{"type": "text", "text": text}])
    };
    let usage = if text.is_empty() {
        json!({"input": 0, "output": 0})
    } else {
        json!({"input": 42, "output": 7, "cost": {"total": 0.0002}})
    };
    emit(json!({"type": "message_end", "message": {"role": "assistant",
        "content": content, "model": model, "usage": usage,
        "stopReason": stop_reason, "duration": 1234.5, "ttft": 999.1}}));
    emit(json!({"type": "turn_end", "message": {}, "toolResults": []}));
    if mode != "noterminal" {
        emit(json!({"type": "agent_end", "messages": [], "isTerminal": true, "yielded": true}));
    }
    std::process::exit(exit);
}
