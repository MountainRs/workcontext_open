//! M5/B6 集成测试：备份→改库→恢复→重启回到备份点（门槛 8）、坏包拒绝、
//! 保留期清理、诊断摘要无正文、迁移前强制备份（§4.3 步骤 2）。
//!
//! 与 open.rs 相同的约定：只用公开 API + 临时目录自建自清；
//! 恢复一律按 desktop 的真实形态经 `Mutex<Connection>` / `&mut Connection` 调用。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use workcontext_store::rusqlite::{params, Connection};
use workcontext_store::{
    backup_list, backup_list_verified, backup_now, cleanup_backups, collect_diagnostics, open,
    open_with_backup, restore_from, retention_sweep, sha256_file, supported_version,
    verify_backup, AppMeta, BackupKind, BackupManifest, KeyCounts, ManifestCounts, OpenError,
    RestoreError, RuleCount, BACKUP_DB_FILENAME, DEFAULT_KEEP_ROUTINE, MANIFEST_FILENAME,
    RESTORE_MARKER_SUFFIX,
};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// 独立临时目录 + 库文件路径；Drop 时连同备份/附属文件整目录清理。
struct TempDb {
    dir: PathBuf,
    db: PathBuf,
}

fn temp_db(tag: &str) -> TempDb {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "workcontext-store-m5-{tag}-{}-{seq}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("创建临时目录失败");
    let db = dir.join("workcontext.sqlite3");
    TempDb { dir, db }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn meta() -> AppMeta {
    AppMeta {
        app_version: "0.1.0-test".into(),
        platform: "test-os".into(),
    }
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, params![], |r| r.get(0)).unwrap()
}

/// FTS5 trigram 命中数（整个查询词按短语加引号，规避 FTS 语法字符）。
fn fts_hits(conn: &Connection, term: &str) -> i64 {
    count(
        conn,
        &format!("SELECT count(*) FROM source_record_fts WHERE source_record_fts MATCH '\"{term}\"'"),
    )
}

fn insert_source_with_record(conn: &Connection, source_id: &str, record_id: &str, summary: &str) {
    conn.execute(
        "INSERT OR IGNORE INTO source(id, kind, display_name, scope_json, created_at)
         VALUES (?1, 'dws_im', '测试源', '[]', 1)",
        params![source_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO source_record(id, source_id, external_id, kind,
                                   occurred_at, ingested_at,
                                   summary, body, body_masked, content_hash)
         VALUES (?1, ?2, ?3, 'im_message', 1, 2, ?4, ?4, ?4, 'hash-1')",
        params![record_id, source_id, format!("{record_id}-ext"), summary],
    )
    .unwrap();
}

/// 带 body / body_masked / retention_at 的记录（保留清理用）。
fn insert_retained_record(
    conn: &Connection,
    record_id: &str,
    body: &str,
    retention_at: Option<i64>,
) {
    conn.execute(
        "INSERT INTO source_record(id, source_id, external_id, kind,
                                   occurred_at, ingested_at,
                                   summary, body, body_masked, content_hash, retention_at)
         VALUES (?1, 'src-1', ?2, 'im_message', 1, 2, '摘要', ?3, ?3, 'hash-x', ?4)",
        params![record_id, format!("{record_id}-ext"), body, retention_at],
    )
    .unwrap();
}

fn insert_min_work_item(conn: &Connection, id: &str, title: &str) {
    conn.execute(
        "INSERT INTO work_item (id, title, lifecycle, dedupe_key, created_at, updated_at)
         VALUES (?1, ?2, 'candidate', ?3, 1, 1)",
        params![id, title, format!("dk-{id}")],
    )
    .unwrap();
}

fn insert_checkpoint(conn: &Connection, source_id: &str, last_success_at: i64) {
    conn.execute(
        "INSERT INTO sync_checkpoint (source_id, cursor_json, updated_at, last_success_at)
         VALUES (?1, '{}', 1, ?2)",
        params![source_id, last_success_at],
    )
    .unwrap();
}

fn write_manifest(dir: &Path, manifest: &BackupManifest) {
    std::fs::write(
        dir.join(MANIFEST_FILENAME),
        serde_json::to_vec_pretty(manifest).unwrap(),
    )
    .unwrap();
}

/// 手工构造一个备份包（坏包用例：内容任意，manifest 与文件内容自洽）。
fn craft_package(dir: &Path, db_bytes: &[u8], counts: ManifestCounts) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let db_file = dir.join(BACKUP_DB_FILENAME);
    std::fs::write(&db_file, db_bytes).unwrap();
    let manifest = BackupManifest {
        schema_version: supported_version(),
        app_version: "0.1.0-test".into(),
        platform: "test-os".into(),
        kind: "manual".into(),
        created_at: 1,
        sha256: sha256_file(&db_file).unwrap(),
        size_bytes: db_bytes.len() as u64,
        counts,
    };
    write_manifest(dir, &manifest);
    dir.to_path_buf()
}

// ==== 备份与 manifest ====

