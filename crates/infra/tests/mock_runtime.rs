//! MockAgentRuntime × 判读夹具 的离线集成测试（无网络，详细设计 §19 步 2）。
//!
//! 核心不变量：
//! 1. manifest 中每个判读批次都能从 `fixtures/cases/*.json` 重建出同一
//!    `input_sha256` 并在 runtime 中精确命中（哈希规则可复核）；
//! 2. 输入复原件与重建文档逐字节同哈希；
//! 3. 未匹配输入 → 明确 `Unknown`，绝不回放伪造答案；
//! 4. A11 纠错重放（同批记录）→ 同一哈希、同一输出；
//! 5. `fixtures/schema/classify_v1.json` 与 Rust 契约常量对账。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use serde_json::Value;
use sha2::{Digest, Sha256};
use workcontext_infra::classify_input::{canonical_json, input_sha256, ClassifyInput};

use workcontext_infra::contract::{
    validate_classify_v1, ALLOWED_KEYS, BELONGS_TO_ME, CLASSIFICATIONS, CURRENT_TURNS,
    FOLLOWUP_TYPES, REQUIRED_KEYS, RESOLUTION_EVIDENCE_TYPES,
};
use workcontext_infra::fixture_case::{build_classify_input, find_case_dir, load_case_input};
use workcontext_infra::mock::{MockAgentRuntime, MockOutcome};

const SCOPE_CASES: [&str; 5] = ["A01", "A02", "A03", "A06", "A11"];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates 目录")
        .parent()
        .expect("workspace 根")
        .to_path_buf()
}

fn fixtures_root() -> PathBuf {
    repo_root().join("fixtures")
}

fn load_json(path: &Path) -> Value {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|source| panic!("读取 {} 失败：{source}", path.display()));
    serde_json::from_str(&raw)
        .unwrap_or_else(|source| panic!("解析 {} 失败：{source}", path.display()))
}

