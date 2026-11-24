//! Incremental local evidence for the desktop organizer.
use crate::external_archive::{archive_ref, bound_snapshot, mask_text, masked_body, unpack, ArchiveError, Policy, FIRST_VALID_MS, LAST_VALID_MS, READ_BODY};
use serde_json::Value;
use std::path::Path;
use workcontext_store::rusqlite::{params, Connection, OptionalExtension};

#[derive(Debug)]
pub struct Change {
    pub seq: i64,
    pub source_ref: String,
    pub occurred_at: i64,
}

#[derive(Debug)]
pub struct ChangePage {
    pub changes: Vec<Change>,
    /// Last examined revision, including inaccessible or deleted rows.
    pub checkpoint: i64,
}

#[derive(Debug)]
pub struct Evidence {
    pub seq: i64,
    pub occurred_at: i64,
    pub body: Option<String>,
    pub attachments: Vec<String>,
    pub reply_parent: Option<ReplyParent>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ReplyParent {
    pub source_ref: String,
    pub excerpt: String,
    pub seq: i64,
    pub expires_at: i64,
}

fn snapshot(work: &Connection, root: &Path, profile: &str, now: i64) -> Result<(Connection, Policy, String, bool), ArchiveError> {
    let cfg: Value = serde_json::from_slice(&std::fs::read(root.join("config.json")).map_err(|_| ArchiveError::Denied)?)
        .map_err(|_| ArchiveError::Denied)?;
    let consent = cfg.get("contextOrganizer").ok_or(ArchiveError::Denied)?;
    if crate::external_access::workspace_profile(root).map_err(|_| ArchiveError::Denied)? != profile
        || cfg.pointer("/workspaceAccount/profile").and_then(Value::as_str) != Some(profile)
        || consent.get("enabled").and_then(Value::as_bool) != Some(true)
        || consent.get("workspace").and_then(Value::as_str).map(Path::new) != Some(root.canonicalize().map_err(|_| ArchiveError::Denied)?.as_path())
        || consent.get("profile").and_then(Value::as_str) != Some(profile)
        || ["model", "epoch"].iter().any(|key| consent.get(*key).and_then(Value::as_str).is_none_or(|s| s.trim().is_empty())) {
        return Err(ArchiveError::Denied);
    }
    let policy = Policy::from_settings(work, &cfg, profile, now)?;
    Ok((bound_snapshot(root, profile, now)?.0, policy, consent["epoch"].as_str().ok_or(ArchiveError::Denied)?.to_owned(),
        consent.get("adjacent").and_then(Value::as_bool)==Some(true)))
}

pub fn grant_key(work: &Connection, root: &Path, profile: &str, now: i64) -> Result<String, ArchiveError> {
    let (_,policy,epoch,_) = snapshot(work,root,profile,now)?;
    policy.grant_key(now,&epoch)
}

fn allowed(policy: &Policy, cid: &str, sender: Option<&str>, time: i64, now: i64) -> bool {
    (FIRST_VALID_MS..=LAST_VALID_MS.min(now)).contains(&time) && time > policy.cutoff && policy.covers(cid, sender, time)
}

/// Existing archives can be organized before the next DWS capture. The collector and
/// desktop advance the same cursor under a SQLite write transaction.
fn bootstrap(root: &Path, profile: &str, limit: usize) -> Result<(), ArchiveError> {
    use workcontext_store::rusqlite::{OpenFlags, TransactionBehavior};
    let mut db = Connection::open_with_flags(root.join("archive/archive.sqlite3"), OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    db.execute_batch("PRAGMA trusted_schema=OFF")?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let bound: String = tx.query_row("SELECT value FROM archive_state WHERE key='profile'", [], |r| r.get(0))?;
    if bound != profile { return Err(ArchiveError::Denied); }
    tx.execute_batch("CREATE TABLE IF NOT EXISTS organizer_changes(seq INTEGER PRIMARY KEY AUTOINCREMENT,
        cid TEXT NOT NULL, mid TEXT NOT NULL, UNIQUE(cid,mid))")?;
    let cursor: Option<String> = tx.query_row("SELECT value FROM archive_state WHERE key='organizer_bootstrap_rowid'", [], |r| r.get(0)).optional()?;
    if cursor.as_deref() != Some("done") {
        let start: i64 = cursor.as_deref().unwrap_or("0").parse().map_err(|_| ArchiveError::Unavailable)?;
        let (count,last): (usize,Option<i64>) = tx.query_row("SELECT count(*),max(rid) FROM (
            SELECT rowid AS rid FROM messages WHERE rowid>?1 ORDER BY rowid LIMIT ?2)", params![start,limit as i64], |r| Ok((r.get(0)?,r.get(1)?)))?;
        if let Some(end) = last {
            tx.execute("INSERT INTO organizer_changes(cid,mid) SELECT cid,mid FROM messages WHERE rowid>?1 AND rowid<=?2
                ORDER BY rowid ON CONFLICT(cid,mid) DO NOTHING",params![start,end])?;
        }
        let next = last.filter(|_| count == limit).map_or_else(|| "done".to_owned(), |n| n.to_string());
        tx.execute("INSERT INTO archive_state(key,value) VALUES('organizer_bootstrap_rowid',?1)
            ON CONFLICT(key) DO UPDATE SET value=excluded.value", [next])?;
    }
    tx.commit()?;
    Ok(())
}

pub fn changes(work: &Connection, root: &Path, profile: &str, after: i64, limit: usize, now: i64) -> Result<ChangePage, ArchiveError> {
    read_changes(work,root,profile,after,limit,now,None)
}

pub fn changes_for_grant(work: &Connection, root: &Path, profile: &str, after: i64, limit: usize, now: i64, key: &str) -> Result<ChangePage, ArchiveError> {
    read_changes(work,root,profile,after,limit,now,Some(key))
}

fn read_changes(work: &Connection, root: &Path, profile: &str, after: i64, limit: usize, now: i64, key: Option<&str>) -> Result<ChangePage, ArchiveError> {
    if after < 0 || !(1..=512).contains(&limit) { return Err(ArchiveError::Invalid); }
    let (archive, policy, epoch, _) = snapshot(work, root, profile, now)?;
    if let Some(expected) = key {
        if policy.grant_key(now,&epoch)? != expected { return Err(ArchiveError::Denied); }
    }
    let seeded: bool = archive.query_row("SELECT EXISTS(SELECT 1 FROM archive_state WHERE key='organizer_bootstrap_rowid' AND value='done')", [], |r| r.get(0))?;
    let archive = if seeded { archive } else {
        drop(archive);
        bootstrap(root,profile,limit)?;
        bound_snapshot(root,profile,now)?.0
    };
    let mut stmt = archive.prepare("SELECT c.seq,c.cid,c.mid,m.occurred_at,m.sender_id FROM organizer_changes c
        LEFT JOIN messages m ON c.cid=m.cid AND c.mid=m.mid AND typeof(m.occurred_at)='integer'
        WHERE c.seq>?1 ORDER BY c.seq LIMIT ?2")?;
    let mut rows = stmt.query(params![after, limit as i64])?;
    let mut page = ChangePage {changes: Vec::new(), checkpoint: after};
    let mut known = work.prepare("SELECT source_occurred_at FROM context_input WHERE profile=?1 AND origin_ref=?2 AND source_version<?3")?;
    while let Some(row) = rows.next()? {
        page.checkpoint = row.get(0)?;
        let cid: String = row.get(1)?;
        let mid: String = row.get(2)?;
        let source_ref = archive_ref(profile,&cid,&mid);
        let sender: Option<String> = row.get(4)?;
        let current = row.get::<_,Option<i64>>(3)?.filter(|t| allowed(&policy,&cid,sender.as_deref(),*t,now));
        let occurred_at = match current {
            Some(time) => time,
            None => match known.query_row(params![profile,source_ref,page.checkpoint], |r| r.get::<_,Option<i64>>(0)).optional()?.flatten() {
                Some(previous) => previous,
                None => continue,
            },
        };
        page.changes.push(Change {seq: page.checkpoint, source_ref, occurred_at});
    }
    Ok(page)
}

pub fn latest(work: &Connection, root: &Path, profile: &str, source_ref: &str, now: i64) -> Result<Option<Evidence>, ArchiveError> {
    let (owner,cid,mid): (String,String,String) = unpack("archive1-",source_ref)?;
    if owner != profile { return Err(ArchiveError::Denied); }
    let (archive, policy, _, adjacent) = snapshot(work, root, profile, now)?;
    let row: Option<(i64,i64,Option<String>,Option<String>)> = archive.query_row("SELECT seq,occurred_at,sender_id,sender_name
        FROM organizer_changes JOIN messages USING(cid,mid) WHERE cid=?1 AND mid=?2 AND typeof(occurred_at)='integer'",
        params![cid,mid], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let Some((seq,occurred_at,sender,name)) = row else { return Ok(None) };
    if !allowed(&policy, &cid, sender.as_deref(), occurred_at, now) { return Ok(None); }
    let body = masked_body(&mut archive.prepare(READ_BODY)?, &cid, &mid)?.map(|t| t.chars().take(8192).collect());
    // Only directly attributed top-level resources in the bound extraction store.
    let mut stmt = archive.prepare(&format!("SELECT substr(a.text_content,1,4096) {ATTACHMENTS} ORDER BY a.id LIMIT 4"))?;
    let attachments = stmt.query_map(params![cid,mid,profile], |r| r.get::<_,String>(0))?
        .map(|r| r.map(|t| mask_text(&t,name.as_deref()).chars().take(2048).collect())).collect::<Result<Vec<_>,_>>()?;
    let reply_parent = parent_evidence(&archive, &policy, profile, &cid, &mid, now, adjacent)?;
    Ok(Some(Evidence {seq, occurred_at, body, attachments, reply_parent}))
}

fn parent_evidence(archive: &Connection, policy: &Policy, profile: &str, cid: &str, mid: &str, now: i64, adjacent: bool) -> Result<Option<ReplyParent>, ArchiveError> {
    use sha2::{Digest, Sha256};
    let row: Option<(String,i64,Option<String>)> = archive.query_row("SELECT p.mid,p.occurred_at,p.sender_id
        FROM messages c JOIN messages p ON p.cid=c.cid AND p.mid=c.reply_to AND p.mid<>c.mid
        WHERE c.cid=?1 AND c.mid=?2 AND typeof(p.occurred_at)='integer' AND p.occurred_at<=c.occurred_at AND EXISTS (
            SELECT 1 FROM message_relations r WHERE r.cid=c.cid AND r.mid=c.mid AND r.kind='quote'
              AND r.path IN ('quotedMessage','detail.quotedMessage') AND r.source_cid=p.cid AND r.source_mid=p.mid)",
        params![cid,mid], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    // A native reply with a missing or inaccessible parent never falls back to conversation history.
    let native = row.is_some();
    let row = if native || !adjacent { row } else {
        let mut stmt = archive.prepare("SELECT p.mid,p.occurred_at,p.sender_id FROM messages c JOIN messages p ON p.cid=c.cid
            WHERE c.cid=?1 AND c.mid=?2 AND c.reply_to IS NULL AND typeof(p.occurred_at)='integer'
              AND NOT EXISTS (SELECT 1 FROM message_relations r WHERE r.cid=c.cid AND r.mid=c.mid AND r.kind IN ('quote','forward'))
              AND p.occurred_at BETWEEN c.occurred_at-600000 AND c.occurred_at-1
              AND NOT EXISTS (SELECT 1 FROM messages q WHERE q.cid=c.cid AND q.occurred_at=c.occurred_at AND q.mid<>c.mid)
            ORDER BY p.occurred_at DESC LIMIT 2")?;
        let mut rows = stmt.query(params![cid,mid])?;
        let first = rows.next()?.map(|r| Ok::<_,workcontext_store::rusqlite::Error>((r.get(0)?,r.get::<_,i64>(1)?,r.get(2)?))).transpose()?;
        if let (Some((_,time,_)), Some(second)) = (&first,rows.next()?) {
            if *time==second.get::<_,i64>(1)? { return Ok(None); }
        }
        first
    };
    let Some((parent,time,sender)) = row else { return Ok(None) };
    if !native {
        let same_thread: bool = archive.query_row("SELECT EXISTS(SELECT 1 FROM message_capture c JOIN message_capture p
            ON c.cid=p.cid AND c.thread_id IS p.thread_id WHERE c.cid=?1 AND c.mid=?2 AND p.mid=?3)",
            params![cid,mid,parent], |row| row.get(0))?;
        if !same_thread { return Ok(None); }
    }
    if !allowed(policy,cid,sender.as_deref(),time,now) { return Ok(None); }
    let Some(text) = masked_body(&mut archive.prepare(READ_BODY)?,cid,&parent)? else { return Ok(None) };
    let excerpt: String = text.trim().chars().take(1024).collect();
    if excerpt.is_empty() { return Ok(None); }
    let digest = Sha256::digest(excerpt.as_bytes());
    let seq = i64::from_be_bytes(digest[..8].try_into().expect("SHA256 prefix")) & i64::MAX;
    // Negative fingerprints distinguish adjacency from native reply proofs already on disk.
    let seq = if native { seq } else { !seq };
    let expires_at = time.saturating_add(now.saturating_sub(policy.cutoff));
    Ok(Some(ReplyParent {source_ref: archive_ref(profile,cid,&parent), excerpt, seq, expires_at}))
}

const ATTACHMENTS: &str = "FROM attachments a
    WHERE a.cid=?1 AND a.mid=?2 AND a.status='downloaded' AND a.extraction_status='extracted' AND length(a.text_content)>0
      AND EXISTS(SELECT 1 FROM attachment_profile WHERE id=1 AND selector=?3)
      AND (a.resource_type='fileId' OR (a.download_cid=?1 AND a.download_mid=?2))
      AND EXISTS(SELECT 1 FROM attachment_occurrences o WHERE o.attachment_id=a.id AND o.cid=a.cid AND o.mid=a.mid
          AND o.source_cid=a.cid AND o.source_mid=a.mid AND (o.path GLOB '/resources/[0-9]*' OR o.path GLOB '/resourceRefs/[0-9]*'
            OR o.path GLOB '/_verified_detail/resources/[0-9]*' OR o.path GLOB '/_verified_detail/resourceRefs/[0-9]*'))";

/// Check each input and its contextual excerpt fingerprint against the same authorized snapshot.
pub fn authorized_inputs(work: &Connection, root: &Path, profile: &str, now: i64) -> Result<Vec<String>, ArchiveError> {
    visible(work, root, profile, now, None)
}

pub fn authorized_subset(work: &Connection, root: &Path, profile: &str, now: i64, ids: &[String]) -> Result<Vec<String>, ArchiveError> {
    if ids.is_empty() { return Ok(Vec::new()); }
    if ids.len() > 32 { return Err(ArchiveError::Invalid); }
    let subset = serde_json::to_string(ids).map_err(|_| ArchiveError::Invalid)?;
    visible(work, root, profile, now, Some(subset))
}

fn visible(work: &Connection, root: &Path, profile: &str, now: i64, subset: Option<String>) -> Result<Vec<String>, ArchiveError> {
    let (archive, policy, _, adjacent) = snapshot(work, root, profile, now)?;
    let select = if subset.is_some() {
        "SELECT id,origin_ref,source_version FROM context_input WHERE id IN (SELECT value FROM json_each(?2)) AND profile=?1 AND origin_ref IS NOT NULL"
    } else {
        "SELECT id,origin_ref,source_version FROM context_input WHERE profile=?1 AND origin_ref IS NOT NULL"
    };
    let mut candidates = work.prepare(select)?;
    let mut rows = match subset {
        Some(ids) => candidates.query(params![profile,ids])?,
        None => candidates.query([profile])?,
    };
    let mut grant = archive.prepare("SELECT m.occurred_at,m.sender_id FROM messages m JOIN organizer_changes c USING(cid,mid)
        WHERE cid=?1 AND mid=?2 AND seq=?3 AND typeof(occurred_at)='integer'")?;
    let mut available = archive.prepare(&format!("SELECT EXISTS(SELECT 1 FROM ({READ_BODY}) WHERE length(body)>0) OR EXISTS(SELECT 1 {ATTACHMENTS})"))?;
    let mut dependencies = work.prepare("SELECT DISTINCT reply_ref,reply_version FROM organizer_facet
        WHERE profile=?1 AND input_id=?2 AND (reply_ref IS NOT NULL OR reply_version IS NOT NULL) LIMIT 2")?;
    let mut ids = Vec::new();
    while let Some(row) = rows.next()? {
        let reference: String = row.get(1)?;
        let Ok((owner,cid,mid)) = unpack::<(String,String,String)>("archive1-", &reference) else { continue };
        if owner != profile { continue; }
        let version: i64 = row.get(2)?;
        let meta = grant.query_row(params![cid,mid,version], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,Option<String>>(1)?))).optional()?;
        if meta.is_some_and(|(time,sender)| allowed(&policy,&cid,sender.as_deref(),time,now))
            && available.query_row(params![cid,mid,profile], |r| r.get::<_,bool>(0))? {
            let id: String = row.get(0)?;
            let mut deps = dependencies.query(params![profile,id])?;
            if let Some(dep) = deps.next()? {
                let expected = (dep.get::<_,Option<String>>(0)?,dep.get::<_,Option<i64>>(1)?);
                if deps.next()?.is_some() || !parent_evidence(&archive,&policy,profile,&cid,&mid,now,adjacent)?.is_some_and(|p|
                    expected.0.as_deref()==Some(p.source_ref.as_str()) && expected.1==Some(p.seq)) { continue; }
            }
            ids.push(id);
        }
    }
    Ok(ids)
}