#[test]
fn backup_manifest_is_camelcase_and_list_roundtrips() {
    let t = temp_db("manifest");
    let backups = t.dir.join("backups");
    let conn = open(&t.db).unwrap();
    insert_source_with_record(&conn, "src-1", "rec-1", "唯一记录");

    let entry = backup_now(&conn, &backups, BackupKind::Manual, 5_000, &meta()).unwrap();
    assert!(entry.dir.ends_with("manual-5000"));

    // 磁盘上的 manifest 是 camelCase JSON，字段与文件内容自洽。
    let raw: serde_json::Value = serde_json::from_slice(
        &std::fs::read(entry.dir.join(MANIFEST_FILENAME)).unwrap(),
    )
    .unwrap();
    assert_eq!(raw["schemaVersion"], serde_json::json!(supported_version()));
    assert_eq!(raw["kind"], "manual");
    assert_eq!(raw["createdAt"], serde_json::json!(5_000));
    assert_eq!(raw["appVersion"], "0.1.0-test");
    assert_eq!(raw["platform"], "test-os");
    assert_eq!(raw["counts"]["sourceRecord"], serde_json::json!(1));
    assert_eq!(raw["counts"]["workItem"], serde_json::json!(0));
    let sha = raw["sha256"].as_str().unwrap();
    assert_eq!(sha.len(), 64);
    assert!(sha.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_eq!(raw["sizeBytes"], serde_json::json!(std::fs::metadata(entry.db_path()).unwrap().len()));
    assert_eq!(sha, sha256_file(&entry.db_path()).unwrap());
    // manifest 只有计数与摘要，不含正文。
    let manifest_str = String::from_utf8(
        std::fs::read(entry.dir.join(MANIFEST_FILENAME)).unwrap(),
    )
    .unwrap();
    assert!(!manifest_str.contains("唯一记录"));

    let listed = backup_list(&backups);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].dir, entry.dir);
    assert_eq!(listed[0].manifest, entry.manifest);
    // 列表项默认「尚未校验」；backup_list_verified 逐个复核 sha256。
    assert_eq!(entry.sha256_verified, Some(true));
    assert_eq!(listed[0].sha256_verified, None);
    assert_eq!(backup_list_verified(&backups)[0].sha256_verified, Some(true));

    // 序列化即 IPC 契约的 Backup：顶层 path / createdAt / sizeBytes /
    // schemaVersion / appVersion / sha256Verified。
    let wire = serde_json::to_value(&entry).unwrap();
    assert_eq!(
        wire["path"],
        serde_json::json!(entry.dir.to_string_lossy().to_string())
    );
    assert_eq!(wire["createdAt"], serde_json::json!(5_000));
    assert_eq!(wire["sizeBytes"], serde_json::json!(entry.manifest.size_bytes));
    assert_eq!(wire["schemaVersion"], serde_json::json!(supported_version()));
    assert_eq!(wire["appVersion"], "0.1.0-test");
    assert_eq!(wire["sha256Verified"], serde_json::json!(true));
    assert_eq!(wire["kind"], "manual");
    assert!(!wire.to_string().contains("唯一记录"));
}

#[test]
fn cleanup_keeps_recent_routine_and_all_special_backups() {
    let t = temp_db("cleanup");
    let backups = t.dir.join("backups");
    let conn = open(&t.db).unwrap();
    for i in 0..9 {
        backup_now(&conn, &backups, BackupKind::Daily, 1_000 + i, &meta()).unwrap();
    }
    backup_now(&conn, &backups, BackupKind::PreMigration { from: 0, to: 1 }, 2_000, &meta())
        .unwrap();
    backup_now(&conn, &backups, BackupKind::PreRestore, 3_000, &meta()).unwrap();
    std::fs::create_dir_all(backups.join(".tmp-junk")).unwrap();
    // 其他并发运行者的 staging：cleanup 一律不动。
    let foreign_staging = backups.join(".restore-staging-1-99-0");
    std::fs::create_dir_all(&foreign_staging).unwrap();

    let removed = cleanup_backups(&backups, DEFAULT_KEEP_ROUTINE).unwrap();
    // 2 个最旧 daily + 1 个 .tmp 残留；staging 不在其中。
    assert_eq!(removed.len(), 3);
    assert!(!removed.iter().any(|p| p == &foreign_staging));
    assert!(foreign_staging.exists());
    let _ = std::fs::remove_dir_all(&foreign_staging);

    let listed = backup_list(&backups);
    assert_eq!(listed.len(), 9); // 7 daily + pre-migration + pre-restore
    let daily: Vec<_> = listed
        .iter()
        .filter(|e| e.manifest.kind == "daily")
        .map(|e| e.manifest.created_at)
        .collect();
    assert_eq!(daily, vec![1_008, 1_007, 1_006, 1_005, 1_004, 1_003, 1_002]);
    assert!(listed.iter().any(|e| e.manifest.kind == "pre-migration"));
    assert!(listed.iter().any(|e| e.manifest.kind == "pre-restore"));
}

// ==== 门槛 8：备份 → 改库 → 恢复 → 重启 ====

