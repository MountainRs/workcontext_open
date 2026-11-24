//! External callers can submit proposals. Only desktop review consumes a reviewed snapshot.
use crate::{context::{require_scope, save_scope_in, validate_text, SaveScopeArgs, Scope}, items::ItemsError};
use serde::{Deserialize, Serialize};
use workcontext_store::{now_ms, rusqlite::{self, params, Connection, OptionalExtension, Row, TransactionBehavior}};

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SuggestArgs {
    pub producer: String,
    pub request_id: String,
    pub scope_id: Option<String>,
    pub base_version: Option<i64>,
    pub kind: String,
    pub title: String,
    pub goal: String,
    pub background: String,
    pub reason: String,
    pub source: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    pub suggestion_id: String,
    pub status: String,
    pub scope_id: Option<String>,
}

fn receipt(row: &Row<'_>) -> rusqlite::Result<Receipt> {
    Ok(Receipt { suggestion_id: row.get(0)?, status: row.get(1)?, scope_id: row.get(2)? })
}

fn rejected(message: &str) -> ItemsError {
    ItemsError::ValidationRejected { message: message.into(), hint: None }
}

pub fn suggest(db: &mut Connection, profile: &str, args: &SuggestArgs) -> Result<Receipt, ItemsError> {
    validate_text(profile, 1024, true)?;
    validate_text(&args.producer, 256, true)?;
    validate_text(&args.request_id, 256, true)?;
    validate_text(&args.title, 512, true)?;
    validate_text(&args.goal, 65536, false)?;
    validate_text(&args.background, 65536, false)?;
    validate_text(&args.reason, 4096, true)?;
    validate_text(&args.source, 4096, true)?;
    if !matches!(args.kind.as_str(), "project" | "domain") || args.title.trim() != args.title {
        return Err(ItemsError::InvalidArgument { message: "类型无效或名称含首尾空白。".into() });
    }
    let payload = serde_json::to_string(args).map_err(|_| rejected("建议内容无效"))?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let existing = tx.query_row("SELECT id,status,result_scope_id,payload FROM scope_proposal WHERE profile=?1 AND producer=?2 AND request_id=?3",
        params![profile,args.producer,args.request_id], |r| Ok((receipt(r)?, r.get::<_, String>(3)?))).optional()?;
    if let Some((saved, previous)) = existing {
        if previous != payload { return Err(rejected("同一请求 ID 已提交不同内容。")); }
        return Ok(saved);
    }
    match (&args.scope_id, args.base_version) {
        (None, None) => (),
        (Some(id), Some(version)) if version > 0 && args.kind == "project" => {
            let old = require_scope(&tx, profile, id)?;
            if old.kind != args.kind || old.version != version || old.title != args.title {
                return Err(rejected("请按当前项目名称和版本提交目标／背景变更。"));
            }
        }
        _ => return Err(rejected("修改项目需要范围 ID 和基准版本。")),
    }
    let saved = tx.query_row("INSERT INTO scope_proposal(profile,producer,request_id,payload,created_at) VALUES (?1,?2,?3,?4,?5) RETURNING id,status,result_scope_id",
        params![profile,args.producer,args.request_id,payload,now_ms()], receipt)?;
    tx.commit()?;
    Ok(saved)
}

#[derive(Serialize)]
#[serde(rename_all="camelCase")]
pub struct Pending { pub id: String, pub title: String, pub kind: String }

