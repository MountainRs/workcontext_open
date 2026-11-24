use serde_json::json;
use workcontext_app::items::{item_get_for_profile, items_query_for_profile, ItemPage, ItemsQueryArgs};
use workcontext_app::ownership_flow::{move_card, set_conclusion, view, Candidate, State};
use workcontext_store::rusqlite::Connection;

fn db() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    for migration in [include_str!("../../../migrations/0001_init.sql"), include_str!("../../../migrations/0002_judge_meta.sql"),
        include_str!("../../../migrations/0003_ownership_flow.sql"), include_str!("../../../migrations/0004_ownership_conclusion.sql"),
        include_str!("../../../migrations/0005_ownership_project.sql"), include_str!("../../../migrations/0006_context.sql")] {
        db.execute_batch(migration).unwrap();
    }
    db
}

fn candidate(mid: &str, title: &str, state: State) -> Candidate {
    serde_json::from_value(json!({
        "evidence":[{"cid":"chat","mid":mid}], "sourceWork":title, "state":state, "work":title,
        "action":"Review", "when":"", "sourceSummary":{"time":"","place":"Team room","people":"","event":""},
        "project":"  Build  ", "evidenceDetails":[{"time":"Monday","conversation":"Team room","kind":"message","excerpt":mid}]
    })).unwrap()
}

fn page(db: &Connection, filter: &str, query: Option<&str>, offset: i64) -> ItemPage {
    items_query_for_profile(db, &ItemsQueryArgs { filter: Some(filter.into()), query: query.map(str::to_owned),
        offset: Some(offset), limit: Some(1) }, Some("a")).unwrap()
}

#[test]
fn historical_notes_evidence_and_project_links_remain_scoped_to_one_delivery() {
    let mut db = db();
    let first = candidate("first", "Drawing %_", State::Mine);
    let second = candidate("second", "Schedule", State::Waiting);
    let cards = view(&mut db, "a", &[first, second.clone()]).unwrap();
    let foreign = view(&mut db, "b", std::slice::from_ref(&second)).unwrap().remove(0);
    set_conclusion(&mut db, "a", &cards[0].id, &cards[0].version, "Draft", false, 1).unwrap();
    let revision = item_get_for_profile(&db,&cards[0].id,Some("a")).unwrap().delivery.unwrap().version;
    set_conclusion(&mut db, "a", &cards[0].id, &revision, "Dimensions checked", false, 2).unwrap();
    view(&mut db, "a", &[second]).unwrap();

    let historical = page(&db,"me",Some("%_"),0);
    assert_eq!(historical.total,1);
    assert!(historical.items[0].historical);
    assert_eq!(historical.items[0].id,cards[0].id);
    assert_eq!(historical.items[0].updated_at,None);
    let detail = item_get_for_profile(&db,&cards[0].id,Some("a")).unwrap();
    assert_eq!(detail.item.created_at,None);
    let delivery = detail.delivery.unwrap();
    assert!(!delivery.formal && !delivery.present);
    assert_eq!(delivery.state,State::Mine);
    assert_eq!(delivery.confirmation,"confirmed");
    assert_eq!(delivery.conclusion.as_deref(),Some("Dimensions checked"));
    assert_eq!(delivery.evidence_details[0].excerpt,"first");
    assert_eq!(delivery.project.as_deref(),Some("Build"));
    assert_eq!(delivery.related.len(),1);
    assert_eq!(delivery.related[0].id,cards[1].id);
    assert_eq!(page(&db,"waiting",None,0).items[0].confirmation,"suggested");
    assert_eq!(page(&db,"all",Some("BUILD"),1).total,2);
    assert_eq!(page(&db,"all",None,1).items[0].id,cards[0].id);
    assert!(page(&db,"all",None,2).items.is_empty());
    assert_eq!(page(&db,"all",None,2).total,2);
    assert_eq!(page(&db,"all",Some("\" OR 1=1 --"),0).total,0);
    for profile in [Some("b"),None] {
        assert_eq!(item_get_for_profile(&db,&cards[0].id,profile).unwrap_err().code(),"NOT_FOUND");
    }
    assert_eq!(item_get_for_profile(&db,&foreign.id,Some("a")).unwrap_err().code(),"NOT_FOUND");
    assert_eq!(db.query_row("SELECT count(*) FROM work_item",[],|r| r.get::<_,i64>(0)).unwrap(),0);
}

