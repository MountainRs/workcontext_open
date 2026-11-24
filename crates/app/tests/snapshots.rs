//! A12 集成测试（M4，详细设计 §16.2）：真实 SQLite 快照创建、列表、
//! MD/JSON 导出与默认脱敏。
//!
//! 数据全部来自 `fixtures/cases/A12_snapshot_export/input.json`（夹具即链路
//! 输入）；`expected.json` 只作断言输入（断言快照行含期望/版本/引用、
//! 默认导出不含正文与原始手机号——PRD §12 A12、§10.2、§13.2-9）。
//!
//! 不读 `body` 原文列、不把 `agent_run.raw_output` 带进快照：种子数据里
//! 故意同时放了原始手机号，任何导出物里都不允许再出现。

use std::path::PathBuf;

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use workcontext_app::snapshots::{
    self, CreateContext, ExportFormat, ExportFile, IncludeBody, NewSnapshot, Severity,
    SnapshotKind, SnapshotStatus,
};
use workcontext_store::rusqlite::{Connection, params};
use workcontext_store::open;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// 独立临时目录 + 库文件；Drop 时整目录清理。
struct TempDb {
    dir: std::path::PathBuf,
}

impl TempDb {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "wc-a12-snapshots-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        Self { dir }
    }

    fn db_path(&self) -> std::path::PathBuf {
        self.dir.join(workcontext_core::DB_FILENAME)
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("仓库根目录可定位")
        .join("fixtures")
}

fn case_json(file: &str) -> Value {
    let path = fixtures_root().join("cases").join("A12_snapshot_export").join(file);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("读取 {}: {e}", path.display()));
    serde_json::from_str(&text).expect("夹具 JSON 合法")
}

fn stage<'a>(case: &'a Value, index: u64) -> &'a Value {
    case["stages"]
        .as_array()
        .expect("stages 数组")
        .iter()
        .find(|s| s["index"].as_u64() == Some(index))
        .unwrap_or_else(|| panic!("缺少阶段 {index}"))
}

/// RFC3339（带 +08:00 偏移）→ UTC 毫秒。
fn utc_ms(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .unwrap_or_else(|e| panic!("时间 {s} 不合法: {e}"))
        .timestamp_millis()
}

const APP_VERSION: &str = "0.1.7";
const RECORD_ROW_ID: &str = "sr_a12_r1";
const ITEM_ID: &str = "wi_a12";