#[test]
fn backup_modify_restore_restart_returns_to_backup_point() {
    let t = temp_db("restore-point");
    let backups = t.dir.join("backups");
    // desktop 的真实形态：Mutex<Connection>，恢复经 guard 的 DerefMut。
    let state = Mutex::new(open(&t.db).unwrap());
    {
        let conn = state.lock().unwrap();
        insert_source_with_record(&conn, "src-1", "rec-1", "备份点记录一 MARKERKEEP");
        insert_source_with_record(&conn, "src-1", "rec-2", "备份点记录二");
        insert_source_with_record(&conn, "src-1", "rec-3", "备份点记录三");
        insert_min_work_item(&conn, "item-1", "备份点事项");
        insert_checkpoint(&conn, "src-1", 111_111);
    }
    let backup_dir = {
        let conn = state.lock().unwrap();
        backup_now(&conn, &backups, BackupKind::Manual, 10_000, &meta())
            .unwrap()
            .dir
    };


    // 备份点之后改库：加记录、删事项、动检查点。
    {
        let conn = state.lock().unwrap();
        insert_source_with_record(&conn, "src-1", "rec-4", "备份后新增");
        conn.execute("DELETE FROM work_item WHERE id = 'item-1'", params![])
            .unwrap();
        conn.execute(
            "UPDATE sync_checkpoint SET last_success_at = 999 WHERE source_id = 'src-1'",
            params![],
        )
        .unwrap();
    }

    // 确认前先预检（只读），再执行恢复。
    let verified = verify_backup(&backup_dir).unwrap();
    assert_eq!(verified.manifest.counts.source_record, 3);
    let outcome = {
        let mut conn = state.lock().unwrap();
        restore_from(&mut conn, &t.db, &backups, &backup_dir, &meta()).unwrap()
    };

    {
        let conn = state.lock().unwrap();
        // 数据回到备份点：记录 3 条、事项回来了、检查点还原、新增记录消失。
        assert_eq!(count(&conn, "SELECT count(*) FROM source_record"), 3);
        assert_eq!(
            count(&conn, "SELECT count(*) FROM work_item WHERE id = 'item-1'"),
            1
        );
        assert_eq!(
            count(&conn, "SELECT last_success_at FROM sync_checkpoint WHERE source_id='src-1'"),
            111_111
        );
        assert_eq!(fts_hits(&conn, "MARKERKEEP"), 1);
        assert_eq!(fts_hits(&conn, "备份后新增"), 0);
        // §14.2 步骤 7：restored 事件（actor=human）。
        assert_eq!(
            count(
                &conn,
                &format!(
                    "SELECT count(*) FROM work_event
                     WHERE id = '{}' AND event_type = 'restored' AND actor = 'human'",
                    outcome.restored_event_id
                )
            ),
            1
        );
        // PRAGMA 契约在换库后依旧成立。
        let mode: String = conn
            .query_row("PRAGMA journal_mode", params![], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }
    // 失败前库保留：原库改名存在且可打开；pre-restore 备份存在。
    assert!(outcome.broken_db.is_file());
    assert!(open(&outcome.broken_db).is_ok());
    assert!(outcome.pre_restore_backup.dir.is_dir());
    assert_eq!(outcome.pre_restore_backup.manifest.kind, "pre-restore");
    // 序列化满足 IPC RestoreResult 的 restored 分支。
    assert_eq!(outcome.status, "restored");
    assert!(!outcome.restarted);
    assert_eq!(outcome.pre_restore_backup_path, outcome.pre_restore_backup.dir);
    let wire = serde_json::to_value(&outcome).unwrap();
    assert_eq!(wire["status"], "restored");
    assert_eq!(wire["restarted"], serde_json::json!(false));
    assert_eq!(
        wire["preRestoreBackupPath"],
        serde_json::json!(outcome.pre_restore_backup.dir.to_string_lossy().to_string())
    );

    // 「重启」：重新打开同一文件，数据仍在恢复点。
    drop(state);
    let conn2 = open(&t.db).unwrap();
    assert_eq!(count(&conn2, "SELECT count(*) FROM source_record"), 3);
    assert_eq!(
        count(&conn2, "SELECT count(*) FROM work_event WHERE event_type='restored'"),
        1
    );
}

// ==== 坏包拒绝：当前库零影响 ====

fn assert_current_db_intact(t: &TempDb) {
    let conn = open(&t.db).unwrap();
    assert_eq!(
        count(&conn, "SELECT count(*) FROM source_record"),
        1,
        "坏包恢复不得改动当前库"
    );
}

fn bad_package_fixture(tag: &str) -> (TempDb, PathBuf) {
    let t = temp_db(tag);
    let conn = open(&t.db).unwrap();
    insert_source_with_record(&conn, "src-1", "rec-1", "当前库记录");
    conn.close().unwrap();
    let pkg = t.dir.join("pkg");
    (t, pkg)
}

#[test]
fn restore_rejects_sha_mismatch_without_touching_current_db() {
    let (t, pkg) = bad_package_fixture("sha");
    let conn = open(&t.db).unwrap();
    let good = backup_now(&conn, &t.dir.join("backups"), BackupKind::Manual, 1, &meta()).unwrap();
    conn.close().unwrap();

    // 复制整包（manifest 带原 sha），再向数据库文件尾部追加一字节 → sha256 不匹配。
    let tampered = pkg.join("tampered");
    std::fs::create_dir_all(&tampered).unwrap();
    std::fs::copy(good.db_path(), tampered.join(BACKUP_DB_FILENAME)).unwrap();
    std::fs::copy(good.dir.join(MANIFEST_FILENAME), tampered.join(MANIFEST_FILENAME)).unwrap();
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(tampered.join(BACKUP_DB_FILENAME))
            .unwrap();
        f.write_all(&[0]).unwrap();
    }

    let mut conn = open(&t.db).unwrap();
    let err = restore_from(
        &mut conn,
        &t.db,
        &t.dir.join("backups"),
        &tampered,
        &meta(),
    )
    .unwrap_err();
    assert!(matches!(err, RestoreError::ShaMismatch { .. }), "{err}");
    assert!(!err.db_was_touched());
    conn.close().unwrap();
    assert_current_db_intact(&t);
    // 坏包在触碰当前库之前被拒：连 pre-restore 备份都不应产生。
    assert!(backup_list(&t.dir.join("backups"))
        .iter()
        .all(|e| e.manifest.kind != "pre-restore"));
    // 列表层面的复核同样能识别出被篡改的包。
    let mut listed = backup_list(&pkg);
    assert_eq!(listed.len(), 1);
    assert!(!listed[0].verify_sha256().unwrap());
    assert_eq!(backup_list_verified(&pkg)[0].sha256_verified, Some(false));
}

#[test]
fn restore_rejects_missing_manifest_and_missing_db() {
    let (t, pkg) = bad_package_fixture("missing");
    let mut conn = open(&t.db).unwrap();

    let empty = pkg.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let err = restore_from(&mut conn, &t.db, &t.dir.join("backups"), &empty, &meta()).unwrap_err();
    assert!(matches!(err, RestoreError::MissingManifest { .. }), "{err}");
    assert!(!err.db_was_touched());

    let no_db = craft_package(&pkg.join("no-db"), b"", ManifestCounts::default());
    std::fs::remove_file(no_db.join(BACKUP_DB_FILENAME)).unwrap();
    let err = restore_from(&mut conn, &t.db, &t.dir.join("backups"), &no_db, &meta()).unwrap_err();
    assert!(matches!(err, RestoreError::MissingDb { .. }), "{err}");
    assert!(!err.db_was_touched());
    conn.close().unwrap();
    assert_current_db_intact(&t);
}

