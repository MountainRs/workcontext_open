//! SyncEngine × MockSource 的离线集成测试（无网络，遗留事项 B3 / M3 验收）。
//!
//! 覆盖 §6.2 硬规则与 M3 门槛：
//! 1. 重复同步：同一窗口两轮 → 第二轮 `inserted=0, unchanged=N`，记录不重建；
//! 2. 中断回填：分页中途超时 → 已完成页入库、游标不动；断线期间来源新增
//!    消息后重拉 → 旧记录去重、新回复回填，游标一次推到位；
//! 3. 鉴权失败：401 → `auth_required`，游标不动、失败计数不动、自动重试被
//!    拒绝（来源未被触碰），`reset` 后恢复；
//! 4. 连续超时：5 次失败 → `degraded`，引擎拒绝自动重试，`reset` 恢复；
//! 5. 不完整页/游标停滞：截断无安全续点、游标原地踏步 → 整轮失败、游标
//!    不冒进、可疑页不落库；
//! 6. 空页有后续 → 继续；单轮上限 → `partial`，游标不动，下轮续传；
//! 7. 白名单：范围外记录不落库；空白名单 fail closed。

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use workcontext_infra::sync::{
    backoff_delay_ms, CheckpointState, ConversationType, DingTalkSource, Direction, MockSource,
    PageParams, RawRecord, RecordPage, Scope, SourceError, SyncConfig, SyncEngine, SyncStatus,
};
use workcontext_store::rusqlite::Connection;

// ---------------------------------------------------------------------------
// 脚手架
// ---------------------------------------------------------------------------

/// 独占的临时数据库目录，Drop 时整体清理（含 WAL/SHM 副本）。
struct TempDb {
    dir: PathBuf,
}

impl TempDb {
    fn new(tag: &str) -> Self {
        let unique = format!(
            "wctx-sync-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        Self { dir }
    }

    fn open(&self) -> Connection {
        workcontext_store::open(&self.dir.join("work.sqlite")).expect("打开工作库")
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 会话 `cid` + 人 `p1` 的显式白名单。
fn scope() -> Scope {
    Scope {
        conversations: BTreeSet::from(["cid".to_string()]),
        people: BTreeSet::from(["p1".to_string()]),
    }
}

fn rec(id: &str, at: i64, body: &str) -> RawRecord {
    RawRecord::im(id, at, "cid", "p1", Direction::In, body)
}

fn cursor(at: i64) -> Option<Value> {
    Some(json!({ "since": at }))
}

fn page(records: Vec<RawRecord>, next_cursor: Option<Value>, has_more: bool, snapshot_at: i64) -> RecordPage {
    RecordPage { records, next_cursor, has_more, snapshot_at }
}

fn timeout() -> SourceError {
    SourceError::Timeout { message: "模拟 DWS 失联".into() }
}

fn auth() -> SourceError {
    SourceError::Auth { status: 401, message: "token 失效".into() }
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).expect("计数查询")
}

fn record_count(conn: &Connection) -> i64 {
    count(conn, "SELECT COUNT(*) FROM source_record")
}

fn run_count(conn: &Connection, state: &str) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM sync_run WHERE state = ?1", [state], |row| {
        row.get(0)
    })
    .expect("run 计数")
}

/// 注册好的引擎 + 可续写脚本的来源。
fn setup(tag: &str, config: SyncConfig) -> (TempDb, SyncEngine, Arc<MockSource>) {
    let db = TempDb::new(tag);
    let source = Arc::new(MockSource::new("dws-im-1", scope()));
    let engine = SyncEngine::new(db.open(), config);
    engine
        .register(Arc::clone(&source) as Arc<dyn DingTalkSource>, "钉钉 IM（mock）", 1_000)
        .expect("注册来源");
    (db, engine, source)
}

const T0: i64 = 1_790_000_000_000; // 任意固定基准时刻（ms）

// ---------------------------------------------------------------------------
// 1. 重复同步不重建记录（M3 门槛 5）
// ---------------------------------------------------------------------------

