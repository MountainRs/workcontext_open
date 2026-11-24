//! `sources` 模块的单元测试：真实临时 SQLite 库（跑真实迁移）+ 临时
//! `config.json`，不触网、不启动任何 CLI。覆盖模块文档的四条硬规则。

use std::path::PathBuf;

use workcontext_infra::sync::Scope;
use workcontext_store::rusqlite::Connection;

use super::*;
use super::config::MAX_FIRST_PULL_LOOKBACK_HOURS;

/// `egress_log` 的一行：`(at, policy, decision, target, run_id)`。
type AuditRow = (i64, String, String, Option<String>, Option<String>);

/// 每个测试一套独立的数据目录：库文件与配置文件都在临时目录里。
pub(super) struct Fixture {
    pub dir: tempfile::TempDir,
    pub state: SourceState,
}

impl Fixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("创建临时目录");
        let state = SourceState {
            db_gate: Arc::new(RwLock::new(())),
            db_path: dir.path().join("workcontext.sqlite3"),
            config_path: dir.path().join("config.json"),
            config_lock: Mutex::new(()),
            organizer_generation: Default::default(),
        };
        // 真实建库（含迁移 0001/0002）。
        with_conn(&state, |_| Ok(())).expect("建库");
        Self { dir, state }
    }

    fn scope(conversations: &[&str], people: &[&str]) -> ScopeDto {
        ScopeDto {
            conversations: conversations.iter().map(|s| s.to_string()).collect(),
            people: people.iter().map(|s| s.to_string()).collect(),
        }
    }

    pub fn register(
        &self,
        id: &str,
        conversations: &[&str],
        people: &[&str],
        profile: &str,
    ) -> Result<SourceView, AppError> {
        register_source(
            &self.state,
            Some(id.to_string()),
            "测试来源".into(),
            Self::scope(conversations, people),
            profile.to_string(),
        )
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<SourceView, AppError> {
        set_source_enabled(&self.state, id.to_string(), enabled)
    }

    pub fn with_conn<T>(&self, f: impl FnOnce(&mut Connection) -> T) -> T {
        super::with_conn(&self.state, |conn| Ok(f(conn))).expect("数据库操作")
    }

    pub fn config(&self) -> SettingsFile {
        config::load(&self.state.config_path).expect("读取配置")
    }

    /// 库内 `source.enabled` 的原始值（不看账本）。
    pub fn db_enabled(&self, id: &str) -> bool {
        self.with_conn(|conn| {
            conn.query_row("SELECT enabled FROM source WHERE id = ?1", [id], |row| {
                row.get::<_, i64>(0)
            })
            .expect("来源行")
                == 1
        })
    }

    pub fn checkpoint_state(&self, id: &str) -> String {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT state FROM sync_checkpoint WHERE source_id = ?1",
                [id],
                |row| row.get(0),
            )
            .expect("检查点行")
        })
    }

    pub fn db_scope(&self, id: &str) -> Scope {
        let raw: String = self.with_conn(|conn| {
            conn.query_row("SELECT scope_json FROM source WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .expect("来源行")
        });
        serde_json::from_str(&raw).expect("白名单")
    }

    /// 本轮 `sync_now` 会触达的来源（空 = 没有有效授权的来源）。
    pub fn sync_targets(&self, only: Option<&str>) -> Result<Vec<String>, AppError> {
        let cfg = load_settings(&self.state)?;
        super::with_conn(&self.state, |conn| {
            sync::authorized_target_ids(conn, &cfg, only)
        })
    }

    pub fn reconcile(&self) -> ReconcileReport {
        self.with_conn(|conn| {
            reconcile_after_restore(&self.state, conn).expect("对账")
        })
    }

    /// 把当前库整体快照成字节（模拟「备份」）：先截断 WAL 保证内容都在主文件。
    fn snapshot_db(&self) -> Vec<u8> {
        self.with_conn(|conn| {
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .expect("截断 WAL");
        });
        std::fs::read(&self.state.db_path).expect("读取库文件")
    }

    /// 用快照覆盖当前库（模拟 restore 换回旧库；config.json 不受影响）。
    fn restore_db(&self, bytes: &[u8]) {
        for suffix in ["wal", "shm"] {
            let sidecar = PathBuf::from(format!("{}-{suffix}", self.state.db_path.display()));
            let _ = std::fs::remove_file(sidecar);
        }
        std::fs::write(&self.state.db_path, bytes).expect("写回旧库");
    }
}