#[test]
fn restore_rejects_non_sqlite_package() {
    let (t, pkg) = bad_package_fixture("notdb");
    let garbage = craft_package(&pkg, b"this is definitely not a sqlite database", ManifestCounts::default());
    let mut conn = open(&t.db).unwrap();
    let err = restore_from(&mut conn, &t.db, &t.dir.join("backups"), &garbage, &meta()).unwrap_err();
    assert!(matches!(err, RestoreError::NotDatabase { .. }), "{err}");
    assert!(!err.db_was_touched());
    conn.close().unwrap();
    assert_current_db_intact(&t);
}

#[test]
fn restore_rejects_future_schema_package() {
    let (t, pkg) = bad_package_fixture("future");
    // 造一个 user_version 超前的库（模拟新版程序产生的备份）。
    let src = t.dir.join("future-src.sqlite3");
    let conn = open(&src).unwrap();
    conn.pragma_update(None, "user_version", supported_version() + 1)
        .unwrap();
    let future = pkg.join("future");
    std::fs::create_dir_all(&future).unwrap();
    let db_file = future.join(BACKUP_DB_FILENAME);
    conn.execute(
        "VACUUM INTO ?1",
        [db_file.to_str().unwrap().to_string()],
    )
    .unwrap();
    conn.close().unwrap();
    let manifest = BackupManifest {
        schema_version: supported_version() + 1,
        app_version: "9.9.9-future".into(),
        platform: "test-os".into(),
        kind: "manual".into(),
        created_at: 1,
        sha256: sha256_file(&db_file).unwrap(),
        size_bytes: std::fs::metadata(&db_file).unwrap().len(),
        counts: ManifestCounts::default(),
    };
    write_manifest(&future, &manifest);

    let mut conn = open(&t.db).unwrap();
    let err = restore_from(&mut conn, &t.db, &t.dir.join("backups"), &future, &meta()).unwrap_err();
    assert!(matches!(err, RestoreError::FutureSchema { .. }), "{err}");
    assert!(!err.db_was_touched());
    conn.close().unwrap();
    assert_current_db_intact(&t);
}

#[test]
fn restore_rejects_count_mismatch_package() {
    let (t, pkg) = bad_package_fixture("counts");
    let conn = open(&t.db).unwrap();
    let good = backup_now(&conn, &t.dir.join("backups"), BackupKind::Manual, 1, &meta()).unwrap();
    conn.close().unwrap();
    // 复制包但把 manifest 计数改大（sha256 仍然正确）。
    let lying = pkg.join("lying");
    std::fs::create_dir_all(&lying).unwrap();
    std::fs::copy(good.db_path(), lying.join(BACKUP_DB_FILENAME)).unwrap();
    let mut lying_manifest = good.manifest.clone();
    lying_manifest.counts.source_record += 1;
    write_manifest(&lying, &lying_manifest);

    let mut conn = open(&t.db).unwrap();
    let err = restore_from(&mut conn, &t.db, &t.dir.join("backups"), &lying, &meta()).unwrap_err();
    assert!(
        matches!(err, RestoreError::CountMismatch { table: "source_record", expected: 2, actual: 1 }),
        "{err}"
    );
    assert!(!err.db_was_touched());
    conn.close().unwrap();
    assert_current_db_intact(&t);
}

#[test]
fn verify_backup_reports_same_rejections_without_work_db() {
    // verify_backup 不需要工作库连接：坏包同样被拒。
    let t = temp_db("verify");
    let garbage = craft_package(&t.dir.join("pkg"), b"not a database at all", ManifestCounts::default());
    let err = verify_backup(&garbage).unwrap_err();
    assert!(matches!(err, RestoreError::NotDatabase { .. }), "{err}");
    assert!(!err.db_was_touched());
}

// ==== 保留期清理（§14.3）====

