//! Transactional handoff between the local archive, desktop worker and record projections.
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use workcontext_store::rusqlite::{self, params, Connection, OptionalExtension, TransactionBehavior};

use crate::{context::{require_scope, validate_text}, items::ItemsError, organizer_archive::ChangePage};

type Result<T, E = ItemsError> = std::result::Result<T, E>;
const LEASE_MS: i64 = 300_000;

#[derive(Debug)]
pub struct Job {
    pub input_id: String, pub origin_ref: String, pub original: String, pub token: String,
    pub source_version: i64, pub archive: bool,
    pub authorized_inputs: Vec<String>,
    /// Current redacted excerpt, rebuilt from the authorized snapshot before send and apply.
    pub reply_excerpt: Option<String>,
    pub reply_to: Option<String>, pub reply_version: Option<i64>, pub reply_expires_at: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all="camelCase")]
pub struct Candidate {
    pub id: String, pub title: String, pub scope_id: Option<String>, pub delivery: String, pub anchors: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all="camelCase")]
pub struct Facets { pub facts: Vec<String>, pub inferences: Vec<String>, pub open_questions: Vec<String> }

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all="camelCase")]
pub struct Disposition {
    pub title: String, pub content: String, pub kind: String, pub scope_id: Option<String>,
    pub target_id: Option<String>, pub anchor: String, pub delivery: String, pub facets: Facets,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision { pub records: Vec<Disposition> }

#[derive(Debug, Serialize)]
#[serde(rename_all="camelCase")]
pub struct Status {
    pub pending: i64, pub claimed: i64, pub retrying: i64, pub organized: i64, pub no_record: i64,
    pub checkpoint: i64, pub last_organized_at: Option<i64>, pub next_retry_at: Option<i64>, pub last_error: Option<String>,
    pub oldest_pending_at: Option<i64>,
}

fn invalid() -> ItemsError { ItemsError::InvalidArgument { message: "整理输入、证据或结果无效。".into() } }

pub fn checkpoint(db: &Connection, profile: &str) -> Result<i64> {
    Ok(db.query_row("SELECT seq FROM organizer_checkpoint WHERE profile=?1", [profile], |r| r.get(0)).optional()?.unwrap_or(0))
}

pub fn reset_if_grant_changed(db:&mut Connection,profile:&str,key:&str) -> Result<bool> {
    validate_text(profile,1024,true)?;
    validate_text(key,1024,true)?;
    let tx=db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let changed=tx.execute("INSERT INTO organizer_checkpoint(profile,seq,grant_key) VALUES(?1,0,?2)
        ON CONFLICT(profile) DO UPDATE SET seq=0,grant_key=excluded.grant_key WHERE organizer_checkpoint.grant_key IS NOT excluded.grant_key",params![profile,key])?;
    if changed>0 {
        tx.execute("UPDATE context_input SET processing_state='pending' WHERE profile=?1 AND origin_ref IS NOT NULL",[profile])?;
    }
    tx.commit()?;
    Ok(changed>0)
}

/// Compare-and-set the reader position. Replayed pages and concurrent older reads cannot roll it back.
pub fn ingest(db: &mut Connection, profile: &str, after: i64, page: &ChangePage) -> Result<bool> {
    validate_text(profile,1024,true)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if checkpoint(&tx,profile)? != after { return Ok(false); }
    let mut previous = after;
    let received_at=workcontext_store::now_ms();
    for change in &page.changes {
        if change.seq <= previous || change.seq > page.checkpoint || change.source_ref.is_empty() { return Err(invalid()); }
        previous = change.seq;
        tx.execute("INSERT INTO context_input(profile,original,created_at,origin_ref,source_version,source_occurred_at,processing_state)
            VALUES(?1,'',?5,?3,?4,?2,'pending') ON CONFLICT(profile,origin_ref) DO UPDATE SET
            source_version=excluded.source_version,source_occurred_at=excluded.source_occurred_at,processing_state='pending'
            WHERE excluded.source_version>context_input.source_version",
            params![profile,change.occurred_at,change.source_ref,change.seq,received_at])?;
        tx.execute("DELETE FROM organizer_facet WHERE profile=?1 AND origin_ref=?2 AND version<?3",
            params![profile,change.source_ref,change.seq])?;
    }
    if page.checkpoint < previous { return Err(invalid()); }
    tx.execute("INSERT INTO organizer_checkpoint(profile,seq) VALUES(?1,?2) ON CONFLICT(profile) DO UPDATE SET seq=excluded.seq",params![profile,page.checkpoint])?;
    tx.commit()?;
    Ok(true)
}

pub fn claim(db: &mut Connection, profile: &str, now: i64) -> Result<Option<Job>> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let expired=tx.prepare_cached("UPDATE organizer_job SET reply_ref=NULL,reply_version=NULL,reply_expires_at=NULL
        WHERE profile=?1 AND input_id IN (SELECT input_id FROM organizer_job WHERE profile=?1 AND reply_expires_at<=?2
        ORDER BY reply_expires_at,input_id LIMIT 32) RETURNING input_id")?
        .query_map(params![profile,now],|row| row.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
    for input in expired {
        tx.execute("DELETE FROM organizer_facet WHERE profile=?1 AND input_id=?2 AND reply_ref IS NOT NULL",params![profile,input])?;
        tx.execute("UPDATE context_input SET processing_state='pending' WHERE profile=?1 AND id=?2",params![profile,input])?;
    }
    let selected = tx.query_row("UPDATE organizer_job SET state='claimed',token=lower(hex(randomblob(16))),lease_until=?3,attempts=MIN(attempts+1,32)
        WHERE profile=?1 AND input_id=(SELECT j.input_id FROM organizer_job j JOIN context_input i ON i.profile=j.profile AND i.id=j.input_id
        WHERE j.profile=?1 AND ((j.state IN ('pending','retry') AND j.next_at<=?2) OR (j.state='claimed' AND j.lease_until<=?2))
        ORDER BY j.next_at,coalesce(i.source_occurred_at,i.created_at),i.created_at,j.input_id LIMIT 1) RETURNING input_id,version,token",
        params![profile,now,now.saturating_add(LEASE_MS)], |r| Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?))).optional()?;
    let Some((input_id,source_version,token)) = selected else { return Ok(None); };
    let job = tx.query_row("SELECT coalesce(origin_ref,id),CASE WHEN producer IS NULL THEN original ELSE json_extract(original,'$.result') END,
        origin_ref IS NOT NULL FROM context_input WHERE profile=?1 AND id=?2",
        params![profile,input_id], |r| Ok(Job { origin_ref:r.get(0)?, original:r.get(1)?,archive:r.get(2)?,input_id:input_id.clone(),source_version,token,authorized_inputs:Vec::new(),reply_excerpt:None,reply_to:None,reply_version:None,reply_expires_at:None }))?;
    tx.commit()?;
    Ok(Some(job))
}

pub fn live(db: &Connection,profile:&str,job:&Job,now:i64) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM organizer_job WHERE profile=?1 AND input_id=?2 AND version=?3 AND token=?4 AND state='claimed' AND lease_until>?5)",
        params![profile,job.input_id,job.source_version,job.token,now], |r| r.get(0))?)
}