fn view<'a>(views: &'a [SourceView], id: &str) -> &'a SourceView {
    views.iter().find(|v| v.id == id).expect("来源视图")
}

/// 断言操作被以 INVALID_ARGUMENT 拒绝，并返回该错误。
fn expect_invalid<T: std::fmt::Debug>(result: Result<T, AppError>, what: &str) -> AppError {
    match result {
        Ok(unexpected) => panic!("应拒绝：{what}（实际得到 {unexpected:?}）"),
        Err(err) => {
            assert_eq!(err.code, "INVALID_ARGUMENT", "{what}");
            err
        }
    }
}

// ---------------------------------------------------------------------------
// 规则 1：注册 ≠ 授权；撤权即停
// ---------------------------------------------------------------------------

#[test]
fn registering_a_source_does_not_authorize_it() {
    let fx = Fixture::new();
    let registered = fx.register("s1", &["cidA"], &[], "corp1:user1").expect("注册");
    assert!(!registered.enabled);
    assert_eq!(registered.kind, "dws_im");
    assert!(registered.authorized_at.is_none());
    assert_eq!(registered.scope_summary, "1 个会话 · 0 个联系人");
    // 库内与账本两边都未授权；检查点压制为 disabled；同步不会触达。
    assert!(!fx.db_enabled("s1"));
    assert_eq!(fx.checkpoint_state("s1"), "disabled");
    assert!(!fx.config().sources["s1"].authorized);
    assert!(
        fx.config().sources["s1"].window_start_ms.is_none(),
        "注册不批准任何读取时间窗"
    );
    assert!(fx.sync_targets(None).expect("目标解析").is_empty());
}

#[test]
fn authorization_needs_both_ledger_and_database() {
    let fx = Fixture::new();
    fx.register("s1", &["cidA"], &["p1"], "corp1:user1").expect("注册");
    let enabled = fx.set_enabled("s1", true).expect("授权");
    assert!(enabled.enabled);
    assert!(enabled.authorized_at.is_some());
    assert!(fx.db_enabled("s1"));
    assert_eq!(fx.checkpoint_state("s1"), "idle");
    // 账本记下被授权的白名单（成为上限）。
    let ledger = fx.config().sources.remove("s1").expect("账本条目");
    assert!(ledger.authorized);
    assert!(ledger.scope.conversations.contains("cidA"));
    assert!(ledger.scope.people.contains("p1"));
    // 获批读取时间窗：起点 = 授权时刻 − 默认回溯 24 小时，上界开放。
    let now = now_ms();
    let start_ms = ledger.window_start_ms.expect("授权必须带读取窗口起点");
    assert!(
        (now - 24 * 3_600_000 - 5_000..=now - 24 * 3_600_000).contains(&start_ms),
        "{start_ms}"
    );
    assert_eq!(ledger.window_end_ms, None);
    assert_eq!(fx.sync_targets(None).expect("目标解析"), vec!["s1".to_string()]);
    assert_eq!(
        fx.sync_targets(Some("s1")).expect("显式目标"),
        vec!["s1".to_string()]
    );

    // 撤权即停：库内停用 + 检查点压制 + 账本撤销，三处同时成立。
    let revoked = fx.set_enabled("s1", false).expect("撤权");
    assert!(!revoked.enabled);
    assert!(revoked.revoked_at.is_some());
    assert!(!fx.db_enabled("s1"));
    assert_eq!(fx.checkpoint_state("s1"), "disabled");
    assert!(!fx.config().sources["s1"].authorized);
    // 撤权封闭读取时间窗：终点 = 撤权时刻，不早于起点、不晚于现在。
    let end_ms = fx.config().sources["s1"]
        .window_end_ms
        .expect("撤权必须封闭读取时间窗");
    assert!(start_ms <= end_ms && end_ms <= now_ms(), "{end_ms}");
    assert!(fx.sync_targets(None).expect("目标解析").is_empty());
    expect_invalid(fx.sync_targets(Some("s1")), "已撤权来源");

    // 仅账本授权、库内停用同样不触达：不会自动启用。
    let mut cfg = fx.config();
    cfg.sources.get_mut("s1").expect("账本").authorized = true;
    config::save(&fx.state.config_path, &cfg).expect("写配置");
    assert!(!view(&list_sources(&fx.state).expect("列表"), "s1").enabled);
    assert!(fx.sync_targets(None).expect("目标解析").is_empty());
}