#[test]
fn retention_sweep_clears_expired_bodies_but_keeps_metadata_and_fts() {
    let t = temp_db("retention");
    let conn = open(&t.db).unwrap();
    conn.execute(
        "INSERT INTO source(id, kind, display_name, scope_json, created_at)
         VALUES ('src-1', 'dws_im', '测试源', '[]', 1)",
        params![],
    )
    .unwrap();
    let now = 1_000_000_i64;
    insert_retained_record(&conn, "rec-expired", "EXPIREDSECRET13800001111", Some(now - 1));
    insert_retained_record(&conn, "rec-future", "FUTURESECRETKEEP", Some(now + 1_000_000));
    insert_retained_record(&conn, "rec-none", "NO-RETENTION-KEEP", None);

    let report = retention_sweep(&conn, now).unwrap();
    assert_eq!(report.swept, 1);
    assert_eq!(report.batches, 1);

    let (body, masked, state): (Option<String>, Option<String>, String) = conn
        .query_row(
            "SELECT body, body_masked, raw_state FROM source_record WHERE id='rec-expired'",
            params![],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(body, None);
    assert_eq!(masked, None);
    assert_eq!(state, "redacted");
    // 元数据保留：summary / content_hash / 发送者仍在。
    let (summary, hash): (String, String) = conn
        .query_row(
            "SELECT summary, content_hash FROM source_record WHERE id='rec-expired'",
            params![],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(summary, "摘要");
    assert_eq!(hash, "hash-x");
    // 未到期与无保留期的记录原样保留。
    for rec in ["rec-future", "rec-none"] {
        let body: Option<String> = conn
            .query_row(
                "SELECT body FROM source_record WHERE id = ?1",
                params![rec],
                |r| r.get(0),
            )
            .unwrap();
        assert!(body.is_some(), "{rec} 不应被清理");
    }
    // FTS 触发器把过期正文从索引剔除，未过期的仍在。
    assert_eq!(fts_hits(&conn, "EXPIREDSECRET13800001111"), 0);
    assert_eq!(fts_hits(&conn, "FUTURESECRETKEEP"), 1);

    // 汇总事件 + 幂等。
    assert_eq!(
        count(&conn, "SELECT count(*) FROM work_event WHERE event_type='retention_swept' AND actor='system'"),
        1
    );
    let again = retention_sweep(&conn, now).unwrap();
    assert_eq!(again.swept, 0);
    assert_eq!(
        count(&conn, "SELECT count(*) FROM work_event WHERE event_type='retention_swept'"),
        1
    );
}

#[test]
fn retention_sweep_batches_in_chunks_of_500() {
    let t = temp_db("batch");
    let conn = open(&t.db).unwrap();
    conn.execute(
        "INSERT INTO source(id, kind, display_name, scope_json, created_at)
         VALUES ('src-1', 'dws_im', '测试源', '[]', 1)",
        params![],
    )
    .unwrap();
    let now = 1_000_000_i64;
    let tx = conn.unchecked_transaction().unwrap();
    for i in 0..1_201 {
        tx.execute(
            "INSERT INTO source_record(id, source_id, external_id, kind,
                                       occurred_at, ingested_at, summary, body,
                                       body_masked, content_hash, retention_at)
             VALUES (?1, 'src-1', ?2, 'im_message', 1, 2, 's', 'b', 'b', 'h', ?3)",
            params![format!("rec-{i}"), format!("ext-{i}"), Some(now - 1)],
        )
        .unwrap();
    }
    tx.commit().unwrap();

    let report = retention_sweep(&conn, now).unwrap();
    assert_eq!(report.swept, 1_201);
    assert_eq!(report.batches, 3);
    assert_eq!(
        count(&conn, "SELECT count(*) FROM source_record WHERE body IS NOT NULL"),
        0
    );
    assert_eq!(
        count(&conn, "SELECT count(*) FROM source_record WHERE raw_state='redacted'"),
        1_201
    );
}

// ==== 诊断摘要无正文 ====

#[test]
fn diagnostics_match_ipc_contract_and_carry_no_body() {
    let t = temp_db("diag");
    let backups = t.dir.join("backups");
    let conn = open(&t.db).unwrap();
    insert_source_with_record(&conn, "src-1", "rec-1", "DIAG-SECRET-SUMMARY-13800001111");
    conn.execute(
        "UPDATE source_record SET body='正文机密 DIAG-SECRET-BODY',
                                  body_masked='掩码机密 DIAG-SECRET-MASKED'",
        params![],
    )
    .unwrap();
    insert_checkpoint(&conn, "src-1", 222_222);
    conn.execute(
        "UPDATE sync_checkpoint SET last_error_msg = ?1 WHERE source_id = 'src-1'",
        params!["x".repeat(500)],
    )
    .unwrap();
    // 固定时钟 + 7 天窗口：最后一条 ok 在窗口外，不得计入。
    let now = 5_000_000_000_000_i64;
    let day = 86_400_000_i64;
    let mut runs: Vec<(String, &str, Option<&str>, i64)> = Vec::new();
    for (i, (status, rule, age_days)) in [
        ("ok", None, 0),
        ("rejected", Some("V03"), 1),
        ("rejected", Some("V01"), 6),
        ("failed", None, 2),
        ("timeout", None, 3),
        ("ok", None, 8),
    ]
    .into_iter()
    .enumerate()
    {
        runs.push((format!("run-{i}"), status, rule, now - age_days * day));
    }
    for (id, status, rule, created_at) in &runs {
        conn.execute(
            "INSERT INTO agent_run (id, kind, runtime, prompt_version, input_refs_json,
                                    input_sha256, raw_output, status, rejection_rule, created_at)
             VALUES (?1, 'classify', 'mock', 'p1', '[]', ?2, 'RAW-OUTPUT-SECRET-ABC', ?3, ?4, ?5)",
            params![id, format!("sha-{id}"), status, rule, created_at],
        )
        .unwrap();
    }
    let entry = backup_now(&conn, &backups, BackupKind::Daily, 42, &meta()).unwrap();

    let diag = collect_diagnostics(&conn, &t.db, &backups, &meta(), now, 7);
    assert_eq!(diag.db.integrity, "ok");
    assert_eq!(diag.db.path, t.db.display().to_string());
    assert_eq!(diag.app.version, "0.1.0-test");
    assert_eq!(diag.app.platform, "test-os");
    assert_eq!(diag.app.schema_version, supported_version());
    assert_eq!(diag.app.supported_schema_version, supported_version());
    assert_eq!(
        diag.db.counts,
        KeyCounts {
            source_record: 1,
            ..Default::default()
        }
    );
    assert_eq!(diag.backups.count, 1);
    let last = diag.backups.last.as_ref().unwrap();
    assert_eq!(last.path, entry.dir.display().to_string());
    assert_eq!(last.created_at, 42);
    // 判读窗口统计：窗口内 1 ok / 2 rejected / 1 failed / 1 timeout。
    assert_eq!(diag.judge.window_days, 7);
    assert_eq!(diag.judge.ok, 1);
    assert_eq!(diag.judge.rejected, 2);
    assert_eq!(diag.judge.failed, 1);
    assert_eq!(diag.judge.timeout, 1);
    assert_eq!(diag.judge.finished(), 5);
    assert_eq!(diag.judge.success_rate(), 0.2);
    assert_eq!(
        diag.judge.rejection_rules,
        vec![
            RuleCount {
                rule: "V01".into(),
                count: 1,
            },
            RuleCount {
                rule: "V03".into(),
                count: 1,
            },
        ]
    );
    // sources 与 sync_status 命令同形。
    assert_eq!(diag.sources.len(), 1);
    assert_eq!(diag.sources[0].source_id, "src-1");
    assert_eq!(diag.sources[0].state, "idle");
    assert_eq!(diag.sources[0].last_success_at, Some(222_222));
    assert_eq!(
        diag.sources[0]
            .last_error_msg
            .as_deref()
            .map(|m| m.chars().count()),
        Some(201),
        "错误说明截断到 200 字 + 省略号"
    );
    // store 自有的补充指标。
    assert_eq!(diag.storage.journal_mode, "wal");
    assert_eq!(diag.storage.table_counts.get("agent_run"), Some(&6));
    assert_eq!(diag.storage.raw_state_counts.get("available"), Some(&1));
    assert!(diag.storage.errors.is_empty(), "{:?}", diag.storage.errors);

    // 线上形状覆盖 ui/src/lib/api.ts isDiagnostics 粗校验要求的全部字段。
    let json = serde_json::to_value(&diag).unwrap();
    for key in [
        "app.version",
        "app.platform",
        "app.schemaVersion",
        "app.supportedSchemaVersion",
        "db.path",
        "db.integrity",
        "db.counts.sourceRecord",
        "db.counts.workItem",
        "db.counts.workEvent",
        "db.counts.feedbackSnapshot",
        "backups.count",
        "sources",
        "judge.windowDays",
        "judge.rejectionRules",
    ] {
        let mut cursor = &json;
        for segment in key.split('.') {
            cursor = &cursor[segment];
        }
        assert!(!cursor.is_null(), "缺少契约字段 {key}");
    }
    assert_eq!(json["db"]["integrity"], "ok");
    assert!(json["sources"].is_array());
    assert!(json["judge"]["rejectionRules"].is_array());

    // 门槛 12 的组成部分：摘要 JSON 不含任何正文/原始输出/掩码/摘要内容。
    let text = json.to_string();
    for secret in ["DIAG-SECRET", "13800001111", "RAW-OUTPUT-SECRET", "正文机密"] {
        assert!(!text.contains(secret), "诊断摘要泄漏: {secret}");
    }
}

