//! DWS CLI 适配器 × 合成 CLI subprocess 验收（离线；遗留事项 B7 / M6 门槛）。
//!
//! 覆盖：
//! 1. **白名单外不读**：fetch 只对 `scope` 内目标发 `chat +chat-messages`，
//!    argv 全部落在封闭命令面；空白名单零调用；
//! 2. **只读 + 参数安全**：profile/时间窗/游标 token 校验，无 shell 拼接；
//! 3. **信封边界**：契约版本、完整性台账、续页/截断不冒进、字段缺失；
//! 4. **错误信封映射**：auth → `Auth`、retryable → `RateLimited`、超时、
//!    启动失败；错误信息不含子进程输出正文；
//! 5. **游标语义**：整窗游标 + overlap 重开、轮内续页窗口原样重复；
//! 6. **可接入**：与 `SyncEngine` 端到端（落库、幂等、auth_required 不冒进）。
//!
//! 全程使用替身二进制，不触真实 `dws`、不登录、不读真实消息。

mod common;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use common::{flag_value, FakeCli};
use workcontext_infra::dws::{
    auth_status, cli_version, ApprovedWindow, CliDingTalkSource, DwsAuthStatus, DwsCliConfig,
    DwsReadCommand, READ_COMMANDS,
};
use workcontext_infra::sync::{
    CheckpointState, ConversationType, Direction, DingTalkSource, PageParams, Scope, SourceError,
    SyncConfig, SyncEngine, SyncStatus,
};

const NOW: i64 = 1_795_305_600_000; // 2026-11-22T00:00:00Z（整秒）
const PROFILE: &str = "corp1:u1";
const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;

/// 多数用例的批准时间窗：覆盖首轮 24h 回溯、6h overlap 与所有测试时刻，
/// 不对请求构成实际约束。
fn wide_approval() -> ApprovedWindow {
    ApprovedWindow {
        start_ms: NOW - 7 * DAY,
        end_ms: None,
    }
}

/// 适配器配置：短超时 + 给定批准时间窗。
fn config_for(fake: &FakeCli, approved: ApprovedWindow) -> DwsCliConfig {
    let mut config = DwsCliConfig::new(fake.exe(), PROFILE);
    config.call_timeout = Duration::from_secs(10);
    config.approved_window = Some(approved);
    config
}

fn scope() -> Scope {
    Scope {
        conversations: BTreeSet::from(["cidA".to_string()]),
        people: BTreeSet::from(["p1".to_string()]),
    }
}

/// 只有群 `cidA` 的白名单（剧本只定义该目标时使用）。
fn group_scope() -> Scope {
    Scope {
        conversations: BTreeSet::from(["cidA".to_string()]),
        people: BTreeSet::new(),
    }
}

fn page(messages: Vec<Value>) -> Value {
    json!({
        "contractVersion": "im.message-list.v1",
        "messages": messages,
        "complete": true,
        "hasMore": false,
        "failures": [],
        "partial": false,
        "truncated": false
    })
}

fn more_page(messages: Vec<Value>, next: &str) -> Value {
    let mut value = page(messages);
    value["complete"] = json!(false);
    value["hasMore"] = json!(true);
    value["nextCursor"] = json!(next);
    value
}

fn msg(id: &str, conv: &str, sender: &str, at: i64, text: &str) -> Value {
    json!({"messageId": id, "conversationId": conv, "senderId": sender,
           "sender": "显示名", "messageType": "text", "text": text, "createTime": at})
}

fn source(fake: &FakeCli, scope: Scope) -> CliDingTalkSource {
    CliDingTalkSource::new("dws-im-1", scope, config_for(fake, wide_approval())).with_clock(|| NOW)
}

fn page_params() -> PageParams {
    PageParams {
        size: 100,
        overlap_ms: 6 * 3600 * 1000,
    }
}

fn script(chat: Value) -> Value {
    json!({
        "dws": {
            "version": {"stdout": {"version": "v1.0.62", "edition": "open"}},
            "auth_status": {"stdout": {"authenticated": true, "token_valid": true,
                                       "refresh_token_valid": true}},
            "chat": chat
        }
    })
}

