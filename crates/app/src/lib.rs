//! 用例服务层：today / item / ingest / snapshot / diagnostics。
//!
//! M2（B2）当前内容（详细设计 §16.2、§19 步 2）：跨层业务回放——把
//! core 的 [`PolicyValidator`] 与 infra 的 [`MockAgentRuntime`] 接成完整
//! 用例链路：预筛 → 判读 → 校验 → 内存业务状态 → 人工动作（收尾/纠错）。
//!
//! 结果一律由链路本身产生（case `input.json` + `fixtures/mock` 夹具 +
//! core 策略），绝不从 `expected.json` 生成；后者只供集成测试断言。
//!
//! 依赖方向：core ← store ← infra ← app ← desktop（详细设计 §1）。

pub mod context;
pub mod external_access;
pub mod external_archive;
pub mod external_read;
pub mod external_suggestions;
pub mod external_write;
pub mod items;
pub mod judge;
pub mod organizer;
pub mod organizer_archive;
pub mod model;
pub mod ownership_flow;
pub mod prefilter;
pub mod replay;
pub mod skills;
pub mod snapshots;

pub use model::{JudgeState, WorkItem};
pub use prefilter::{PrefilterVerdict, prefilter_record};
pub use replay::{FinalSnapshot, ItemView, RecordView, ReplayError, ReplayOutcome, RunView, StageSnapshot, replay_case};