fn sha256_hex(s: &str) -> String {
    let digest = Sha256::digest(s.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn manifest() -> Value {
    load_json(&fixtures_root().join("mock").join("manifest.json"))
}

fn classify(case_id: &str, records: &[&str]) -> (ClassifyInput, Value) {
    let root = fixtures_root();
    let case_dir = find_case_dir(&root.join("cases"), case_id).expect("case 目录");
    let case = load_case_input(&case_dir).expect("case input.json");
    let input = build_classify_input(&case, records).expect("构造规范输入");
    let runtime = MockAgentRuntime::open(&root).expect("加载 mock runtime");
    match runtime.classify(&input) {
        MockOutcome::Replay { output, .. } => (input, output),
        MockOutcome::Unknown { input_sha256 } => {
            panic!("{case_id} {records:?} 应命中夹具，实际 unknown：{input_sha256}")
        }
    }
}

#[test]
fn manifest_replays_against_case_fixtures() {
    let root = fixtures_root();
    let manifest = manifest();
    assert_eq!(manifest["schema_version"], "mock-manifest/v1");
    assert_eq!(manifest["runtime"]["name"], "mock");
    assert_eq!(manifest["runtime"]["network"], "none");

    let runtime = MockAgentRuntime::open(&root).expect("加载 mock runtime");
    let runs = manifest["runs"].as_array().expect("runs 数组");

    let mut seen: BTreeSet<String> = BTreeSet::new();
    for run in runs {
        let case_id = run["case_id"].as_str().expect("case_id");
        let records: Vec<&str> = run["records"]
            .as_array()
            .expect("records 数组")
            .iter()
            .map(|v| v.as_str().expect("记录 ID 应为字符串"))
            .collect();
        let case_dir = find_case_dir(&root.join("cases"), case_id).expect("case 目录");
        let case = load_case_input(&case_dir).expect("case input.json");
        let input = build_classify_input(&case, &records)
            .unwrap_or_else(|e| panic!("{case_id} 构造规范输入失败：{e}"));
        let sha = input_sha256(&input);
        assert_eq!(
            sha,
            run["input_sha256"].as_str().expect("input_sha256"),
            "{case_id} 从 case 重建的哈希与 manifest 不一致"
        );

        // 全链路回放：命中 + 契约 + source_ids 与批次一致
        match runtime.classify(&input) {
            MockOutcome::Replay {
                input_sha256,
                output,
            } => {
                assert_eq!(input_sha256, sha);
                validate_classify_v1(&output)
                    .unwrap_or_else(|e| panic!("{case_id} 回放输出违反契约：{e}"));
                let source_ids: Vec<&str> = output["source_ids"]
                    .as_array()
                    .expect("source_ids")
                    .iter()
                    .map(|v| v.as_str().expect("source_id"))
                    .collect();
                assert_eq!(source_ids, records, "{case_id} source_ids 应等于判读批次");
            }
            MockOutcome::Unknown { .. } => panic!("{case_id} manifest 批次必须可回放：{sha}"),
        }

        // 输入复原件：同一文档、同一规范串、同一哈希，且可用 ClassifyInput 反序列化
        let input_file = run["input_file"].as_str().expect("input_file");
        let input_doc = load_json(&root.join("mock").join(input_file));
        assert_eq!(
            input_doc,
            serde_json::to_value(&input).expect("序列化规范输入"),
            "{case_id} 输入复原件应与从 case 重建的文档一致（{input_file}）"
        );
        assert_eq!(sha256_hex(&canonical_json(&input_doc)), sha);
        serde_json::from_value::<ClassifyInput>(input_doc)
            .unwrap_or_else(|e| panic!("{input_file} 应能反序列化为 ClassifyInput：{e}"));

        // 响应文件按哈希寻址
        let response_file = run["response_file"].as_str().expect("response_file");
        assert_eq!(response_file, format!("classify/{sha}.json"));
        assert!(
            root.join("mock").join(response_file).is_file(),
            "响应文件缺失：{response_file}"
        );
        seen.insert(sha);
    }

    assert_eq!(
        seen.len(),
        9,
        "9 个唯一判读身份（A11 阶段3 与阶段1 共用同一哈希）"
    );
    assert_eq!(
        runtime.len(),
        seen.len(),
        "classify/ 目录不允许存在未被 manifest 引用的孤儿夹具"
    );
}

#[test]
fn manifest_covers_every_judge_run_of_scope_cases() {
    let root = fixtures_root();
    let manifest = manifest();
    for case_id in SCOPE_CASES {
        let case_dir = find_case_dir(&root.join("cases"), case_id).expect("case 目录");
        let expected = load_json(&case_dir.join("expected.json"));
        let expected_runs: usize = expected["stages"]
            .as_array()
            .expect("stages")
            .iter()
            .map(|s| {
                s.get("agent_runs")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len)
            })
            .sum();
        let manifest_runs = manifest["runs"]
            .as_array()
            .expect("runs")
            .iter()
            .filter(|r| r["case_id"].as_str() == Some(case_id))
            .count();
        assert_eq!(
            expected_runs, manifest_runs,
            "{case_id} 的判读批次未被 manifest 全量覆盖"
        );
    }
}

#[test]
fn unmatched_input_is_explicit_unknown() {
    let root = fixtures_root();
    let runtime = MockAgentRuntime::open(&root).expect("加载 mock runtime");
    let case_dir = find_case_dir(&root.join("cases"), "A01").expect("case 目录");
    let case = load_case_input(&case_dir).expect("case input.json");
    let mut input =
        build_classify_input(&case, &["A01-R1", "A01-R2"]).expect("构造规范输入");
    input.records[0].body.push_str("（改动后不属于任何夹具）");
    match runtime.classify(&input) {
        MockOutcome::Unknown { input_sha256 } => {
            assert!(!runtime.contains(&input_sha256));
            assert_eq!(input_sha256.len(), 64);
        }
        MockOutcome::Replay { .. } => panic!("被改动的输入不应命中夹具（必须精确寻址）"),
    }
}

#[test]
fn a11_correction_retry_replays_identically() {
    // A11 阶段3 judge_retry 与阶段1 同批记录 → 同一 input_sha256 → 同一输出；
    // 「拒绝覆盖（R_LOCKED）」由策略层执行，mock 侧保证重放不变形。
    let root = fixtures_root();
    let case_dir = find_case_dir(&root.join("cases"), "A11").expect("case 目录");
    let case = load_case_input(&case_dir).expect("case input.json");
    let first = build_classify_input(&case, &["A11-R1"]).expect("阶段1 输入");
    let retry = build_classify_input(&case, &["A11-R1"]).expect("阶段3 重试输入");
    assert_eq!(input_sha256(&first), input_sha256(&retry));

    let runtime = MockAgentRuntime::open(&root).expect("加载 mock runtime");
    match (runtime.classify(&first), runtime.classify(&retry)) {
        (
            MockOutcome::Replay {
                output: a,
                input_sha256: sha_a,
            },
            MockOutcome::Replay {
                output: b,
                input_sha256: sha_b,
            },
        ) => {
            assert_eq!(sha_a, sha_b);
            assert_eq!(a, b, "同哈希必须回放同一输出");
            assert_eq!(a["classification"], "followup");
            assert_eq!(a["belongs_to_me"], "yes", "mock 侧保持「合理误判」，纠错靠人工锁");
        }
        _ => panic!("A11 两次判读都必须命中夹具"),
    }
}

/// 语义抽检表的一行：某判读批次应产出的基础判读语义（具名字段替代六元组，
/// 避免 clippy type_complexity）。
struct CaseRow<'a> {
    id: &'a str,
    records: &'a [&'a str],
    classification: &'a str,
    belongs: &'a str,
    needs_action: bool,
    turn: &'a str,
}

