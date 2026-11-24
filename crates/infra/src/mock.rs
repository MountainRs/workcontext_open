//! 离线 `MockAgentRuntime`（详细设计 §8.1 实现表第一行；遗留事项 B2）。
//!
//! - **无网络**：本 crate 不引入任何网络依赖，判读在进程内完成。
//! - **按 `input_sha256` 精确寻址**：只命中 `fixtures/mock/classify/<sha>.json`
//!   （加载时逐个通过 [`crate::contract`] 的 classify/v1 契约校验）。
//! - **无匹配 → [`MockOutcome::Unknown`]**：明确 unknown，绝不编造「看起来合理」
//!   的答案（防止测试自欺）。`classification` 枚举没有 `unknown` 值——那是判读
//!   *结果*层的状态；本 runtime 在*接口*层用 `Unknown` 变体表达，调用方必须按
//!   「无判读」处理（`agent_run.status='skipped'` 或等价路径）。
//! - trait 化（`AgentRuntime`）与 cli/http 实现留待 B7；mock 同步即时返回，
//!   不引入 async 运行时依赖。接口返回 `serde_json::Value`，避免耦合未稳定的
//!   core DTO（详见 crate 文档）。

use crate::classify_input::{self, ClassifyInput};
use crate::contract::validate_classify_v1;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// runtime 名（写入 `agent_run.runtime`）。
pub const RUNTIME_NAME: &str = "mock";
/// 夹具判读的 model_id（写入 `agent_run.model_id`，参与幂等唯一索引）。
pub const MODEL_ID: &str = "fixture-classify/v1";

/// mock runtime 加载失败。
#[derive(Debug, thiserror::Error)]
pub enum MockAgentError {
    #[error("mock 判读夹具目录不存在：{0}")]
    MissingDir(PathBuf),
    #[error("mock 判读夹具读取失败 {path}：{source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("mock 判读夹具不是合法 JSON {path}：{source}")]
    BadJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("mock 判读夹具文件名必须是 64 位小写十六进制 input_sha256：{0}")]
    BadFileName(PathBuf),
    #[error("mock 判读夹具不满足 classify/v1 契约 {path}：{source}")]
    Contract {
        path: PathBuf,
        #[source]
        source: crate::contract::ContractError,
    },
    #[error("mock 判读夹具目录读取失败 {path}：{source}")]
    ReadDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// 一次 mock 判读的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockOutcome {
    /// 命中夹具：可回放的 `classify/v1` 输出（加载时已通过契约校验）。
    Replay {
        input_sha256: String,
        output: Value,
    },
    /// 无匹配夹具：明确 unknown。**没有**伪造载荷——调用方按「无判读」处理。
    Unknown {
        input_sha256: String,
    },
}

/// 离线判读 runtime：`input_sha256` → 预置 classify/v1 响应。
#[derive(Debug, Clone)]
pub struct MockAgentRuntime {
    fixtures: HashMap<String, Value>,
}

impl MockAgentRuntime {
    /// 从仓库 fixtures 根目录加载（`<fixtures_root>/mock/classify/*.json`）。
    pub fn open(fixtures_root: &Path) -> Result<Self, MockAgentError> {
        Self::load_dir(&fixtures_root.join("mock").join("classify"))
    }

