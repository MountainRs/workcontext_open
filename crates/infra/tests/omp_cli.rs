//! OMP CLI runtime × 合成 CLI subprocess 验收（离线；遗留事项 B7 / M6 门槛）。
//!
//! 覆盖：
//! 1. **默认阻断出域**：`egress.policy=none`（出厂）→ `Blocked`，**不 spawn**；
//!    Redacted/Full 无已批准模型同样阻断；
//! 2. **argv/stdin 通道**：隔离旗标组精确、`--max-time` = timeout − margin、
//!    正文只走 stdin 不进 argv；
//! 3. **JSONL 边界**：正常抽取（model/usage/session/latency）、空回复、
//!    无 `agent_end`、非 JSON 行、超硬上限；
//! 4. **失败/超时**：内层 aborted、外层 kill、exit 1/2/3；
//! 5. **日志无正文**：结局与 error_code 不含提示词/模型输出片段；
//! 6. **批准模型绑定**：载荷批准模型 ≠ runtime 模型 → 拒绝且不 spawn；
//! 7. `health()` 版本探测与漂移。
//!
//! 全程使用替身二进制，不触真实 `omp`、不读凭据、不发网络请求。

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use common::{flag_value, FakeCli};
use workcontext_infra::classify_input::{
    ClassifyInput, ConversationRef, InputRecord, PersonRef, SourceRef,
};
use workcontext_infra::egress::{EgressDecision, EgressGate, EgressPolicy};
use workcontext_infra::omp::{
    CliAgentOutcome, FailCode, OmpCliConfig, OmpCliRuntime, RejectCode, RuntimeHealth,
    RuntimeHealthCode, TimeoutKind, UnavailCode,
};

const MODEL: &str = "fake/model";

/// 一条私聊入向消息的判读输入（正文可注入敏感片段）。
fn sample_input(body: &str) -> ClassifyInput {
    ClassifyInput::new(
        SourceRef {
            id: "dws-im-1".into(),
            kind: "dws_im".into(),
        },
        ConversationRef {
            id: "cidX".into(),
            conversation_type: "dm".into(),
            display_name: "张三".into(),
        },
        PersonRef {
            user_id: "me".into(),
            display_name: "李四".into(),
        },
        vec![PersonRef {
            user_id: "p1".into(),
            display_name: "张三".into(),
        }],
        vec![InputRecord {
            id: "ext-1".into(),
            direction: "in".into(),
            sender_id: "p1".into(),
            sender_name: "张三".into(),
            mentions: vec![],
            reply_to: None,
            occurred_at: "2026-11-20T09:12:00Z".into(),
            body: body.to_string(),
        }],
    )
}

const BODY: &str = "请帮我确认图纸复核结果，电话13812345678，谢谢";

fn omp_script(mode: &str) -> Value {
    json!({ "omp": { "mode": mode, "model": MODEL } })
}

fn config(exe: PathBuf) -> OmpCliConfig {
    OmpCliConfig::new(exe, MODEL)
}

/// 出域放行配置（Full + 已批准模型 = 本 runtime 模型）。
fn approved(mut config: OmpCliConfig, policy: EgressPolicy) -> OmpCliConfig {
    config.egress_policy = policy;
    config.approved_model = Some(MODEL.to_string());
    config
}

// ---------------------------------------------------------------------------
// 1. 默认阻断出域
// ---------------------------------------------------------------------------

#[test]
fn default_policy_blocks_without_spawning() {
    let fake = FakeCli::install("omp", &omp_script("ok"));
    let runtime = OmpCliRuntime::new(config(fake.exe()));
    let outcome = runtime.run_judge(&sample_input(BODY));
    assert_eq!(
        outcome,
        CliAgentOutcome::Blocked {
            policy: EgressPolicy::None,
            reason: workcontext_infra::egress::EgressBlockReason::PolicyNone,
        }
    );
    // 正文一步不出工作台：子进程从未启动。
    assert!(fake.argvs().is_empty(), "阻断时不得 spawn omp");
    assert_eq!(outcome.status(), "rejected");
    assert_eq!(outcome.error_code().as_deref(), Some("EGRESS_POLICY_NONE"));
}