/// 只看 `chat +chat-messages` 调用。
fn chat_calls(fake: &FakeCli) -> Vec<Vec<String>> {
    fake.argvs()
        .into_iter()
        .filter(|argv| argv.len() >= 2 && argv[0] == "chat")
        .collect()
}

// ---------------------------------------------------------------------------
// 1. 白名单外不读
// ---------------------------------------------------------------------------

#[test]
fn fetch_requests_only_whitelisted_targets() {
    let fake = FakeCli::install(
        "dws",
        &script(json!({
            "cidA": [page(vec![msg("m1", "cidA", "x9", NOW - 1000, "群消息")])],
            "p1":   [page(vec![msg("m3", "dm-c", "p1", NOW - 500, "私聊")])]
        })),
    );
    let src = source(&fake, scope());
    let result = src
        .fetch(&scope(), None, &page_params())
        .expect("拉取成功");

    // 恰两次调用：一次群、一次人；没有任何白名单外目标。
    let calls = chat_calls(&fake);
    assert_eq!(calls.len(), 2);
    for argv in &calls {
        assert_eq!(argv[1], "+chat-messages");
        let target = flag_value(argv, "--group").or_else(|| flag_value(argv, "--open-dingtalk-id"));
        assert!(
            matches!(target, Some("cidA") | Some("p1")),
            "白名单外目标被请求：{argv:?}"
        );
        assert_eq!(flag_value(argv, "--profile"), Some(PROFILE));
        // 只读约束：无任何写操作旗标。
        assert!(!argv.iter().any(|a| a.contains("send") || a.contains("recall") || a.contains("update")));
    }
    assert_eq!(flag_value(&calls[0], "--group"), Some("cidA"));
    assert_eq!(flag_value(&calls[1], "--open-dingtalk-id"), Some("p1"));

    assert_eq!(result.records.len(), 2);
    assert!(!result.has_more);
}

#[test]
fn argv_stays_inside_closed_read_only_command_set() {
    let fake = FakeCli::install(
        "dws",
        &script(json!({"cidA": [page(vec![])]})),
    );
    let src = source(&fake, group_scope());
    src.probe().expect("probe");
    src.fetch(&src.scope().clone(), None, &page_params()).expect("fetch");
    for argv in fake.argvs() {
        let name = match argv[0].as_str() {
            "version" => "version",
            "auth" => "auth status",
            "chat" => "chat +chat-messages",
            other => panic!("白名单外命令被调用：{other}"),
        };
        assert!(READ_COMMANDS.contains(&name));
        // 参数数组直传：没有任何参数是带引号的 shell 片段。
        assert!(argv.iter().all(|a| !a.contains('"') && !a.contains(';')));
    }
    // 探测只读且固定形态：本机 dws v1.0.62 实测不支持 --readonly（unknown_flag），
    // 因此不带该旗标，也不因字段缺失扩大为其它命令。
    assert_eq!(
        fake.argvs()
            .into_iter()
            .find(|a| a[0] == "auth")
            .expect("auth status 调用"),
        vec!["auth", "status", "--profile", PROFILE, "--format", "json"]
    );
}

#[test]
fn empty_scope_makes_no_subprocess_calls() {
    let fake = FakeCli::install("dws", &script(json!({})));
    let src = source(&fake, Scope::default());
    let result = src.fetch(&Scope::default(), None, &page_params()).expect("fail closed");
    assert!(result.records.is_empty());
    assert!(!result.has_more);
    assert!(result.next_cursor.is_none());
    assert!(fake.argvs().is_empty(), "空白名单不得触 CLI");
}

#[test]
fn out_of_scope_records_inside_envelope_are_dropped() {
    // 纵深防御：即便信封混入白名单外会话，也不返回给引擎。
    let fake = FakeCli::install(
        "dws",
        &script(json!({"cidA": [page(vec![
            msg("m1", "cidA", "x9", NOW - 1000, "在范围内"),
            msg("m2", "other-conv", "stranger", NOW - 900, "白名单外会话"),
        ])]})),
    );
    let src = source(&fake, group_scope());
    let result = src.fetch(&src.scope().clone(), None, &page_params()).expect("拉取");
    assert_eq!(result.records.len(), 1);
    assert_eq!(result.records[0].external_id, "m1");
}

