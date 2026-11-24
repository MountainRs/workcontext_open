//! 适配层：dws / agent / platform / logging / clock（详细设计 §1、§8）。
//!
//! 当前实现（详细设计 §19 步 2 / 遗留事项 B2、B3）：
//! - [`mock`]：离线 `MockAgentRuntime`，按 `input_sha256` 精确寻址判读夹具，
//!   无匹配返回明确 unknown，全程无网络。
//! - [`classify_input`]：判读输入的规范形与 `input_sha256` 生成规则（可复核，
//!   夹具清单见 `fixtures/mock/manifest.json`）。
//! - [`contract`]：`classify/v1` 输出契约校验（Schema 文件在
//!   `fixtures/schema/classify_v1.json`）。
//! - [`fixture_case`]：case/v1 夹具 → 规范判读输入的桥接。
//! - [`sync`]：B3 最小可用离线同步引擎——白名单来源、分页去重、游标事务
//!   （游标只在整轮成功后推进）、401/截断不冒进、连续超时降级；来源为
//!   进程内 mock 或 B7 真实 CLI 适配器（[`dws`]）。
//! - [`dws`]：真实 DWS CLI 适配器（B7/M6）——严格白名单只读命令封闭枚举，
//!   只按 `source.scope` 逐目标拉取（白名单外不请求）、错误信封→
//!   `SourceError` 映射、`im.message-list.v1` 契约核对、整窗游标。
//! - [`omp`]：真实 OMP CLI 判读 runtime（B7/M6）——argv 直传 + stdin 载荷、
//!   JSONL 事件流边界、双层超时、失败/超时/不可用三态、日志无正文。
//! - [`egress`]：出域闸门（默认 `policy=none` 阻断一切正文）与 §7.2 判读
//!   提示词确定性组装、§7.3 脱敏；`EgressPayload` 是唯一可发送形态。
//! - [`subprocess`]：子进程执行底座（env_clear + 最小环境、墙钟超时 kill、
//!   流硬上限、Windows 无窗）；真实 CLI 适配器共用。
//! - [`timefmt`]：UTC 毫秒 → CLI 时间窗 / RFC3339 的确定性纯函数。
//!
//! 依赖方向：core ← store ← **infra** ← app ← desktop（详细设计 §1）。
//! 本 crate 暂不依赖 workcontext-core 的 DTO：core 的判读/策略类型仍在 B2 并行
//! 开发，判读接口以 `serde_json::Value` 交换 classify/v1 载荷，待契约稳定后再
//! 收窄为强类型（见 `Cargo.toml` 注释）。

pub mod classify_input;
pub mod contract;
pub mod dws;
pub mod egress;
pub mod fixture_case;
pub mod mock;
pub mod omp;
pub mod subprocess;
pub mod sync;
pub mod timefmt;