// ==== 迁移前强制备份（§4.3 步骤 2）====

#[test]
fn open_with_backup_forces_pre_migration_backup_only_when_migrating() {
    let t = temp_db("pre-migration");
    let backups = t.dir.join("backups");
    {
        let _conn = open_with_backup(&t.db, &backups, &meta()).unwrap();
        let list = backup_list(&backups);
        assert_eq!(list.len(), 1, "首次建库（0→N）必须先留 pre-migration 备份");
        assert_eq!(list[0].manifest.kind, "pre-migration");
        assert_eq!(list[0].manifest.schema_version, 0, "备份的是迁移前状态");
        assert!(
            list[0]
                .dir
                .to_string_lossy()
                .contains(&format!("pre-migration-v0-to-v{}-", supported_version())),
            "目录名记录迁移前后版本: {}",
            list[0].dir.display()
        );
    }
    // 重启：已是最新版本，不再新增 pre-migration 备份。
    {
        let _conn = open_with_backup(&t.db, &backups, &meta()).unwrap();
        assert_eq!(backup_list(&backups).len(), 1);
    }
    // 自由 open 永不产生备份目录（infra/测试用）。
    let t2 = temp_db("pre-migration-none");
    {
        let _conn = open(&t2.db).unwrap();
    }
    assert!(!t2.dir.join("backups").exists());
}

// ==== 崩溃安全：恢复中断后的启动检查（不得把空库当成新库）====

/// 在 `db` 旁写一份恢复标记，等价于恢复在写完标记之后崩溃。
fn write_restore_marker(db: &Path, broken: &Path, staged_dir: &Path) -> PathBuf {
    let marker = PathBuf::from(format!("{}{RESTORE_MARKER_SUFFIX}", db.display()));
    std::fs::write(
        &marker,
        serde_json::json!({ "brokenDb": broken, "stagedDir": staged_dir }).to_string(),
    )
    .unwrap();
    marker
}

/// 造「原库有 1 条记录且已干净关闭」的前提。
fn seed_original(db: &Path) {
    let conn = open(db).unwrap();
    insert_source_with_record(&conn, "src-1", "rec-1", "原库记录");
    conn.close().unwrap();
}

#[test]
fn open_rolls_back_when_restore_crashed_after_parking_original() {
    let t = temp_db("crash-parked");
    seed_original(&t.db);
    // 原库已改名、恢复副本尚未就位：当前路径为空。
    let broken = PathBuf::from(format!("{}.broken-1", t.db.display()));
    std::fs::rename(&t.db, &broken).unwrap();
    let staging = t.dir.join(".restore-staging-9-1-0");
    std::fs::create_dir_all(&staging).unwrap();
    let marker = write_restore_marker(&t.db, &broken, &staging);
    assert!(!t.db.exists(), "前提：naive open 会在这个空路径上建出新库");

    let conn = open(&t.db).unwrap();
    assert_eq!(count(&conn, "SELECT count(*) FROM source_record"), 1);
    assert!(!marker.exists());
    assert!(!broken.exists(), "原库已改回原名");
    assert!(!staging.exists(), "标记里记录的 staging 被清理");
}

#[test]
fn open_discards_unconfirmed_copy_when_restore_crashed_after_swap() {
    let t = temp_db("crash-swapped");
    seed_original(&t.db);
    let broken = PathBuf::from(format!("{}.broken-1", t.db.display()));
    std::fs::rename(&t.db, &broken).unwrap();
    // 未确认的恢复副本已就位（内容不同的空库）：在别处建好再改名进来。
    let staging = t.dir.join(".restore-staging-9-2-0");
    std::fs::create_dir_all(&staging).unwrap();
    let staged_db = staging.join("workcontext.sqlite3");
    open(&staged_db).unwrap().close().unwrap();
    std::fs::rename(&staged_db, &t.db).unwrap();
    let marker = write_restore_marker(&t.db, &broken, &staging);

    let conn = open(&t.db).unwrap();
    assert_eq!(
        count(&conn, "SELECT count(*) FROM source_record"),
        1,
        "未确认的恢复一律作废，原库优先"
    );
    assert!(!marker.exists());
    assert!(!broken.exists());
}

#[test]
fn open_refuses_when_interrupted_restore_lost_everything() {
    let t = temp_db("crash-lost");
    let marker = write_restore_marker(
        &t.db,
        &t.dir.join("none.broken-1"),
        &t.dir.join(".restore-staging-9-3-0"),
    );

    let err = open(&t.db).unwrap_err();
    assert!(matches!(err, OpenError::InterruptedRestore { .. }), "{err}");
    assert!(!t.db.exists(), "拒绝打开时不得创建新库文件");
    assert!(marker.exists(), "标记保留，等待用户从备份恢复");
}

