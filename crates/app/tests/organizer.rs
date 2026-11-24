use std::{fs, path::PathBuf, sync::atomic::{AtomicUsize,Ordering}};
use workcontext_app::{external_write, organizer::*, organizer_archive::{Change,ChangePage}};
use workcontext_store::{open,rusqlite::Connection};

struct Workspace(PathBuf);
impl Workspace {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path=std::env::temp_dir().join(format!("wc-organizer-{}-{}-{}",std::process::id(),workcontext_store::now_ms(),NEXT.fetch_add(1,Ordering::Relaxed)));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn open(&self) -> Connection { open(&self.0.join("work.db")).unwrap() }
}
impl Drop for Workspace { fn drop(&mut self) { let _=fs::remove_dir_all(&self.0); } }

fn page(seq:i64) -> ChangePage {
    ChangePage {changes:vec![Change {seq,source_ref:"synthetic-archive-message".into(),occurred_at:99}],checkpoint:seq}
}
fn output(job:&Job,delivery:&str,fact:&str) -> Disposition {
    Disposition {title:"工作进展".into(),content:fact.into(),kind:"progress".into(),scope_id:None,target_id:None,
        anchor:job.origin_ref.clone(), delivery:delivery.into(),facets:Facets {facts:vec![fact.into()],inferences:vec![],open_questions:vec![]}}
}
fn apply_items(db:&mut Connection,job:&Job,records:Vec<Disposition>,now:i64) -> bool {
    apply(db,"demo",job,&Decision {records},now).unwrap()
}
fn ids(db:&Connection) -> Vec<String> {
    db.prepare("SELECT record_id FROM organizer_facet ORDER BY input_id,slot").unwrap().query_map([],|r| r.get(0)).unwrap().collect::<Result<_,_>>().unwrap()
}

#[test]
fn crash_retry_stale_and_revised_multi_output_are_atomic() {
    let dir=Workspace::new();
    let mut db=dir.open();
    assert!(ingest(&mut db,"demo",0,&page(1)).unwrap());
    let first=claim(&mut db,"demo",1).unwrap().unwrap();
    drop(db);
    let mut db=dir.open();
    assert!(claim(&mut db,"demo",2).unwrap().is_none());
    let second=claim(&mut db,"demo",300002).unwrap().unwrap();
    assert!(!apply_items(&mut db,&first,vec![],300003));
    assert!(fail(&mut db,"demo",&second,300003,"model_offline").unwrap());
    assert!(claim(&mut db,"demo",300004).unwrap().is_none());
    assert_eq!(status(&db,"demo").unwrap().retrying,1);
    retry(&mut db,"demo",300004).unwrap();
    let mut job=claim(&mut db,"demo",300004).unwrap().unwrap();
    job.original="等待回复。测试中。".into();
    let wrong=Disposition {anchor:"unrelated".into(),..output(&job,"B","测试中。")};
    assert!(apply(&mut db,"demo",&job,&Decision {records:vec![output(&job,"A","等待回复。"),wrong]},300005).is_err());
    assert!(ids(&db).is_empty());
    assert!(apply_items(&mut db,&job,vec![output(&job,"A","等待回复。"),output(&job,"B","测试中。")],300005));
    let records=ids(&db);
    assert_eq!(records.len(),2);
    let invalid=ChangePage {changes:vec![Change {seq:2,source_ref:job.origin_ref.clone(),occurred_at:99}],checkpoint:1};
    assert!(ingest(&mut db,"demo",1,&invalid).is_err());
    assert_eq!(ids(&db),records);
    assert!(ingest(&mut db,"demo",1,&page(2)).unwrap());
    assert!(ids(&db).is_empty());
    assert!(!ingest(&mut db,"demo",1,&page(2)).unwrap());
    let mut change=claim(&mut db,"demo",300006).unwrap().unwrap();
    change.original="测试中。".into();
    assert!(apply_items(&mut db,&change,vec![output(&change,"B","测试中。")],300007));
    assert_eq!(ids(&db),vec![records[1].clone()]);
    assert!(ingest(&mut db,"demo",2,&page(3)).unwrap());
    let deleted=claim(&mut db,"demo",300008).unwrap().unwrap();
    assert!(apply_items(&mut db,&deleted,vec![],300009));
    assert!(ids(&db).is_empty());
    assert_eq!(status(&db,"demo").unwrap().no_record,1);
    assert_eq!(checkpoint(&db,"other").unwrap(),0);
}

