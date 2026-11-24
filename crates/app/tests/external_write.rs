use std::{sync::Barrier, thread};
use workcontext_app::{context::{records, RecordsArgs}, external_write::{add_record, AddRecord, WriteError}};
use workcontext_store::{open, rusqlite::Connection};

const REPORT: &str = r#"{"producer":"agent","requestId":"fixed","title":"色差复核","result":"导出颜色一致",
    "inference":"预览可能使用了旧色板","openQuestions":["触发条件？"],"materials":[{"path":"Z:/absent/proof.md","summary":"复测"}],
    "scopeHints":["未建档名称"]}"#;
fn report() -> AddRecord { serde_json::from_str(REPORT).unwrap() }

struct TestDirectory(std::path::PathBuf);
impl TestDirectory {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("wc-append-{label}-{}-{}", std::process::id(), workcontext_store::now_ms()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for TestDirectory {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

#[test]
fn lost_receipt_concurrent_retry_and_payload_conflict() {
    let directory = TestDirectory::new("retry");
    let path = directory.0.join("work.db");
    let conn = open(&path).unwrap();
    let mut a = Connection::open(&path).unwrap();
    let mut b = Connection::open(&path).unwrap();
    let barrier = Barrier::new(2);
    let (first, second) = thread::scope(|s| {
        let worker = s.spawn(|| { barrier.wait(); add_record(&mut a, "acct", &report()).unwrap() });
        barrier.wait();
        let second = add_record(&mut b, "acct", &report()).unwrap();
        (worker.join().unwrap(), second)
    });
    assert_eq!(first.record_id, second.record_id);
    assert_eq!(first.input_id, second.input_id);
    assert_eq!(second.processing_state, "pending");
    let mut changed = report();
    changed.materials[0].summary = Some("another report".into());
    assert!(matches!(add_record(&mut b, "acct", &changed), Err(WriteError::Conflict)));
    let found = records(&conn, "acct", &RecordsArgs { query: Some("色板".into()), ..Default::default() }).unwrap();
    assert_eq!(found.total, 1);
    assert_eq!(found.items[0].id, first.record_id);
    assert!(found.items[0].scopes.is_empty());
    let (result, inference, questions, summary): (String,String,String,String) = conn.query_row(
        "SELECT reported_result,inference,open_questions,summary FROM context_record r JOIN record_ref f ON f.record_id=r.id",
        [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
    assert_eq!((result.as_str(),inference.as_str(),questions.as_str(),summary.as_str()), ("导出颜色一致","预览可能使用了旧色板","[\"触发条件？\"]","复测"));
}

#[test]
fn foreign_reference_rolls_back_whole_request_and_cannot_claim_human_authority() {
    let directory = TestDirectory::new("rollback");
    let mut conn = open(&directory.0.join("work.db")).unwrap();
    let previous = add_record(&mut conn,"a",&report()).unwrap();
    let mut request = report();
    request.related_record_ids.push(previous.record_id);
    assert!(matches!(add_record(&mut conn,"b",&request),Err(WriteError::UnknownReference)));
    request.related_record_ids.clear();
    request.scope_ids.push("unknown".into());
    assert!(matches!(add_record(&mut conn,"b",&request),Err(WriteError::UnknownReference)));
    assert_eq!(conn.query_row("SELECT count(*) FROM context_input WHERE profile='b'",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    request.scope_ids.clear();
    add_record(&mut conn,"b",&request).unwrap();
    let mut forged: serde_json::Value = serde_json::from_str(REPORT).unwrap();
    forged["actor"] = "human".into();
    assert!(serde_json::from_value::<AddRecord>(forged).is_err());
}