#[test]
fn egress_policy_without_approved_model_blocks() {
    let fake = FakeCli::install("omp", &omp_script("ok"));
    for policy in [EgressPolicy::Redacted, EgressPolicy::Full] {
        let mut cfg = config(fake.exe());
        cfg.egress_policy = policy;
        let outcome = OmpCliRuntime::new(cfg).run_judge(&sample_input(BODY));
        assert_eq!(
            outcome,
            CliAgentOutcome::Blocked {
                policy,
                reason: workcontext_infra::egress::EgressBlockReason::NoApprovedModel,
            }
        );
    }
    assert!(fake.argvs().is_empty());
}

// ---------------------------------------------------------------------------
// 2. argv / stdin 通道
// ---------------------------------------------------------------------------

#[test]
fn ok_path_exact_argv_and_stdin_payload() {
    let fake = FakeCli::install("omp", &omp_script("ok"));
    let cfg = approved(config(fake.exe()), EgressPolicy::Full);
    let runtime = OmpCliRuntime::new(cfg);
    let outcome = runtime.run_judge(&sample_input(BODY));

    let CliAgentOutcome::Ok(ok) = &outcome else {
        panic!("应成功：{outcome:?}");
    };
    assert_eq!(ok.output["classification"], "followup");
    assert_eq!(ok.model_id, MODEL);
    assert_eq!(ok.session_ref.as_deref(), Some("f9c0ffee-0000-4000-8000-000000000001"));
    assert_eq!(ok.tokens_in, Some(42));
    assert_eq!(ok.tokens_out, Some(7));
    assert_eq!(ok.cost_usd, Some(0.0002));
    assert_eq!(ok.latency_ms, Some(1234.5));
    assert_eq!(outcome.status(), "ok");
    assert!(outcome.error_code().is_none());

    let argvs = fake.argvs();
    assert_eq!(argvs.len(), 1, "恰一次调用");
    let argv = &argvs[0];
    // 调研 §7.1 模板：隔离旗标组 + 内层 max-time = 60s − 5s。
    assert_eq!(
        argv[..17],
        [
            "-p", "--mode", "json", "--no-tools", "--no-session", "--no-extensions",
            "--no-skills", "--no-rules", "--no-context-files", "--no-lsp", "--no-title",
            "--thinking", "off", "--max-time", "55", "--model", MODEL,
        ]
    );
    assert_eq!(argv[17], "--system-prompt");
    assert_eq!(argv.len(), 19, "system 提示是最后一个参数，用户内容不进 argv");
    assert!(!argv.iter().any(|a| a.contains("图纸复核")), "正文不得出现在 argv");

    // 正文经 stdin 完整送达，且只送一次。
    let stdins = fake.stdins();
    assert_eq!(stdins.len(), 1);
    assert!(stdins[0].contains("[DATA]"));
    assert!(stdins[0].contains(BODY));
    assert!(stdins[0].contains("<msg id=\"s1\""));
    assert!(!stdins[0].contains("ext-1"), "模型不得看到真实 ID");
}

#[test]
fn redacted_policy_masks_body_before_leaving() {
    let fake = FakeCli::install("omp", &omp_script("ok"));
    let runtime = OmpCliRuntime::new(approved(config(fake.exe()), EgressPolicy::Redacted));
    runtime.run_judge(&sample_input(BODY));
    let sent = &fake.stdins()[0];
    assert!(sent.contains("1**********"), "手机号须在出域前打码：{sent}");
    assert!(!sent.contains("13812345678"));
    assert!(sent.contains("图纸复核"), "非敏感正文保留");
}

// ---------------------------------------------------------------------------
// 3. JSONL 边界
// ---------------------------------------------------------------------------

#[test]
fn empty_assistant_content_is_failed_not_ok() {
    // abort 模式 content=[] + stopReason=aborted + exit 1 → 内层超时。
    let fake = FakeCli::install("omp", &omp_script("abort"));
    let outcome = OmpCliRuntime::new(approved(config(fake.exe()), EgressPolicy::Full))
        .run_judge(&sample_input(BODY));
    assert_eq!(outcome, CliAgentOutcome::Timeout(TimeoutKind::InnerAborted));
    assert_eq!(outcome.status(), "timeout");
    assert!(outcome.retryable());
    assert_eq!(outcome.error_code().as_deref(), Some("AGENT_TIMEOUT_INNER"));
}

