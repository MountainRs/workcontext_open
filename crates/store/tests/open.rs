//! 持久化与失败的集成测试（详细设计 §16.1：`store` 集成用临时文件库）。
//!
//! 只通过公开 API（`open` / `schema_version` / `supported_version`）和
//! 重新导出的 `rusqlite` 操作，模拟 desktop 的真实调用方式；不依赖外部
//! 测试工具，临时目录自建自清。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use workcontext_store::rusqlite::{params, Connection};
use workcontext_store::{open, schema_version, supported_version, OpenError};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// 独立临时目录 + 库文件路径；Drop 时连同 -wal/-shm 附属文件整目录清理。
struct TempDb {
    dir: PathBuf,
    path: PathBuf,
}

fn temp_db(tag: &str) -> TempDb {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "workcontext-store-test-{tag}-{}-{seq}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("创建临时目录失败");
    let path = dir.join("workcontext.sqlite3");
    TempDb { dir, path }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = ?1",
            params![name],
            |r| r.get(0),
        )
        .unwrap();
    n > 0
}

fn pragma_str(conn: &Connection, name: &str) -> String {
    conn.query_row(&format!("PRAGMA {name}"), params![], |r| r.get(0))
        .unwrap()
}

fn pragma_i64(conn: &Connection, name: &str) -> i64 {
    conn.query_row(&format!("PRAGMA {name}"), params![], |r| r.get(0))
        .unwrap()
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, params![], |r| r.get(0)).unwrap()
}