// ---------------------------------------------------------------------------
// 2. 记录映射（含判读元数据）
// ---------------------------------------------------------------------------

#[test]
fn records_carry_direction_reply_and_conversation_type() {
    let mut quoted = msg("m1", "cidA", "x9", NOW - 1000, "引用我");
    quoted["createTime"] = json!(workcontext_infra::timefmt::format_datetime(NOW - 1000, 8 * HOUR));
    quoted["quotedMessage"] = json!({"messageId": "m0"});
    let fake = FakeCli::install(
        "dws",
        &script(json!({
            "cidA": [page(vec![quoted, msg("m2", "cidA", "u1", NOW - 900, "我的回复")])],
            "p1":   [page(vec![msg("m3", "dm-c", "p1", NOW - 500, "私聊"),
                               msg("m4", "dm-c", "u1", NOW - 400, "我的私聊回复")])]
        })),
    );
    let mut cfg = config_for(&fake, wide_approval());
    cfg.tz_offset_ms = 8 * HOUR;
    let src = CliDingTalkSource::new("dws-im-1", scope(), cfg).with_clock(|| NOW);
    let result = src.fetch(&scope(), None, &page_params()).expect("拉取");
    let by_id = |id: &str| result.records.iter().find(|r| r.external_id == id).expect(id);

    let m1 = by_id("m1");
    assert_eq!(m1.direction, Direction::In);
    assert_eq!(m1.reply_to_id.as_deref(), Some("m0"));
    assert_eq!(m1.conversation_type, Some(ConversationType::Group));
    assert_eq!(m1.conversation_id.as_deref(), Some("cidA"));
    assert_eq!(m1.sender_name.as_deref(), Some("显示名"));
    assert_eq!(m1.body.as_deref(), Some("引用我"));
    assert_eq!(m1.occurred_at, NOW - 1000);

    let m2 = by_id("m2");
    assert_eq!(m2.direction, Direction::Out, "senderId==me → out");
    assert_eq!(m2.reply_to_id, None);

    // 私聊目标：对端与我本人都在授权拉取范围内；会话类型为 dm。
    let m3 = by_id("m3");
    let m4 = by_id("m4");
    assert_eq!(m3.conversation_type, Some(ConversationType::Dm));
    assert_eq!(m4.direction, Direction::Out);
    // 读取契约无 @ 列表字段：mentions 恒空（模块文档「已知缺口」）。
    assert!(m1.mentions.is_empty());
}

// ---------------------------------------------------------------------------
// 3. 信封边界
// ---------------------------------------------------------------------------

fn fetch_err(script_chat: Value) -> SourceError {
    let fake = FakeCli::install("dws", &script(script_chat));
    let src = source(&fake, Scope {
        conversations: BTreeSet::from(["cidA".to_string()]),
        people: BTreeSet::new(),
    });
    src.fetch(&src.scope().clone(), None, &page_params())
        .expect_err("应失败")
}

#[test]
fn foreign_contract_version_is_rejected() {
    let mut bad = page(vec![msg("m1", "cidA", "x9", NOW, "x")]);
    bad["contractVersion"] = json!("im.message-list.v2");
    let err = fetch_err(json!({"cidA": [bad]}));
    let SourceError::Other { message } = err else { panic!("应归 Other") };
    assert!(message.contains("contractVersion"), "{message}");
}

#[test]
fn incomplete_ledger_never_claims_success() {
    for (name, mut bad) in [
        ("failures", page(vec![msg("m1", "cidA", "x9", NOW, "x")])),
        ("truncated", page(vec![msg("m1", "cidA", "x9", NOW, "x")])),
        ("partial", page(vec![msg("m1", "cidA", "x9", NOW, "x")])),
    ] {
        match name {
            "failures" => bad["failures"] = json!([{"page": 2, "error": "x"}]),
            "truncated" => bad["truncated"] = json!(true),
            _ => bad["partial"] = json!(true),
        }
        let err = fetch_err(json!({"cidA": [bad]}));
        assert!(matches!(err, SourceError::Other { .. }), "{name}: {err:?}");
    }
    // complete=false 且无续点：不冒认完整。
    let mut bad = page(vec![]);
    bad["complete"] = json!(false);
    assert!(matches!(fetch_err(json!({"cidA": [bad]})), SourceError::Other { .. }));
    // hasMore 却无安全续点：截断不冒进。
    let mut bad = page(vec![]);
    bad["hasMore"] = json!(true);
    assert!(matches!(fetch_err(json!({"cidA": [bad]})), SourceError::Other { .. }));
}