/// 按 input.json 阶段 1 种子数据：来源、来源记录（正文含原始手机号）、
/// 误判产生的事项、证据链、以及一次 ok 的 mock 判读运行（raw_output
/// 里也埋了原始手机号，验证快照绝不携带它）。
fn seed(conn: &Connection) {
    let input = case_json("input.json");
    let source = &input["sources"][0];
    let record = &stage(&input, 1)["records"][0];
    let authorized_at = utc_ms(source["authorized_at"].as_str().unwrap());

    conn.execute(
        "INSERT INTO source(id, kind, display_name, scope_json, enabled, authorized_at, created_at)
         VALUES (?1, 'dws_im', ?2, ?3, 1, ?4, ?4)",
        params![
            source["id"].as_str().unwrap(),
            source["display_name"].as_str().unwrap(),
            source["scope"].to_string(),
            authorized_at,
        ],
    )
    .expect("插入 source");

    let occurred_at = utc_ms(record["occurred_at"].as_str().unwrap());
    let ingested_at = utc_ms(record["ingested_at"].as_str().unwrap());
    conn.execute(
        "INSERT INTO source_record(
            id, source_id, external_id, kind, occurred_at, ingested_at, conversation_id,
            sender_id, sender_name, direction, summary, body, body_masked, content_hash,
            raw_state, judge_state, prefilter_reason)
         VALUES (?1, ?2, ?3, 'im_message', ?4, ?5, ?6, ?7, ?8, 'in', ?9, ?10, ?11, ?12,
                 'available', 'judged', 'dm_in')",
        params![
            RECORD_ROW_ID,
            source["id"].as_str().unwrap(),
            record["external_id"].as_str().unwrap(),
            occurred_at,
            ingested_at,
            record["conversation_id"].as_str().unwrap(),
            record["sender_id"].as_str().unwrap(),
            record["sender_name"].as_str().unwrap(),
            record["summary"].as_str().unwrap(),
            record["body"].as_str().unwrap(),
            record["body_masked"].as_str().unwrap(),
            format!("hash-{}", record["external_id"].as_str().unwrap()),
        ],
    )
    .expect("插入 source_record");

    // 误判出的「停机维护」待办（mock 判读 spurious_item 缺陷样本）。
    conn.execute(
        "INSERT INTO work_item(
            id, title, lifecycle, attention, next_action, followup_type, dedupe_key,
            confidence, origin_source_record_id, last_evidence_at, created_at, updated_at)
         VALUES (?1, ?2, 'active', 'me', '确认停机维护是否需要跟进', 'none', ?3, 80, ?4, ?5, ?5, ?5)",
        params![
            ITEM_ID,
            format!("停机维护通知：{}", record["summary"].as_str().unwrap()),
            format!("dd-{ITEM_ID}"),
            RECORD_ROW_ID,
            ingested_at,
        ],
    )
    .expect("插入 work_item");
    conn.execute(
        "INSERT INTO link_evidence(id, source_record_id, target_type, target_id, link_type,
            state, created_at)
         VALUES ('le_a12', ?1, 'work_item', ?2, 'origin', 'confirmed', ?3)",
        params![RECORD_ROW_ID, ITEM_ID, ingested_at],
    )
    .expect("插入 link_evidence");

    // 判读运行：input_refs_json 用 ItemService 约定的对象形。
    conn.execute(
        "INSERT INTO agent_run(id, kind, runtime, model_id, prompt_version, input_refs_json,
            input_sha256, raw_output, status, created_at)
         VALUES ('run_a12_1', 'classify', 'mock', 'mock-model', 'p1', ?1, 'sha-a12-r1', ?2,
                 'ok', ?3)",
        params![
            serde_json::json!({ "sourceRecordIds": [RECORD_ROW_ID], "externalIds": ["A12-R1"] }).to_string(),
            // raw_output 是诊断留档：含原始手机号，快照只取元数据，绝不携带它。
            serde_json::json!({ "rationale": "联系值班电话 13800001234 确认，判断需要跟进" }).to_string(),
            ingested_at,
        ],
    )
    .expect("插入 agent_run");
}