/// 插入满足全部约束的最小 source + source_record（kind/枚举均取默认合法值）。
fn insert_source_with_record(conn: &Connection, source_id: &str, record_id: &str, summary: &str) {
    conn.execute(
        "INSERT INTO source(id, kind, display_name, scope_json, created_at)
         VALUES (?1, 'dws_im', '测试源', '[]', 1)",
        params![source_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO source_record(id, source_id, external_id, kind,
                                   occurred_at, ingested_at,
                                   summary, body_masked, content_hash)
         VALUES (?1, ?2, ?3, 'im_message', 1, 2, ?4, ?4, 'hash-1')",
        params![record_id, source_id, format!("{record_id}-ext"), summary],
    )
    .unwrap();
}

/// FTS5 trigram 命中数（整个查询词按短语加引号，规避 FTS 语法字符）。
fn fts_hits(conn: &Connection, term: &str) -> i64 {
    count(
        conn,
        &format!(
            "SELECT count(*) FROM source_record_fts
             WHERE source_record_fts MATCH '\"{term}\"'"
        ),
    )
}

/// §4.1 连接参数契约：每次打开都必须成立。
fn assert_pragma_contract(conn: &Connection) {
    assert_eq!(pragma_str(conn, "journal_mode"), "wal");
    assert_eq!(pragma_i64(conn, "foreign_keys"), 1);
    assert_eq!(pragma_i64(conn, "synchronous"), 1, "NORMAL");
    assert_eq!(pragma_i64(conn, "busy_timeout"), 5000);
    assert_eq!(pragma_i64(conn, "temp_store"), 2, "MEMORY");
    assert_eq!(pragma_i64(conn, "trusted_schema"), 0, "OFF");
}

#[test]
fn fresh_open_migrates_to_latest_and_passes_checks() {
    let db = temp_db("fresh");
    let conn = open(&db.path).expect("空目录首次建库应成功");

    assert_eq!(schema_version(&conn).unwrap(), supported_version());
    for object in [
        "source",
        "source_record",
        "work_thread",
        "work_item",
        "agent_run",
        "work_event",
        "link_evidence",
        "field_override",
        "ownership_card",
        "ownership_undo",
        "ownership_conclusion",
        "sync_checkpoint",
        "sync_run",
        "egress_log",
        "feedback_snapshot",
        "app_release",
        "source_record_fts",
        "work_item_fts",
        "v_item_turn",
    ] {
        assert!(table_exists(&conn, object), "迁移后缺少对象 {object}");
    }
    assert_eq!(pragma_str(&conn, "integrity_check"), "ok");
    assert_eq!(
        count(&conn, "SELECT count(*) FROM pragma_foreign_key_check"),
        0,
        "新库不应有外键违规"
    );
}

#[test]
fn ownership_migration_preserves_unscoped_v2_items() {
    let db = temp_db("v2-ownership");
    {
        let old = Connection::open(&db.path).unwrap();
        old.execute_batch(include_str!("../../../migrations/0001_init.sql")).unwrap();
        old.execute_batch(include_str!("../../../migrations/0002_judge_meta.sql")).unwrap();
        old.execute_batch("PRAGMA user_version=2;
            INSERT INTO work_item(id,title,dedupe_key,created_at,updated_at) VALUES ('legacy','原事项','old',1,1);").unwrap();
    }
    let conn = open(&db.path).unwrap();
    assert_eq!(count(&conn, "SELECT count(*) FROM ownership_card"), 0);
    assert_eq!(count(&conn, "SELECT count(*) FROM work_item WHERE id='legacy' AND title='原事项'"), 1);
    assert_eq!(schema_version(&conn).unwrap(), supported_version());
}

#[test]
fn connection_pragmas_match_design_on_every_open() {
    let db = temp_db("pragmas");
    let conn = open(&db.path).unwrap();
    assert_pragma_contract(&conn);
    drop(conn);

    // 重开（含已建好的库）同样生效：PRAGMA 是每次打开的契约，不依赖上次会话。
    let conn = open(&db.path).unwrap();
    assert_pragma_contract(&conn);
}

#[test]
fn reopen_preserves_data_and_fts_without_remigrating() {
    let db = temp_db("persist");
    {
        let conn = open(&db.path).unwrap();
        insert_source_with_record(&conn, "s1", "r1", "quarterly report ready");
        conn.execute(
            "INSERT INTO work_item(id, title, dedupe_key, created_at, updated_at)
             VALUES ('w1', '跟进季度报告', 'dk1', 1, 1)",
            params![],
        )
        .unwrap();
    }

    {
        // 若 open() 对已达最新版本的库重跑 0001，CREATE TABLE 会报已存在而失败；
        // open 成功本身即证明没有重复迁移。
        let conn = open(&db.path).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), supported_version());
        assert_eq!(count(&conn, "SELECT count(*) FROM source_record"), 1);
        assert_eq!(count(&conn, "SELECT count(*) FROM work_item"), 1);
        // FTS5 外部内容表由触发器维护，重开后索引仍在。
        assert_eq!(fts_hits(&conn, "quarterly"), 1);

        // UPDATE 走 tr_sr_au（trusted_schema=OFF 下触发器写 FTS 仍可用）。
        conn.execute(
            "UPDATE source_record
             SET summary = 'monthly report done', body_masked = 'monthly report done'
             WHERE id = 'r1'",
            params![],
        )
        .unwrap();
        assert_eq!(fts_hits(&conn, "quarterly"), 0, "旧词应被触发器删除");
        assert_eq!(fts_hits(&conn, "monthly"), 1);
    }

    {
        // 再次重开：更新与 FTS 索引均已落盘。
        let conn = open(&db.path).unwrap();
        assert_eq!(fts_hits(&conn, "monthly"), 1);
        let title: String = conn
            .query_row(
                "SELECT title FROM work_item WHERE id = 'w1'",
                params![],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(title, "跟进季度报告");
    }
}

#[test]
fn future_schema_version_is_rejected_without_touching_db() {
    let db = temp_db("future");
    drop(open(&db.path).unwrap());

    let future = supported_version() + 1;
    Connection::open(&db.path)
        .unwrap()
        .execute_batch(&format!("PRAGMA user_version = {future};"))
        .unwrap();

    let err = open(&db.path).unwrap_err();
    assert!(
        matches!(err,
                 OpenError::FutureSchema { found, supported }
                     if found == i64::from(future) && supported == supported_version()),
        "实际错误: {err:?}"
    );

    // 拒绝路径不得改版本号、不得动 schema。
    let conn = Connection::open(&db.path).unwrap();
    assert_eq!(pragma_i64(&conn, "user_version"), i64::from(future));
    assert!(table_exists(&conn, "work_item"));
}

#[test]
fn failed_migration_rolls_back_atomically() {
    let db = temp_db("rollback");
    // 人为制造半套库：只有 0001 中段才创建的表、版本号仍为 0，
    // 迫使迁移执行若干语句后在文件中段失败。
    Connection::open(&db.path)
        .unwrap()
        .execute_batch("CREATE TABLE sync_checkpoint (x); PRAGMA user_version = 0;")
        .unwrap();

    let err = open(&db.path).unwrap_err();
    assert!(
        matches!(err, OpenError::Migrate { version: 1, .. }),
        "实际错误: {err:?}"
    );

    let conn = Connection::open(&db.path).unwrap();
    assert_eq!(pragma_i64(&conn, "user_version"), 0, "失败后版本不得前移");
    // 同一事务中先建的表必须一并回滚，不允许半套 schema。
    for table in [
        "source",
        "source_record",
        "work_thread",
        "work_item",
        "agent_run",
        "work_event",
    ] {
        assert!(!table_exists(&conn, table), "迁移失败后 {table} 不应残留");
    }
}

#[test]
fn non_sqlite_file_is_rejected_fast() {
    let db = temp_db("notdb");
    std::fs::write(&db.path, b"this is definitely not a sqlite database file").unwrap();

    let err = open(&db.path).unwrap_err();
    // 文件头在第一条 PRAGMA 就会被读取：错误应在连接参数阶段暴露，
    // 而不是返回一个"看似可用"的连接或推迟到业务写入时。
    assert!(
        matches!(err, OpenError::Pragma { name: "journal_mode", .. }),
        "实际错误: {err:?}"
    );
}

#[test]
fn missing_parent_directory_is_rejected() {
    let db = temp_db("noparent");
    let path = db.path.join("no_such_dir").join("workcontext.sqlite3");
    assert!(matches!(open(&path), Err(OpenError::Open { .. })));
}

#[test]
fn external_open_never_creates_recovers_or_migrates() {
    use workcontext_store::{open_external, ExternalOpenError};
    let db = temp_db("external-open");
    assert!(open_external(&db.path, true).is_err());
    assert_eq!(std::fs::read_dir(&db.dir).unwrap().count(), 0);
    Connection::open(&db.path).unwrap().execute_batch("PRAGMA user_version=1").unwrap();
    assert!(matches!(open_external(&db.path, true), Err(ExternalOpenError::Version(1))));
    assert_eq!(schema_version(&Connection::open(&db.path).unwrap()).unwrap(), 1);
    let marker = db.dir.join("workcontext.sqlite3.restore-pending");
    std::fs::write(&marker, b"pending").unwrap();
    assert!(matches!(open_external(&db.path, false), Err(ExternalOpenError::Restoring)));
    assert!(marker.exists());
}

#[test]
fn external_requests_share_lock_and_block_exclusive_restore() {
    use workcontext_store::{open_external, restore_from, AppMeta, ExternalOpenError, RestoreError};
    let db = temp_db("external-lock");
    let mut desktop = open(&db.path).unwrap();
    let first = open_external(&db.path, false).unwrap();
    let second = open_external(&db.path, true).unwrap();
    let meta = AppMeta { app_version: "test".into(), platform: "test".into() };
    assert!(matches!(restore_from(&mut desktop, &db.path, &db.dir, &db.dir, &meta), Err(RestoreError::Maintenance(ExternalOpenError::Busy))));
    drop((first, second));
    let lease = std::fs::OpenOptions::new().read(true).write(true).open(db.dir.join("workcontext.sqlite3.maintenance.lock")).unwrap();
    lease.try_lock().unwrap();
    assert!(matches!(open_external(&db.path, false), Err(ExternalOpenError::Busy)));
    drop(lease);
    assert!(open_external(&db.path, true).is_ok());
}