#[test]
fn formal_rows_keep_their_identity_and_only_human_decisions_confirm() {
    let mut db = db();
    let first = view(&mut db,"a",&[candidate("one","Snapshot title",State::Triage)]).unwrap().remove(0);
    move_card(&mut db,"a",&first.id,&first.version,State::Waiting,Some("Await approval"),17).unwrap();
    view(&mut db,"a",&[]).unwrap();
    db.execute("INSERT INTO work_item(id,title,dedupe_key,created_at,updated_at) VALUES ('legacy','Local','local',8,9)",[]).unwrap();
    let waiting = page(&db,"waiting",Some("approval"),0);
    assert_eq!(waiting.total,1);
    assert_eq!(waiting.items[0].id,first.id);
    assert!(!waiting.items[0].historical);
    db.execute("INSERT INTO work_thread(id,title,created_at,updated_at) VALUES ('thread','Build',8,9)",[]).unwrap();
    db.execute("UPDATE work_item SET thread_id='thread' WHERE id='legacy'",[]).unwrap();
    let detail = item_get_for_profile(&db,&first.id,Some("a")).unwrap();
    assert_eq!(detail.item.created_at,Some(17));
    let delivery = detail.delivery.unwrap();
    assert!(delivery.formal && !delivery.present);
    assert_eq!(delivery.state,State::Waiting);
    assert_eq!(delivery.confirmation,"confirmed");
    assert_eq!(delivery.related.len(),1);
    assert_eq!(delivery.related[0].id,"legacy");
    assert_eq!(page(&db,"all",None,0).total,2);
    assert_eq!(page(&db,"all",Some("Local"),0).items[0].confirmation,"suggested");
    let legacy = item_get_for_profile(&db,"legacy",None).unwrap();
    assert!(legacy.delivery.is_none());
    assert_eq!(legacy.item.project.as_deref(),Some("Build"));
    assert_eq!(legacy.item.confirmation,"suggested");
    assert_eq!(item_get_for_profile(&db,&first.id,Some("b")).unwrap_err().code(),"NOT_FOUND");
}