#[test]
fn independent_agent_and_archive_observations_require_shared_ticket() {
    let dir=Workspace::new();
    let mut db=dir.open();
    let mut report=external_write::AddRecord::from_text("synthetic".into(),"request".into(),"缓存巡检 DWG-123 正等待确认".into());
    report.inference=Some("未验证的 DWG-888 推断".into());
    report.materials.push(external_write::Material {path:"synthetic/private-report".into(),summary:None});
    let receipt=external_write::add_record(&mut db,"demo",&report).unwrap();
    let seed=claim(&mut db,"demo",1).unwrap().unwrap();
    assert_eq!(seed.original,report.result);
    assert!(apply_items(&mut db,&seed,vec![output(&seed,"缓存检查","DWG-123 正等待确认")],2));
    assert_eq!(ids(&db),vec![receipt.record_id.clone()]);
    ingest(&mut db,"demo",0,&page(1)).unwrap();
    let mut message=claim(&mut db,"demo",3).unwrap().unwrap();
    message.original="缓存巡检仍在等待。".into();
    assert!(candidates(&db,"demo",&message).unwrap().is_empty());
    message.original="DWG-123 等待回复。".into();
    let available=candidates(&db,"demo",&message).unwrap();
    assert_eq!(available.len(),1);
    assert_eq!(available[0].anchors,vec!["DWG-123"]);
    assert_ne!(message.origin_ref,seed.origin_ref);
    let attach=Disposition {target_id:Some(receipt.record_id.clone()),..output(&message,&available[0].delivery,"DWG-123 等待回复。")};
    let forged=Disposition {content:"已完成。".into(),..output(&message,"缓存","DWG-123 等待回复。")};
    assert!(apply(&mut db,"demo",&message,&Decision {records:vec![forged]},4).is_err());
    assert!(apply_items(&mut db,&message,vec![attach],4));
    assert_eq!(ids(&db),vec![receipt.record_id.clone(),receipt.record_id.clone()]);
    let (title,result):(String,String)=db.query_row("SELECT title,reported_result FROM context_record WHERE id=?1",[receipt.record_id],|r| Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!(title,report.title);
    assert_eq!(result,report.result);
}

#[test]
fn an_archive_destination_requires_a_current_grant_and_revision() {
    let dir=Workspace::new();
    let mut db=dir.open();
    ingest(&mut db,"demo",0,&page(1)).unwrap();
    let mut first=claim(&mut db,"demo",1).unwrap().unwrap();
    first.original="DWG-234 需要检查。".into();
    assert!(apply_items(&mut db,&first,vec![output(&first,"缓存检查","DWG-234 需要检查。")],2));
    let destination=ids(&db).pop().unwrap();
    let report=external_write::AddRecord::from_text("synthetic".into(),"second".into(),"DWG-234 仍在等待回复。".into());
    external_write::add_record(&mut db,"demo",&report).unwrap();
    let mut second=claim(&mut db,"demo",3).unwrap().unwrap();
    assert!(!candidates(&db,"demo",&second).unwrap().iter().any(|c| c.id==destination));
    second.authorized_inputs=candidate_archive_ids(&db,"demo",&second).unwrap();
    assert_eq!(second.authorized_inputs,vec![first.input_id]);
    let candidate=candidates(&db,"demo",&second).unwrap().into_iter().find(|c| c.id==destination).unwrap();
    let attached=Disposition {target_id:Some(destination),..output(&second,&candidate.delivery,"DWG-234 仍在等待回复。")};
    assert!(apply_items(&mut db,&second,vec![attached],4));
    ingest(&mut db,"demo",1,&page(2)).unwrap();
    assert!(ids(&db).is_empty());
    assert_eq!(status(&db,"demo").unwrap().pending,2);
}

#[test]
fn scoped_reply_requires_parent_proof_delivery_and_current_message_facts() {
    use workcontext_app::context::{save_scope,SaveScopeArgs};
    use workcontext_app::external_archive::archive_ref;
    let dir=Workspace::new();
    let mut db=dir.open();
    let scope=save_scope(&mut db,"demo",&SaveScopeArgs {id:None,version:None,kind:"project".into(),
        title:"演示交接".into(),goal:String::new(),background:String::new()}).unwrap();
    let parent_ref=archive_ref("demo","synthetic-room","parent");
    let reply_ref=archive_ref("demo","synthetic-room","reply");
    let change=|seq,source_ref:&str| ChangePage {changes:vec![Change {seq,source_ref:source_ref.into(),occurred_at:123}],checkpoint:seq};
    assert!(reset_if_grant_changed(&mut db,"demo","scope-a").unwrap());
    ingest(&mut db,"demo",0,&change(1,&parent_ref)).unwrap();
    let mut parent=claim(&mut db,"demo",1).unwrap().unwrap();
    parent.original="交接清单已经发出，等现场确认。".into();
    let parent_item=Disposition {scope_id:Some(scope.id.clone()),..output(&parent,"现场交接清单",&parent.original)};
    assert!(apply_items(&mut db,&parent,vec![parent_item],2));
    ingest(&mut db,"demo",1,&change(2,&reply_ref)).unwrap();
    let mut reply=claim(&mut db,"demo",3).unwrap().unwrap();
    reply.original="现场在复核，明早确认。".into();
    assert!(candidates(&db,"demo",&reply).unwrap().is_empty());
    reply.reply_to=Some(parent_ref.clone());
    reply.reply_version=Some(987);
    reply.reply_expires_at=Some(100);
    assert!(candidates(&db,"demo",&reply).unwrap().is_empty());
    reply.authorized_inputs=candidate_archive_ids(&db,"demo",&reply).unwrap();
    let candidate=candidates(&db,"demo",&reply).unwrap().pop().unwrap();
    assert_eq!(candidate.scope_id.as_ref(),Some(&scope.id));
    assert_eq!(candidate.anchors,vec![parent_ref.clone()]);
    reply.reply_version=Some(-10);
    assert!(candidate_archive_ids(&db,"demo",&reply).unwrap().is_empty());
    assert!(candidates(&db,"demo",&reply).unwrap().is_empty());
    reply.reply_version=Some(987);
    let attachment=|delivery:&str,body:&str| Disposition {scope_id:Some(scope.id.clone()),target_id:Some(candidate.id.clone()),..output(&reply,delivery,body)};
    assert!(apply(&mut db,"demo",&reply,&Decision {records:vec![attachment("其他事项",&reply.original)]},4).is_err());
    assert!(apply(&mut db,"demo",&reply,&Decision {records:vec![attachment(&candidate.delivery,&parent.original)]},4).is_err());
    assert!(apply_items(&mut db,&reply,vec![attachment(&candidate.delivery,&reply.original)],4));
    assert_eq!(ids(&db),vec![candidate.id.clone(),candidate.id]);
    let proof:(String,i64)=db.query_row("SELECT reply_ref,reply_version FROM organizer_facet WHERE input_id=?1",[reply.input_id],|r| Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!(proof,(parent_ref.clone(),987));
    ingest(&mut db,"demo",2,&change(3,&parent_ref)).unwrap();
    assert!(ids(&db).is_empty());
    assert!(!reset_if_grant_changed(&mut db,"demo","scope-a").unwrap());
    assert_eq!(checkpoint(&db,"demo").unwrap(),3);
    assert!(reset_if_grant_changed(&mut db,"demo","scope-b").unwrap());
    assert_eq!(checkpoint(&db,"demo").unwrap(),0);
    assert_eq!(status(&db,"demo").unwrap().pending,2);
}

#[test]
fn late_indexed_native_parent_reopens_already_organized_reply() {
    let dir=Workspace::new();
    let mut db=dir.open();
    ingest(&mut db,"demo",0,&page(1)).unwrap();
    let mut child=claim(&mut db,"demo",1).unwrap().unwrap();
    child.original="还在复核".into();
    child.reply_to=Some("late-parent".into());
    child.reply_version=Some(123);
    child.reply_expires_at=Some(100);
    assert!(apply_items(&mut db,&child,vec![output(&child,"复核",&child.original)],2));
    ingest(&mut db,"demo",1,&ChangePage {changes:vec![Change {seq:2,source_ref:"late-parent".into(),occurred_at:98}],checkpoint:2}).unwrap();
    let mut parent=claim(&mut db,"demo",3).unwrap().unwrap();
    parent.original="请求复核".into();
    assert!(apply_items(&mut db,&parent,vec![output(&parent,"复核",&parent.original)],4));
    assert_eq!(claim(&mut db,"demo",5).unwrap().unwrap().input_id,child.input_id);
}

#[test]
fn contextual_parent_offers_only_its_own_authorized_words_to_a_native_child() {
    let dir=Workspace::new();
    let mut db=dir.open();
    ingest(&mut db,"demo",0,&page(1)).unwrap();
    let mut parent=claim(&mut db,"demo",1).unwrap().unwrap();
    parent.original="请复查样本。".into();
    parent.reply_to=Some("sensitive-predecessor".into());
    parent.reply_version=Some(-42);
    parent.reply_expires_at=Some(100);
    assert!(apply_items(&mut db,&parent,vec![output(&parent,"前一条的敏感事项",&parent.original)],2));
    let child_page=ChangePage {changes:vec![Change {seq:2,source_ref:"native-child".into(),occurred_at:100}],checkpoint:2};
    ingest(&mut db,"demo",1,&child_page).unwrap();
    let mut child=claim(&mut db,"demo",3).unwrap().unwrap();
    child.original="还有两处待查。".into();
    child.reply_to=Some(parent.origin_ref.clone());
    child.reply_version=Some(234);
    child.reply_expires_at=Some(200);
    child.reply_excerpt=Some(parent.original.clone());
    assert!(candidates(&db,"demo",&child).unwrap().is_empty());
    child.authorized_inputs=candidate_archive_ids(&db,"demo",&child).unwrap();
    let found=candidates(&db,"demo",&child).unwrap();
    assert_eq!(found.len(),1);
    let candidate=&found[0];
    assert_eq!(candidate.title,parent.original);
    assert_eq!(candidate.delivery,parent.original);
    assert!(candidate.scope_id.is_none());
    assert_eq!(candidate.anchors,vec![parent.origin_ref]);
    child.reply_version=Some(-234);
    assert!(candidates(&db,"demo",&child).unwrap().is_empty());
    child.reply_version=Some(234);
    child.reply_excerpt=Some("已修订的其他正文".into());
    let attach=|job:&Job|Disposition { target_id:Some(candidate.id.clone()),..output(job,&candidate.delivery,&job.original) };
    assert!(apply(&mut db,"demo",&child,&Decision {records:vec![attach(&child)]},4).is_err());
    child.reply_excerpt=Some(parent.original);
    assert!(apply_items(&mut db,&child,vec![attach(&child)],4));
    assert_eq!(ids(&db),vec![candidate.id.clone(),candidate.id.clone()]);
}

#[test]
fn parent_retention_expiry_reorganizes_without_a_journal_change() {
    let dir=Workspace::new();
    let mut db=dir.open();
    ingest(&mut db,"demo",0,&page(1)).unwrap();
    let mut reply=claim(&mut db,"demo",1).unwrap().unwrap();
    reply.original="现场仍在复核。".into();
    reply.reply_to=Some("synthetic-parent".into());
    reply.reply_version=Some(987);
    reply.reply_expires_at=Some(10);
    assert!(apply_items(&mut db,&reply,vec![output(&reply,"现场复核",&reply.original)],2));
    assert!(claim(&mut db,"demo",9).unwrap().is_none());
    let fresh=claim(&mut db,"demo",10).unwrap().unwrap();
    assert_eq!(fresh.input_id,reply.input_id);
    assert!(fresh.reply_to.is_none());
    assert!(ids(&db).is_empty());
    assert!(apply_items(&mut db,&fresh,vec![],11));
    assert!(claim(&mut db,"demo",12).unwrap().is_none());
    assert_eq!(checkpoint(&db,"demo").unwrap(),1);
}