#[test]
fn fixture_responses_carry_expected_judgement_semantics() {
    // 锁定每个阶段 mock 夹具承载的判读意图（完整业务断言由回放器按
    // expected.json 执行，见 fixtures/mock/manifest.json 的 judgement 摘要）。
    let cases = [
        CaseRow {
            id: "A01",
            records: &["A01-R1", "A01-R2"],
            classification: "followup",
            belongs: "yes",
            needs_action: true,
            turn: "me",
        },
        CaseRow {
            id: "A01",
            records: &["A01-R3"],
            classification: "followup",
            belongs: "yes",
            needs_action: true,
            turn: "other",
        },
        CaseRow {
            id: "A01",
            records: &["A01-R4"],
            classification: "followup",
            belongs: "yes",
            needs_action: false,
            turn: "other",
        },
        CaseRow {
            id: "A01",
            records: &["A01-R5"],
            classification: "followup",
            belongs: "yes",
            needs_action: false,
            turn: "none",
        },
        CaseRow {
            id: "A02",
            records: &["A02-R1", "A02-R2"],
            classification: "followup",
            belongs: "yes",
            needs_action: true,
            turn: "me",
        },
        CaseRow {
            id: "A03",
            records: &["A03-R1", "A03-R2"],
            classification: "one_off",
            belongs: "yes",
            needs_action: false,
            turn: "none",
        },
        CaseRow {
            id: "A06",
            records: &["A06-R2"],
            classification: "info",
            belongs: "no",
            needs_action: false,
            turn: "none",
        },
        CaseRow {
            id: "A06",
            records: &["A06-R3"],
            classification: "followup",
            belongs: "yes",
            needs_action: true,
            turn: "me",
        },
        CaseRow {
            id: "A11",
            records: &["A11-R1"],
            classification: "followup",
            belongs: "yes",
            needs_action: true,
            turn: "me",
        },
    ];
    for case in &cases {
        let (_, output) = classify(case.id, case.records);
        assert_eq!(
            output["classification"], case.classification,
            "{} {:?}",
            case.id, case.records
        );
        assert_eq!(
            output["belongs_to_me"], case.belongs,
            "{} {:?}",
            case.id, case.records
        );
        assert_eq!(
            output["needs_action"], case.needs_action,
            "{} {:?}",
            case.id, case.records
        );
        assert_eq!(
            output["current_turn"], case.turn,
            "{} {:?}",
            case.id, case.records
        );
        // 全部首发用例都不得凭空造日期（V04）：followup.type=none 且 at=null
        assert_eq!(
            output["followup"]["type"], "none",
            "{} {:?}",
            case.id, case.records
        );
        assert!(
            output["followup"]["at"].is_null(),
            "{} {:?}",
            case.id,
            case.records
        );
        let confidence = output["confidence"].as_i64().expect("confidence");
        assert!((0..=100).contains(&confidence));
    }

    // resolution：出现即提议收尾。除 A01 阶段3/4 外，任何批次不得携带 resolution。
    for (case_id, records) in [
        ("A01", &["A01-R1", "A01-R2"][..]),
        ("A01", &["A01-R3"][..]),
        ("A02", &["A02-R1", "A02-R2"][..]),
        ("A03", &["A03-R1", "A03-R2"][..]),
        ("A06", &["A06-R2"][..]),
        ("A06", &["A06-R3"][..]),
        ("A11", &["A11-R1"][..]),
    ] {
        let (_, output) = classify(case_id, records);
        assert!(
            output.get("resolution").is_none(),
            "{case_id} {records:?} 不应提议收尾（无实际收尾证据）"
        );
    }

    // A01 阶段3：本人完成声明并提议收尾 → explicit_complete（白名单提议不关单：
    // 保持 active + awaiting_acceptance + awaiting_confirmation=false；关单走
    // 阶段5 human_confirmed）。
    let (_, a01_report) = classify("A01", &["A01-R4"]);
    let s3_resolution = a01_report["resolution"]
        .as_object()
        .expect("A01 阶段3 应带 resolution 提议收尾");
    assert_eq!(
        s3_resolution["evidence_type"], "explicit_complete",
        "本人完成声明按实际证据类型标为 explicit_complete"
    );
    let s3_evidence_ids: Vec<&str> = s3_resolution["evidence_ids"]
        .as_array()
        .expect("evidence_ids")
        .iter()
        .map(|v| v.as_str().expect("证据 ID"))
        .collect();
    assert_eq!(s3_evidence_ids, vec!["A01-R4"]);

    // A01 阶段4：交办人仅回「好的」，按实际证据类型标注 ack_only（V05 据此降级）。
    let (_, a01_ack) = classify("A01", &["A01-R5"]);
    let resolution = a01_ack["resolution"]
        .as_object()
        .expect("A01 阶段4 应带 resolution 提议收尾");
    assert_eq!(
        resolution["evidence_type"], "ack_only",
        "「好的」只能标为 ack_only，不得伪装成更强证据"
    );
    let evidence_ids: Vec<&str> = resolution["evidence_ids"]
        .as_array()
        .expect("evidence_ids")
        .iter()
        .map(|v| v.as_str().expect("证据 ID"))
        .collect();
    assert_eq!(evidence_ids, vec!["A01-R5"]);
    assert!(
        !a01_ack["uncertainties"]
            .as_array()
            .expect("uncertainties")
            .is_empty(),
        "ack_only 提议收尾必须自带不确定性说明（V05 收尾门线索）"
    );
    let (_, a06_trap) = classify("A06", &["A06-R3"]);
    assert_eq!(a06_trap["belongs_to_me"], "yes", "故意误判样本：留待 V03 降级");
    assert!(
        !a06_trap["uncertainties"]
            .as_array()
            .expect("uncertainties")
            .is_empty()
    );
}

