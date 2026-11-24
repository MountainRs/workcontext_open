use super::*;
use std::{fs, path::Path};

fn reopen(root: &Path) -> AppState {
    let db_path = root.join("work.sqlite3");
    AppState { db: workcontext_store::open(&db_path).unwrap().into(), db_path }
}

fn archive(path: &Path, account: Option<&str>) {
    let db = Connection::open(path).unwrap();
    db.execute_batch("CREATE TABLE IF NOT EXISTS archive_state(key TEXT PRIMARY KEY,value TEXT); DELETE FROM archive_state;").unwrap();
    if let Some(account) = account {
        db.execute("INSERT INTO archive_state VALUES ('profile',?1)", [account]).unwrap();
    }
}

fn add(state: &AppState, body: &str, scopes: Vec<String>) -> Result<String, String> {
    run(state, None, |db, profile| app::add_record(db, profile, &app::AddRecordArgs {
        title: "记录".into(), content: body.into(), scope_ids: scopes,
        materials: vec![], kind: "experience".into(), occurred_at: None,
    })).map(|detail| detail.record.id)
}

fn original(state: &AppState, local: Option<bool>, id: &str) -> Result<String, String> {
    run(state, local, |db, profile| app::get_record(db, profile, id)).map(|detail| detail.original)
}

fn search(state: &AppState, local: Option<bool>, text: &str) -> Vec<String> {
    run(state, local, |db, profile| app::records(db, profile, &app::RecordsArgs {
        query: Some(text.into()), ..Default::default()
    })).unwrap().items.into_iter().map(|row| row.id).collect()
}

#[test]
fn interrupted_and_restored_binding_isolates_record_reads_and_writes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let path = root.join("archive/archive.sqlite3");
    fs::create_dir(path.parent().unwrap()).unwrap();
    let local = add(&reopen(root), "绑定前原文", vec![]).unwrap();

    archive(&path, Some("corp:first"));
    let state = reopen(root);
    let private = add(&state, "账号原文", vec![]).unwrap();
    let scope = run(&state, None, |db, account| app::save_scope(db, account, &app::SaveScopeArgs {
        id: None, version: None, kind: "project".into(), title: "私有项目".into(),
        goal: String::new(), background: String::new(),
    })).unwrap().id;
    drop(state);

    let mut supplements = Vec::new();
    for interrupted in ["missing", "unbound", "invalid", "corrupt"] {
        match interrupted {
            "missing" => fs::remove_file(&path).unwrap(),
            "unbound" => archive(&path, None),
            "invalid" => archive(&path, Some("@local")),
            _ => fs::write(&path, b"invalid sqlite file").unwrap(),
        }
        let state = reopen(root);
        assert_eq!(search(&state, None, "绑定前原文"), vec![local.clone()]);
        assert_eq!(original(&state, None, &local).unwrap(), "绑定前原文");
        assert!(search(&state, None, "账号原文").is_empty());
        assert!(original(&state, None, &private).is_err());
        assert!(add(&state, "不可见范围", vec![scope.clone()]).is_err());
        assert!(search(&state, None, "不可见范围").is_empty());
        assert_eq!(set_item_scope(&state, "item", "v1", None).unwrap_err(), bound_profile(&state).unwrap_err());
        supplements.push(add(&state, interrupted, vec![]).unwrap());
    }

    fs::remove_file(&path).unwrap();
    archive(&path, Some("corp:second"));
    let state = reopen(root);
    assert!(original(&state, None, &private).is_err());
    assert!(add(&state, "跨账号范围", vec![scope.clone()]).is_err());
    drop(state);

    archive(&path, Some("corp:first"));
    let state = reopen(root);
    assert_eq!(original(&state, None, &private).unwrap(), "账号原文");
    assert!(original(&state, Some(true), &private).is_err());
    assert!(add(&state, "恢复后关联", vec![scope]).is_ok());
    assert_eq!(original(&state, Some(true), &local).unwrap(), "绑定前原文");
    for id in supplements.iter().chain([&local]) {
        assert!(original(&state, None, id).is_err());
        assert!(original(&state, Some(true), id).is_ok());
    }
}