    /// 从判读夹具目录加载：文件名必须就是 `input_sha256`（64 位小写十六进制），
    /// 内容必须在加载时通过 classify/v1 契约校验，坏一个即整体失败（不静默跳过）。
    pub fn load_dir(dir: &Path) -> Result<Self, MockAgentError> {
        if !dir.is_dir() {
            return Err(MockAgentError::MissingDir(dir.to_path_buf()));
        }
        let entries = std::fs::read_dir(dir).map_err(|source| MockAgentError::ReadDir {
            path: dir.to_path_buf(),
            source,
        })?;
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                (path.extension().and_then(|e| e.to_str()) == Some("json")).then_some(path)
            })
            .collect();
        paths.sort(); // 确定性加载顺序 → 确定性错误信息

        let mut fixtures = HashMap::with_capacity(paths.len());
        for path in paths {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| MockAgentError::BadFileName(path.clone()))?;
            if !is_sha256_hex(stem) {
                return Err(MockAgentError::BadFileName(path.clone()));
            }
            let raw = std::fs::read_to_string(&path).map_err(|source| MockAgentError::Io {
                path: path.clone(),
                source,
            })?;
            let output: Value =
                serde_json::from_str(&raw).map_err(|source| MockAgentError::BadJson {
                    path: path.clone(),
                    source,
                })?;
            validate_classify_v1(&output).map_err(|source| MockAgentError::Contract {
                path: path.clone(),
                source,
            })?;
            fixtures.insert(stem.to_string(), output);
        }
        Ok(Self { fixtures })
    }

    /// 夹具数量。
    pub fn len(&self) -> usize {
        self.fixtures.len()
    }

    /// 是否没有任何夹具。
    pub fn is_empty(&self) -> bool {
        self.fixtures.is_empty()
    }

    /// 按 `input_sha256` 精确寻址（O(1)，无模糊匹配）。
    pub fn lookup(&self, input_sha256: &str) -> Option<&Value> {
        self.fixtures.get(input_sha256)
    }

    pub fn contains(&self, input_sha256: &str) -> bool {
        self.fixtures.contains_key(input_sha256)
    }

    /// 判读：规范输入 → 哈希 → 精确查表。命中返回可回放输出，未命中返回
    /// 明确的 [`MockOutcome::Unknown`]。
    pub fn classify(&self, input: &ClassifyInput) -> MockOutcome {
        let input_sha256 = classify_input::input_sha256(input);
        match self.fixtures.get(&input_sha256) {
            Some(output) => MockOutcome::Replay {
                input_sha256,
                output: output.clone(),
            },
            None => MockOutcome::Unknown { input_sha256 },
        }
    }

    pub fn runtime_name(&self) -> &'static str {
        RUNTIME_NAME
    }

    pub fn model_id(&self) -> &'static str {
        MODEL_ID
    }
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid_output() -> Value {
        json!({
            "schema_version": "classify/v1",
            "source_ids": ["R1"],
            "classification": "followup",
            "belongs_to_me": "yes",
            "needs_action": true,
            "suggested_title": "标题",
            "next_action": null,
            "current_turn": "me",
            "followup": {"type": "none", "at": null, "basis": null},
            "evidence": ["R1"],
            "rationale": "理由",
            "confidence": 70,
            "uncertainties": [],
            "related_hint": null
        })
    }

    fn sha256_hex_of(content: &str) -> String {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(content.as_bytes());
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("workcontext-infra-mock-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录失败");
        dir
    }

    #[test]
    fn missing_dir_is_an_error() {
        let missing = std::env::temp_dir().join("workcontext-infra-no-such-dir");
        assert!(matches!(
            MockAgentRuntime::load_dir(&missing),
            Err(MockAgentError::MissingDir(_))
        ));
    }

    #[test]
    fn loads_fixture_addressed_by_sha_and_rejects_bad_ones() {
        let dir = temp_dir("load");
        let output = valid_output();
        let sha = sha256_hex_of(&output.to_string());
        std::fs::write(dir.join(format!("{sha}.json")), output.to_string())
            .expect("写入夹具失败");

        let runtime = MockAgentRuntime::load_dir(&dir).expect("加载失败");
        assert_eq!(runtime.len(), 1);
        assert!(runtime.contains(&sha));
        assert!(runtime.lookup(&sha).is_some());
        assert_eq!(runtime.runtime_name(), "mock");
        assert_eq!(runtime.model_id(), "fixture-classify/v1");

        // 文件名非哈希 → 拒绝加载
        let dir2 = temp_dir("badname");
        std::fs::write(dir2.join("guess.json"), valid_output().to_string())
            .expect("写入夹具失败");
        assert!(matches!(
            MockAgentRuntime::load_dir(&dir2),
            Err(MockAgentError::BadFileName(_))
        ));

        // 坏 JSON → 拒绝加载
        let dir3 = temp_dir("badjson");
        let sha3 = sha256_hex_of("x");
        std::fs::write(dir3.join(format!("{sha3}.json")), "{not json").expect("写入夹具失败");
        assert!(matches!(
            MockAgentRuntime::load_dir(&dir3),
            Err(MockAgentError::BadJson { .. })
        ));

        // 契约违例（未知枚举）→ 拒绝加载，坏夹具不静默跳过
        let dir4 = temp_dir("badcontract");
        let mut bad = valid_output();
        bad["classification"] = json!("random");
        let sha4 = sha256_hex_of("y");
        std::fs::write(dir4.join(format!("{sha4}.json")), bad.to_string())
            .expect("写入夹具失败");
        assert!(matches!(
            MockAgentRuntime::load_dir(&dir4),
            Err(MockAgentError::Contract { .. })
        ));

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
        let _ = std::fs::remove_dir_all(&dir3);
        let _ = std::fs::remove_dir_all(&dir4);
    }

    #[test]
    fn unknown_input_returns_explicit_unknown() {
        let dir = temp_dir("unknown");
        let runtime = MockAgentRuntime::load_dir(&dir).expect("加载失败");
        assert!(runtime.is_empty());

        let input = crate::classify_input::tests_support::sample_input();
        match runtime.classify(&input) {
            MockOutcome::Unknown { input_sha256 } => {
                assert_eq!(input_sha256.len(), 64);
                assert!(!runtime.contains(&input_sha256));
            }
            MockOutcome::Replay { .. } => panic!("空夹具目录不可能命中回放"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