#[test]
fn missing_agent_end_is_timeout() {
    let fake = FakeCli::install("omp", &omp_script("noterminal"));
    let outcome = OmpCliRuntime::new(approved(config(fake.exe()), EgressPolicy::Full))
        .run_judge(&sample_input(BODY));
    assert_eq!(outcome, CliAgentOutcome::Timeout(TimeoutKind::NoTerminalEvent));
}

#[test]
fn oversized_stdout_is_failed_without_holding_payload() {
    let fake = FakeCli::install("omp", &omp_script("huge"));
    let mut cfg = approved(config(fake.exe()), EgressPolicy::Full);
    cfg.stream_cap = 64 * 1024;
    let outcome = OmpCliRuntime::new(cfg).run_judge(&sample_input(BODY));
    assert_eq!(outcome, CliAgentOutcome::Failed(FailCode::OutputTooLarge));
    assert_eq!(outcome.error_code().as_deref(), Some("AGENT_OUTPUT_TOO_LARGE"));
}

// ---------------------------------------------------------------------------
// 4. 失败 / 超时
// ---------------------------------------------------------------------------

#[test]
fn outer_wall_clock_kills_hung_process() {
    let fake = FakeCli::install("omp", &omp_script("hang"));
    let mut cfg = approved(config(fake.exe()), EgressPolicy::Full);
    cfg.timeout = Duration::from_millis(1500);
    let started = Instant::now();
    let outcome = OmpCliRuntime::new(cfg).run_judge(&sample_input(BODY));
    assert!(started.elapsed() < Duration::from_secs(10), "必须被外层 kill");
    assert_eq!(outcome, CliAgentOutcome::Timeout(TimeoutKind::OuterKill));
    assert_eq!(outcome.error_code().as_deref(), Some("AGENT_TIMEOUT_OUTER"));
}

#[test]
fn exit_codes_map_to_distinct_outcomes() {
    let cases: [(&str, CliAgentOutcome); 3] = [
        ("exit1", CliAgentOutcome::Failed(FailCode::Exit { code: 1 })),
        ("exit2", CliAgentOutcome::Unavailable(UnavailCode::ConfigExit)),
        ("exit3", CliAgentOutcome::CostGuard),
    ];
    for (mode, expected) in cases {
        let fake = FakeCli::install("omp", &omp_script(mode));
        let outcome = OmpCliRuntime::new(approved(config(fake.exe()), EgressPolicy::Full))
            .run_judge(&sample_input(BODY));
        assert_eq!(outcome, expected, "mode={mode}");
        assert_eq!(outcome.status(), "failed");
    }
}

#[test]
fn bad_outputs_are_rejected_without_retry() {
    let fake = FakeCli::install("omp", &omp_script("badjson"));
    let outcome = OmpCliRuntime::new(approved(config(fake.exe()), EgressPolicy::Full))
        .run_judge(&sample_input(BODY));
    assert_eq!(outcome, CliAgentOutcome::Rejected(RejectCode::BadJson));
    assert_eq!(outcome.status(), "rejected");
    assert!(!outcome.retryable());
    assert_eq!(outcome.error_code().as_deref(), Some("AGENT_BAD_JSON"));

    let fake = FakeCli::install("omp", &omp_script("schemabad"));
    let outcome = OmpCliRuntime::new(approved(config(fake.exe()), EgressPolicy::Full))
        .run_judge(&sample_input(BODY));
    assert_eq!(outcome, CliAgentOutcome::Rejected(RejectCode::Schema));
    assert_eq!(outcome.error_code().as_deref(), Some("AGENT_SCHEMA_V1"));
}

// ---------------------------------------------------------------------------
// 5. 日志无正文 / 6. 批准模型绑定
// ---------------------------------------------------------------------------