#[test]
fn message_missing_identity_is_rejected_not_guessed() {
    let mut bad = msg("m1", "cidA", "x9", NOW, "x");
    bad.as_object_mut().unwrap().remove("messageId");
    let err = fetch_err(json!({"cidA": [page(vec![bad])]}));
    let SourceError::Other { message } = err else { panic!("应归 Other") };
    assert!(message.contains("messageId"), "{message}");
}

#[test]
fn conversation_merged_shape_is_accepted() {
    // list-all 的合并形 result.conversationMessagesList[].messages[] 同样可解析。
    let fake = FakeCli::install(
        "dws",
        &script(json!({"cidA": [json!({
            "contractVersion": "im.message-list.v1", "complete": true, "hasMore": false,
            "result": {"conversationMessagesList": [
                {"conversationId": "cidA", "messages": [msg("m9", "cidA", "x9", NOW, "合并形")]}
            ]}
        })]})),
    );
    let src = source(&fake, group_scope());
    let result = src.fetch(&src.scope().clone(), None, &page_params()).expect("拉取");
    assert_eq!(result.records.len(), 1);
    assert_eq!(result.records[0].external_id, "m9");
}

// ---------------------------------------------------------------------------
// 4. 错误信封映射
// ---------------------------------------------------------------------------

#[test]
fn auth_envelope_maps_to_auth_and_rate_limit_carries_retry_after() {
    let auth_err = json!({"exit": 3, "stderr": {"error": {"code": 3, "category": "auth",
        "reason": "auth_refresh_failed", "retryable": false, "message": "RAW-BODY-NOT-LOGGED"}}});
    let err = fetch_err(json!({"cidA": [auth_err]}));
    assert!(matches!(err, SourceError::Auth { status: 401, .. }), "{err:?}");
    assert!(!err.to_string().contains("RAW-BODY"), "日志无正文：{err}");

    let limited = json!({"exit": 1, "stderr": {"error": {"category": "api", "retryable": true,
        "retry_after_seconds": 2}}});
    let err = fetch_err(json!({"cidA": [limited]}));
    assert_eq!(
        err,
        SourceError::RateLimited {
            message: "dws retryable (category=api)".to_string(),
            retry_after_ms: Some(2000),
        }
    );
}

#[test]
fn timeout_and_spawn_failure_are_distinct() {
    let slow = json!({"sleep_ms": 5000, "stdout": page(vec![])});
    let fake = FakeCli::install("dws", &script(json!({"cidA": [slow]})));
    let mut config = config_for(&fake, wide_approval());
    config.call_timeout = Duration::from_millis(400);
    let src = CliDingTalkSource::new("dws-im-1", scope(), config).with_clock(|| NOW);
    let err = src.fetch(&scope(), None, &page_params()).expect_err("应超时");
    assert!(matches!(err, SourceError::Timeout { .. }), "{err:?}");

    let mut missing = DwsCliConfig::new(PathBuf::from("/nonexistent/wctx-dws"), PROFILE);
    missing.approved_window = Some(wide_approval());
    let missing = CliDingTalkSource::new("dws-im-1", scope(), missing);
    let err = missing.fetch(&scope(), None, &page_params()).expect_err("应失败");
    let SourceError::Other { message } = err else { panic!("应归 Other") };
    // 是启动失败（io 错误摘要），不是「未授权」拒绝。
    assert!(!message.contains("未授权") && !message.is_empty(), "{message}");
}