#[test]
fn schema_file_matches_rust_contract_constants() {
    let schema = load_json(&fixtures_root().join("schema").join("classify_v1.json"));
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(
        schema["properties"]["schema_version"]["const"],
        workcontext_infra::contract::OUTPUT_SCHEMA_VERSION
    );

    let required: Vec<&str> = schema["required"]
        .as_array()
        .expect("required")
        .iter()
        .map(|v| v.as_str().expect("required 项"))
        .collect();
    for key in REQUIRED_KEYS {
        assert!(required.contains(&key), "schema required 缺少 {key}");
    }
    assert!(
        !required.contains(&"resolution"),
        "resolution 是可选字段（出现即提议收尾），不得列入 required"
    );
    let properties = schema["properties"].as_object().expect("properties");
    assert_eq!(properties.len(), ALLOWED_KEYS.len());

    let enum_of = |field: &[&str]| -> Vec<String> {
        let mut node = &schema;
        for seg in field {
            node = &node["properties"][seg];
        }
        node["enum"]
            .as_array()
            .unwrap_or_else(|| panic!("{field:?} 缺 enum"))
            .iter()
            .map(|v| v.as_str().expect("enum 项").to_string())
            .collect()
    };
    assert_eq!(enum_of(&["classification"]), CLASSIFICATIONS.to_vec());
    assert_eq!(enum_of(&["belongs_to_me"]), BELONGS_TO_ME.to_vec());
    assert_eq!(enum_of(&["current_turn"]), CURRENT_TURNS.to_vec());
    assert_eq!(enum_of(&["followup", "type"]), FOLLOWUP_TYPES.to_vec());
    let resolution_enum = enum_of(&["resolution", "evidence_type"]);
    assert_eq!(resolution_enum, RESOLUTION_EVIDENCE_TYPES.to_vec());
    assert!(
        !resolution_enum.contains(&"human_confirmed".to_string()),
        "human_confirmed 只能由人工操作产生，模型输出一律拒绝"
    );
}

#[test]
fn every_response_file_is_valid_and_referenced() {
    let root = fixtures_root();
    let runtime = MockAgentRuntime::open(&root).expect("加载 mock runtime");
    let manifest = manifest();
    let referenced: BTreeSet<String> = manifest["runs"]
        .as_array()
        .expect("runs")
        .iter()
        .map(|r| r["input_sha256"].as_str().expect("input_sha256").to_string())
        .collect();

    let classify_dir = root.join("mock").join("classify");
    let mut files: Vec<String> = std::fs::read_dir(&classify_dir)
        .expect("读取 classify 目录")
        .filter_map(|e| {
            let name = e.ok()?.file_name().into_string().ok()?;
            name.ends_with(".json").then_some(name)
        })
        .collect();
    files.sort();
    assert_eq!(files.len(), referenced.len(), "夹具文件数应等于被引用哈希数");
    for name in &files {
        let sha = name.trim_end_matches(".json");
        assert!(
            referenced.contains(sha),
            "孤儿夹具（manifest 未引用）：{name}"
        );
        let output = runtime.lookup(sha).expect("runtime 应已加载该夹具");
        validate_classify_v1(output).expect("夹具须满足 classify/v1 契约");
        if let Some(resolution) = output.get("resolution") {
            assert_ne!(
                resolution["evidence_type"], "human_confirmed",
                "{name}：模型输出不得声明人工确认"
            );
        }
    }
}