#[test]
fn repeat_sync_is_idempotent_and_whitelist_filters_first() {
    let (_db, engine, source) = setup("repeat", SyncConfig::default());
    let mk_pages = |s: &MockSource| {
        // 页 1：3 条白名单内 + 1 条范围外 + 1 条靠「人」维度放行的私聊；
        // 页 2：1 条收尾。
        s.push_page(page(
            vec![
                rec("m1", T0, "周三前给我初稿"),
                rec("m2", T0 + 1, "好的"),
                RawRecord::im("m3", T0 + 2, "other-conv", "stranger", Direction::In, "与我无关"),
                RawRecord::im("m4", T0 + 3, "dm-x", "p1", Direction::Out, "我的回复"),
            ],
            cursor(T0 + 100),
            true,
            T0 + 100,
        ));
        s.push_page(page(vec![rec("m5", T0 + 4, "收到")], cursor(T0 + 200), false, T0 + 200));
    };

    // 第一轮走 sync_all：单源注册序遍历。
    mk_pages(&source);
    let outcomes = engine.sync_all(T0 + 300).expect("第一轮");
    assert_eq!(outcomes.len(), 1);
    let first = &outcomes[0];
    assert_eq!(first.status, SyncStatus::Ok);
    assert!(first.cursor_advanced);
    assert_eq!((first.inserted, first.unchanged, first.out_of_scope, first.fetched), (4, 0, 1, 5));

    // 已入库 4 条（范围外 1 条不落库）；复核走独立只读连接（引擎连接仍在，
    // WAL 下可并发读同一文件）。
    let ro = workcontext_store::open(&_db.dir.join("work.sqlite")).expect("只读复核连接");
    assert_eq!(record_count(&ro), 4);
    drop(ro);

    // 同一窗口再拉一遍：全部 unchanged，记录数不变，游标仍推进。
    mk_pages(&source);
    let second = engine.sync_source("dws-im-1", T0 + 400).expect("第二轮");
    assert_eq!(second.status, SyncStatus::Ok);
    assert_eq!((second.inserted, second.unchanged, second.updated, second.out_of_scope), (0, 4, 0, 1));
    assert_eq!(second.pages, 2);

    let cp = engine.checkpoint("dws-im-1").expect("检查点").expect("已注册");
    assert_eq!(cp.state, CheckpointState::Idle);
    assert_eq!(cp.cursor_json.as_deref(), Some(r#"{"since":1790000000200}"#));
    assert_eq!(cp.cursor_at, Some(T0 + 200));
    assert_eq!(cp.last_success_at, Some(T0 + 200));
    assert_eq!(cp.consecutive_failures, 0);

    // 范围外消息绝不入库（未经授权的数据不进工作库）。
    let ro = workcontext_store::open(&_db.dir.join("work.sqlite")).unwrap();
    let out: i64 = ro
        .query_row("SELECT COUNT(*) FROM source_record WHERE external_id = 'm3'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(out, 0);
    assert_eq!(record_count(&ro), 4);
    // 两轮 run 均落库为 ok，第二轮计数 inserted=0/unchanged=4。
    assert_eq!(run_count(&ro, "ok"), 2);
    let (ins, unch): (i64, i64) = ro
        .query_row(
            "SELECT inserted, unchanged FROM sync_run WHERE state='ok' ORDER BY started_at DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((ins, unch), (0, 4));
}

// ---------------------------------------------------------------------------
// 2. 中断回填：中途超时 → 游标不动；重拉吸收重复并回填断线期间的回复
// ---------------------------------------------------------------------------

#[test]
fn interrupted_sync_backfills_from_stale_cursor() {
    let (_db, engine, source) = setup("backfill", SyncConfig::default());

    // 第一轮：页 1 成功，页 2 超时（DWS 失联）。
    source.push_page(page(
        vec![rec("a1", T0, "周报今天发"), rec("a2", T0 + 1, "收到")],
        cursor(T0 + 100),
        true,
        T0 + 100,
    ));
    source.push_error(timeout());

    let run1 = engine.sync_source("dws-im-1", T0 + 200).expect("第一轮");
    assert_eq!(run1.status, SyncStatus::Failed);
    assert!(!run1.cursor_advanced);
    assert_eq!(run1.error.as_ref().unwrap().code, "timeout");
    assert_eq!(run1.inserted, 2);

    // 已完成页落库；游标没动（首轮无游标 → 仍为 NULL），失败计数 = 1。
    let ro = workcontext_store::open(&_db.dir.join("work.sqlite")).unwrap();
    assert_eq!(record_count(&ro), 2);
    let cp = engine.checkpoint("dws-im-1").unwrap().unwrap();
    assert_eq!(cp.cursor_json, None);
    assert_eq!(cp.state, CheckpointState::Idle);
    assert_eq!(cp.consecutive_failures, 1);
    assert_eq!(cp.last_success_at, None);
    assert_eq!(run_count(&ro, "failed"), 1);
    drop(ro);

    // 断线期间来源新增 a3；恢复后从旧游标（None）重拉：旧记录 + 新回复。
    source.push_page(page(
        vec![
            rec("a1", T0, "周报今天发"),
            rec("a2", T0 + 1, "收到"),
            rec("a3", T0 + 500, "断线期间的回复"),
        ],
        cursor(T0 + 600),
        true,
        T0 + 600,
    ));
    source.push_page(page(vec![rec("a4", T0 + 700, "补充")], cursor(T0 + 800), false, T0 + 800));

    let run2 = engine.sync_source("dws-im-1", T0 + 900).expect("第二轮");
    assert_eq!(run2.status, SyncStatus::Ok);
    assert_eq!((run2.inserted, run2.unchanged), (2, 2)); // a3/a4 新增，a1/a2 去重

    // 重拉确实从旧游标出发：本轮第一次 fetch 收到的仍是 None。
    let log = source.fetch_log();
    assert_eq!(log[log.len() - 2].as_deref(), None);

    let cp = engine.checkpoint("dws-im-1").unwrap().unwrap();
    assert_eq!(cp.cursor_json.as_deref(), Some(r#"{"since":1790000000800}"#));
    assert_eq!(cp.last_success_at, Some(T0 + 800));
    assert_eq!(cp.consecutive_failures, 0); // 成功清零（§6.2 规则 1）
    let ro = workcontext_store::open(&_db.dir.join("work.sqlite")).unwrap();
    assert_eq!(record_count(&ro), 4);
    // 成功轮留下 synced 事件（事件流是唯一历史真相）。
    let synced: i64 = ro
        .query_row(
            "SELECT COUNT(*) FROM work_event WHERE target_type='source' AND event_type='synced'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(synced, 1);
}

// ---------------------------------------------------------------------------
// 3. 鉴权失败：auth_required、游标不动、不重试、reset 恢复
// ---------------------------------------------------------------------------

#[test]
fn auth_failure_keeps_cursor_and_requires_manual_reset() {
    let (_db, engine, source) = setup("auth", SyncConfig::default());

    // 先成功一轮，把游标推到 T0+200。
    source.push_page(page(vec![rec("b1", T0, "内容")], cursor(T0 + 200), false, T0 + 200));
    let ok = engine.sync_source("dws-im-1", T0 + 300).expect("首轮");
    assert_eq!(ok.status, SyncStatus::Ok);

    // 401：auth_required，不累计失败、不清游标。
    source.push_error(auth());
    let run = engine.sync_source("dws-im-1", T0 + 400).expect("401 轮");
    assert_eq!(run.status, SyncStatus::AuthRequired);
    assert!(!run.cursor_advanced);
    assert_eq!(run.error.as_ref().unwrap().code, "auth");
    let cp = engine.checkpoint("dws-im-1").unwrap().unwrap();
    assert_eq!(cp.state, CheckpointState::AuthRequired);
    assert_eq!(cp.cursor_json.as_deref(), Some(r#"{"since":1790000000200}"#));
    assert_eq!(cp.consecutive_failures, 0);
    assert_eq!(cp.last_success_at, Some(T0 + 200)); // 最后成功时间保留（A13 展示）
    let ro = workcontext_store::open(&_db.dir.join("work.sqlite")).unwrap();
    assert_eq!(run_count(&ro, "failed"), 1);
    drop(ro);

    // 自动重试被拒绝：来源脚本未被触碰。
    source.push_error(auth());
    let again = engine.sync_source("dws-im-1", T0 + 500).expect("拒绝轮");
    assert_eq!(again.status, SyncStatus::AuthRequired);
    assert_eq!(again.run_id, None); // 没开新 run
    assert_eq!(source.remaining(), 1);

    // reset（用户重新授权后的手动入口）→ 恢复同步，游标沿用。
    engine.reset("dws-im-1", T0 + 600).expect("reset");
    let cp = engine.checkpoint("dws-im-1").unwrap().unwrap();
    assert_eq!(cp.state, CheckpointState::Idle);
    assert_eq!(cp.consecutive_failures, 0);
    let resumed = engine.sync_source("dws-im-1", T0 + 700).expect("恢复轮");
    // 脚本里剩下的那个 401 被消费：恢复后第一次拉取仍可能撞鉴权失败。
    assert_eq!(resumed.status, SyncStatus::AuthRequired);
    assert_eq!(source.remaining(), 0);

    engine.reset("dws-im-1", T0 + 800).unwrap();
    source.push_page(page(vec![rec("b2", T0 + 900, "新内容")], cursor(T0 + 950), false, T0 + 950));
    let ok = engine.sync_source("dws-im-1", T0 + 999).expect("再恢复");
    assert_eq!(ok.status, SyncStatus::Ok);
    assert_eq!(ok.inserted, 1);
}

// ---------------------------------------------------------------------------
// 4. 连续超时降级：5 次失败 → degraded，拒绝自动重试，reset 恢复
// ---------------------------------------------------------------------------

#[test]
fn five_consecutive_failures_degrade_and_stop_retrying() {
    let (_db, engine, source) = setup("degrade", SyncConfig::default());

    for i in 1..=5 {
        source.push_error(timeout());
        let run = engine.sync_source("dws-im-1", T0 + i as i64 * 100).expect("第 {i} 轮");
        assert_eq!(run.status, SyncStatus::Failed, "第 {i} 轮应为可重试失败");
        let cp = engine.checkpoint("dws-im-1").unwrap().unwrap();
        assert_eq!(cp.consecutive_failures, i as u32);
        let expected_state = if i < 5 { CheckpointState::Idle } else { CheckpointState::Degraded };
        assert_eq!(cp.state, expected_state, "第 {i} 轮后的状态");
    }

    // 降级后自动同步被拒绝：不开 run、不触来源。
    source.push_error(timeout());
    let refused = engine.sync_source("dws-im-1", T0 + 1_000).expect("拒绝轮");
    assert_eq!(refused.status, SyncStatus::Degraded);
    assert_eq!(refused.run_id, None);
    assert_eq!(source.remaining(), 1);
    assert_eq!(source.fetch_log().len(), 5);

    // 退避序列供调度方使用：第 5 次失败后 → 300s（±20%）。
    assert_eq!(backoff_delay_ms(5, 0.5), 300_000);

    // 手动重试恢复：清零失败，下一轮正常执行（脚本里剩的 timeout 被消费，
    // failures 重新从 1 计）。
    engine.reset("dws-im-1", T0 + 2_000).expect("reset");
    let run = engine.sync_source("dws-im-1", T0 + 2_100).expect("恢复轮");
    assert_eq!(run.status, SyncStatus::Failed);
    let cp = engine.checkpoint("dws-im-1").unwrap().unwrap();
    assert_eq!(cp.state, CheckpointState::Idle);
    assert_eq!(cp.consecutive_failures, 1);
    assert_eq!(source.remaining(), 0);
}

// ---------------------------------------------------------------------------
// 5. 不完整页 / 游标停滞：不冒进
// ---------------------------------------------------------------------------

#[test]
fn incomplete_page_fails_without_advancing_or_persisting() {
    let (_db, engine, source) = setup("incomplete", SyncConfig::default());
    // has_more=true 但没有 next_cursor：截断、无安全续点。
    source.push_page(page(vec![rec("c1", T0, "可疑页")], None, true, T0 + 100));

    let run = engine.sync_source("dws-im-1", T0 + 200).expect("截断轮");
    assert_eq!(run.status, SyncStatus::Failed);
    assert_eq!(run.error.as_ref().unwrap().code, "incomplete_page");
    assert!(!run.cursor_advanced);

    // 可疑页整页不落库；游标不动；计入失败。
    let ro = workcontext_store::open(&_db.dir.join("work.sqlite")).unwrap();
    assert_eq!(record_count(&ro), 0);
    let cp = engine.checkpoint("dws-im-1").unwrap().unwrap();
    assert_eq!(cp.cursor_json, None);
    assert_eq!(cp.consecutive_failures, 1);
}

#[test]
fn stalled_cursor_fails_without_advancing() {
    let (_db, engine, source) = setup("stall", SyncConfig::default());
    // 页 1 正常给一个游标 X；页 2 对同样的 X 原地踏步 → 来源无进展。
    let x = cursor(T0 + 100);
    source.push_page(page(vec![rec("d1", T0, "第一页")], x.clone(), true, T0 + 100));
    source.push_page(page(vec![rec("d2", T0 + 101, "踏步页")], x.clone(), true, T0 + 200));

    let run = engine.sync_source("dws-im-1", T0 + 300).expect("停滞轮");
    assert_eq!(run.status, SyncStatus::Failed);
    assert_eq!(run.error.as_ref().unwrap().code, "cursor_stalled");
    assert_eq!(run.pages, 2);

    // 游标绝不推进到 X；停滞页不落库（形状校验先于入库）。
    let cp = engine.checkpoint("dws-im-1").unwrap().unwrap();
    assert_eq!(cp.cursor_json, None);
    let ro = workcontext_store::open(&_db.dir.join("work.sqlite")).unwrap();
    assert_eq!(record_count(&ro), 1); // 只有第一页的 d1
}

// ---------------------------------------------------------------------------
// 6. 空页有后续 → 继续；单轮上限 → partial 续传
// ---------------------------------------------------------------------------

#[test]
fn empty_page_with_more_continues_to_terminal_page() {
    let (_db, engine, source) = setup("empty-more", SyncConfig::default());
    // 空页但 has_more=true 且给出了新游标：继续，不算失败。
    source.push_page(page(vec![], cursor(T0 + 50), true, T0 + 50));
    source.push_page(page(vec![rec("e1", T0 + 60, "迟到的一页")], cursor(T0 + 70), false, T0 + 70));

    let run = engine.sync_source("dws-im-1", T0 + 100).expect("空页轮");
    assert_eq!(run.status, SyncStatus::Ok);
    assert_eq!(run.pages, 2);
    assert_eq!(run.inserted, 1);
    let cp = engine.checkpoint("dws-im-1").unwrap().unwrap();
    assert_eq!(cp.cursor_at, Some(T0 + 70));
}

#[test]
fn page_or_time_cap_ends_partial_and_next_run_resumes() {
    let (_db, engine1, source) = setup(
        "partial",
        SyncConfig {
            max_pages: 1,
            ..SyncConfig::default()
        },
    );
    source.push_page(page(
        vec![rec("f1", T0, "第一页"), rec("f2", T0 + 1, "第一页 2")],
        cursor(T0 + 100),
        true,
        T0 + 100,
    ));
    let run1 = engine1.sync_source("dws-im-1", T0 + 150).expect("页数超限轮");
    assert_eq!(run1.status, SyncStatus::Partial);
    assert!(!run1.cursor_advanced);
    assert_eq!(run1.inserted, 2);

    let cp = engine1.checkpoint("dws-im-1").unwrap().unwrap();
    assert_eq!(cp.cursor_json, None); // 游标不动，下轮从旧游标重拉
    assert_eq!(cp.state, CheckpointState::Idle);
    assert_eq!(cp.consecutive_failures, 0); // partial 不是失败
    let ro = workcontext_store::open(&_db.dir.join("work.sqlite")).unwrap();
    assert_eq!(run_count(&ro, "partial"), 1);
    assert_eq!(record_count(&ro), 2); // 已入库的页保留
    drop(ro);
    drop(engine1);

    // 时长上限同样以 partial 收尾（max_duration=0：完成一页即超）。
    let engine2 = SyncEngine::new(
        workcontext_store::open(&_db.dir.join("work.sqlite")).unwrap(),
        SyncConfig {
            max_duration: Duration::ZERO,
            ..SyncConfig::default()
        },
    );
    // 重建的引擎只持有内存注册表：重新 register（幂等，不动检查点）。
    engine2
        .register(Arc::clone(&source) as Arc<dyn DingTalkSource>, "钉钉 IM（mock）", T0 + 200)
        .expect("engine2 注册来源");
    source.push_page(page(
        vec![rec("f1", T0, "第一页"), rec("f2", T0 + 1, "第一页 2")],
        cursor(T0 + 100),
        true,
        T0 + 100,
    ));
    let run2 = engine2.sync_source("dws-im-1", T0 + 200).expect("时长超限轮");
    assert_eq!(run2.status, SyncStatus::Partial);
    assert_eq!(run2.pages, 1);
    drop(engine2);

    // 恢复默认上限的下一轮：从旧游标重拉吸收重复，把尾部页拉完。
    let engine3 = SyncEngine::new(
        workcontext_store::open(&_db.dir.join("work.sqlite")).unwrap(),
        SyncConfig::default(),
    );
    engine3
        .register(Arc::clone(&source) as Arc<dyn DingTalkSource>, "钉钉 IM（mock）", T0 + 500)
        .expect("engine3 注册来源");
    source.push_page(page(
        vec![rec("f1", T0, "第一页"), rec("f2", T0 + 1, "第一页 2")],
        cursor(T0 + 100),
        true,
        T0 + 100,
    ));
    source.push_page(page(vec![rec("f3", T0 + 300, "尾页")], cursor(T0 + 400), false, T0 + 400));
    let run3 = engine3.sync_source("dws-im-1", T0 + 500).expect("续传轮");
    assert_eq!(run3.status, SyncStatus::Ok);
    assert_eq!((run3.inserted, run3.unchanged), (1, 2));
    let ro = workcontext_store::open(&_db.dir.join("work.sqlite")).unwrap();
    assert_eq!(record_count(&ro), 3);
}

// ---------------------------------------------------------------------------
// 7. 内容变化：同键不同文 → 更新而不是重复入库（§4.4）
// ---------------------------------------------------------------------------

#[test]
fn changed_content_updates_row_instead_of_duplicating() {
    let (_db, engine, source) = setup("update", SyncConfig::default());
    source.push_page(page(vec![rec("g1", T0, "原始正文")], cursor(T0 + 100), false, T0 + 100));
    let run1 = engine.sync_source("dws-im-1", T0 + 150).expect("首轮");
    assert_eq!(run1.inserted, 1);

    source.push_page(page(vec![rec("g1", T0, "编辑后的正文")], cursor(T0 + 200), false, T0 + 200));
    let run2 = engine.sync_source("dws-im-1", T0 + 250).expect("更新轮");
    assert_eq!(run2.status, SyncStatus::Ok);
    assert_eq!((run2.inserted, run2.unchanged, run2.updated), (0, 0, 1));

    let ro = workcontext_store::open(&_db.dir.join("work.sqlite")).unwrap();
    assert_eq!(record_count(&ro), 1);
    let (body, hash): (Option<String>, String) = ro
        .query_row(
            "SELECT body, content_hash FROM source_record WHERE external_id='g1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(body.as_deref(), Some("编辑后的正文"));
    assert_eq!(hash, rec("g1", T0, "编辑后的正文").content_hash());
}

// ---------------------------------------------------------------------------
// 8. 多源隔离与停用拒绝
// ---------------------------------------------------------------------------

#[test]
fn sources_are_independent_and_disabled_source_is_refused() {
    let db = TempDb::new("multi");
    let good = Arc::new(MockSource::new("src-good", scope()));
    let dead = Arc::new(MockSource::new("src-dead", scope()));
    let engine = SyncEngine::new(db.open(), SyncConfig::default());
    engine
        .register(Arc::clone(&good) as Arc<dyn DingTalkSource>, "好源", T0)
        .unwrap();
    engine
        .register(Arc::clone(&dead) as Arc<dyn DingTalkSource>, "坏源", T0)
        .unwrap();

    good.push_page(page(vec![rec("h1", T0, "内容")], cursor(T0 + 10), false, T0 + 10));
    dead.push_error(timeout());

    let outcomes = engine.sync_all(T0 + 100).unwrap();
    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0].source_id, "src-dead");
    assert_eq!(outcomes[0].status, SyncStatus::Failed);
    assert_eq!(outcomes[1].source_id, "src-good");
    assert_eq!(outcomes[1].status, SyncStatus::Ok);

    // 坏源独立累计失败，好源不受牵连。
    let cp_dead = engine.checkpoint("src-dead").unwrap().unwrap();
    assert_eq!(cp_dead.consecutive_failures, 1);
    let cp_good = engine.checkpoint("src-good").unwrap().unwrap();
    assert_eq!(cp_good.consecutive_failures, 0);
    assert_eq!(cp_good.last_success_at, Some(T0 + 10));

    // app 层把来源停用后（state='disabled'）：引擎拒绝，不触来源。
    let ro = workcontext_store::open(&db.dir.join("work.sqlite")).unwrap();
    ro.execute("UPDATE sync_checkpoint SET state='disabled' WHERE source_id='src-good'", [])
        .unwrap();
    drop(ro);
    good.push_page(page(vec![rec("h2", T0 + 20, "不该被拉")], cursor(T0 + 30), false, T0 + 30));
    let refused = engine.sync_source("src-good", T0 + 200).unwrap();
    assert_eq!(refused.status, SyncStatus::Disabled);
    assert_eq!(refused.run_id, None);
    assert_eq!(good.remaining(), 1);
}

// ---------------------------------------------------------------------------
// 9. 引擎级边界
// ---------------------------------------------------------------------------

#[test]
fn unknown_source_is_rejected_without_touching_db() {
    let db = TempDb::new("unknown");
    let engine = SyncEngine::new(db.open(), SyncConfig::default());
    let err = engine.sync_source("nope", T0).unwrap_err();
    assert!(matches!(err, workcontext_infra::sync::SyncError::UnknownSource(_)));
}

#[test]
fn page_params_carry_design_defaults() {
    // 引擎把 §6.1 的页参数（size=100、overlap=6h）原样交给来源。
    let (_db, engine, source) = setup("params", SyncConfig::default());
    source.push_page(page(vec![], None, false, T0));
    engine.sync_source("dws-im-1", T0 + 1).unwrap();

    // 白名单在 fetch 前已传递给来源（每次 fetch 都带上，来源可据此约束拉取）；
    // 页参数经专用 mock 断言。
    assert!(!source.scope_log().is_empty());
    assert!(source.scope_log().iter().all(|s| s == &scope().to_json()));
    assert!(!source.fetch_log().is_empty());
    let params_probe = Arc::new(ParamsProbe::default());
    let probe_engine = SyncEngine::new(_db.open(), SyncConfig::default());
    probe_engine
        .register(Arc::clone(&params_probe) as Arc<dyn DingTalkSource>, "参数探针", T0)
        .unwrap();
    probe_engine.sync_source("probe", T0 + 2).unwrap();
    assert_eq!(
        params_probe.last_page(),
        (100u16, workcontext_infra::sync::OVERLAP_MS_6H)
    );
}

#[derive(Default)]
struct ParamsProbe {
    last: std::sync::Mutex<Option<(u16, i64)>>,
}

impl ParamsProbe {
    fn last_page(&self) -> (u16, i64) {
        self.last.lock().unwrap().expect("fetch 已调用")
    }
}

impl DingTalkSource for ParamsProbe {
    fn id(&self) -> &str {
        "probe"
    }
    fn scope(&self) -> &Scope {
        static SCOPE: std::sync::LazyLock<Scope> = std::sync::LazyLock::new(|| Scope {
            conversations: BTreeSet::from(["cid".to_string()]),
            people: BTreeSet::from(["p1".to_string()]),
        });
        &SCOPE
    }
    fn probe(&self) -> Result<(), SourceError> {
        Ok(())
    }
    fn fetch(
        &self,
        _scope: &Scope,
        _cursor: Option<&Value>,
        page: &PageParams,
    ) -> Result<RecordPage, SourceError> {
        *self.last.lock().unwrap() = Some((page.size, page.overlap_ms));
        Ok(RecordPage { records: vec![], next_cursor: None, has_more: false, snapshot_at: T0 })
    }
}

// ---------------------------------------------------------------------------
// 8. 判读元数据（迁移 0002）：不进 content_hash，单独比对决定 updated
// ---------------------------------------------------------------------------

#[test]
fn judge_metadata_compares_alone_and_converges_to_unchanged() {
    let (db, engine, source) = setup("judge-meta", SyncConfig::default());
    let ro = workcontext_store::open(&db.dir.join("work.sqlite")).expect("复核连接");
    let stored = |ro: &Connection| {
        ro.query_row(
            "SELECT content_hash, mentions_json, reply_to_id, conversation_type
             FROM source_record WHERE external_id = 'm1'",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .expect("记录存在")
    };
    let enriched = || {
        let mut record = rec("m1", T0, "周三前给我初稿");
        record.mentions = vec!["me".to_string()];
        record.reply_to_id = Some("m0".to_string());
        record.conversation_type = Some(ConversationType::Group);
        record
    };

    // 轮 1：无元数据入库，落默认值（迁移 0002 的列默认）。
    source.push_page(page(vec![rec("m1", T0, "周三前给我初稿")], None, false, T0 + 100));
    let first = engine.sync_source("dws-im-1", T0 + 300).expect("轮 1");
    assert_eq!((first.inserted, first.unchanged, first.updated), (1, 0, 0));
    let (hash_before, mentions, reply, conv) = stored(&ro);
    assert_eq!((mentions.as_str(), reply.as_deref(), conv.as_deref()), ("[]", None, None));

    // 轮 2：正文不变、元数据补齐 → 只更新三列并计 updated；content_hash 不变。
    source.push_page(page(vec![enriched()], None, false, T0 + 400));
    let second = engine.sync_source("dws-im-1", T0 + 500).expect("轮 2");
    assert_eq!((second.inserted, second.unchanged, second.updated), (0, 0, 1));
    let (hash_after, mentions, reply, conv) = stored(&ro);
    assert_eq!(hash_after, hash_before, "元数据不进 content_hash");
    assert_eq!(mentions, r#"["me"]"#);
    assert_eq!(reply.as_deref(), Some("m0"));
    assert_eq!(conv.as_deref(), Some("group"));
    assert_eq!(
        ro.query_row("SELECT body FROM source_record WHERE external_id = 'm1'", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "周三前给我初稿",
        "元数据更新不动正文"
    );

    // 轮 3：完全相同 → 收敛为 unchanged（重拉不再计 updated）。
    source.push_page(page(vec![enriched()], None, false, T0 + 600));
    let third = engine.sync_source("dws-im-1", T0 + 700).expect("轮 3");
    assert_eq!((third.inserted, third.unchanged, third.updated), (0, 1, 0));

    // 轮 4：正文变化 → 整行更新（含元数据列）。
    let mut changed = enriched();
    changed.body = Some("初稿改到周五".to_string());
    changed.reply_to_id = None;
    source.push_page(page(vec![changed], None, false, T0 + 800));
    let fourth = engine.sync_source("dws-im-1", T0 + 900).expect("轮 4");
    assert_eq!((fourth.inserted, fourth.unchanged, fourth.updated), (0, 0, 1));
    let (hash_changed, _, reply, _) = stored(&ro);
    assert_ne!(hash_changed, hash_before);
    assert_eq!(reply, None);
}

// ---------------------------------------------------------------------------
// 9. 保留期（§14.3）：入库即定期限、重复同步不延期、清理后重拉不复活
// ---------------------------------------------------------------------------

const DAY: i64 = 86_400_000;

/// 复核/清理用的独立连接。
fn reopen(db: &TempDb) -> Connection {
    workcontext_store::open(&db.dir.join("work.sqlite")).expect("复核连接")
}

/// `m1` 的正文保留状态：(body, raw_state, retention_at, ingested_at, content_hash)。
fn retention_state(conn: &Connection) -> (Option<String>, String, Option<i64>, i64, String) {
    conn.query_row(
        "SELECT body, raw_state, retention_at, ingested_at, content_hash
         FROM source_record WHERE source_id = 'dws-im-1' AND external_id = 'm1'",
        [],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
            ))
        },
    )
    .expect("记录 m1 存在")
}

#[test]
fn retention_deadline_is_set_from_first_ingest_and_never_extended() {
    let (db, engine, source) = setup("retention", SyncConfig::default());
    let ro = reopen(&db);

    // 轮 1（业务时钟 T0+300 入库）：期限 = 首次入库时点 + 30 天（默认）。
    source.push_page(page(vec![rec("m1", T0, "周三前给我初稿")], None, false, T0 + 100));
    engine.sync_source("dws-im-1", T0 + 300).expect("轮 1");
    let deadline = T0 + 300 + 30 * DAY;
    assert_eq!(retention_state(&ro).2, Some(deadline));

    // 轮 2（10 天后，内容相同）：期限不动——重复同步不延期。
    source.push_page(page(vec![rec("m1", T0, "周三前给我初稿")], None, false, T0 + 400));
    engine.sync_source("dws-im-1", T0 + 300 + 10 * DAY).expect("轮 2");
    assert_eq!(retention_state(&ro).2, Some(deadline), "内容相同不得延期");

    // 轮 3（20 天后，内容变化）：整行更新，期限仍以首次入库为基准。
    source.push_page(page(
        vec![rec("m1", T0, "初稿改到周五")],
        None,
        false,
        T0 + 500,
    ));
    engine.sync_source("dws-im-1", T0 + 300 + 20 * DAY).expect("轮 3");
    let (body, raw_state, retention_at, ingested_at, _) = retention_state(&ro);
    assert_eq!(body.as_deref(), Some("初稿改到周五"));
    assert_eq!(raw_state, "available");
    assert_eq!((ingested_at, retention_at), (T0 + 300, Some(deadline)));

    // 新记录在各自入库时点起算（不是沿用旧记录的期限）。
    source.push_page(page(vec![rec("m2", T0, "收到")], None, false, T0 + 600));
    engine.sync_source("dws-im-1", T0 + 300 + 20 * DAY + 1).expect("轮 4");
    let m2: Option<i64> = ro
        .query_row(
            "SELECT retention_at FROM source_record WHERE external_id = 'm2'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(m2, Some(T0 + 300 + 20 * DAY + 1 + 30 * DAY));
}

#[test]
fn retention_config_value_is_applied() {
    let (db, engine, source) = setup(
        "retention-cfg",
        SyncConfig {
            raw_body_days: 7,
            ..SyncConfig::default()
        },
    );
    source.push_page(page(vec![rec("m1", T0, "初稿")], None, false, T0 + 100));
    engine.sync_source("dws-im-1", T0 + 300).expect("轮 1");
    assert_eq!(retention_state(&reopen(&db)).2, Some(T0 + 300 + 7 * DAY));
}

#[test]
fn sweep_then_repull_never_resurrects_body() {
    let (db, engine, source) = setup("retention-sweep", SyncConfig::default());
    let ro = reopen(&db);
    let first_ingest = T0 + 300;
    let deadline = first_ingest + 30 * DAY;
    source.push_page(page(vec![rec("m1", T0, "周三前给我初稿")], None, false, T0 + 100));
    engine.sync_source("dws-im-1", first_ingest).expect("入库");
    assert_eq!(retention_state(&ro).0.as_deref(), Some("周三前给我初稿"));

    // 到期清理：业务时钟恰为期限时点（`retention_at <= now`）。
    let swept = workcontext_store::retention_sweep(&ro, deadline).expect("清理");
    assert_eq!(swept.swept, 1);
    assert_eq!(
        retention_state(&ro),
        (None, "redacted".into(), Some(deadline), first_ingest, rec("m1", T0, "周三前给我初稿").content_hash())
    );

    // 清理后重拉相同内容：unchanged，正文仍为空，期限不动。
    source.push_page(page(vec![rec("m1", T0, "周三前给我初稿")], None, false, T0 + 500));
    let outcome = engine.sync_source("dws-im-1", deadline + DAY).expect("重拉");
    assert_eq!((outcome.inserted, outcome.unchanged, outcome.updated), (0, 1, 0));
    assert_eq!(retention_state(&ro).0, None, "相同内容重拉不得复活正文");
    assert_eq!(retention_state(&ro).2, Some(deadline));

    // 清理后上游内容变化：只刷新去重键与非正文列，正文不复活。
    let edited = rec("m1", T0, "清理后才出现的编辑");
    source.push_page(page(vec![edited.clone()], None, false, T0 + 600));
    let outcome = engine.sync_source("dws-im-1", deadline + 2 * DAY).expect("重拉（内容变化）");
    assert_eq!((outcome.inserted, outcome.unchanged, outcome.updated), (0, 0, 1));
    assert_eq!(
        retention_state(&ro),
        (
            None,
            "redacted".into(),
            Some(deadline),
            first_ingest,
            edited.content_hash()
        ),
        "内容变化不得复活已清理的正文"
    );

    // 再次重拉相同的新内容：去重键已更新 → 收敛为 unchanged。
    source.push_page(page(vec![edited], None, false, T0 + 700));
    let outcome = engine.sync_source("dws-im-1", deadline + 3 * DAY).expect("重拉（收敛）");
    assert_eq!((outcome.inserted, outcome.unchanged, outcome.updated), (0, 1, 0));
    assert_eq!(retention_state(&ro).0, None);
}

#[test]
fn legacy_rows_without_deadline_are_backfilled_from_ingest_time() {
    let (db, engine, source) = setup("retention-legacy", SyncConfig::default());
    let ro = reopen(&db);
    source.push_page(page(vec![rec("m1", T0, "初稿")], None, false, T0 + 100));
    engine.sync_source("dws-im-1", T0 + 300).expect("入库");
    // 模拟本字段引入前入库的旧行。
    ro.execute("UPDATE source_record SET retention_at = NULL", []).unwrap();
    assert_eq!(retention_state(&ro).2, None);

    // 重拉相同内容：期限按首次入库时点补齐，内容计数仍是 unchanged。
    source.push_page(page(vec![rec("m1", T0, "初稿")], None, false, T0 + 400));
    let outcome = engine.sync_source("dws-im-1", T0 + 300 + 5 * DAY).expect("重拉");
    assert_eq!((outcome.inserted, outcome.unchanged, outcome.updated), (0, 1, 0));
    assert_eq!(retention_state(&ro).2, Some(T0 + 300 + 30 * DAY));

    // 补齐后可被清理任务清到。
    let swept = workcontext_store::retention_sweep(&ro, T0 + 300 + 30 * DAY).expect("清理");
    assert_eq!(swept.swept, 1);
    assert_eq!(retention_state(&ro).0, None);
}