#[test]
fn probe_reflects_auth_snapshot() {
    let ok = FakeCli::install(
        "dws",
        &script(json!({})),
    );
    source(&ok, scope()).probe().expect("已授权应通过");

    let mut denied = script(json!({}));
    denied["dws"]["auth_status"] = json!({"stdout": {"authenticated": false, "token_valid": false,
        "refresh_token_valid": false}});
    let denied = FakeCli::install("dws", &denied);
    let err = source(&denied, scope()).probe().expect_err("未授权");
    assert!(matches!(err, SourceError::Auth { status: 401, .. }));

    let mut refreshing = script(json!({}));
    refreshing["dws"]["auth_status"] = json!({"stdout": {"authenticated": true, "token_valid": false,
        "refresh_token_valid": true}});
    source(&FakeCli::install("dws", &refreshing), scope())
        .probe()
        .expect("access 过期但 refresh 有效 → 不要求重新授权");
}

#[test]
fn standalone_probes_do_not_need_a_source() {
    let fake = FakeCli::install("dws", &script(json!({})));
    let opts = Default::default();
    assert_eq!(cli_version(&fake.exe(), &opts).expect("版本"), "v1.0.62");
    assert_eq!(
        auth_status(&fake.exe(), PROFILE, &opts).expect("授权快照"),
        DwsAuthStatus {
            authenticated: true,
            token_valid: true,
            refresh_token_valid: true,
        }
    );
    // 独立入口同样过参数校验：不合法 profile 不 spawn。
    let before = fake.argvs().len();
    assert!(auth_status(&fake.exe(), "--evil", &opts).is_err());
    assert_eq!(fake.argvs().len(), before);
}

#[test]
fn unsafe_cursor_or_scope_values_never_reach_argv() {
    let bad = DwsReadCommand::ChatMessages {
        target: workcontext_infra::dws::TargetRef::Group("--profile".into()),
        start: "2026-11-21 00:00:00".into(),
        end: "2026-11-22 00:00:00".into(),
        page_token: None,
        limit: 50,
        profile: PROFILE.to_string(),
    };
    assert!(bad.sanitized().is_none());
    let bad = DwsReadCommand::ChatMessages {
        target: workcontext_infra::dws::TargetRef::Group("cidA".into()),
        start: "2026-11-21 00:00:00".into(),
        end: "2026-11-22 00:00:00".into(),
        page_token: Some("--limit".into()),
        limit: 50,
        profile: PROFILE.to_string(),
    };
    assert!(bad.sanitized().is_none());
}

// ---------------------------------------------------------------------------
// 5. 游标语义
// ---------------------------------------------------------------------------

#[test]
fn cursor_reopens_window_with_overlap() {
    let fake = FakeCli::install(
        "dws",
        &script(json!({"cidA": [page(vec![]), page(vec![])]})),
    );
    let src = source(&fake, group_scope());
    let scope = src.scope().clone();
    let first = src.fetch(&scope, None, &page_params()).expect("首轮");
    let cursor = first.next_cursor.expect("整轮成功必须有游标");
    assert_eq!(cursor["window_end_ms"], json!(NOW));
    // 首轮窗口 = [NOW − 24h, NOW]。
    let calls = chat_calls(&fake);
    assert_eq!(flag_value(&calls[0], "--start"), Some("2026-11-21 00:00:00"));
    assert_eq!(flag_value(&calls[0], "--end"), Some("2026-11-22 00:00:00"));

    src.fetch(&scope, Some(&cursor), &page_params()).expect("次轮");
    let calls = chat_calls(&fake);
    // 次轮窗口 = [上次 window_end − overlap(6h), NOW]：宁可多拉，不可漏拉。
    assert_eq!(flag_value(&calls[1], "--start"), Some("2026-11-21 18:00:00"));
}