#[test]
fn unknown_sources_are_not_found_and_unauthorized_ones_are_refused() {
    let fx = Fixture::new();
    fx.register("s1", &["cidA"], &[], "corp1:user1").expect("注册");
    assert_eq!(fx.sync_targets(Some("nope")).expect_err("不存在").code, "NOT_FOUND");
    expect_invalid(fx.sync_targets(Some("s1")), "未授权来源");
}

// ---------------------------------------------------------------------------
// 规则 2：config.json 是授权上限——恢复旧库不能复活已撤销的授权
// ---------------------------------------------------------------------------

#[test]
fn restoring_an_old_database_cannot_resurrect_revoked_authorization() {
    let fx = Fixture::new();
    fx.register("s1", &["cidA"], &[], "corp1:user1").expect("注册");
    fx.set_enabled("s1", true).expect("授权");
    // 给检查点一些真实进度，用来验证对账不会破坏它。
    fx.with_conn(|conn| {
        conn.execute(
            "UPDATE sync_checkpoint
             SET cursor_json = '{\"window\":1}', last_success_at = 42, window_start = 7,
                 consecutive_failures = 2
             WHERE source_id = 's1'",
            [],
        )
        .expect("写检查点");
    });
    let old_db = fx.snapshot_db(); // 旧库里 s1 是 enabled=1

    // 此后本机撤权：账本落在 config.json，restore 换不回它。
    fx.set_enabled("s1", false).expect("撤权");
    fx.restore_db(&old_db);

    // 旧库确实带回了 enabled=1，但有效授权为否：列表与同步目标都如实反映。
    assert!(fx.db_enabled("s1"), "旧库确实复活了库内标志");
    assert!(!view(&list_sources(&fx.state).expect("列表"), "s1").enabled);
    assert!(fx.sync_targets(None).expect("目标解析").is_empty());

    // restore 之后对账：库内状态收回到账本，检查点进度原样保留。
    let report = fx.reconcile();
    assert_eq!(report.revoked, vec!["s1".to_string()]);
    assert!(report.narrowed.is_empty());
    assert!(!fx.db_enabled("s1"));
    assert_eq!(fx.checkpoint_state("s1"), "disabled");
    let (cursor, last_success, window_start): (Option<String>, Option<i64>, Option<i64>) =
        fx.with_conn(|conn| {
            conn.query_row(
                "SELECT cursor_json, last_success_at, window_start FROM sync_checkpoint
                 WHERE source_id = 's1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("读检查点")
        });
    assert_eq!(cursor.as_deref(), Some("{\"window\":1}"));
    assert_eq!(last_success, Some(42));
    assert_eq!(window_start, Some(7));
    // 留痕：system 撤权事件。
    let revoked_events: i64 = fx.with_conn(|conn| {
        conn.query_row(
            "SELECT COUNT(*) FROM work_event
             WHERE target_id = 's1' AND event_type = 'revoked' AND actor = 'system'",
            [],
            |row| row.get(0),
        )
        .expect("计数")
    });
    assert_eq!(revoked_events, 1);

    // 对账幂等：重复调用不再改动。
    let again = fx.reconcile();
    assert!(again.revoked.is_empty() && again.narrowed.is_empty());
}

#[test]
fn restore_never_auto_enables_a_source() {
    let fx = Fixture::new();
    fx.register("s1", &["cidA"], &[], "corp1:user1").expect("注册");
    let before_authorization = fx.snapshot_db(); // 此时库内 enabled=0

    fx.set_enabled("s1", true).expect("授权"); // 账本 authorized=true
    fx.restore_db(&before_authorization); // 恢复到授权之前的库：库内 enabled=0

    assert!(fx.config().sources["s1"].authorized);
    assert!(!fx.db_enabled("s1"));
    // 对账不替用户启用：账本已授权、库内停用 → 保持停用，须显式重新授权。
    let report = fx.reconcile();
    assert!(report.revoked.is_empty() && report.narrowed.is_empty());
    assert!(!fx.db_enabled("s1"));
    assert!(fx.sync_targets(None).expect("目标解析").is_empty());
    assert!(!view(&list_sources(&fx.state).expect("列表"), "s1").enabled);

    // 显式重新授权后才恢复。
    assert!(fx.set_enabled("s1", true).expect("重新授权").enabled);
    assert_eq!(fx.sync_targets(None).expect("目标解析"), vec!["s1".to_string()]);
}

#[test]
fn restore_revokes_sources_the_ledger_does_not_know() {
    let fx = Fixture::new();
    // 库来自别处（或账本被清空）：库里有启用中的陌生来源。
    fx.with_conn(|conn| {
        conn.execute(
            "INSERT INTO source(id, kind, display_name, scope_json, enabled, authorized_at, created_at)
             VALUES('stranger', 'dws_im', '外来来源', '{\"conversations\":[\"cidX\"]}', 1, 1, 1)",
            [],
        )
        .expect("插入外来来源");
        conn.execute(
            "INSERT INTO sync_checkpoint(source_id, state, consecutive_failures, updated_at)
             VALUES('stranger', 'idle', 0, 1)",
            [],
        )
        .expect("插入检查点");
    });
    // 对账之前：账本不认识它 → 已不是同步目标。
    assert!(fx.sync_targets(None).expect("目标解析").is_empty());

    let report = fx.reconcile();
    assert_eq!(report.revoked, vec!["stranger".to_string()]);
    assert!(!fx.db_enabled("stranger"));
    assert_eq!(fx.checkpoint_state("stranger"), "disabled");
}

#[test]
fn restore_narrows_a_wider_whitelist_to_the_ledger() {
    let fx = Fixture::new();
    fx.register("s1", &["cidA", "cidB"], &[], "corp1:user1").expect("注册");
    fx.set_enabled("s1", true).expect("授权");
    let old_db = fx.snapshot_db(); // 旧库白名单：cidA + cidB

    // 此后本机收窄白名单（收窄不触发撤权，授权保留）。
    let narrowed = fx
        .register("s1", &["cidA"], &[], "corp1:user1")
        .expect("收窄");
    assert!(narrowed.enabled);
    assert_eq!(fx.db_scope("s1").conversations.len(), 1);

    // 恢复旧库：库内白名单被放宽回 cidA + cidB。
    fx.restore_db(&old_db);
    assert_eq!(fx.db_scope("s1").conversations.len(), 2);

    let report = fx.reconcile();
    assert_eq!(report.narrowed, vec!["s1".to_string()]);
    assert!(report.revoked.is_empty());
    let after = fx.db_scope("s1");
    assert_eq!(after.conversations.len(), 1);
    assert!(after.conversations.contains("cidA"));
    // 收窄不影响授权与检查点。
    assert!(fx.db_enabled("s1"));
    assert_eq!(fx.sync_targets(None).expect("目标解析"), vec!["s1".to_string()]);
}

// ---------------------------------------------------------------------------
// 规则 3 的输入侧：白名单/profile 形状校验；扩大白名单不继承旧授权
// ---------------------------------------------------------------------------

#[test]
fn expanding_the_whitelist_drops_the_old_authorization() {
    let fx = Fixture::new();
    fx.register("s1", &["cidA"], &[], "corp1:user1").expect("注册");
    fx.set_enabled("s1", true).expect("授权");

    let expanded = fx
        .register("s1", &["cidA", "cidB"], &[], "corp1:user1")
        .expect("扩大白名单");
    assert!(!expanded.enabled, "扩大白名单不得沿用旧授权");
    assert!(!fx.db_enabled("s1"));
    assert_eq!(fx.checkpoint_state("s1"), "disabled");
    assert!(!fx.config().sources["s1"].authorized);
    assert!(
        fx.config().sources["s1"].window_end_ms.is_some(),
        "授权作废须封闭读取时间窗"
    );
    assert!(fx.sync_targets(None).expect("目标解析").is_empty());
    // 账本记下新的（更大的）白名单，但授权需用户重新给出。
    assert_eq!(fx.config().sources["s1"].scope.conversations.len(), 2);

    // 重新显式授权后恢复；之后仅收窄则保留授权。
    fx.set_enabled("s1", true).expect("重新授权");
    let narrowed = fx.register("s1", &["cidB"], &[], "corp1:user1").expect("收窄");
    assert!(narrowed.enabled);
    assert!(fx.config().sources["s1"].authorized);
    assert_eq!(fx.sync_targets(None).expect("目标解析"), vec!["s1".to_string()]);
}

#[test]
fn changing_the_profile_drops_the_old_authorization() {
    let fx = Fixture::new();
    fx.register("s1", &["cidA"], &[], "corp1:user1").expect("注册");
    fx.set_enabled("s1", true).expect("授权");
    let swapped = fx.register("s1", &["cidA"], &[], "corp2:user2").expect("换账号");
    assert!(!swapped.enabled, "更换账号不得沿用旧授权");
    assert!(!fx.db_enabled("s1"));
    assert_eq!(fx.config().sources["s1"].profile, "corp2:user2");
    assert!(!fx.config().sources["s1"].authorized);
    assert!(
        fx.config().sources["s1"].window_end_ms.is_some(),
        "换账号即撤权，须封闭读取时间窗"
    );
}

#[test]
fn narrowing_or_revoking_a_source_invalidates_the_worker_generation() {
    let fx = Fixture::new();
    let generation = || fx.state.organizer_generation.load(std::sync::atomic::Ordering::Acquire);
    fx.register("demo", &["c1","c2"], &[], "demo:user").unwrap();
    fx.set_enabled("demo",true).unwrap();
    let before = generation();
    fx.register("demo", &["c1"], &[], "demo:user").unwrap();
    assert_ne!(generation(),before);
    let narrowed = generation();
    fx.register("demo", &["c1"], &[], "demo:user").unwrap();
    assert_eq!(generation(),narrowed);
    fx.set_enabled("demo",false).unwrap();
    assert_ne!(generation(),narrowed);
}

#[test]
fn external_grant_is_offline_workspace_bound_and_revoked_on_rebind() {
    use workcontext_app::external_access::{authorize, authorize_archive, AccessError};
    let fx = Fixture::new();
    let root = fx.dir.path();
    let profile = "corp1:user1";
    fx.register("s1", &["cidA"], &[], profile).unwrap();
    assert_eq!(authorize(root), Err(AccessError::Disabled));
    let before_binding = fx.state.organizer_generation.load(std::sync::atomic::Ordering::Acquire);
    crate::external_access::set(&fx.state, true, Some(profile.into()), false).unwrap();
    assert_ne!(fx.state.organizer_generation.load(std::sync::atomic::Ordering::Acquire),before_binding);
    assert_eq!(authorize(root).unwrap(), profile);
    assert_eq!(authorize_archive(root, profile), Err(AccessError::Archive));
    assert!(!fx.config().sources["s1"].authorized);
    let copy = tempfile::tempdir().unwrap();
    std::fs::copy(&fx.state.config_path, copy.path().join("config.json")).unwrap();
    assert_eq!(authorize(copy.path()), Err(AccessError::Binding));
    let mut wrong = fx.config();
    wrong.sources.get_mut("s1").unwrap().profile = "corp1:other".into();
    config::save(&fx.state.config_path, &wrong).unwrap();
    assert_eq!(authorize(root), Err(AccessError::Binding));
    fx.register("s1", &["cidA"], &[], profile).unwrap();
    assert_eq!(authorize(root), Err(AccessError::Disabled));
}

#[test]
fn archive_authority_checks_its_own_binding_on_each_request() {
    use workcontext_app::external_access::{authorize, authorize_archive, AccessError};
    let fx = Fixture::new();
    let root = fx.dir.path();
    let profile = "corp:me";
    fx.register("work-source", &["cidA"], &[], profile).unwrap();
    assert!(crate::external_access::set(&fx.state, true, Some(profile.into()), true).is_err());
    std::fs::create_dir(root.join("archive")).unwrap();
    let db = Connection::open(root.join("archive/archive.sqlite3")).unwrap();
    db.execute_batch("CREATE TABLE archive_state(key TEXT PRIMARY KEY,value TEXT); INSERT INTO archive_state VALUES('profile','corp:me')").unwrap();
    crate::external_access::set(&fx.state, true, Some(profile.into()), true).unwrap();
    assert_eq!(authorize_archive(root, profile), Ok(()));
    db.execute("UPDATE archive_state SET value='corp:other' WHERE key='profile'", []).unwrap();
    assert_eq!(authorize_archive(root, profile), Err(AccessError::Archive));
    assert_eq!(authorize(root).unwrap(), profile);
    crate::external_access::set(&fx.state, false, None, false).unwrap();
    assert_eq!(authorize(root), Err(AccessError::Disabled));
}

#[test]
fn registration_rejects_empty_or_unsafe_inputs() {
    let fx = Fixture::new();
    expect_invalid(fx.register("s1", &[], &[], "corp1:user1"), "空白名单");

    for bad in ["-rf", "--group=x", "has space", "line\nbreak", ""] {
        expect_invalid(fx.register("s1", &[bad], &[], "corp1:user1"), "白名单条目");
    }
    for bad in ["corp1", "corp1:", ":user1", "a:b:c", "corp1:-x", "corp 1:user1"] {
        expect_invalid(fx.register("s1", &["cidA"], &[], bad), "profile");
    }
    // 被拒绝的注册不留下任何痕迹（库、账本都没有来源）。
    assert!(list_sources(&fx.state).expect("列表").is_empty());
    assert!(fx.config().sources.is_empty());
}

// ---------------------------------------------------------------------------
// 规则 4：出域策略默认 none，变更需确认词与已批准模型
// ---------------------------------------------------------------------------

#[test]
fn egress_defaults_to_none_and_changes_need_confirmation() {
    let fx = Fixture::new();
    let current = current_egress(&fx.state).expect("读取");
    assert_eq!(current.policy, "none");
    assert_eq!(current.approved_model, None);
    assert_eq!(current.judge_mode, "mock");

    expect_invalid(
        set_egress(&fx.state, "anything".into(), None, "anything".into()),
        "非法策略",
    );
    expect_invalid(set_egress(&fx.state, "full".into(), None, "full".into()), "未指明模型");
    expect_invalid(
        set_egress(&fx.state, "full".into(), Some("vendor/model-x".into()), "none".into()),
        "确认词不匹配",
    );
    // 屡次被拒后策略仍是 none，也没有留下审计事件。
    assert_eq!(current_egress(&fx.state).expect("读取").policy, "none");
    assert_eq!(count_events(&fx, "egress"), 0);

    let approved = set_egress(
        &fx.state,
        "full".into(),
        Some("vendor/model-x".into()),
        "full".into(),
    )
    .expect("确认后放行");
    assert_eq!(approved.policy, "full");
    assert_eq!(approved.approved_model.as_deref(), Some("vendor/model-x"));
    // 审计留痕：actor=human 的 work_event。
    assert_eq!(count_events(&fx, "egress"), 1);

    // 收紧回 none 永远允许，且清掉已批准模型。
    let granted = fx.state.organizer_generation.load(std::sync::atomic::Ordering::Acquire);
    let tightened = set_egress(&fx.state, "none".into(), None, "none".into()).expect("收紧");
    assert_ne!(fx.state.organizer_generation.load(std::sync::atomic::Ordering::Acquire),granted);
    assert_eq!(tightened.policy, "none");
    assert_eq!(tightened.approved_model, None);
}

fn count_events(fx: &Fixture, target_id: &str) -> i64 {
    fx.with_conn(|conn| {
        conn.query_row(
            "SELECT COUNT(*) FROM work_event
             WHERE target_type = 'source' AND target_id = ?1 AND actor = 'human'",
            [target_id],
            |row| row.get(0),
        )
        .expect("计数")
    })
}

#[test]
fn egress_audit_rows_record_block_and_allow() {
    let fx = Fixture::new();
    let refs = vec!["sr_1".to_string(), "sr_2".to_string()];
    fx.with_conn(|conn| {
        egress::log_blocked(conn, 7, "none", None, &refs, None).expect("写拦截");
        egress::log_allowed(conn, 8, "full", Some("vendor/m"), &refs, "run1").expect("写放行");
    });
    let rows: Vec<AuditRow> = fx.with_conn(|conn| {
        let mut stmt = conn
            .prepare("SELECT at, policy, decision, target, run_id FROM egress_log ORDER BY at")
            .expect("查询");
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .expect("映射")
        .collect::<Result<_, _>>()
        .expect("收集")
    });
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], (7, "none".into(), "blocked".into(), None, None));
    assert_eq!(
        rows[1],
        (
            8,
            "full".into(),
            "allowed".into(),
            Some("vendor/m".into()),
            Some("run1".into())
        )
    );
    let refs_json: String = fx.with_conn(|conn| {
        conn.query_row("SELECT refs_json FROM egress_log LIMIT 1", [], |row| row.get(0))
            .expect("refs")
    });
    assert_eq!(refs_json, r#"["sr_1","sr_2"]"#);
}

