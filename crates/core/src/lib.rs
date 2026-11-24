//! 领域核心：实体、不变量、状态机、规则校验与 DTO（详细设计 §1、§3、§5、§8）。
//!
//! 零 IO：不依赖 tokio / rusqlite，可纯内存测试（§16.1）。依赖方向：
//! core ← store ← infra ← app ← desktop；core 是最底层。
//!
//! - [`classify`]：classify/v1 类型化输入输出与解析（§8.2；JSON Schema 文件由
//!   infra 维护在 `fixtures/schema/classify_v1.json`，以本 crate 类型为准对齐）
//! - [`state`]：生命周期/注意力状态机、转换守卫、收尾证据白名单与人工锁（§3）
//! - [`validate`]：`PolicyValidator` V01–V12 判读校验流水线与 §3.4 映射（§5）
//! - [`correction`]：人工纠错语义与字段锁推导（§9）
//!
//! M2（B2）落地内容；M1 的共享常量保留不动。

/// 应用标识，与 `tauri.conf.json` 的 `identifier` 保持一致。
pub const APP_IDENTIFIER: &str = "com.local.workcontext";

/// 数据库文件名（位于各平台 `app_data_dir` 下）。
pub const DB_FILENAME: &str = "workcontext.sqlite3";

pub mod classify;
pub mod correction;
pub mod state;
pub mod validate;

pub use classify::{
    parse_classify_output, AgentFollowupType, BelongsToMe, Classification, ClassifyOutput,
    ConversationType, Direction, ExistingItem, Followup, JudgeContext, MeContext, ParseError,
    RecordContext, RecordKind, RelatedHint, Resolution, SCHEMA_VERSION,
};
pub use correction::{correction_locks, validate_correction, Correction, CorrectionError};
pub use state::{
    close_verdict, current_turn, evaluate_transition, is_close_evidence_whitelisted, is_locked,
    locked_bool, today_group, Actor, Attention, CloseVerdict, CurrentTurn, DbFollowupType,
    Denial, IgnoreReason, ItemField, Lifecycle, LockedField, ResolutionEvidence,
    ResolutionEvidenceType, TodayGroup, TransitionOutcome, TransitionRequest,
};
pub use validate::{
    dedupe_key, ownership_anchor, validate_classify, AcceptedResolution, AnchorKind, AnchorReport,
    ErrorCode, IntentClass, ItemDecision, JudgeDecision, PolicyValidator, Rejection, RejectionRule,
    SimilarCandidate, ValidatedJudgement, Warning, WarningCode, WriteMode,
};