#[test]
fn in_round_pagination_repeats_window_and_passes_token() {
    let fake = FakeCli::install(
        "dws",
        &script(json!({"cidA": [
            more_page(vec![msg("m1", "cidA", "x9", NOW - 1000, "第一页")], "tok1"),
            page(vec![msg("m2", "cidA", "x9", NOW - 900, "第二页")])
        ]})),
    );
    let src = source(&fake, Scope {
        conversations: BTreeSet::from(["cidA".to_string()]),
        people: BTreeSet::new(),
    });
    let scope = src.scope().clone();
    let first = src.fetch(&scope, None, &page_params()).expect("拉取");
    assert_eq!(first.records.len(), 2, "CLI 续页在单次 fetch 内完成");
    assert!(!first.has_more);
    let calls = chat_calls(&fake);
    assert_eq!(calls.len(), 2);
    assert_eq!(flag_value(&calls[0], "--page-token"), None);
    assert_eq!(flag_value(&calls[1], "--page-token"), Some("tok1"));
    // 续页必须重复原查询条件（page-token 绑定会话+方向+时间范围）。
    assert_eq!(flag_value(&calls[1], "--start"), flag_value(&calls[0], "--start"));
    assert_eq!(flag_value(&calls[1], "--end"), flag_value(&calls[0], "--end"));
    assert_eq!(flag_value(&calls[1], "--group"), flag_value(&calls[0], "--group"));
}

#[test]
fn page_cap_yields_partial_cursor_that_resumes_in_place() {
    let fake = FakeCli::install(
        "dws",
        &script(json!({"cidA": [
            more_page(vec![msg("m1", "cidA", "x9", NOW - 1000, "第一页")], "tok1"),
            page(vec![msg("m2", "cidA", "x9", NOW - 900, "第二页")])
        ]})),
    );
    let mut config = config_for(&fake, wide_approval());
    config.max_pages_per_round = 1;
    let src = CliDingTalkSource::new("dws-im-1", Scope {
        conversations: BTreeSet::from(["cidA".to_string()]),
        people: BTreeSet::new(),
    }, config).with_clock(|| NOW);
    let scope = src.scope().clone();

    let partial = src.fetch(&scope, None, &page_params()).expect("partial");
    assert!(partial.has_more);
    assert_eq!(partial.records.len(), 1);
    let cursor = partial.next_cursor.expect("续页游标");
    assert_eq!(cursor["remaining"], json!(["g:cidA"]));
    assert_eq!(cursor["page_token"], json!("tok1"));

    // 引擎以 partial 游标续拉：窗口原样（不重新按 overlap 计算），带 token。
    let rest = src.fetch(&scope, Some(&cursor), &page_params()).expect("续页");
    assert_eq!(rest.records.len(), 1);
    assert!(!rest.has_more);
    let calls = chat_calls(&fake);
    assert_eq!(flag_value(&calls[1], "--start"), flag_value(&calls[0], "--start"));
    assert_eq!(flag_value(&calls[1], "--page-token"), Some("tok1"));
}

// ---------------------------------------------------------------------------
// 6. 可接入：与 SyncEngine 端到端
// ---------------------------------------------------------------------------

struct TempDb(std::path::PathBuf);