#[test]
fn outcomes_never_carry_body_or_model_text() {
    // 坏输出（含模型文本）与失败路径：Debug 文本不得含提示词正文或模型输出片段。
    for mode in ["badjson", "schemabad", "exit1", "exit2", "exit3", "noterminal", "huge"] {
        let mut script = omp_script(mode);
        script["omp"]["reply"] = json!("模型原文片段SECRET_MODEL_TEXT");
        let fake = FakeCli::install("omp", &script);
        let mut cfg = approved(config(fake.exe()), EgressPolicy::Full);
        cfg.stream_cap = 64 * 1024;
        let outcome = OmpCliRuntime::new(cfg).run_judge(&sample_input("提示词正文SECRET_PROMPT"));
        let rendered = format!("{outcome:?}{}{:?}", outcome.status(), outcome.error_code());
        assert!(!rendered.contains("SECRET_MODEL_TEXT"), "mode={mode}：{rendered}");
        assert!(!rendered.contains("SECRET_PROMPT"), "mode={mode}：{rendered}");
    }
}

#[test]
fn payload_for_other_model_is_refused_without_spawn() {
    let fake = FakeCli::install("omp", &omp_script("ok"));
    // 闸门批准的是另一个模型；本 runtime 模型不同 → 拒绝，且不 spawn。
    let gate = EgressGate::new(EgressPolicy::Full, Some("other/model".into()));
    let EgressDecision::Allowed(payload) = gate.check(&sample_input(BODY)) else {
        panic!("闸门应放行");
    };
    let outcome = OmpCliRuntime::new(config(fake.exe())).run_classify(&payload);
    assert_eq!(outcome, CliAgentOutcome::Unavailable(UnavailCode::ModelNotApproved));
    assert!(fake.argvs().is_empty());
}

#[test]
fn spawn_failure_is_unavailable() {
    let mut cfg = OmpCliConfig::new(PathBuf::from("/nonexistent/wctx-omp"), MODEL);
    cfg.egress_policy = EgressPolicy::Full;
    cfg.approved_model = Some(MODEL.into());
    let outcome = OmpCliRuntime::new(cfg).run_judge(&sample_input(BODY));
    assert_eq!(outcome, CliAgentOutcome::Unavailable(UnavailCode::Spawn));
    assert_eq!(outcome.error_code().as_deref(), Some("AGENT_UNREACHABLE"));
}

// ---------------------------------------------------------------------------
// 7. health
// ---------------------------------------------------------------------------

#[test]
fn health_parses_version_and_detects_drift() {
    let fake = FakeCli::install("omp", &omp_script("ok"));
    let runtime = OmpCliRuntime::new(config(fake.exe()));
    assert_eq!(
        runtime.health(),
        RuntimeHealth::Ok {
            version: "omp/19.1.0".into()
        }
    );

    let mut cfg = config(fake.exe());
    cfg.expected_version_major = Some(18);
    assert_eq!(
        OmpCliRuntime::new(cfg).health(),
        RuntimeHealth::Unavailable {
            code: RuntimeHealthCode::VersionMismatch {
                found: 19,
                expected_major: 18,
            }
        }
    );
    // health 只跑 --version，不触发模型调用。
    assert!(fake.argvs().iter().all(|a| a[0] == "--version"));
}

#[test]
fn health_reports_missing_binary() {
    let runtime = OmpCliRuntime::new(OmpCliConfig::new(
        PathBuf::from("/nonexistent/wctx-omp"),
        MODEL,
    ));
    assert_eq!(
        runtime.health(),
        RuntimeHealth::Unavailable {
            code: RuntimeHealthCode::Spawn,
        }
    );
}

#[test]
fn max_time_argument_tracks_timeout_margin() {
    let fake = FakeCli::install("omp", &omp_script("ok"));
    let mut cfg = approved(config(fake.exe()), EgressPolicy::Full);
    cfg.timeout = Duration::from_secs(12);
    cfg.inner_margin = Duration::from_secs(2);
    OmpCliRuntime::new(cfg).run_judge(&sample_input(BODY));
    assert_eq!(flag_value(&fake.argvs()[0], "--max-time"), Some("10"));
}