#[test]
fn open_proceeds_when_restore_crashed_before_parking_original() {
    let t = temp_db("crash-before-park");
    seed_original(&t.db);
    let marker = write_restore_marker(
        &t.db,
        &t.dir.join("none.broken-1"),
        &t.dir.join(".restore-staging-9-4-0"),
    );

    let conn = open(&t.db).unwrap();
    assert_eq!(count(&conn, "SELECT count(*) FROM source_record"), 1);
    assert!(!marker.exists());
}

#[test]
fn open_with_backup_rolls_back_interrupted_restore_before_migrating() {
    let t = temp_db("crash-with-backup");
    seed_original(&t.db);
    let broken = PathBuf::from(format!("{}.broken-1", t.db.display()));
    std::fs::rename(&t.db, &broken).unwrap();
    let marker = write_restore_marker(&t.db, &broken, &t.dir.join(".restore-staging-9-5-0"));
    let backups = t.dir.join("backups");

    let conn = open_with_backup(&t.db, &backups, &meta()).unwrap();
    assert_eq!(count(&conn, "SELECT count(*) FROM source_record"), 1);
    assert!(!marker.exists());
    assert!(
        backup_list(&backups).is_empty(),
        "回滚后的原库已是最新版本，不应产生迁移前备份"
    );
}

// ==== 审计事件是恢复成功的前提，而非事后补记 ====

#[test]
fn restore_rejects_package_whose_audit_event_cannot_be_written() {
    let (t, pkg) = bad_package_fixture("audit");
    // manifest / sha256 / 计数全部自洽，但库内触发器拒绝写入 restored 事件。
    let blocked = pkg.join("blocked");
    {
        let src = t.dir.join("audit-src.sqlite3");
        let conn = open(&src).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER tr_block_restored BEFORE INSERT ON work_event
             WHEN new.event_type = 'restored'
             BEGIN SELECT RAISE(ABORT, 'audit blocked'); END;",
        )
        .unwrap();
        std::fs::create_dir_all(&blocked).unwrap();
        let db_file = blocked.join(BACKUP_DB_FILENAME);
        conn.execute(
            "VACUUM INTO ?1",
            [db_file.to_str().unwrap().to_string()],
        )
        .unwrap();
        write_manifest(
            &blocked,
            &BackupManifest {
                schema_version: supported_version(),
                app_version: "0.1.0-test".into(),
                platform: "test-os".into(),
                kind: "manual".into(),
                created_at: 1,
                sha256: sha256_file(&db_file).unwrap(),
                size_bytes: std::fs::metadata(&db_file).unwrap().len(),
                counts: ManifestCounts::default(),
            },
        );
        conn.close().unwrap();
    }
    let backups = t.dir.join("backups");
    let mut conn = open(&t.db).unwrap();
    let err = restore_from(&mut conn, &t.db, &backups, &blocked, &meta()).unwrap_err();
    assert!(matches!(err, RestoreError::EventWrite { .. }), "{err}");
    assert!(!err.db_was_touched());
    // 当前库连接仍可用、数据未动；未写标记；未产生 pre-restore 备份。
    assert_eq!(count(&conn, "SELECT count(*) FROM source_record"), 1);
    assert!(!PathBuf::from(format!("{}{RESTORE_MARKER_SUFFIX}", t.db.display())).exists());
    assert!(backup_list(&backups).is_empty());
    conn.close().unwrap();
    assert_current_db_intact(&t);
}

// ==== 旧 schema 的备份恢复进新程序：副本先迁移，再比对计数 ====

fn schema5_snapshot(path: &Path) -> Vec<Vec<String>> {
    use workcontext_store::rusqlite::OpenFlags;
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    [
        "SELECT json_array(id,title,status,created_at,updated_at) FROM work_thread ORDER BY id",
        "SELECT json_array(id,profile,candidate_json,present,revision,project_override) FROM ownership_card ORDER BY id",
        "SELECT json_array(id,title,lifecycle,attention,next_action,thread_id,origin_source_record_id,closed_at,close_reason,dedupe_key,created_at,updated_at) FROM work_item ORDER BY id",
        "SELECT json_array(card_id,note,resolved,anchors_json) FROM ownership_conclusion ORDER BY card_id",
        "SELECT json_array(id,source_id,external_id,conversation_id,body) FROM source_record ORDER BY id",
        "SELECT json_array(id,source_record_id,target_type,target_id,link_type,state) FROM link_evidence ORDER BY id",
        "SELECT json_array(id,target_type,target_id,event_type,actor,source_record_id,note) FROM work_event WHERE event_type <> 'restored' ORDER BY id",
    ].iter().map(|sql| {
        let mut stmt = db.prepare(sql).unwrap();
        let rows = stmt.query_map([], |row| row.get::<_, String>(0)).unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    }).collect()
}

