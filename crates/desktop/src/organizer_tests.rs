use super::*;
use std::{process::Command, time::Instant};
use workcontext_app::context::{self, AddRecordArgs, SaveScopeArgs};

fn grant(root:&Path) -> Consent {
    Consent { workspace:root.canonicalize().unwrap(),profile:"demo:staff".into(),model:MODEL.into(),epoch:"synthetic-grant".into(),enabled:true,adjacent:true }
}

fn control() -> Control {
    let (wake,_) = mpsc::sync_channel(1);
    Control {wake,stopping:AtomicBool::new(false),running:AtomicBool::new(false),generation:Arc::new(AtomicU64::new(1)),notice:Mutex::default(),thread:Mutex::default()}
}

#[test]
fn pending_manual_input_survives_failed_process_then_retries_with_fresh_snapshot() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path();
    let mut db = workcontext_store::open(&root.join("work.sqlite3")).unwrap();
    let grant = grant(root);
    let create_scope = |db:&mut Connection, profile:&str, name:&str| {
        context::save_scope(db,profile,&SaveScopeArgs {id:None,version:None,kind:"project".into(),title:name.into(),goal:"private synthetic background".into(),background:String::new()}).unwrap()
    };
    let scope = create_scope(&mut db,&grant.profile,"样件验收");
    create_scope(&mut db,"other:user","样件验收");
    create_scope(&mut db,&grant.profile,"无关项目");
    let note:AddRecordArgs = serde_json::from_value(json!({"title":"样件验收\npassword=synthetic-title-secret","content":"样件验收需要补齐两份检验表\nAPI_KEY=synthetic-input-secret"})).unwrap();
    context::add_record(&mut db,&grant.profile,&note).unwrap();
    let now = sources::now_ms()+1000;
    let mut job = queue::claim(&mut db,&grant.profile,now).unwrap().unwrap();
    let first = payload(&db,root,&grant,&mut job,now).unwrap().unwrap();
    let request:serde_json::Value = serde_json::from_slice(&first).unwrap();
    assert!(request["source"]["body"].as_str().unwrap().contains("样件验收需要补齐两份检验表"));
    assert!(!String::from_utf8_lossy(&first).contains("synthetic-input-secret"));
    assert!(!String::from_utf8_lossy(&first).contains("synthetic-title-secret"));
    assert_eq!(request["scopes"], json!([{"id":scope.id,"name":"样件验收"}]));
    assert!(!String::from_utf8_lossy(&first).contains("private synthetic background"));
    let missing = Command::new(root.join("missing-model-runtime"));
    let reason = process::call(missing,&first,&control(),1).unwrap_err();
    assert_eq!(reason,"model_unavailable");
    assert!(queue::fail(&mut db,&grant.profile,&job,now,reason).unwrap());
    assert!(queue::claim(&mut db,&grant.profile,now).unwrap().is_none());
    let status = queue::status(&db,&grant.profile).unwrap();
    assert_eq!(status.retrying,1);
    assert_eq!(status.last_error.as_deref(),Some(reason));
    assert!(status.next_retry_at.unwrap()>now);
    queue::retry(&mut db,&grant.profile,now+1).unwrap();
    let mut next = queue::claim(&mut db,&grant.profile,now+1).unwrap().unwrap();
    assert_ne!(next.token,job.token);
    assert!(payload(&db,root,&grant,&mut job,now+1).is_err());
    let current = payload(&db,root,&grant,&mut next,now+1).unwrap().unwrap();
    assert!(verify_snapshot(Some(&first),Some(&current)).is_ok());
    context::save_scope(&mut db,&grant.profile,&SaveScopeArgs {id:Some(scope.id),version:Some(scope.version),kind:"project".into(),title:"其他名称".into(),goal:String::new(),background:String::new()}).unwrap();
    let changed = payload(&db,root,&grant,&mut next,now+1).unwrap().unwrap();
    assert_eq!(verify_snapshot(Some(&current),Some(&changed)).unwrap_err(),"source_changed");
    assert!(verify_snapshot(Some(&changed),None).is_err());
    assert!(verify_snapshot(None,Some(&changed)).is_err());
    assert_eq!(next.original,note.content);
}

