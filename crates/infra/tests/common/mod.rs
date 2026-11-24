//! 合成 CLI subprocess 验收测试的公共脚手架（dws_cli / omp_cli 共用）。
//!
//! 每个测试把替身二进制 `wctx-fake-cli` 拷进独占临时目录并写入剧本
//! （`<exe>.script.json`），替身按「可执行文件旁」约定读取——适配器
//! `env_clear()` 启动子进程，环境变量不可用。日志（`<exe>.log`，JSONL）
//! 记录每次调用的 argv 与 stdin，供断言白名单与载荷通道。
//! **全程不触真实 `dws`/`omp`，不登录、不读真实消息。**

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use serde_json::Value;

pub struct FakeCli {
    dir: PathBuf,
    exe: PathBuf,
}

/// 拷贝替身二进制并校验完整性。
///
/// `target/` 下的产物可能正被并发的 cargo 构建替换；读到半截文件会让子进程
/// 在加载期失败（exit 126 / 被信号杀死），与本适配器无关。拷贝后比对字节数，
/// 不一致则重试。
fn copy_fake_binary(dst: &Path) {
    let src = Path::new(env!("CARGO_BIN_EXE_wctx-fake-cli"));
    let mut last_err = None;
    for _ in 0..5 {
        let expected = std::fs::metadata(src).map(|m| m.len());
        match (std::fs::copy(src, dst), expected) {
            (Ok(copied), Ok(expected)) if copied == expected && copied > 0 => return,
            (result, _) => {
                last_err = Some(match result {
                    Err(err) => format!("{err}"),
                    Ok(copied) => format!("拷贝不完整：{copied}B"),
                });
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("拷贝替身二进制失败：{}", last_err.unwrap_or_default());
}

impl FakeCli {
    /// 安装替身：拷贝二进制为 `<dir>/<name>` 并写入剧本。
    pub fn install(name: &str, script: &Value) -> Self {
        // 目录名必须进程内唯一：并行测试线程可能取到同一纳秒时间戳，
        // 仅靠时钟会撞名（一方 Drop 时删掉另一方正在用的目录）。
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = format!(
            "wctx-fake-{name}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        let exe = dir.join(name);
        copy_fake_binary(&exe);
        let fake = Self { dir, exe };
        fake.set_script(script);
        fake
    }

    /// 适配器应使用的可执行文件路径。
    pub fn exe(&self) -> PathBuf {
        self.exe.clone()
    }

    /// 覆盖剧本（引擎集成测试在两轮之间切换场景）。
    pub fn set_script(&self, script: &Value) {
        std::fs::write(self.script_path(), script.to_string()).expect("写入剧本");
    }

    /// 清空调用日志（剧本按「同 target 第 n 次调用取第 n 条」FIFO）。
    pub fn clear_log(&self) {
        let _ = std::fs::remove_file(self.log_path());
    }

    /// 每次调用的 argv（按调用顺序）。
    pub fn argvs(&self) -> Vec<Vec<String>> {
        self.log()
            .iter()
            .map(|entry| {
                entry["argv"]
                    .as_array()
                    .expect("argv 数组")
                    .iter()
                    .map(|a| a.as_str().unwrap_or_default().to_string())
                    .collect()
            })
            .collect()
    }

    /// 每次调用收到的 stdin。
    pub fn stdins(&self) -> Vec<String> {
        self.log()
            .iter()
            .map(|entry| entry["stdin"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    fn log(&self) -> Vec<Value> {
        let Ok(raw) = std::fs::read_to_string(self.log_path()) else {
            return Vec::new();
        };
        raw.lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn script_path(&self) -> PathBuf {
        self.sidecar("script.json")
    }

    fn log_path(&self) -> PathBuf {
        self.sidecar("log")
    }

    fn sidecar(&self, suffix: &str) -> PathBuf {
        let mut name = self
            .exe
            .file_name()
            .unwrap_or_default()
            .to_os_string();
        name.push(format!(".{suffix}"));
        self.dir.join(Path::new(&name))
    }
}

impl Drop for FakeCli {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 取 argv 中某旗标的值。
pub fn flag_value<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    argv.iter()
        .position(|a| a == flag)
        .and_then(|i| argv.get(i + 1))
        .map(String::as_str)
}