impl TempDb {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "wctx-dws-cli-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        Self(dir)
    }

    fn open(&self) -> workcontext_store::rusqlite::Connection {
        workcontext_store::open(&self.0.join("work.sqlite")).expect("打开工作库")
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn engine_end_to_end_is_idempotent_and_auth_failure_keeps_cursor() {
    let db = TempDb::new("e2e");
    let fake = FakeCli::install(
        "dws",
        &script(json!({"cidA": [page(vec![
            msg("m1", "cidA", "x9", NOW - 1000, "请确认复测结果"),
            msg("m2", "cidA", "u1", NOW - 900, "好的")
        ])]})),
    );
    let src = Arc::new(source(&fake, Scope {
        conversations: BTreeSet::from(["cidA".to_string()]),
        people: BTreeSet::new(),
    }));
    let engine = SyncEngine::new(db.open(), SyncConfig::default());
    engine
        .register(Arc::clone(&src) as Arc<dyn DingTalkSource>, "钉钉 IM（替身）", NOW)
        .expect("注册");

    // 轮 1：落库 2 条，游标推进，状态 idle。
    let first = engine.sync_source("dws-im-1", NOW).expect("轮 1");
    assert_eq!(first.status, SyncStatus::Ok, "{first:?}");
    assert_eq!(first.inserted, 2);
    let checkpoint = engine.checkpoint("dws-im-1").expect("检查点").expect("存在");
    assert_eq!(checkpoint.state, CheckpointState::Idle);
    let first_cursor = checkpoint.cursor_json.clone().expect("游标已推进");
    {
        let conn = db.open();
        let (conv_type, mentions, reply): (Option<String>, String, Option<String>) = conn
            .query_row(
                "SELECT conversation_type, mentions_json, reply_to_id FROM source_record
                 WHERE source_id='dws-im-1' AND external_id='m1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("元数据列");
        assert_eq!(conv_type.as_deref(), Some("group"));
        assert_eq!(mentions, "[]");
        assert_eq!(reply, None);
    }

    // 轮 2：同一窗口重拉 → 全部 unchanged（幂等，不重建记录）。
    fake.clear_log();
    let second = engine.sync_source("dws-im-1", NOW + 60_000).expect("轮 2");
    assert_eq!(second.status, SyncStatus::Ok, "{second:?}");
    assert_eq!(second.inserted, 0);
    assert_eq!(second.unchanged, 2);

    // 轮 2 成功后游标已推进到本轮窗口；轮 3 的鉴权失败必须保持它不动。
    let saved_cursor = engine
        .checkpoint("dws-im-1")
        .expect("检查点")
        .expect("存在")
        .cursor_json
        .expect("轮 2 后游标存在");
    assert_ne!(saved_cursor, first_cursor, "轮 2 成功后窗口起点推进（end − overlap）");

    // 轮 3：授权失效 → auth_required，游标不动，记录不动。
    fake.clear_log();
    let auth_failure = json!({"exit": 3, "stderr": {"error": {
        "category": "auth", "reason": "auth_refresh_failed", "retryable": false}}});
    let mut denied = script(json!({}));
    denied["dws"]["chat"] = json!({ "cidA": [auth_failure] });
    fake.set_script(&denied);
    let third = engine.sync_source("dws-im-1", NOW + 120_000).expect("轮 3");
    assert_eq!(third.status, SyncStatus::AuthRequired, "{third:?}");
    let checkpoint = engine.checkpoint("dws-im-1").expect("检查点").expect("存在");
    assert_eq!(checkpoint.state, CheckpointState::AuthRequired);
    assert_eq!(checkpoint.cursor_json, Some(saved_cursor), "鉴权失败不清游标");
    {
        let conn = db.open();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM source_record", [], |row| row.get(0))
            .expect("计数");
        assert_eq!(count, 2);
    }
}

// ---------------------------------------------------------------------------
// 7. 批准时间窗（真实读取的显式授权边界）
// ---------------------------------------------------------------------------

#[test]
fn fetch_without_approved_window_refuses_without_calls() {
    let fake = FakeCli::install("dws", &script(json!({"cidA": [page(vec![])]})));
    // `approved_window` 缺省为 None：出厂即 fail closed。
    let config = DwsCliConfig::new(fake.exe(), PROFILE);
    let src = CliDingTalkSource::new("dws-im-1", group_scope(), config).with_clock(|| NOW);
    let err = src
        .fetch(&src.scope().clone(), None, &page_params())
        .expect_err("未授权应拒绝");
    let SourceError::Other { message } = err else { panic!("应归 Other") };
    assert!(message.contains("未授权"), "{message}");
    assert!(fake.argvs().is_empty(), "未授权不得发起任何 CLI 调用");
    // probe 是授权状态查询、不读消息，不受时间窗约束。
    src.probe().expect("probe 不属于消息读取");
}

#[test]
fn approved_window_bounds_the_request_and_filters_records() {
    // 批准 [NOW−2h, NOW−1h)：比首轮 24h 回溯窄得多，请求窗口被收紧到批准范围。
    let approved = ApprovedWindow {
        start_ms: NOW - 2 * HOUR,
        end_ms: Some(NOW - HOUR),
    };
    let fake = FakeCli::install(
        "dws",
        &script(json!({"cidA": [page(vec![
            msg("m1", "cidA", "x9", NOW - 2 * HOUR, "起点（含）"),
            msg("m2", "cidA", "x9", NOW - 90 * 60_000, "范围内"),
            msg("m3", "cidA", "x9", NOW - HOUR, "终点（不含）"),
            msg("m4", "cidA", "x9", NOW - 3 * HOUR, "批准起点之前"),
        ])]})),
    );
    let src = CliDingTalkSource::new("dws-im-1", group_scope(), config_for(&fake, approved))
        .with_clock(|| NOW);
    let result = src.fetch(&src.scope().clone(), None, &page_params()).expect("拉取");
    let calls = chat_calls(&fake);
    assert_eq!(calls.len(), 1);
    assert_eq!(flag_value(&calls[0], "--start"), Some("2026-11-21 22:00:00"));
    assert_eq!(flag_value(&calls[0], "--end"), Some("2026-11-21 23:00:00"));
    let ids: Vec<&str> = result.records.iter().map(|r| r.external_id.as_str()).collect();
    assert_eq!(ids, ["m1", "m2"], "批准范围外的记录不返回给引擎");
}

#[test]
fn approved_window_without_intersection_reads_nothing() {
    let cases = [
        // 批准范围已结束（撤权）：本轮窗口整体在批准终点之后。
        ApprovedWindow {
            start_ms: NOW - 10 * DAY,
            end_ms: Some(NOW - 2 * DAY),
        },
        // 批准起点尚未来到。
        ApprovedWindow {
            start_ms: NOW + DAY,
            end_ms: None,
        },
    ];
    for approved in cases {
        let fake = FakeCli::install("dws", &script(json!({"cidA": [page(vec![])]})));
        let src =
            CliDingTalkSource::new("dws-im-1", group_scope(), config_for(&fake, approved))
                .with_clock(|| NOW);
        let result = src
            .fetch(&src.scope().clone(), None, &page_params())
            .expect("无交集不报错（不是失败，是授权范围外不读）");
        assert!(result.records.is_empty());
        assert!(!result.has_more);
        assert!(result.next_cursor.is_some(), "空终页仍带游标，引擎正常推进");
        assert!(fake.argvs().is_empty(), "授权范围外不得调用 CLI");
    }
}

#[test]
fn approved_bounds_round_inward_to_whole_seconds() {
    // 批准边界非整秒：起点向上取整（绝不早于批准起点）、终点向下取整。
    let approved = ApprovedWindow {
        start_ms: NOW - 2 * HOUR + 500,
        end_ms: Some(NOW - HOUR + 999),
    };
    let fake = FakeCli::install("dws", &script(json!({"cidA": [page(vec![])]})));
    let src = CliDingTalkSource::new("dws-im-1", group_scope(), config_for(&fake, approved))
        .with_clock(|| NOW);
    src.fetch(&src.scope().clone(), None, &page_params()).expect("拉取");
    let calls = chat_calls(&fake);
    assert_eq!(flag_value(&calls[0], "--start"), Some("2026-11-21 22:00:01"));
    assert_eq!(flag_value(&calls[0], "--end"), Some("2026-11-21 23:00:00"));
}

#[test]
fn revoked_window_makes_engine_round_a_clean_no_op() {
    // 撤权后的来源：引擎一轮正常结束（Ok、0 条），不累计失败、不调 CLI。
    let db = TempDb::new("revoked");
    let fake = FakeCli::install("dws", &script(json!({"cidA": [page(vec![])]})));
    let approved = ApprovedWindow {
        start_ms: NOW - 10 * DAY,
        end_ms: Some(NOW - 2 * DAY),
    };
    let src = CliDingTalkSource::new("dws-im-1", group_scope(), config_for(&fake, approved))
        .with_clock(|| NOW);
    let engine = SyncEngine::new(db.open(), SyncConfig::default());
    engine
        .register(Arc::new(src) as Arc<dyn DingTalkSource>, "钉钉 IM（已撤权）", NOW)
        .expect("注册");
    let outcome = engine.sync_source("dws-im-1", NOW).expect("一轮");
    assert_eq!(outcome.status, SyncStatus::Ok, "{outcome:?}");
    assert_eq!((outcome.fetched, outcome.inserted), (0, 0));
    assert!(fake.argvs().is_empty(), "撤权范围外不得调用 CLI");
}