pub fn pending(db: &Connection, profile: &str) -> Result<Vec<Pending>, ItemsError> {
    let mut q = db.prepare("SELECT id,json_extract(payload,'$.title'),json_extract(payload,'$.kind') FROM scope_proposal WHERE profile=?1 AND status='pending' ORDER BY created_at,id LIMIT 50")?;
    let rows = q.query_map([profile], |r| Ok(Pending { id: r.get(0)?, title: r.get(1)?, kind: r.get(2)? }))?.collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Constructed from the database, never deserialized from IPC or the CLI.
pub struct Review {
    profile: String,
    receipt: Receipt,
    args: SuggestArgs,
    old: Option<Scope>,
}
impl Review {
    pub fn proposal(&self) -> &SuggestArgs { &self.args }
    pub fn original(&self) -> Option<&Scope> { self.old.as_ref() }
    pub fn status(&self) -> &str { &self.receipt.status }
    pub fn is_stale(&self) -> bool { self.old.as_ref().map(|scope| scope.version) != self.args.base_version }
}

fn read_review(db: &Connection, profile: &str, id: &str) -> Result<Review, ItemsError> {
    let (saved, raw) = db.query_row("SELECT id,status,result_scope_id,payload FROM scope_proposal WHERE profile=?1 AND id=?2",
        params![profile,id], |r| Ok((receipt(r)?, r.get::<_, String>(3)?))).optional()?.ok_or(ItemsError::NotFound)?;
    let args: SuggestArgs = serde_json::from_str(&raw).map_err(|_| rejected("建议内容损坏"))?;
    let old = match &args.scope_id {
        Some(id) => match require_scope(db, profile, id) {
            Ok(scope) => Some(scope), Err(ItemsError::NotFound) => None, Err(e) => return Err(e),
        },
        None => None,
    };
    Ok(Review { profile: profile.into(), receipt: saved, args, old })
}

pub fn review(db: &Connection, profile: &str, id: &str) -> Result<Review, ItemsError> {
    let tx = db.unchecked_transaction()?;
    let snapshot = read_review(&tx, profile, id)?;
    tx.commit()?;
    Ok(snapshot)
}

pub fn decide(db: &mut Connection, profile: &str, shown: Review, approved: bool) -> Result<Receipt, ItemsError> {
    if shown.profile != profile { return Err(rejected("账号已切换，请重新核对。")); }
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = read_review(&tx, profile, &shown.receipt.suggestion_id)?;
    if current.args != shown.args { return Err(rejected("建议内容已变更，请重新核对。")); }
    if current.status() != "pending" { return Ok(current.receipt); }
    let (status, result) = if current.is_stale() || current.old != shown.old {
        ("stale", None)
    } else if !approved {
        ("declined", None)
    } else {
        let a = current.args;
        let scope = save_scope_in(&tx, profile, &SaveScopeArgs { id: a.scope_id, version: a.base_version, kind: a.kind,
            title: a.title, goal: a.goal, background: a.background })?;
        ("accepted", Some(scope.id))
    };
    let saved = tx.query_row("UPDATE scope_proposal SET status=?3,result_scope_id=?4,decided_at=?5 WHERE profile=?1 AND id=?2 AND status='pending' RETURNING id,status,result_scope_id",
        params![profile,shown.receipt.suggestion_id,status,result,now_ms()], receipt)?;
    tx.commit()?;
    Ok(saved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{save_scope, scopes};

    struct TestDir(std::path::PathBuf);
    impl Drop for TestDir {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }
    fn database() -> (TestDir, Connection) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = TestDir(std::env::temp_dir().join(format!("wc-suggestions-{}-{}-{}",
            std::process::id(), now_ms(), NEXT.fetch_add(1, Ordering::Relaxed))));
        std::fs::create_dir(&dir.0).unwrap();
        let db = workcontext_store::open(&dir.0.join("work.sqlite3")).unwrap();
        (dir, db)
    }

    fn args() -> SuggestArgs {
        SuggestArgs { producer: "codex".into(), request_id: "request-1".into(), scope_id: None,
            base_version: None, kind: "project".into(), title: "事故复盘".into(), goal: "复核日志".into(),
            background: "基线".into(), reason: "需要追踪复测结果".into(), source: "record-1".into() }
    }

    #[test]
    fn forged_confirmation_retry_conflict_decline_and_isolation() {
        let (_dir, mut db) = database();
        let mut a = args();
        for field in ["approved", "actor", "status", "profile"] {
            let mut raw = serde_json::to_value(&a).unwrap();
            raw[field] = true.into();
            assert!(serde_json::from_value::<SuggestArgs>(raw).is_err());
        }
        a.kind = "domain".into();
        let id = suggest(&mut db, "a", &a).unwrap().suggestion_id;
        assert_eq!(suggest(&mut db, "a", &a).unwrap().suggestion_id, id);
        assert!(scopes(&db, "a").unwrap().is_empty());
        assert!(pending(&db, "b").unwrap().is_empty());
        assert!(review(&db, "b", &id).is_err());
        let original = review(&db, "a", &id).unwrap();
        assert!(decide(&mut db, "b", original, true).is_err());
        let shown = review(&db, "a", &id).unwrap();
        let duplicate = review(&db, "a", &id).unwrap();
        assert_eq!(decide(&mut db, "a", shown, false).unwrap().status, "declined");
        assert_eq!(decide(&mut db, "a", duplicate, true).unwrap().status, "declined");
        assert_eq!(suggest(&mut db, "a", &a).unwrap().status, "declined");
        a.title.push('二');
        assert!(suggest(&mut db, "a", &a).is_err());
        assert!(scopes(&db, "a").unwrap().is_empty());
        assert!(pending(&db, "a").unwrap().is_empty());
    }

    #[test]
    fn duplicate_acceptance_stale_base_and_snapshot_content_binding() {
        let (_dir, mut db) = database();
        let mut a = args();
        let id = suggest(&mut db, "a", &a).unwrap().suggestion_id;
        let shown = review(&db, "a", &id).unwrap();
        let duplicate = review(&db, "a", &id).unwrap();
        let scope_id = decide(&mut db, "a", shown, true).unwrap().scope_id.unwrap();
        assert_eq!(decide(&mut db, "a", duplicate, true).unwrap().scope_id.as_ref(), Some(&scope_id));
        assert_eq!(scopes(&db, "a").unwrap().len(), 1);
        a.scope_id = Some(scope_id.clone());
        a.base_version = Some(1);
        a.request_id = "update".into();
        a.goal = "重做复测".into();
        assert!(suggest(&mut db, "b", &a).is_err());
        let change = suggest(&mut db, "a", &a).unwrap().suggestion_id;
        let shown = review(&db, "a", &change).unwrap();
        save_scope(&mut db, "a", &SaveScopeArgs { id: Some(scope_id.clone()), version: Some(1), kind: "project".into(),
            title: a.title.clone(), goal: "本人已修订".into(), background: String::new() }).unwrap();
        assert_eq!(decide(&mut db, "a", shown, true).unwrap().status, "stale");
        assert_eq!(suggest(&mut db, "a", &a).unwrap().status, "stale");
        assert_eq!(require_scope(&db, "a", &scope_id).unwrap().goal, "本人已修订");
        a.base_version = Some(2);
        a.request_id = "updated-base".into();
        let change = suggest(&mut db, "a", &a).unwrap().suggestion_id;
        let shown = review(&db, "a", &change).unwrap();
        a.background.push('二');
        db.execute("UPDATE scope_proposal SET payload=?1 WHERE id=?2", params![serde_json::to_string(&a).unwrap(),change]).unwrap();
        assert!(decide(&mut db, "a", shown, true).is_err());
        let refreshed = review(&db, "a", &change).unwrap();
        assert_eq!(decide(&mut db, "a", refreshed, true).unwrap().status, "accepted");
        let scope = require_scope(&db, "a", &scope_id).unwrap();
        assert_eq!((scope.goal,scope.background,scope.version), (a.goal,a.background,3));
    }
}