#[test]
fn copied_grant_and_changed_account_are_denied() {
    let original = tempfile::tempdir().unwrap();
    let restored = tempfile::tempdir().unwrap();
    let approved = grant(original.path());
    let mut cfg = SettingsFile::default();
    cfg.extra.insert(KEY.into(),serde_json::to_value(&approved).unwrap());
    cfg.extra.insert("workspaceAccount".into(),json!({"profile":"other:user","sourceId":null}));
    sources::config::save(&original.path().join("config.json"),&cfg).unwrap();
    assert!(active(&cfg,original.path()).is_err());
    assert!(active(&cfg,restored.path()).is_err());
    assert!(prerequisites(&cfg).is_err());
}

#[test]
fn successful_exit_with_invalid_output_never_creates_a_decision() {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.arg("--list");
    assert_eq!(process::call(command,b"",&control(),1).unwrap_err(), "invalid_model_response");
}

// Invoked only as the isolated child of the cancellation test. No external provider or Python runtime.
#[test]
#[ignore]
fn synthetic_blocked_model() {
    let Some(marker) = std::env::var_os("WORKCONTEXT_SYNTHETIC_MODEL_MARKER") else {return};
    if std::env::var_os("WORKCONTEXT_SYNTHETIC_GRANDCHILD").is_some() {
        std::fs::write(marker,b"started").unwrap();
        thread::sleep(Duration::from_secs(20));
    } else {
        let mut child = Command::new(std::env::current_exe().unwrap()).args(["--ignored","--exact","organizer::tests::synthetic_blocked_model"])
            .env("WORKCONTEXT_SYNTHETIC_GRANDCHILD","1").spawn().unwrap();
        child.wait().unwrap();
    }
}

#[test]
fn revocation_terminates_blocked_child_and_grandchild_before_commit() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("model-started");
    let control = control();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--ignored","--exact","organizer::tests::synthetic_blocked_model"]).env("WORKCONTEXT_SYNTHETIC_MODEL_MARKER",&marker);
    thread::scope(|s| {
        let child = s.spawn(||process::call(command,b"synthetic",&control,1));
        let timeout = Instant::now()+Duration::from_secs(8);
        while !marker.exists() && Instant::now()<timeout {thread::sleep(Duration::from_millis(20));}
        control.generation.fetch_add(1,Ordering::AcqRel);
        assert!(marker.exists());
        let cancelled = Instant::now();
        assert_eq!(child.join().unwrap().unwrap_err(),"consent_changed");
        assert!(cancelled.elapsed()<Duration::from_secs(5),"grandchild retained the model output pipe");
    });
}

