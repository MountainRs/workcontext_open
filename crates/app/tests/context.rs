use std::{fs, path::PathBuf, sync::atomic::{AtomicUsize, Ordering}};
use workcontext_app::context::*;
use workcontext_app::items::ItemsError;
use workcontext_store::{open, rusqlite::Connection};

struct TestDir(PathBuf);
impl TestDir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!("wc-context-{}-{}-{}", std::process::id(),
            workcontext_store::now_ms(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn open(&self) -> Connection { open(&self.0.join("work.db")).unwrap() }
}
impl Drop for TestDir { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

fn scope_args(kind: &str) -> SaveScopeArgs {
    SaveScopeArgs { id: None, version: None, title: "同名范围".into(), kind: kind.into(),
        goal: "目标".into(), background: "项目背景".into() }
}
fn entry(kind: &str, content: &str) -> AddRecordArgs {
    AddRecordArgs { title: "工作笔记".into(), content: content.into(), kind: kind.into(),
        occurred_at: None, materials: vec![], scope_ids: vec![] }
}
fn query(text: &str) -> RecordsArgs { RecordsArgs { query: Some(text.into()), ..Default::default() } }
fn within(id: &str) -> RecordsArgs { RecordsArgs { scope_id: Some(id.into()), ..Default::default() } }

#[test]
fn a02_unscoped_experience_survives_restart_with_literal_query_and_original() {
    let dir = TestDir::new();
    let text = "  图纸预览缩放 100%_；图例遮住标尺。\r\n切换视图后恢复，原因尚未证实。\n";
    let id = {
        let mut conn = dir.open();
        let added = add_record(&mut conn, "@local", &entry("experience", text)).unwrap();
        assert!(added.record.scopes.is_empty());
        added.record.id
    };
    let conn = dir.open();
    assert!(scopes(&conn, "@local").unwrap().is_empty());
    assert_eq!(conn.query_row("SELECT count(*) FROM work_item", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    for term in ["图纸预览", "%_"] {
        let page = records(&conn, "@local", &query(term)).unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].id, id);
    }
    assert_eq!(records(&conn, "@local", &query("100_100")).unwrap().total, 0);
    let detail = get_record(&conn, "@local", &id).unwrap();
    assert_eq!(detail.original, text);
    assert_eq!(detail.record.content, text);
    assert_eq!(detail.record.occurred_at, None);
    assert!(matches!(get_record(&conn, "other", &id), Err(ItemsError::NotFound)));
    assert_eq!(records(&conn, "other", &RecordsArgs::default()).unwrap().total, 0);
}

#[test]
fn a03_same_names_multiple_scopes_materials_and_versioned_rename() {
    let dir = TestDir::new();
    let mut conn = dir.open();
    let project = save_scope(&mut conn, "a", &scope_args("project")).unwrap();
    let twin = save_scope(&mut conn, "a", &scope_args("project")).unwrap();
    let domain = save_scope(&mut conn, "a", &scope_args("domain")).unwrap();
    let foreign = save_scope(&mut conn, "b", &scope_args("project")).unwrap();
    assert_eq!(scopes(&conn, "a").unwrap().len(), 3);
    assert_eq!(records(&conn, "a", &within(&project.id)).unwrap().total, 0);
    let mut progress = entry("progress", "已完成现场调查");
    progress.materials = vec!["D:/docs/观察记录.txt".into(), "https://example.test/brief".into()];
    progress.scope_ids = vec![project.id.clone(), domain.id.clone(), project.id.clone()];
    let saved = add_record(&mut conn, "a", &progress).unwrap();
    assert_eq!(saved.record.scopes.len(), 2);
    let mut decision = entry("decision", "复核后上线");
    decision.scope_ids = vec![project.id.clone()];
    add_record(&mut conn, "a", &decision).unwrap();
    let change_kind = SaveScopeArgs { id: Some(project.id.clone()), version: Some(project.version),
        ..scope_args("domain") };
    assert!(matches!(save_scope(&mut conn, "a", &change_kind), Err(ItemsError::InvalidArgument { .. })));
    let rename = SaveScopeArgs { id: Some(project.id.clone()), version: Some(project.version),
        title: "新项目名".into(), ..scope_args("project") };
    let updated = save_scope(&mut conn, "a", &rename).unwrap();
    assert_eq!(updated.id, project.id);
    assert_eq!(updated.version, project.version + 1);
    assert!(matches!(save_scope(&mut conn, "a", &rename), Err(ItemsError::ValidationRejected { .. })));
    assert!(matches!(save_scope(&mut conn, "b", &rename), Err(ItemsError::NotFound)));
    assert_eq!(records(&conn, "a", &within(&project.id)).unwrap().total, 2);
    assert_eq!(records(&conn, "a", &within(&domain.id)).unwrap().total, 1);
    assert_eq!(records(&conn, "a", &within(&twin.id)).unwrap().total, 0);
    assert!(matches!(records(&conn, "a", &within(&foreign.id)), Err(ItemsError::NotFound)));
    let detail = get_record(&conn, "a", &saved.record.id).unwrap();
    assert_eq!(detail.materials, progress.materials);
    assert!(detail.record.scopes.iter().any(|s| s.id == project.id && s.title == "新项目名" && s.background == "项目背景"));
    progress.scope_ids.push(foreign.id);
    assert!(matches!(add_record(&mut conn, "a", &progress), Err(ItemsError::NotFound)));
    assert_eq!(conn.query_row("SELECT count(*) FROM context_input", [], |r| r.get::<_, i64>(0)).unwrap(), 2);
}

#[test]
fn project_and_library_order_by_occurrence_or_entry_time_then_id() {
    let dir = TestDir::new();
    let mut conn = dir.open();
    let project = save_scope(&mut conn, "a", &scope_args("project")).unwrap();
    let mut insert = |kind, occurred_at, created_at: i64| {
        let mut args = entry(kind, "时间线");
        args.occurred_at = occurred_at;
        args.scope_ids.push(project.id.clone());
        let id = add_record(&mut conn, "a", &args).unwrap().record.id;
        conn.execute("UPDATE context_record SET created_at=?1 WHERE id=?2", (created_at, &id)).unwrap();
        id
    };
    let recent = insert("decision", Some(300), 350);
    let historical = insert("progress", Some(100), 400);
    let mut tied = [insert("event", None, 200), insert("event", Some(200), 500)];
    tied.sort();
    let expected = [recent, tied[0].clone(), tied[1].clone(), historical];
    for args in [RecordsArgs::default(), within(&project.id)] {
        let page = records(&conn, "a", &args).unwrap();
        assert_eq!(page.items.into_iter().map(|row| row.id).collect::<Vec<_>>(), expected);
    }
}

#[test]
fn offset_pages_are_stable_and_scoped() {
    let dir = TestDir::new();
    let mut conn = dir.open();
    let args = entry("event", "相同内容");
    for _ in 0..51 { add_record(&mut conn, "a", &args).unwrap(); }
    add_record(&mut conn, "b", &args).unwrap();
    conn.execute("UPDATE context_record SET created_at=100", []).unwrap();
    let first = records(&conn, "a", &RecordsArgs::default()).unwrap();
    assert_eq!((first.total, first.items.len()), (51, 50));
    let next = |offset| records(&conn, "a", &RecordsArgs { offset: Some(offset), ..Default::default() });
    let second = next(50).unwrap();
    assert_eq!((second.total, second.items.len()), (51, 1));
    assert!(first.items.last().unwrap().id < second.items[0].id);
    assert!(next(51).unwrap().items.is_empty());
    assert!(matches!(next(-1), Err(ItemsError::InvalidArgument { .. })));
}

#[test]
fn filters_use_effective_time_and_scope_before_count_and_page() {
    let dir = TestDir::new();
    let mut conn = dir.open();
    let scope = save_scope(&mut conn, "a", &scope_args("domain")).unwrap();
    let mut note = entry("progress", "无范围的进展");
    note.occurred_at = Some(100);
    note.materials.push("验收说明".into());
    for _ in 0..51 { add_record(&mut conn, "a", &note).unwrap(); }
    note.scope_ids.push(scope.id);
    add_record(&mut conn, "a", &note).unwrap();
    note.scope_ids.clear();
    note.occurred_at = Some(101);
    add_record(&mut conn, "a", &note).unwrap();
    let mut filter = RecordsArgs { from_ms: Some(100), until_ms: Some(101), origin: Some("manual".into()), unclassified: Some(true),
        section: Some("materials".into()), offset: Some(50), ..Default::default() };
    let page = records(&conn, "a", &filter).unwrap();
    assert_eq!((page.total, page.items.len()), (51, 1));
    assert_eq!(page.items[0].highlights, note.materials);
    filter.origin = Some("archive".into());
    assert_eq!(records(&conn, "a", &filter).unwrap().total, 0);
}

#[test]
fn external_activity_preserves_account_boundaries_and_unknown_occurrence() {
    use workcontext_app::external_read::{self as read, ReadArgs};
    let dir = TestDir::new();
    let mut conn = dir.open();
    let project = save_scope(&mut conn, "a", &scope_args("project")).unwrap();
    let mut note = entry("experience", "现场复核");
    note.materials.push("D:/记录/复测.txt".into());
    note.scope_ids.push(project.id.clone());
    let id = add_record(&mut conn, "a", &note).unwrap().record.id;
    note.scope_ids.clear();
    add_record(&mut conn, "b", &note).unwrap();
    let args = ReadArgs::default();
    let hit = read::search(&conn,"a","复测",&args).unwrap();
    assert_eq!(hit["items"].as_array().unwrap().len(),1);
    assert_eq!(hit["items"][0]["objectType"],"material");
    assert_eq!(hit["items"][0]["recordId"],id);
    assert_eq!(records(&conn,"a",&query("复测")).unwrap().total,1);
    let detail = read::record(&conn,"a",&id,&args).unwrap();
    assert!(detail["occurredAt"].is_null());
    assert_eq!(detail["timeBasis"],"recorded");
    assert_eq!(detail["factStatus"],"reported");
    let source = detail["sourceRef"].as_str().unwrap();
    assert_eq!(read::input(&conn,"a",source,&args).unwrap()["body"],"现场复核");
    assert!(matches!(read::input(&conn,"b",source,&args),Err(ItemsError::NotFound)));
    assert!(matches!(read::record(&conn,"b",&id,&args),Err(ItemsError::NotFound)));
    assert!(matches!(read::project(&conn,"b",&project.id,&args),Err(ItemsError::NotFound)));
    let brief = read::desktop_overview(&conn,"a").unwrap();
    assert_eq!(brief["projects"]["items"][0]["activity"]["items"][0]["id"],id);
    assert!(read::desktop_search(&conn,"@local","现场",None).unwrap()["items"].as_array().unwrap().is_empty());
    let local_id = add_record(&mut conn,"@local",&note).unwrap().record.id;
    assert_eq!(read::desktop_overview(&conn,"@local").unwrap()["items"][0]["id"],local_id);
    assert_eq!(read::desktop_search(&conn,"@local","现场",None).unwrap()["items"][0]["id"],local_id);
    let first = ReadArgs {limit:Some(1),..Default::default()};
    let activity = read::project(&conn,"a",&project.id,&first).unwrap();
    let second = ReadArgs {cursor:Some(activity["activity"]["nextCursor"].as_str().unwrap().into()),..first};
    let next = read::project(&conn,"a",&project.id,&second).unwrap();
    assert_ne!(activity["activity"]["items"][0]["id"],next["activity"]["items"][0]["id"]);
    assert!(next["activity"]["nextCursor"].is_null());
    conn.execute("UPDATE context_record SET occurred_at=0,created_at='unknown' WHERE id=?1",[&id]).unwrap();
    conn.execute("UPDATE context_input SET created_at=9223372036854775807 WHERE id=?1",[source]).unwrap();
    let unknown = read::record(&conn,"a",&id,&args).unwrap();
    assert_eq!(unknown["timeBasis"],"unknown");
    assert!(unknown["recordedAt"].is_null() && unknown["watermarks"]["latestInputAt"].is_null());
    let ranged = ReadArgs {since:Some(0),..Default::default()};
    assert!(read::recent(&conn,"a",&ranged).unwrap()["items"].as_array().unwrap().is_empty());
    assert_eq!(read::overview(&conn,"a",&args).unwrap()["projects"]["items"][0]["id"],project.id);
}

#[test]
fn manual_note_is_immediate_queued_and_keeps_original_through_correction_and_reanalysis() {
    use workcontext_app::external_read as read;
    use workcontext_store::rusqlite::params;
    let dir = TestDir::new();
    let mut db = dir.open();
    let args: AddRecordArgs = serde_json::from_value(serde_json::json!({"content":"  模拟诊断原文\n第二行 "})).unwrap();
    let initial = add_record(&mut db,"p",&args).unwrap();
    let id = &initial.record.id;
    let source = &initial.record.source_ref;
    assert_eq!(initial.record.title,"模拟诊断原文");
    assert_eq!(db.query_row("SELECT state FROM organizer_job WHERE input_id=?1",[source],|r|r.get::<_,String>(0)).unwrap(),"pending");
    let scope = save_scope(&mut db,"p",&scope_args("domain")).unwrap();
    let facets = r#"{"facts":["模拟诊断原文"],"inferences":["模拟假设"],"openQuestions":["模拟疑问"]}"#;
    db.execute("INSERT INTO organizer_facet(profile,record_id,input_id,slot,title,content,kind,scope_id,origin_ref,version,delivery,facets_json)
        VALUES ('p',?1,?2,0,'模拟整理','模拟诊断原文','event',?3,?2,1,'模拟批次',?4)",params![id,source,scope.id,facets]).unwrap();
    let projected = get_record(&db,"p",id).unwrap();
    assert_eq!(projected.record.derived[0]["classification"]["inferences"][0],"模拟假设");
    assert_eq!(records(&db,"p",&within(&scope.id)).unwrap().total,1);
    let mut change = CorrectRecordArgs {id:id.clone(), version:initial.record.version,content:Some("本人核对后的陈述".into()),attribution:Some("confirmed".into()),scope_ids:Some(vec![])};
    assert!(matches!(correct_record(&mut db,"p",&change),Err(ItemsError::ValidationRejected{..})));
    change.version = projected.record.version;
    correct_record(&mut db,"p",&change).unwrap();
    db.execute("UPDATE organizer_facet SET title='再次整理' WHERE record_id=?1",[id]).unwrap();
    let current = get_record(&db,"p",id).unwrap();
    assert_eq!(current.original,args.content);
    assert_eq!(current.record.content,"本人核对后的陈述");
    assert!(current.record.scopes.is_empty());
    let filters = Default::default();
    let cli = read::record(&db,"p",id,&filters).unwrap();
    assert_eq!(cli["content"],current.record.content);
    assert_eq!(cli["factStatus"],"confirmed");
    assert_eq!(cli["reportedResult"],args.content);
    assert_eq!(read::search(&db,"p","模拟假设",&filters).unwrap()["items"][0]["id"],*id);
    assert_eq!(read::recent(&db,"p",&filters).unwrap()["items"].as_array().unwrap().len(),1);
    assert!(read::project(&db,"p",&scope.id,&filters).unwrap()["activity"]["items"].as_array().unwrap().is_empty());
    let clear = CorrectRecordArgs {id:id.clone(),version:current.record.version,content:None,attribution:None,scope_ids:None};
    let restored = correct_record(&mut db,"p",&clear).unwrap();
    assert_eq!(restored.record.content,args.content);
    assert_eq!(restored.record.scopes[0].id,scope.id);
    assert_eq!(db.query_row("SELECT count(*) FROM record_correction WHERE record_id=?1",[id],|r|r.get::<_,i64>(0)).unwrap(),2);
}

#[test]
fn inaccessible_archive_inputs_are_excluded_even_when_attached_to_a_visible_record() {
    use workcontext_app::external_read as read;
    use workcontext_store::rusqlite::params;
    let dir = TestDir::new();
    let mut db = dir.open();
    let note = add_record(&mut db,"p",&entry("event","人工原文")).unwrap();
    db.execute("INSERT INTO context_input(id,profile,created_at,original,origin_ref) VALUES('input-hidden','p',1,'过期摘录','archive1-denied')",[]).unwrap();
    db.execute("INSERT INTO context_record(id,profile,input_id,title,kind,content,created_at,organizer_created)
        VALUES('hidden','p','input-hidden','','event','',1,1)",[]).unwrap();
    for (slot,id) in [(0,"hidden"),(1,note.record.id.as_str())] {
        db.execute("INSERT INTO organizer_facet(profile,record_id,input_id,slot,title,content,kind,origin_ref,version,delivery,facets_json)
            VALUES('p',?1,'input-hidden',?2,'过期摘录','过期摘录','event','archive1-denied',1,'样本','{\"facts\":[\"过期摘录\"],\"inferences\":[],\"openQuestions\":[]}')",params![id,slot]).unwrap();
    }
    let filters = Default::default();
    assert_eq!(records(&db,"p",&query("过期摘录")).unwrap().total,0);
    assert!(read::search(&db,"p","过期摘录",&filters).unwrap()["items"].as_array().unwrap().is_empty());
    assert!(matches!(read::input(&db,"p","input-hidden",&filters),Err(ItemsError::NotFound)));
    assert!(matches!(read::record(&db,"p","hidden",&filters),Err(ItemsError::NotFound)));
    assert!(get_record(&db,"p",&note.record.id).unwrap().record.derived.as_array().unwrap().is_empty());
    db.execute("INSERT INTO record_correction(profile,record_id,version,content,attribution,evidence_versions,corrected_at)
        VALUES('p',?1,7,'含过期摘录的修订','confirmed','{\"input-hidden\":1}',1)",[&note.record.id]).unwrap();
    let guarded = get_record(&db,"p",&note.record.id).unwrap();
    assert_eq!(guarded.original,"人工原文");
    assert!(guarded.record.content.is_empty());
    assert!(!serde_json::to_string(&guarded).unwrap().contains("过期摘录"));
    db.execute("INSERT INTO organizer_facet(profile,record_id,input_id,slot,title,content,kind,origin_ref,version,delivery,facets_json)
        VALUES('p','hidden',?1,0,'独立佐证','人工原文','event',?1,1,'样本','{\"facts\":[\"人工原文\"],\"inferences\":[],\"openQuestions\":[]}')",[&note.record.source_ref]).unwrap();
    let independently_visible = read::record(&db,"p","hidden",&filters).unwrap();
    assert_eq!(independently_visible["sourceRef"],note.record.source_ref);
    assert_eq!(independently_visible["content"],"人工原文");
    assert!(!independently_visible.to_string().contains("过期摘录"));
}