pub fn fail(db: &mut Connection,profile:&str,job:&Job,now:i64,reason:&str) -> Result<bool> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if !live(&tx,profile,job,now)? { return Ok(false); }
    // Persist a bounded machine-readable code only. Provider messages may include request excerpts.
    let code = if reason.len()<=64 && !reason.is_empty() && reason.bytes().all(|c| c.is_ascii_alphanumeric() || c==b'_') { reason } else { "organizer_failed" };
    tx.execute("UPDATE organizer_job SET state='retry',token=NULL,lease_until=NULL,next_at=?3+MIN(3600000,5000*(1<<MIN(attempts-1,10))),last_error=?4
        WHERE profile=?1 AND input_id=?2",params![profile,job.input_id,now,code])?;
    tx.execute("UPDATE context_input SET processing_state='retry' WHERE profile=?1 AND id=?2",params![profile,job.input_id])?;
    tx.commit()?;
    Ok(true)
}

pub fn retry(db:&mut Connection,profile:&str,now:i64) -> Result<usize> {
    Ok(db.execute("UPDATE organizer_job SET next_at=?2 WHERE profile=?1 AND state='retry' AND next_at>?2",params![profile,now])?)
}

pub fn status(db:&Connection,profile:&str) -> Result<Status> {
    Ok(db.query_row("SELECT count(*) FILTER (WHERE state='pending'),count(*) FILTER (WHERE state='claimed'),count(*) FILTER (WHERE state='retry'),
        count(*) FILTER (WHERE state='organized'),count(*) FILTER (WHERE state='no_record'),
        coalesce((SELECT seq FROM organizer_checkpoint WHERE profile=?1),0),(SELECT last_organized_at FROM organizer_checkpoint WHERE profile=?1),min(next_at) FILTER (WHERE state='retry'),
        (SELECT last_error FROM organizer_job WHERE profile=?1 AND state='retry' ORDER BY next_at DESC,input_id LIMIT 1),
        min(queued_at) FILTER (WHERE state IN ('pending','claimed','retry'))
        FROM organizer_job WHERE profile=?1",[profile],|r| Ok(Status {pending:r.get(0)?,claimed:r.get(1)?,retrying:r.get(2)?,organized:r.get(3)?,no_record:r.get(4)?,
            checkpoint:r.get(5)?,last_organized_at:r.get(6)?,next_retry_at:r.get(7)?,last_error:r.get(8)?,oldest_pending_at:r.get(9)?}))?)
}