// ---------------------------------------------------------------------------
// IPC 契约：逐字段对齐 ui/IPC.md §1
// ---------------------------------------------------------------------------

fn keys(value: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .expect("对象")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

#[test]
fn wire_dtos_match_ipc_contract_field_by_field() {
    let fx = Fixture::new();
    fx.register("s1", &["cidA", "cidB"], &["p1"], "corp1:user1").expect("注册");
    let sources = serde_json::to_value(list_sources(&fx.state).expect("列表")).expect("序列化");
    assert_eq!(
        keys(&sources[0]),
        [
            "authorizedAt",
            "createdAt",
            "displayName",
            "enabled",
            "id",
            "kind",
            "revokedAt",
            "scopeSummary",
        ]
    );
    assert_eq!(sources[0]["scopeSummary"], "2 个会话 · 1 个联系人");
    assert_eq!(sources[0]["enabled"], false);

    let statuses =
        serde_json::to_value(sync_status_of(&fx.state).expect("状态")).expect("序列化");
    assert_eq!(
        keys(&statuses[0]),
        [
            "consecutiveFailures",
            "cursorAt",
            "displayName",
            "lastAttemptAt",
            "lastErrorCode",
            "lastErrorMsg",
            "lastSuccessAt",
            "sourceId",
            "state",
            "windowEnd",
            "windowStart",
        ]
    );
    assert_eq!(statuses[0]["state"], "disabled");
    assert_eq!(statuses[0]["lastSuccessAt"], serde_json::Value::Null);
    assert_eq!(statuses[0]["consecutiveFailures"], 0);
}

#[test]
fn every_source_lifecycle_step_leaves_a_human_or_system_event() {
    let fx = Fixture::new();
    fx.register("s1", &["cidA"], &[], "corp1:user1").expect("注册");
    fx.set_enabled("s1", true).expect("授权");
    fx.set_enabled("s1", false).expect("撤权");
    let events: Vec<String> = fx.with_conn(|conn| {
        let mut stmt = conn
            .prepare("SELECT event_type FROM work_event WHERE target_id = 's1' ORDER BY at, rowid")
            .expect("查询");
        stmt.query_map([], |row| row.get(0))
            .expect("映射")
            .collect::<Result<_, _>>()
            .expect("收集")
    });
    assert_eq!(events, vec!["created", "authorized", "revoked"]);
}

// ---------------------------------------------------------------------------
// 纯函数
// ---------------------------------------------------------------------------

#[test]
fn scope_summary_variants() {
    assert_eq!(
        scope_summary(r#"{"conversations":["cidA"],"people":[]}"#),
        "1 个会话 · 0 个联系人"
    );
    assert_eq!(
        scope_summary(r#"{"conversations":[],"people":["p1","p2"]}"#),
        "0 个会话 · 2 个联系人"
    );
    assert_eq!(scope_summary("{}"), "未设置白名单（不读取任何记录）");
    assert_eq!(scope_summary("not json"), "白名单数据损坏");
}

#[test]
fn id_tokens_reject_injection_shapes() {
    assert!(is_id_token("cidabc123"));
    assert!(is_id_token("012345"));
    assert!(is_id_token("zhangsan@example.com"));
    assert!(!is_id_token(""));
    assert!(!is_id_token("-rf"));
    assert!(!is_id_token("has space"));
    assert!(!is_id_token("--group=x"));
    assert!(!is_id_token("换行\n"));
}

#[test]
fn profile_needs_two_id_segments() {
    assert!(is_profile("ding1234:manager01"));
    assert!(!is_profile("ding1234"));
    assert!(!is_profile("ding1234:"));
    assert!(!is_profile("a:b:c"));
    assert!(!is_profile("ding1234:-x"));
}

#[test]
fn scope_expansion_is_detected_against_ceiling() {
    let scope = |conversations: &[&str], people: &[&str]| Scope {
        conversations: conversations.iter().map(|s| s.to_string()).collect(),
        people: people.iter().map(|s| s.to_string()).collect(),
    };
    let ceiling = scope(&["c1"], &["p1"]);
    assert!(within(&scope(&["c1"], &[]), &ceiling));
    assert!(within(&ceiling, &ceiling));
    assert!(!within(&scope(&["c1", "c2"], &[]), &ceiling));
    assert!(!within(&scope(&["c1"], &["p2"]), &ceiling));
}

#[test]
fn only_fixed_names_resolve_to_absolute_paths() {
    // 找不到时给出明确错误而不是回落到相对路径；找到时必为绝对路径。
    match trusted_exe(DWS_EXE) {
        Ok(path) => assert!(path.is_absolute(), "{}", path.display()),
        Err(err) => {
            assert_eq!(err.code, "AGENT_UNREACHABLE");
            assert!(!err.retryable);
        }
    }
    for dir in [DWS_EXE, OMP_EXE].into_iter().flat_map(fixed_install_dirs) {
        assert!(dir.is_absolute(), "{}", dir.display());
    }
}

#[test]
fn ledger_is_persisted_beside_the_database_in_the_temp_directory() {
    let fx = Fixture::new();
    assert!(fx.state.db_path.starts_with(fx.dir.path()));
    assert!(fx.state.config_path.starts_with(fx.dir.path()));
    assert!(!fx.state.config_path.exists(), "未变更配置时不落盘");
    fx.register("s1", &["cidA"], &[], "corp1:user1").expect("注册");
    assert!(fx.state.config_path.is_file(), "注册后账本落盘");
    let text = std::fs::read_to_string(&fx.state.config_path).expect("读取配置");
    assert!(text.contains("\"authorized\": false"), "新来源默认未授权：{text}");
}

#[test]
fn re_authorizing_opens_a_fresh_window_and_lookback_applies_at_authorization() {
    let fx = Fixture::new();
    fx.register("s1", &["cidA"], &[], "corp1:user1").expect("注册");
    fx.set_enabled("s1", true).expect("授权");
    fx.set_enabled("s1", false).expect("撤权");
    let closed_at = fx.config().sources["s1"].window_end_ms;
    assert!(closed_at.is_some());

    // 把回溯改为 2 小时：只影响之后的授权，已封闭的窗口原样保留。
    let mut cfg = fx.config();
    cfg.sync.first_pull_lookback_hours = 2;
    config::save(&fx.state.config_path, &cfg).expect("写配置");
    assert_eq!(fx.config().sources["s1"].window_end_ms, closed_at);

    let before = now_ms();
    let view = fx.set_enabled("s1", true).expect("重新授权");
    let after = now_ms();
    assert!(view.enabled);
    let entry = fx.config().sources.remove("s1").expect("账本");
    let start_ms = entry.window_start_ms.expect("新窗口起点");
    assert!(
        (before - 2 * 3_600_000..=after - 2 * 3_600_000).contains(&start_ms),
        "{start_ms}"
    );
    assert_eq!(entry.window_end_ms, None, "重新授权打开新的上界");
}

#[test]
fn a_hand_edited_out_of_range_lookback_still_bounds_the_window() {
    let fx = Fixture::new();
    fx.register("s1", &["cidA"], &[], "corp1:user1").expect("注册");
    std::fs::write(
        &fx.state.config_path,
        r#"{"schema":1,"sync":{"firstPullLookbackHours":999999},
            "sources":{"s1":{"profile":"corp1:user1","scope":{"conversations":["cidA"]}}}}"#,
    )
    .expect("写配置");

    let before = now_ms();
    assert!(fx.set_enabled("s1", true).expect("授权").enabled);
    let start_ms = fx.config().sources["s1"].window_start_ms.expect("窗口起点");
    assert!(
        start_ms >= before - MAX_FIRST_PULL_LOOKBACK_HOURS as i64 * 3_600_000,
        "越界回溯被钳制到 7 天上界：{start_ms}"
    );
}