#[test]
fn project_edits_do_not_confirm_or_rewrite_legacy_suggestions() {
    let mut db = db();
    let card = view(&mut db,"a",&[candidate("legacy","Older report",State::Mine)]).unwrap().remove(0);
    view(&mut db,"a",&[]).unwrap();
    db.execute("UPDATE ownership_card SET project_override='Another project',
        candidate_json=json_remove(candidate_json,'$.project','$.anchors','$.evidenceDetails','$.needsVerification') WHERE id=?1",[&card.id]).unwrap();
    let detail = item_get_for_profile(&db,&card.id,Some("a")).unwrap();
    let delivery = detail.delivery.unwrap();
    assert_eq!(delivery.confirmation,"suggested");
    assert_eq!(delivery.project.as_deref(),Some("Another project"));
    assert!(delivery.evidence_details.is_empty());
    assert!(delivery.related.is_empty());
    assert!(!delivery.present && !delivery.formal);
    db.execute("UPDATE ownership_card SET project_override='', candidate_json=json_set(candidate_json,'$.project','Reported') WHERE id=?1",[&card.id]).unwrap();
    assert_eq!(page(&db,"all",None,0).items[0].project,None);
    assert_eq!(page(&db,"all",Some("Reported"),0).total,0);
}

#[test]
fn confirmed_projects_group_by_id_across_renames_and_independent_closure() {
    use workcontext_app::items::context_scope_items;
    use workcontext_app::ownership_flow::{context_item_scope_set, set_project};

    let mut db = db();
    db.execute_batch("INSERT INTO work_thread(id,title,profile,kind,created_at,updated_at) VALUES
        ('p1','Build','a','project',1,1),('p2','Build','a','project',1,1),
        ('foreign','Build','b','project',1,1),('domain','Build','a','domain',1,1)").unwrap();
    let cards = view(&mut db,"a",&[candidate("one","First",State::Mine), candidate("two","Second",State::Waiting),
        candidate("three","Third",State::Mine), candidate("four","Fourth",State::Mine)]).unwrap();
    let detail = |db: &Connection, index: usize| item_get_for_profile(db,&cards[index].id,Some("a")).unwrap().delivery.unwrap();
    for invalid in ["foreign","domain","missing"] {
        assert!(context_item_scope_set(&db,"a",&cards[0].id,&cards[0].version,Some(invalid)).is_err());
    }
    for (card, project) in cards.iter().zip(["p1","p1","p2"]) {
        context_item_scope_set(&db,"a",&card.id,&card.version,Some(project)).unwrap();
    }
    assert!(context_item_scope_set(&db,"a",&cards[0].id,&cards[0].version,None).is_err());
    assert_eq!(detail(&db,0).related.len(),1);
    assert_eq!(detail(&db,0).related[0].id,cards[1].id);
    assert!(detail(&db,2).related.is_empty() && detail(&db,3).related.is_empty());
    assert!(context_scope_items(&db,"b","p1").unwrap().is_empty());
    db.execute("UPDATE work_thread SET title='Revised' WHERE id='p1'",[]).unwrap();
    assert_eq!(detail(&db,0).project.as_deref(),Some("Revised"));
    let version = detail(&db,0).version;
    move_card(&mut db,"a",&cards[0].id,&version,State::Done,None,10).unwrap();
    let members = context_scope_items(&db,"a","p1").unwrap();
    assert_eq!(members.len(),2);
    assert!(members.iter().any(|item| item.id==cards[0].id && item.lifecycle=="closed" && item.next_action.is_none()));
    assert!(members.iter().any(|item| item.id==cards[1].id && item.lifecycle=="active"));
    set_project(&db,"a",&cards[0].id,&detail(&db,0).version,Some("Build")).unwrap();
    assert!(detail(&db,0).scope_id.is_none());
    assert_eq!(detail(&db,0).related[0].id,cards[3].id);
    context_item_scope_set(&db,"a",&cards[1].id,&detail(&db,1).version,None).unwrap();
    assert!(detail(&db,1).project.is_none());
    assert!(context_scope_items(&db,"a","p1").unwrap().is_empty());
}

#[test]
fn formal_thread_corrections_preserve_profile_boundaries_and_explicit_unlink() {
    use workcontext_app::items::item_correct;
    let mut db = db();
    db.execute_batch("INSERT INTO work_thread(id,title,profile,created_at,updated_at) VALUES
        ('project','Build','a',1,1),('foreign','Build','b',1,1),('legacy','Old',NULL,1,1)").unwrap();
    let card = view(&mut db,"a",&[candidate("one","First",State::Mine)]).unwrap().remove(0);
    move_card(&mut db,"a",&card.id,&card.version,State::Mine,Some("Check"),10).unwrap();
    let correct = |db: &mut Connection, thread: Option<&str>| {
        let args = serde_json::from_value(json!({"id":card.id,"patch":{"threadId":thread},"reason":"wrong_link"})).unwrap();
        item_correct(db,&args,11)
    };
    assert!(correct(&mut db,Some("foreign")).is_err());
    let linked = correct(&mut db,Some("project")).unwrap();
    assert_eq!(linked.item.thread_id.as_deref(),Some("project"));
    assert_eq!(linked.delivery.unwrap().scope_id.as_deref(),Some("project"));
    let legacy = correct(&mut db,Some("legacy")).unwrap().delivery.unwrap();
    assert_eq!(legacy.project.as_deref(),Some("Old"));
    assert!(legacy.scope_id.is_none());
    let cleared = correct(&mut db,None).unwrap();
    assert!(cleared.item.thread_id.is_none());
    assert!(cleared.delivery.unwrap().project.is_none());
}

#[test]
fn unlinking_legacy_thread_records_explicit_removal_only_at_current_revision() {
    use workcontext_app::ownership_flow::context_item_scope_set;
    let mut db = db();
    let card = view(&mut db,"a",&[candidate("one","First",State::Mine)]).unwrap().remove(0);
    move_card(&mut db,"a",&card.id,&card.version,State::Mine,Some("Check"),10).unwrap();
    db.execute("INSERT INTO work_thread(id,title,created_at,updated_at) VALUES ('old-thread','发布',1,1)",[]).unwrap();
    db.execute("UPDATE work_item SET thread_id='old-thread' WHERE id=?1",[&card.id]).unwrap();
    let relation = || db.query_row("SELECT project_override,context_scope_id,thread_id,revision
        FROM ownership_card JOIN work_item USING(id) WHERE id=?1",[&card.id],|row| Ok((
        row.get::<_,Option<String>>(0)?,row.get::<_,Option<String>>(1)?,row.get::<_,Option<String>>(2)?,row.get::<_,i64>(3)?
    ))).unwrap();
    let revision = relation().3;
    let original = (None,None,Some("old-thread".to_owned()),revision);
    let detail = item_get_for_profile(&db,&card.id,Some("a")).unwrap();
    assert_eq!(detail.item.thread_id.as_deref(),Some("old-thread"));
    assert_eq!(relation(),original);
    assert!(context_item_scope_set(&db,"a",&card.id,&(revision-1).to_string(),None).is_err());
    assert_eq!(relation(),original);
    context_item_scope_set(&db,"a",&card.id,&detail.delivery.unwrap().version,None).unwrap();
    let (name_override, scope, thread, new_revision) = relation();
    assert_eq!((name_override, scope, thread), (Some(String::new()), None, None));
    assert!(new_revision > revision);
    assert!(context_item_scope_set(&db,"a",&card.id,&revision.to_string(),None).is_err());
}

#[test]
fn evidence_fallback_uses_newest_recorded_dws_time_and_ignores_unparseable_dates() {
    let mut db = db();
    let mut report = candidate("proof","Review",State::Mine);
    report.source_summary.place.clear();
    let mut newest = report.evidence_details[0].clone();
    newest.time = "1970-01-02 08:00:01".into();
    newest.conversation = "New room".into();
    let mut earlier = newest.clone();
    earlier.time = "1970-01-01 08:00:01".into();
    earlier.conversation = "Earlier room".into();
    report.evidence_details.extend([newest,earlier]);
    let card = view(&mut db,"a",&[report]).unwrap().remove(0);
    assert_eq!(workcontext_app::items::item_get(&db,&card.id).unwrap_err().code(),"NOT_FOUND");
    let summary = page(&db,"all",None,0).items.remove(0);
    assert_eq!(summary.source_label.as_deref(),Some("New room"));
    assert_eq!(summary.last_evidence_at,Some(86_401_000));
    assert_eq!(item_get_for_profile(&db,&card.id,Some("a")).unwrap().item.last_evidence_at,summary.last_evidence_at);
    db.execute("UPDATE ownership_card SET candidate_json=json_remove(candidate_json,'$.evidenceDetails[2]','$.evidenceDetails[1]') WHERE id=?1",[&card.id]).unwrap();
    assert_eq!(page(&db,"all",None,0).items[0].last_evidence_at,None);
}