/// A ticket is a complete ASCII token, never a substring of another ID. Multiple tickets are ambiguous.
fn ticket(text: &str) -> Option<&str> {
    let mut found = None;
    for token in text.split(|c: char| !c.is_ascii_alphanumeric() && c!='-' && c!='_') {
        let Some((prefix,number))=token.split_once('-') else { continue };
        if !(2..=12).contains(&prefix.len()) || !prefix.as_bytes()[0].is_ascii_uppercase()
            || !prefix.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            || !(1..=12).contains(&number.len()) || number.starts_with('0') || !number.bytes().all(|c| c.is_ascii_digit()) { continue; }
        if found.is_some_and(|existing| existing!=token) { return None; }
        found=Some(token);
    }
    found
}

fn native_reply(job: &Job) -> Option<&str> {
    job.reply_to.as_deref().filter(|_| job.reply_version.is_some_and(|version| version>=0))
}

pub fn candidate_archive_ids(db:&Connection,profile:&str,job:&Job) -> Result<Vec<String>> {
    let key=ticket(&job.original);
    let reply = native_reply(job);
    if key.is_none() && reply.is_none() { return Ok(Vec::new()); }
    let mut stmt=db.prepare_cached("SELECT DISTINCT f.input_id FROM organizer_facet f
        JOIN context_record r ON r.profile=f.profile AND r.id=f.record_id
        JOIN context_input i ON i.profile=f.profile AND i.id=f.input_id
        WHERE f.profile=?1 AND i.origin_ref IS NOT NULL AND ((f.ticket=?2 AND r.organizer_created=1 AND r.input_id=f.input_id)
            OR f.origin_ref=?3)
        UNION SELECT r.input_id FROM organizer_facet f JOIN context_record r ON r.profile=f.profile AND r.id=f.record_id
        WHERE f.profile=?1 AND f.origin_ref=?3 AND r.organizer_created=1 ORDER BY 1 LIMIT 33")?;
    let ids=stmt.query_map(params![profile,key,reply],|r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(if ids.len()>32 {Vec::new()} else {ids})
}

pub fn candidates(db:&Connection,profile:&str,job:&Job) -> Result<Vec<Candidate>> {
    let key=ticket(&job.original);
    let allowed=serde_json::to_string(&job.authorized_inputs).map_err(|_|invalid())?;
    let mut stmt=db.prepare_cached("SELECT r.id,r.title,s.scope_id,coalesce(r.reported_result,r.content),
        CASE WHEN r.input_id=?2 OR EXISTS(SELECT 1 FROM record_ref WHERE record_id=r.id AND material=?3) THEN 1
            WHEN EXISTS(SELECT 1 FROM record_ref WHERE record_id=r.id AND material=?6) THEN 2 ELSE 0 END AS direct
        FROM context_record r LEFT JOIN effective_record_scope s ON s.profile=r.profile AND s.record_id=r.id
        WHERE r.profile=?1 AND r.organizer_created=0 AND (direct OR instr(coalesce(r.reported_result,r.content),?4)>0)
        UNION ALL SELECT r.id,CASE WHEN f.reply_ref IS NULL THEN f.delivery ELSE f.content END,CASE WHEN c.scope_ids IS NOT NULL OR s.scope_id IS NOT NULL THEN s.scope_id ELSE f.scope_id END,?4,
        CASE WHEN f.origin_ref=?6 THEN 2 ELSE 0 END
        FROM organizer_facet f JOIN context_record r ON r.profile=f.profile AND r.id=f.record_id
        LEFT JOIN current_record_correction c ON c.profile=r.profile AND c.record_id=r.id
        LEFT JOIN effective_record_scope s ON s.profile=r.profile AND s.record_id=r.id
        WHERE f.profile=?1 AND (f.reply_ref IS NULL OR (
            f.origin_ref=?6 AND r.organizer_created=1 AND r.input_id=f.input_id AND f.scope_id IS NULL AND s.scope_id IS NULL
            AND length(f.content) BETWEEN 1 AND 512 AND length(trim(?7))>0 AND json_array_length(f.facets_json,'$.facts')>0
            AND f.content=(SELECT group_concat(value,char(10)) FROM json_each(f.facets_json,'$.facts'))
            AND NOT EXISTS (SELECT 1 FROM json_each(f.facets_json,'$.facts') fact WHERE instr(?7,fact.value)=0)
            AND NOT EXISTS (SELECT 1 FROM organizer_facet other WHERE other.profile=f.profile AND other.input_id=f.input_id AND other.slot<>f.slot)
        )) AND ((f.ticket=?4 AND r.organizer_created=1 AND r.input_id=f.input_id)
            OR (f.origin_ref=?6 AND (r.organizer_created=0 OR r.input_id IN (SELECT value FROM json_each(?5)))))
            AND f.input_id IN (SELECT value FROM json_each(?5))
            AND NOT EXISTS (SELECT 1 FROM context_input earlier JOIN context_input current ON current.profile=earlier.profile
                WHERE earlier.profile=f.profile AND earlier.id=f.input_id AND current.id=?2
                  AND earlier.source_occurred_at>current.source_occurred_at)
        ORDER BY 1,3 LIMIT 33")?;
    let mut rows=stmt.query(params![profile,job.input_id,job.origin_ref,key,allowed,native_reply(job),job.reply_excerpt])?;
    let mut matches=Vec::new();
    let mut scanned=0;
    while let Some(row)=rows.next()? {
        scanned+=1;
        if scanned>32 { return Ok(Vec::new()); }
        let direct:i64=row.get(4)?;
        let text:Option<String>=row.get(3)?;
        let (anchor,title)=if direct==1 { (job.origin_ref.clone(),row.get::<_,String>(1)?) }
            else if direct==2 { (job.reply_to.clone().ok_or_else(invalid)?,row.get(1)?) }
            else if let Some(key)=key.filter(|k| text.as_deref().and_then(ticket)==Some(*k)) { (key.to_owned(),row.get(1)?) }
            else { continue };
        matches.push(Candidate { id:row.get(0)?,scope_id:row.get(2)?,delivery:title.clone(),title,anchors:vec![anchor] });
    }
    // Do not let provider ordering choose between competing records for one piece of evidence.
    let mut seen=HashSet::new();
    let mut ambiguous=HashSet::new();
    for c in &matches {
        let identity=(c.scope_id.clone(),c.anchors[0].clone());
        if !seen.insert(identity.clone()) { ambiguous.insert(identity); }
    }
    matches.retain(|c| !ambiguous.contains(&(c.scope_id.clone(),c.anchors[0].clone())));
    Ok(matches)
}

fn validate(disposition:&Disposition,job:&Job) -> Result<()> {
    validate_text(&disposition.title,512,true)?;
    validate_text(&disposition.content,65536,true)?;
    validate_text(&disposition.delivery,512,true)?;
    if disposition.anchor != job.origin_ref || !matches!(disposition.kind.as_str(),"event"|"progress"|"decision"|"experience") { return Err(invalid()); }
    for values in [&disposition.facets.facts,&disposition.facets.inferences,&disposition.facets.open_questions] {
        if values.len()>64 { return Err(invalid()); }
        for value in values { validate_text(value,4096,true)?; }
    }
    let facts=&disposition.facets.facts;
    if facts.is_empty() || facts.iter().any(|fact| !job.original.contains(fact.as_str())) { return Err(invalid()); }
    // Compare incrementally so output text cannot smuggle an unquoted conclusion between factual statements.
    let mut remaining=disposition.content.as_str();
    for (position,fact) in facts.iter().enumerate() {
        if position>0 { remaining=remaining.strip_prefix('\n').ok_or_else(invalid)?; }
        remaining=remaining.strip_prefix(fact.as_str()).ok_or_else(invalid)?;
    }
    if !remaining.is_empty() { return Err(invalid()); }
    Ok(())
}

/// Each output owns a source-versioned facet. Confirmed record fields, corrections and scope membership are untouched.
pub fn apply(db:&mut Connection,profile:&str,job:&Job,result:&Decision,now:i64) -> Result<bool> {
    let tx=db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let applied=apply_in(&tx,profile,job,result,now)?;
    tx.commit()?;
    Ok(applied)
}

/// The caller retains a single immediate transaction through authorization and final model-payload comparison.
pub fn apply_in(tx:&rusqlite::Transaction<'_>,profile:&str,job:&Job,result:&Decision,now:i64) -> Result<bool> {
    if !live(tx,profile,job,now)? { return Ok(false); }
    if job.reply_to.is_some()!=job.reply_version.is_some() || job.reply_to.as_deref()==Some(job.origin_ref.as_str())
        || job.reply_to.is_some()!=job.reply_expires_at.is_some() || (job.reply_to.is_some() && !job.archive) { return Err(invalid()); }
    if job.reply_expires_at.is_some_and(|end| end<=now) { return Ok(false); }
    if result.records.len()>32 { return Err(invalid()); }
    let available=candidates(tx,profile,job)?;
    let mut deliveries=HashSet::new();
    tx.execute("DELETE FROM organizer_facet WHERE profile=?1 AND input_id=?2",params![profile,job.input_id])?;
    for (slot,item) in result.records.iter().enumerate() {
        validate(item,job)?;
        if !deliveries.insert(&item.delivery) { return Err(invalid()); }
        if let Some(scope)=&item.scope_id { require_scope(tx,profile,scope)?; }
        let record_id=if let Some(target)=&item.target_id {
            if !available.iter().any(|c| c.id==*target && c.scope_id==item.scope_id && c.delivery==item.delivery
                && c.anchors.iter().any(|a| a==&job.origin_ref || Some(a)==job.reply_to.as_ref() || item.facets.facts.iter().any(|fact| ticket(fact)==Some(a.as_str())))) { return Err(invalid()); }
            target.clone()
        } else {
            let known:Option<String>=tx.query_row("SELECT record_id FROM organizer_identity WHERE profile=?1 AND input_id=?2 AND delivery=?3
                UNION ALL SELECT id FROM context_record WHERE profile=?1 AND input_id=?2 AND organizer_created=0 AND ?4=1 LIMIT 1",
                params![profile,job.input_id,item.delivery,result.records.len() as i64],|r| r.get(0)).optional()?;
            if let Some(id)=known { id } else {
                let id:String=tx.query_row("INSERT INTO context_record(profile,input_id,title,content,kind,occurred_at,created_at,organizer_created)
                    SELECT profile,id,'','',?3,source_occurred_at,?4,1 FROM context_input WHERE profile=?1 AND id=?2 RETURNING id",
                    params![profile,job.input_id,item.kind,now],|r| r.get(0))?;
                tx.execute("INSERT INTO organizer_identity(profile,input_id,delivery,record_id) VALUES(?1,?2,?3,?4)",params![profile,job.input_id,item.delivery,id])?;
                id
            }
        };
        let facets=serde_json::to_string(&item.facets).map_err(|_|invalid())?;
        let key=ticket(&job.original).filter(|key| item.facets.facts.iter().any(|fact| ticket(fact)==Some(*key)));
        tx.execute("INSERT INTO organizer_facet(profile,record_id,input_id,slot,title,content,kind,scope_id,origin_ref,version,delivery,facets_json,ticket,reply_ref,reply_version)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",params![profile,record_id,job.input_id,slot as i64,item.title,item.content,item.kind,item.scope_id,job.origin_ref,job.source_version,item.delivery,facets,key,job.reply_to,job.reply_version])?;
    }
    // A native child may have finished before its parent was indexed or retried. Only
    // wake for sources without a native parent to avoid cycles in same-timestamp native replies.
    if job.archive && !result.records.is_empty() && (job.reply_to.is_none() || job.reply_version.is_some_and(|v| v<0)) {
        tx.execute("UPDATE context_input SET processing_state='pending' WHERE profile=?1 AND id IN (
            SELECT input_id FROM organizer_job WHERE profile=?1 AND reply_ref=?2 AND reply_version>=0
                AND state IN ('organized','no_record'))",params![profile,job.origin_ref])?;
    }
    let state=if result.records.is_empty() {"no_record"} else {"organized"};
    tx.execute("UPDATE organizer_job SET state=?3,finished_at=?4,last_error=NULL,token=NULL,lease_until=NULL,reply_ref=?5,reply_version=?6,reply_expires_at=?7
        WHERE profile=?1 AND input_id=?2",params![profile,job.input_id,state,now,job.reply_to,job.reply_version,job.reply_expires_at])?;
    tx.execute("UPDATE context_input SET processing_state=?3 WHERE profile=?1 AND id=?2",params![profile,job.input_id,state])?;
    tx.execute("INSERT INTO organizer_checkpoint(profile,seq,last_organized_at) VALUES(?1,0,?2) ON CONFLICT(profile) DO UPDATE SET
        last_organized_at=max(coalesce(last_organized_at,excluded.last_organized_at),excluded.last_organized_at)",params![profile,now])?;
    Ok(true)
}
