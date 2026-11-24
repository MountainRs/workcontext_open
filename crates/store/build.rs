//! 从仓库根 `migrations/` 收集 `NNNN_name.sql`，生成按版本排序的嵌入列表
//! （OUT_DIR/migrations.rs，由 src/lib.rs include!）。
//!
//! 新增迁移文件即自动纳入，无需修改本 crate；文件名不符合约定、
//! 版本重复或有空洞会在编译期报错，而不是等运行时迁移炸掉。

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR 未设置"));
    let migrations_dir = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/store 应位于仓库 crates/ 目录下")
        .join("migrations");
    println!("cargo:rerun-if-changed={}", migrations_dir.display());

    let mut entries: Vec<(u32, String, PathBuf)> = fs::read_dir(&migrations_dir)
        .unwrap_or_else(|e| panic!("无法读取迁移目录 {}: {e}", migrations_dir.display()))
        .filter_map(|entry| {
            let path = entry
                .unwrap_or_else(|e| panic!("读取迁移目录条目失败: {e}"))
                .path();
            if path.extension().and_then(|e| e.to_str()) != Some("sql") {
                return None; // 忽略 .DS_Store 等无关文件
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_else(|| panic!("迁移文件名不是 UTF-8: {}", path.display()))
                .to_string();
            let (num, rest) = name
                .split_once('_')
                .unwrap_or_else(|| panic!("迁移文件名 {name} 缺少 NNNN_ 前缀（应为 NNNN_name.sql）"));
            let version = num
                .parse::<u32>()
                .unwrap_or_else(|_| panic!("迁移文件名 {name} 的版本前缀不是数字（应为 NNNN_name.sql）"));
            let stem = rest
                .strip_suffix(".sql")
                .expect("前面已按 .sql 扩展名过滤");
            if stem.is_empty()
                || !stem
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            {
                panic!("迁移文件名 {name} 的名称段应为小写字母/数字/下划线");
            }
            Some((version, name, path))
        })
        .collect();
    if entries.is_empty() {
        panic!(
            "迁移目录 {} 中没有任何 .sql 文件",
            migrations_dir.display()
        );
    }
    entries.sort_unstable_by_key(|&(version, _, _)| version);
    for (idx, (version, name, _)) in entries.iter().enumerate() {
        let expected = idx as u32 + 1;
        if *version != expected {
            panic!(
                "迁移版本必须从 1 起连续且不重复：期望 v{expected}，实际是 {name}（v{version}）"
            );
        }
    }

    let mut out = String::from(
        "// 由 crates/store/build.rs 生成，请勿手改。\nuse super::Migration;\n\npub(super) static MIGRATIONS: &[Migration] = &[\n",
    );
    for (version, name, path) in entries {
        out.push_str(&format!(
            "    Migration {{ version: {version}, name: {name:?}, sql: include_str!(r\"{}\") }},\n",
            path.display()
        ));
    }
    out.push_str("];\n");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR 未设置"));
    fs::write(out_dir.join("migrations.rs"), out).expect("写入生成的迁移列表失败");
}