#[test]
fn schema5_delivery_upgrade_and_rollback() {
    use workcontext_store::rusqlite::OpenFlags;
    let t = temp_db("schema5-delivery");
    let backups = t.dir.join("backups");
    let archive = t.dir.join("archive.sqlite3");
    Connection::open(&archive).unwrap().execute_batch(
        "CREATE TABLE messages(cid TEXT,mid TEXT,body TEXT,PRIMARY KEY(cid,mid));
         INSERT INTO messages VALUES('chat','request','已签收交接清单');"
    ).unwrap();
    let archive_hash = sha256_file(&archive).unwrap();
    {
        let old = Connection::open(&t.db).unwrap();
        old.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        for migration in [
            include_str!("../../../migrations/0001_init.sql"),
            include_str!("../../../migrations/0002_judge_meta.sql"),
            include_str!("../../../migrations/0003_ownership_flow.sql"),
            include_str!("../../../migrations/0004_ownership_conclusion.sql"),
            include_str!("../../../migrations/0005_ownership_project.sql"),
        ] {
            old.execute_batch(migration).unwrap();
        }
        old.pragma_update(None, "user_version", 5).unwrap();
        insert_source_with_record(&old, "old-source", "source-request", "已签收交接清单");
        old.execute_batch("UPDATE source_record SET conversation_id='chat',external_id='request'").unwrap();
        old.execute_batch(include_str!("fixtures/schema5_delivery.sql")).unwrap();
    }
    let before = schema5_snapshot(&t.db);
    let mut current = open_with_backup(&t.db, &backups, &meta()).unwrap();
    assert!(count(&current, "PRAGMA user_version") >= 6);
    assert_eq!(schema5_snapshot(&t.db), before);
    assert_eq!(count(&current, "SELECT count(*) FROM ownership_card WHERE context_scope_id IS NULL"), 2);
    assert_eq!(count(&current, "SELECT count(*) FROM work_thread WHERE profile IS NULL"), 1);
    let packages = backup_list(&backups);
    assert_eq!(packages.len(), 1);
    let package = verify_backup(&packages[0].dir).unwrap();
    assert_eq!(package.manifest.kind, "pre-migration");
    assert_eq!(package.manifest.schema_version, 5);
    assert_eq!(package.manifest.counts, ManifestCounts { source_record: 1, work_item: 2, work_event: 1 });
    assert_eq!(schema5_snapshot(&package.db_path()), before);

    // A matching old client can open this verified copy at version 5 before any current store opens it.
    let rollback = t.dir.join("rolled-back.sqlite3");
    std::fs::copy(package.db_path(), &rollback).unwrap();
    let old = Connection::open_with_flags(&rollback, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(count(&old, "PRAGMA user_version"), 5);
    assert_eq!(count(&old, "SELECT count(*) FROM pragma_table_info('ownership_card') WHERE name='context_scope_id'"), 0);
    assert_eq!(schema5_snapshot(&rollback), before);
    let (cid, mid): (String, String) = old.query_row(
        "SELECT json_extract(anchors_json,'$[0].cid'),json_extract(anchors_json,'$[0].mid') FROM ownership_conclusion WHERE card_id='removed'",
        [], |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    let archived = Connection::open_with_flags(&archive, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let body: String = archived.query_row("SELECT body FROM messages WHERE cid=? AND mid=?", [cid, mid], |row| row.get(0)).unwrap();
    assert_eq!(body, "已签收交接清单");

    // Current-client restore saves post-upgrade changes and automatically migrates its staged copy.
    insert_min_work_item(&current, "after-upgrade", "升级后输入");
    let result = restore_from(&mut current, &t.db, &backups, &package.dir, &meta()).unwrap();
    assert_eq!(result.schema_version, 5);
    assert_eq!(result.pre_restore_backup.manifest.counts.work_item, 3);
    assert_eq!(count(&current, "PRAGMA user_version"), i64::from(supported_version()));
    assert_eq!(schema5_snapshot(&t.db), before);
    assert_eq!(count(&current, "SELECT count(*) FROM work_event WHERE event_type='restored'"), 1);
    assert_eq!(sha256_file(&archive).unwrap(), archive_hash);
}

#[test]
fn restore_migrates_old_schema_backup_on_the_staged_copy() {
    if supported_version() < 2 {
        // 仓库只有一个迁移时无法构造「旧 schema 备份」，本用例暂无对象。
        eprintln!("skip: 仅存在 0001，无旧 schema 可模拟");
        return;
    }
    let t = temp_db("old-schema");
    let backups = t.dir.join("backups");
    // 只用 0001 建一个 v1 库并放入数据，模拟旧版本程序留下的备份。
    let old_pkg = t.dir.join("old-pkg");
    {
        let conn = Connection::open(t.dir.join("v1.sqlite3")).unwrap();
        conn.execute_batch(include_str!("../../../migrations/0001_init.sql"))
            .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute(
            "INSERT INTO source(id, kind, display_name, scope_json, created_at)
             VALUES ('src-1', 'dws_im', '旧源', '[]', 1)",
            params![],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source_record(id, source_id, external_id, kind, occurred_at,
                                       ingested_at, summary, body_masked, content_hash)
             VALUES ('rec-1', 'src-1', 'e1', 'im_message', 1, 2, '旧备份记录', '旧备份记录', 'h')",
            params![],
        )
        .unwrap();
        std::fs::create_dir_all(&old_pkg).unwrap();
        let db_file = old_pkg.join(BACKUP_DB_FILENAME);
        conn.execute(
            "VACUUM INTO ?1",
            [db_file.to_str().unwrap().to_string()],
        )
        .unwrap();
        write_manifest(
            &old_pkg,
            &BackupManifest {
                schema_version: 1,
                app_version: "0.0.1-old".into(),
                platform: "test-os".into(),
                kind: "manual".into(),
                created_at: 1,
                sha256: sha256_file(&db_file).unwrap(),
                size_bytes: std::fs::metadata(&db_file).unwrap().len(),
                counts: ManifestCounts {
                    source_record: 1,
                    work_item: 0,
                    work_event: 0,
                },
            },
        );
        conn.close().unwrap();
    }

    // 当前库（最新版本）里有另一批数据；恢复后回到旧备份点，并被迁到最新版本。
    let mut conn = open(&t.db).unwrap();
    insert_source_with_record(&conn, "src-2", "rec-new", "当前库记录");
    let outcome = restore_from(&mut conn, &t.db, &backups, &old_pkg, &meta()).unwrap();
    assert_eq!(outcome.schema_version, 1, "结果记录的是备份包自身的 schema 版本");
    assert_eq!(
        workcontext_store::schema_version(&conn).unwrap(),
        supported_version(),
        "恢复后的库已被迁到最新版本"
    );
    assert_eq!(count(&conn, "SELECT count(*) FROM source_record"), 1);
    assert_eq!(
        count(&conn, "SELECT count(*) FROM source_record WHERE id = 'rec-1'"),
        1
    );
    // 0002 新增的列在恢复后的库中可用；FTS 内容随备份完整恢复。
    assert_eq!(
        count(&conn, "SELECT count(*) FROM source_record WHERE mentions_json = '[]'"),
        1
    );
    assert_eq!(fts_hits(&conn, "旧备份记录"), 1);
}