/// 阶段 2 的一次性快照创建（走真实 IPC 入参形态：无显式 refs，
/// 目标是事项 → 自动从证据链反查引用）。
fn create_a12_snapshot(conn: &Connection) -> snapshots::SnapshotRow {
    let input = case_json("input.json");
    let action = stage(&input, 2)["human_actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["op"] == "snapshot_create")
        .expect("snapshot_create 动作");
    let now_ms = utc_ms(stage(&input, 2)["clock_at"].as_str().unwrap());
    snapshots::create(
        conn,
        &NewSnapshot {
            route: Some(action["route"].as_str().unwrap().to_string()),
            target_type: Some(action["target_type"].as_str().unwrap().to_string()),
            target_id: Some(ITEM_ID.to_string()),
            description: action["description"].as_str().unwrap().to_string(),
            expected: Some(action["expected"].as_str().unwrap().to_string()),
            kind: SnapshotKind::parse(action["kind"].as_str().unwrap()).unwrap(),
            severity: Severity::parse(action["severity"].as_str().unwrap()),
            record_refs: Vec::new(),
        },
        &CreateContext { now_ms, app_version: APP_VERSION.to_string() },
    )
    .expect("创建快照")
}

fn export_a12(conn: &Connection, format: ExportFormat, include_body: IncludeBody) -> ExportFile {
    let input = case_json("input.json");
    let now_ms = utc_ms(stage(&input, 3)["clock_at"].as_str().unwrap());
    let id = snapshots::list(conn, None).unwrap()[0].id.clone();
    snapshots::export(conn, &[id], format, include_body, now_ms).expect("导出")
}

fn assert_contains_all(where_: &str, content: &str, needles: &[&str]) {
    for needle in needles {
        assert!(content.contains(needle), "{where_} 应包含「{needle}」：\n{content}");
    }
}

fn assert_not_contains(where_: &str, content: &str, needles: &[&str]) {
    for needle in needles {
        assert!(!content.contains(needle), "{where_} 不得包含「{needle}」：\n{content}");
    }
}

/// 断言簇 (1)：快照行含期望、版本、引用、schema_version，redaction=masked、
/// status=open（expected.json 阶段 2 的 `snapshot` 块逐键断言）。
#[test]
fn a12_snapshot_row_matches_expected_block() {
    let db = TempDb::new("row");
    let conn = open(&db.db_path()).unwrap();
    seed(&conn);
    let row = create_a12_snapshot(&conn);

    let exp = stage(&case_json("expected.json"), 2)["snapshot"].clone();
    assert_eq!(exp["count"].as_u64(), Some(1));
    assert_eq!(row.kind, Some(SnapshotKind::SpuriousItem));
    assert_eq!(exp["kind"].as_str(), Some("spurious_item"));
    assert_eq!(row.severity, Some(Severity::Medium));
    assert_eq!(exp["severity"].as_str(), Some("medium"));
    assert_eq!(row.status, SnapshotStatus::Open);
    assert_eq!(exp["status"].as_str(), Some("open"));
    assert_eq!(row.redaction.as_str(), exp["redaction"].as_str().unwrap());
    assert!(
        row.app_version.starts_with(exp["app_version_prefix"].as_str().unwrap()),
        "app_version 应带 0.1. 前缀：{}",
        row.app_version
    );
    assert_eq!(row.schema_version, exp["schema_version"].as_i64().unwrap());
    for key in exp["description_contains"].as_array().unwrap() {
        assert!(row.description.contains(key.as_str().unwrap()));
    }
    for key in exp["expected_contains"].as_array().unwrap() {
        assert!(row.expected.as_deref().unwrap().contains(key.as_str().unwrap()));
    }
    let ref_ids: Vec<&str> = row.refs.iter().map(|r| r.external_id.as_str()).collect();
    for want in exp["input_refs_include"].as_array().unwrap() {
        assert!(ref_ids.contains(&want.as_str().unwrap()), "引用应含 {}", want);
    }
    assert_eq!(row.route.as_deref(), Some(exp["route"].as_str().unwrap()));
    // 引用带脱敏摘要与来源元数据（PRD §11.2 最小可复现证据）。
    assert!(row.refs[0].summary.as_deref().unwrap().contains("停机"));
    assert_eq!(row.refs[0].source_id.as_deref(), Some("src-dm-sun"));
    // 判读运行按 input_refs_json 的带引号 token 命中，且只有元数据。
    assert_eq!(row.runs.len(), 1);
    assert_eq!(row.runs[0].run_id, "run_a12_1");
    assert_eq!(row.runs[0].status.as_deref(), Some("ok"));
    // 创建即留痕（事件流）。
    let events: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM work_event WHERE target_type='snapshot' AND target_id=?1",
            params![row.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(events, 1);
}

/// 断言簇 (2)：默认 `includeBody=false` 的 MD + JSON 导出——机器与人读两种
/// 表示都含期望/版本/引用，都不含消息正文与原始手机号（阶段 3 的 `export` 块）。
#[test]
fn a12_default_export_is_redacted_and_body_free() {
    let db = TempDb::new("export");
    let conn = open(&db.db_path()).unwrap();
    seed(&conn);
    create_a12_snapshot(&conn);

    let exp = stage(&case_json("expected.json"), 3)["export"].clone();
    assert_eq!(exp["include_body"].as_bool(), Some(false));

    let json = export_a12(&conn, ExportFormat::Json, IncludeBody::No);
    let json_value: Value = serde_json::from_str(&json.content).unwrap();
    assert_eq!(json_value["schema"].as_str(), Some("snapshot/v1"));
    let snap_obj = &json_value["snapshots"][0];
    for field in exp["json"]["fields_present"].as_array().unwrap() {
        assert!(
            snap_obj.get(field.as_str().unwrap()).is_some(),
            "JSON 导出缺字段 {field}"
        );
    }
    assert_contains_all("JSON 导出", &json.content, &str_list(&exp["json"]["contains_all"]));
    assert_not_contains("JSON 导出", &json.content, &str_list(&exp["json"]["not_contains"]));
    assert!(
        json_value["body_included"].as_bool() == Some(false)
            && snap_obj.get("bodies").is_none(),
        "默认导出不得有正文区块"
    );

    let md = export_a12(&conn, ExportFormat::Markdown, IncludeBody::No);
    assert_contains_all("MD 导出", &md.content, &str_list(&exp["md"]["contains_all"]));
    assert_not_contains("MD 导出", &md.content, &str_list(&exp["md"]["not_contains"]));

    for needle in str_list(&exp["forbidden_any_format"]) {
        assert!(!json.content.contains(needle) && !md.content.contains(needle), "任何格式都不得出现 {needle}");
    }
    // 判读运行元数据进导出，但 raw_output 的原文不进。
    assert!(json.content.contains("run_a12_1"));
    assert_not_contains("JSON 导出", &json.content, &["rationale"]);
}

/// 明确同意（`IncludeBody::Explicit`）才带正文，且带的是 body_masked：
/// 脱敏手机号可见、原始手机号仍绝不出现。
#[test]
fn a12_body_only_with_explicit_consent_and_still_masked() {
    let db = TempDb::new("body");
    let conn = open(&db.db_path()).unwrap();
    seed(&conn);
    create_a12_snapshot(&conn);

    let json = export_a12(&conn, ExportFormat::Json, IncludeBody::Explicit);
    assert!(json.body_included);
    let v: Value = serde_json::from_str(&json.content).unwrap();
    assert_eq!(v["body_included"].as_bool(), Some(true));
    assert!(v["snapshots"][0]["bodies"][0]["body_masked"].as_str().unwrap().contains("138****1234"));
    assert_not_contains("JSON 显式导出", &json.content, &["13800001234"]);

    let md = export_a12(&conn, ExportFormat::Markdown, IncludeBody::Explicit);
    assert!(md.content.contains("138****1234"));
    assert_not_contains("MD 显式导出", &md.content, &["13800001234"]);
}

/// 列表过滤 + 重启（重开库）后快照仍在（PRD §13.2-9）。
#[test]
fn snapshots_persist_across_reopen_and_filter_by_status() {
    let db = TempDb::new("persist");
    let conn = open(&db.db_path()).unwrap();
    seed(&conn);
    let first = create_a12_snapshot(&conn);
    // 第二条：描述里埋原始手机号，导出必须再脱敏（用户输入同样过闸）。
    snapshots::create(
        &conn,
        &NewSnapshot {
            route: None,
            target_type: None,
            target_id: None,
            description: "界面上写死了我的电话 13800001234".to_string(),
            expected: Some("显示为脱敏形式".to_string()),
            kind: SnapshotKind::Ui,
            severity: Some(Severity::Low),
            record_refs: vec!["A12-R1".to_string()],
        },
        &CreateContext { now_ms: first.created_at + 60_000, app_version: APP_VERSION.to_string() },
    )
    .unwrap();

    let all = snapshots::list(&conn, None).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(snapshots::list(&conn, Some(SnapshotStatus::Open)).unwrap().len(), 2);
    assert!(snapshots::list(&conn, Some(SnapshotStatus::Resolved)).unwrap().is_empty());
    assert!(snapshots::list(&conn, Some(SnapshotStatus::WontFix)).unwrap().is_empty());
    // created_at DESC：最新在前。
    assert!(all[0].created_at > all[1].created_at);
    // 用户输入的敏感串在导出里必须脱敏。
    let md = snapshots::export(
        &conn,
        &[all[0].id.clone()],
        ExportFormat::Markdown,
        IncludeBody::No,
        first.created_at + 120_000,
    )
    .unwrap();
    assert!(md.content.contains("138****1234"));
    assert_not_contains("用户输入导出", &md.content, &["13800001234"]);

    drop(conn);
    let conn = open(&db.db_path()).unwrap();
    let reopened = snapshots::list(&conn, None).unwrap();
    assert_eq!(reopened.len(), 2);
    assert!(reopened.iter().any(|r| r.id == first.id && r.description.contains("停机")));
    assert_eq!(reopened[1].refs[0].external_id, "A12-R1");
}

/// 参数与错误路径：未知枚举、空描述、超长、空 ids、不存在的快照。
#[test]
fn invalid_inputs_fail_loudly() {
    let db = TempDb::new("invalid");
    let conn = open(&db.db_path()).unwrap();
    seed(&conn);
    let ctx = CreateContext { now_ms: 1_769_000_000_000, app_version: APP_VERSION.to_string() };

    let empty = snapshots::create(
        &conn,
        &NewSnapshot {
            route: None,
            target_type: None,
            target_id: None,
            description: "   ".to_string(),
            expected: None,
            kind: SnapshotKind::Other,
            severity: None,
            record_refs: vec![],
        },
        &ctx,
    );
    assert!(matches!(empty, Err(snapshots::SnapshotError::InvalidArgument(_))));

    let long = snapshots::create(
        &conn,
        &NewSnapshot {
            route: None,
            target_type: None,
            target_id: None,
            description: "长".repeat(2001),
            expected: None,
            kind: SnapshotKind::Other,
            severity: None,
            record_refs: vec![],
        },
        &ctx,
    );
    assert!(matches!(long, Err(snapshots::SnapshotError::InvalidArgument(_))));

    let empty_ids = snapshots::export(&conn, &[], ExportFormat::Json, IncludeBody::No, 0);
    assert!(matches!(empty_ids, Err(snapshots::SnapshotError::InvalidArgument(_))));

    let missing = snapshots::export(
        &conn,
        &["fb_不存在".to_string()],
        ExportFormat::Markdown,
        IncludeBody::No,
        0,
    );
    assert!(matches!(missing, Err(snapshots::SnapshotError::NotFound(_))));

    assert!(snapshots::record_refs_for_item(&conn, "wi_没有").unwrap().is_empty());
    assert_eq!(snapshots::record_refs_for_item(&conn, ITEM_ID).unwrap(), vec!["A12-R1"]);
}

/// 回归：一段文本里有多个敏感项时，导出必须逐项脱敏（曾经只有位置最靠后的
/// 一项生效，前面的手机号/邮箱会原样进入导出文件——门槛 12）。
#[test]
fn export_redacts_every_sensitive_item_in_one_field() {
    let db = TempDb::new("multi-redact");
    let conn = open(&db.db_path()).unwrap();
    let row = snapshots::create(
        &conn,
        &NewSnapshot {
            route: None,
            target_type: None,
            target_id: None,
            description: "回拨 13800001234，邮件 zhangsan@example.com，备用 13900004321".to_string(),
            expected: None,
            kind: SnapshotKind::Other,
            severity: None,
            record_refs: vec![],
        },
        &CreateContext { now_ms: 1_769_000_000_000, app_version: APP_VERSION.to_string() },
    )
    .unwrap();

    for format in [ExportFormat::Json, ExportFormat::Markdown] {
        let file = snapshots::export(
            &conn,
            &[row.id.clone()],
            format,
            IncludeBody::No,
            1_769_000_000_001,
        )
        .unwrap();
        assert_not_contains("多项脱敏导出", &file.content, &["13800001234", "zhangsan@", "13900004321"]);
        assert_contains_all(
            "多项脱敏导出",
            &file.content,
            &["138****1234", "z***@example.com", "139****4321"],
        );
    }
}

fn str_list(v: &Value) -> Vec<&str> {
    v.as_array().unwrap().iter().map(|s| s.as_str().unwrap()).collect()
}