#[test]
fn offline_intake_retains_checkpoint_retry_and_authorized_reply_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let approved = grant(root);
    let now = sources::now_ms();
    let scope = json!({"conversations":["demo-room"],"people":[]});
    let mut cfg:SettingsFile = serde_json::from_value(json!({
        "workspaceAccount":{"profile":approved.profile,"sourceId":"demo"},
        "contextOrganizer":approved,
        "judge":{"mode":"cli","modelId":MODEL},"egress":{"policy":"full","approvedModel":MODEL},
        "sources":{"demo":{"profile":approved.profile,"scope":scope,"authorized":true,"windowStartMs":now-60000}}
    })).unwrap();
    sources::config::save(&root.join("config.json"),&cfg).unwrap();
    let work_path = root.join("work.sqlite3");
    let mut db = workcontext_store::open(&work_path).unwrap();
    db.execute("INSERT INTO source(id,kind,display_name,scope_json,enabled,authorized_at,created_at) VALUES('demo','dws_im','演示',?1,1,?2,?2)",params![scope.to_string(),now-60000]).unwrap();
    std::fs::create_dir(root.join("archive")).unwrap();
    let archive = Connection::open(root.join("archive/archive.sqlite3")).unwrap();
    archive.execute_batch("CREATE TABLE archive_state(key TEXT PRIMARY KEY,value TEXT);
        INSERT INTO archive_state VALUES('profile','demo:staff'),('organizer_bootstrap_rowid','done');
        CREATE TABLE messages(cid TEXT,mid TEXT,occurred_at INTEGER,sender_id TEXT,sender_name TEXT,body TEXT,reply_to TEXT);
        CREATE TABLE message_capture(cid,mid,thread_id);
        CREATE TABLE message_relations(cid,mid,kind,path,source_cid,source_mid);
        CREATE TABLE attachment_profile(id,selector);
        CREATE TABLE attachments(id,cid,mid,status,extraction_status,text_content,resource_type,download_cid,download_mid);
        CREATE TABLE attachment_occurrences(attachment_id,cid,mid,source_cid,source_mid,path);
        CREATE TABLE organizer_changes(seq INTEGER PRIMARY KEY,cid,mid);
        INSERT INTO organizer_changes VALUES(1,'demo-room','reply'),(2,'unapproved','private');
        INSERT INTO message_capture(cid,mid) VALUES('demo-room','reply'),('demo-room','parent');
        INSERT INTO message_relations VALUES('demo-room','reply','quote','quotedMessage','demo-room','parent');").unwrap();
    archive.execute("INSERT INTO messages VALUES ('demo-room','parent',?1,'sender',NULL,'核对样件',NULL),
        ('demo-room','reply',?1,'sender',NULL,'正在补充附件','parent'),('unapproved','private',?1,'sender',NULL,'禁止发送',NULL)",[now-1000]).unwrap();
    assert!(active(&cfg,root).is_ok());
    assert_eq!(intake(&mut db,root,&approved.profile,now).unwrap(),(true,None));
    assert_eq!(queue::checkpoint(&db,&approved.profile).unwrap(),2);
    let mut job = queue::claim(&mut db,&approved.profile,now).unwrap().unwrap();
    let source = payload(&db,root,&approved,&mut job,now).unwrap().unwrap();
    let data:serde_json::Value = serde_json::from_slice(&source).unwrap();
    assert_eq!(data["source"]["body"],"正在补充附件");
    assert_eq!(data["source"]["replyTo"]["body"],"核对样件");
    let first_proof = job.reply_version.unwrap();
    let reason = model_command(Path::new("synthetic.py"), false).unwrap_err();
    assert_eq!(reason,"credentials_missing");
    assert!(queue::fail(&mut db,&approved.profile,&job,now,reason).unwrap());
    drop(db);
    let mut db = workcontext_store::open(&work_path).unwrap();
    assert_eq!(intake(&mut db,root,&approved.profile,now+1).unwrap(),(false,None));
    let status = queue::status(&db,&approved.profile).unwrap();
    assert_eq!((status.checkpoint,status.retrying,status.pending),(2,1,0));
    assert_eq!(status.last_error.as_deref(),Some("credentials_missing"));
    assert!(queue::claim(&mut db,&approved.profile,now+1).unwrap().is_none());
    queue::retry(&mut db,&approved.profile,now+1).unwrap();
    let mut retry = queue::claim(&mut db,&approved.profile,now+1).unwrap().unwrap();
    archive.execute("UPDATE messages SET body='改为核对包装' WHERE cid='demo-room' AND mid='parent'",[]).unwrap();
    let changed = payload(&db,root,&approved,&mut retry,now+1).unwrap().unwrap();
    assert_ne!(Some(first_proof),retry.reply_version);
    assert!(verify_snapshot(Some(&source),Some(&changed)).is_err());
    archive.execute("DELETE FROM message_relations",[]).unwrap();
    let changed = payload(&db,root,&approved,&mut retry,now+1).unwrap().unwrap();
    let data:serde_json::Value = serde_json::from_slice(&changed).unwrap();
    assert!(data["source"].get("replyTo").is_none());
    assert!(retry.reply_version.is_none());
    archive.execute("UPDATE messages SET reply_to=NULL WHERE mid='reply'",[]).unwrap();
    archive.execute("UPDATE messages SET occurred_at=?1 WHERE mid='parent'",[now-3000]).unwrap();
    let bytes = payload(&db,root,&approved,&mut retry,now+1).unwrap().unwrap();
    let data:serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(data["source"]["previous"]["body"],"改为核对包装");
    assert!(data["source"].get("replyTo").is_none());
    assert!(retry.reply_version.unwrap()<0);
    let output:queue::Decision=serde_json::from_value(json!({"records":[{"title":"补充说明","delivery":"附件","content":"正在补充附件","anchor":retry.origin_ref,"kind":"progress","scopeId":null,"targetId":null,"facets":{"facts":["正在补充附件"],"inferences":[],"openQuestions":[]}}]})).unwrap();
    assert!(queue::apply(&mut db,&approved.profile,&retry,&output,now+1).unwrap());
    let id:String = db.query_row("SELECT record_id FROM organizer_facet WHERE input_id=?1",[&retry.input_id],|r| r.get(0)).unwrap();
    let detail = context::get_record(&db,&approved.profile,&id).unwrap();
    context::correct_record(&mut db,&approved.profile,&context::CorrectRecordArgs {id:id.clone(),version:detail.record.version,
        content:Some("核对时参考了上一条消息".into()),attribution:Some("reported".into()),scope_ids:None}).unwrap();
    let visible = |time| evidence::authorized_inputs(&db,root,&approved.profile,time).unwrap();
    assert!(visible(now+1).contains(&retry.input_id));
    assert!(!visible(retry.reply_expires_at.unwrap()).contains(&retry.input_id));
    archive.execute("UPDATE message_capture SET thread_id='other' WHERE mid='parent'",[]).unwrap();
    assert!(!visible(now+1).contains(&retry.input_id));
    archive.execute("UPDATE message_capture SET thread_id=NULL WHERE mid='parent'",[]).unwrap();
    cfg.extra.get_mut(KEY).unwrap()["adjacent"]=json!(false);
    sources::config::save(&root.join("config.json"),&cfg).unwrap();
    assert!(!visible(now+1).contains(&retry.input_id));
    db.execute("DELETE FROM organizer_facet WHERE input_id=?1",[&retry.input_id]).unwrap();
    assert!(context::get_record(&db,&approved.profile,&id).is_err());
    cfg.egress.policy="none".into();
    assert!(active(&cfg,root).is_err());
}

#[test]
fn gateway_requires_exact_model_and_full_approval() {
    let root = tempfile::tempdir().unwrap();
    let approval = grant(root.path());
    let mut cfg = SettingsFile::default();
    cfg.extra.insert("workspaceAccount".into(),json!({"profile":approval.profile,"sourceId":null}));
    cfg.judge.mode = sources::config::JudgeMode::Cli;
    cfg.extra.insert(KEY.into(),serde_json::to_value(&approval).unwrap());
    cfg.judge.model_id=Some(MODEL.into());
    cfg.egress.approved_model=Some(MODEL.into());
    cfg.egress.policy="full".into();
    sources::config::save(&root.path().join("config.json"),&cfg).unwrap();
    assert!(active(&cfg,root.path()).is_ok());
    for denied in ["redacted","none"] {
        cfg.egress.policy=denied.into();
        assert!(active(&cfg,root.path()).is_err());
    }
    cfg.egress.policy="full".into();
    cfg.judge.model_id=Some("synthetic/other-model".into());
    assert!(active(&cfg,root.path()).is_err());
    cfg.egress.approved_model=cfg.judge.model_id.clone();
    assert!(active(&cfg,root.path()).is_err());
}

#[test]
fn agent_envelope_metadata_and_inference_stay_out_of_model_input() {
    use workcontext_app::external_write::{add_record, AddRecord};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut db = workcontext_store::open(&root.join("work.sqlite3")).unwrap();
    let grant = grant(root);
    let mut report = AddRecord::from_text("synthetic-producer-id".into(),"synthetic-request-id".into(),"已补齐验收记录。\nAPI_KEY=synthetic-agent-secret".into());
    report.inference=Some("synthetic-unverified-guess".into());
    report.open_questions.push("synthetic-question".into());
    add_record(&mut db,&grant.profile,&report).unwrap();
    let now = sources::now_ms();
    let mut job = queue::claim(&mut db,&grant.profile,now).unwrap().unwrap();
    let bytes = payload(&db,root,&grant,&mut job,now).unwrap().unwrap();
    let input = std::str::from_utf8(&bytes).unwrap();
    assert!(input.contains("已补齐验收记录。"));
    for private in ["synthetic-producer-id","synthetic-request-id","synthetic-agent-secret","synthetic-unverified-guess","synthetic-question"] {
        assert!(!input.contains(private));
    }
}
