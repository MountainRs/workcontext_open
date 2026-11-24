//! Local archive evidence. The external dispatcher supplies the guarded workspace/profile.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path};
use workcontext_infra::{egress, sync::Scope};
use workcontext_store::rusqlite::{params, Connection, OpenFlags, OptionalExtension};

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("archive access denied")]
    Denied,
    #[error("archive missing or unavailable")]
    Unavailable,
    #[error("invalid archive query or reference")]
    Invalid,
}
impl ArchiveError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Denied => "archive_access_denied",
            Self::Unavailable => "archive_unavailable",
            Self::Invalid => "invalid_request",
        }
    }
}
impl From<workcontext_store::rusqlite::Error> for ArchiveError {
    fn from(_: workcontext_store::rusqlite::Error) -> Self { Self::Unavailable }
}

#[derive(Debug, Default)]
pub struct Search {
    pub keyword: Option<String>,
    pub conversation: Option<String>,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub cursor: Option<String>,
    pub limit: usize,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SourceGrant {
    profile: String,
    authorized: bool,
    scope: Scope,
    window_start_ms: Option<i64>,
    window_end_ms: Option<i64>,
}
#[derive(Serialize)]
struct Grant { local: SourceGrant, stored: Scope }
pub(crate) struct Policy { grants: Vec<Grant>, pub(crate) cutoff: i64 }

fn allows(scope: &Scope, cid: &str, sender: Option<&str>) -> bool {
    scope.conversations.contains(cid) || sender.is_some_and(|s| scope.people.contains(s))
}
impl Policy {
    fn load(work: &Connection, root: &Path, profile: &str, now: i64) -> Result<Self, ArchiveError> {
        let bytes = std::fs::read(root.join("config.json")).map_err(|_| ArchiveError::Denied)?;
        let cfg: Value = serde_json::from_slice(&bytes).map_err(|_| ArchiveError::Denied)?;
        if cfg.pointer("/externalAccess/readArchive").and_then(Value::as_bool) != Some(true) {
            return Err(ArchiveError::Denied);
        }
        Self::from_settings(work, &cfg, profile, now)
    }
    pub(crate) fn from_settings(work: &Connection, cfg: &Value, profile: &str, now: i64) -> Result<Self, ArchiveError> {
        let ledger: BTreeMap<String, SourceGrant> = serde_json::from_value(cfg.get("sources").cloned().ok_or(ArchiveError::Denied)?)
            .map_err(|_| ArchiveError::Denied)?;
        let days = match cfg.pointer("/retention/rawBodyDays") {
            None => 30,
            Some(v) => match v.as_u64().ok_or(ArchiveError::Denied)? { 0 => 30, n => n.min(90) },
        };
        let mut grants = Vec::new();
        let mut lookup = work.prepare("SELECT scope_json FROM source WHERE id=?1 AND kind='dws_im' AND enabled=1 AND authorized_at IS NOT NULL AND revoked_at IS NULL")?;
        for (id, local) in ledger {
            if !local.authorized || local.profile != profile || local.window_start_ms.is_none() { continue; }
            if let Some(raw) = lookup.query_row([id], |r| r.get::<_, String>(0)).optional()? {
                let stored = serde_json::from_str(&raw).map_err(|_| ArchiveError::Denied)?;
                grants.push(Grant { local, stored });
            }
        }
        if grants.is_empty() { return Err(ArchiveError::Denied); }
        Ok(Self { grants, cutoff: now.saturating_sub(days as i64 * 86_400_000) })
    }
    pub(crate) fn grant_key(&self, now: i64, epoch: &str) -> Result<String, ArchiveError> {
        use sha2::{Digest, Sha256};
        let bytes = serde_json::to_vec(&(epoch, now.saturating_sub(self.cutoff), &self.grants)).map_err(|_| ArchiveError::Invalid)?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }
    pub(crate) fn covers(&self, cid: &str, sender: Option<&str>, time: i64) -> bool {
        self.grants.iter().any(|g| g.local.window_start_ms.is_some_and(|s| s <= time)
            && g.local.window_end_ms.is_none_or(|e| time < e)
            && allows(&g.local.scope, cid, sender) && allows(&g.stored, cid, sender))
    }
}

fn token<T: Serialize>(prefix: &str, value: &T) -> String {
    use std::fmt::Write;
    let bytes = serde_json::to_vec(value).expect("primitive tuple");
    let mut out = String::with_capacity(prefix.len() + 2 * bytes.len());
    out.push_str(prefix);
    for byte in bytes { write!(out, "{byte:02x}").expect("String write"); }
    out
}
pub(crate) fn unpack<T: serde::de::DeserializeOwned>(prefix: &str, input: &str) -> Result<T, ArchiveError> {
    let s = input.strip_prefix(prefix).ok_or(ArchiveError::Invalid)?;
    if s.len() > 65_536 || s.len() % 2 != 0 || !s.is_ascii() { return Err(ArchiveError::Invalid); }
    let bytes: Result<Vec<_>, _> = (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i+2], 16)).collect();
    serde_json::from_slice(&bytes.map_err(|_| ArchiveError::Invalid)?).map_err(|_| ArchiveError::Invalid)
}
pub fn archive_ref(profile: &str, cid: &str, mid: &str) -> String {
    token("archive1-", &(profile, cid, mid))
}

pub(crate) const FIRST_VALID_MS: i64 = 31_536_000_000;
pub(crate) const LAST_VALID_MS: i64 = 253_402_300_799_999;

fn open_snapshot(root: &Path, profile: &str, now: i64) -> Result<(Connection, Option<i64>), ArchiveError> {
    crate::external_access::authorize_archive(root, profile).map_err(|_| ArchiveError::Denied)?;
    bound_snapshot(root, profile, now)
}

pub(crate) fn bound_snapshot(root: &Path, profile: &str, now: i64) -> Result<(Connection, Option<i64>), ArchiveError> {
    let archive = Connection::open_with_flags(root.join("archive/archive.sqlite3"), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    archive.execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF; BEGIN")?;
    let bound: Option<String> = archive.query_row("SELECT value FROM archive_state WHERE key='profile'", [], |r| r.get(0)).optional()?;
    if bound.as_deref() != Some(profile) { return Err(ArchiveError::Denied); }
    let stamp: Option<String> = archive.query_row("SELECT CASE WHEN typeof(value) IN ('text','integer') THEN CAST(value AS TEXT) END FROM archive_state WHERE key IN ('messages_ready_ms','last_success_ms') ORDER BY key='messages_ready_ms' DESC LIMIT 1", [], |r| r.get(0)).optional()?.flatten();
    let observed = stamp.and_then(|s| s.parse().ok()).filter(|t| (FIRST_VALID_MS..=now.min(LAST_VALID_MS)).contains(t));
    Ok((archive, observed))
}

pub fn watermark(root: &Path, profile: &str) -> Option<i64> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_millis().try_into().ok()?;
    open_snapshot(root, profile, now).ok()?.1
}

fn snapshot(work: &Connection, root: &Path, profile: &str, now: i64) -> Result<(Connection, Policy, Option<i64>), ArchiveError> {
    let (archive, watermark) = open_snapshot(root, profile, now)?;
    Ok((archive, Policy::load(work, root, profile, now)?, watermark))
}

// A combined-forward preview may reproduce another conversation's text. Only the
// outer sender's attributable body can be read under this message's scope.
pub(crate) const READ_BODY: &str = "SELECT m.body,m.sender_name FROM messages m JOIN message_capture c USING(cid,mid)
 WHERE m.cid=?1 AND m.mid=?2 AND NOT EXISTS(SELECT 1 FROM message_relations r WHERE r.cid=m.cid AND r.mid=m.mid AND r.kind='forward')";
pub(crate) fn masked_body(stmt: &mut workcontext_store::rusqlite::Statement<'_>, cid: &str, mid: &str) -> Result<Option<String>, ArchiveError> {
    let row: Option<(Option<String>, Option<String>)> = stmt.query_row([cid,mid], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
    Ok(row.and_then(|(body,name)| body.map(|text| mask_text(&text,name.as_deref()))))
}

pub(crate) fn mask_text(text: &str, sender_name: Option<&str>) -> String {
    let masked = egress::redact_text(&crate::snapshots::redact_text(text));
    egress::redact_names(&masked, &[sender_name.unwrap_or("")])
}

fn desktop_snapshot(work: &Connection, root: &Path, profile: &str, now: i64) -> Result<(Connection, Policy, Option<i64>), ArchiveError> {
    let bytes = std::fs::read(root.join("config.json")).map_err(|_| ArchiveError::Denied)?;
    let cfg = serde_json::from_slice(&bytes).map_err(|_| ArchiveError::Denied)?;
    let policy = Policy::from_settings(work,&cfg,profile,now)?;
    let (db,stamp) = bound_snapshot(root,profile,now)?;
    Ok((db,policy,stamp))
}

type Snapshot = fn(&Connection, &Path, &str, i64) -> Result<(Connection, Policy, Option<i64>), ArchiveError>;
type Cursor = (String, Option<String>, Option<String>, Option<i64>, Option<i64>, i64, String, String);
pub fn search(work: &Connection, root: &Path, profile: &str, query: &Search, now_ms: i64) -> Result<Value, ArchiveError> {
    search_with(work,root,profile,query,now_ms,snapshot)
}
pub fn desktop_search(work: &Connection, root: &Path, profile: &str, query: &Search, now_ms: i64) -> Result<Value, ArchiveError> {
    search_with(work,root,profile,query,now_ms,desktop_snapshot)
}
fn search_with(work: &Connection, root: &Path, profile: &str, query: &Search, now_ms: i64, load: Snapshot) -> Result<Value, ArchiveError> {
    if !(1..=100).contains(&query.limit) || query.start_ms.zip(query.end_ms).is_some_and(|(a,b)| a >= b)
        || [&query.keyword, &query.conversation].iter().any(|s| s.as_ref().is_some_and(|s| s.len() > 2048)) { return Err(ArchiveError::Invalid); }
    let cursor = query.cursor.as_deref().map(|s| unpack::<Cursor>("page1-", s)).transpose()?;
    if cursor.as_ref().is_some_and(|c| c.0 != profile || c.1 != query.keyword || c.2 != query.conversation || c.3 != query.start_ms || c.4 != query.end_ms) { return Err(ArchiveError::Invalid); }
    let (archive, policy, watermark) = load(work, root, profile, now_ms)?;
    let grants: Vec<_> = policy.grants.iter().map(|g| json!({
        "start":g.local.window_start_ms, "end":g.local.window_end_ms,
        "local":g.local.scope, "stored":g.stored
    })).collect();
    let mut stmt = archive.prepare("SELECT occurred_at,cid,mid FROM messages
        WHERE typeof(occurred_at)='integer' AND occurred_at>?1 AND occurred_at>=?2 AND occurred_at<?3 AND (?4 IS NULL OR cid=?4)
          AND (?5 IS NULL OR (occurred_at,cid,mid)<(?5,?6,?7))
          AND EXISTS (SELECT 1 FROM json_each(?8) g
              WHERE occurred_at >= json_extract(g.value,'$.start')
                AND (json_extract(g.value,'$.end') IS NULL OR occurred_at < json_extract(g.value,'$.end'))
                AND (cid IN (SELECT value FROM json_each(g.value,'$.local.conversations'))
                     OR sender_id IN (SELECT value FROM json_each(g.value,'$.local.people')))
                AND (cid IN (SELECT value FROM json_each(g.value,'$.stored.conversations'))
                     OR sender_id IN (SELECT value FROM json_each(g.value,'$.stored.people'))))
        ORDER BY occurred_at DESC,cid DESC,mid DESC LIMIT 513")?;
    let start = query.start_ms.unwrap_or(FIRST_VALID_MS).max(FIRST_VALID_MS);
    let end = query.end_ms.unwrap_or(i64::MAX).min(now_ms.min(LAST_VALID_MS).saturating_add(1));
    let mut rows = stmt.query(params![policy.cutoff,start,end,query.conversation,cursor.as_ref().map(|c|c.5),cursor.as_ref().map(|c|&c.6),cursor.as_ref().map(|c|&c.7),Value::Array(grants).to_string()])?;
    let mut body = archive.prepare(READ_BODY)?;
    let mut items = Vec::new();
    let mut scanned = 0;
    let mut last = None;
    let mut next = None;
    while let Some(row) = rows.next()? {
        if scanned == 512 || items.len() == query.limit { next = last; break; }
        scanned += 1;
        let t: i64 = row.get(0)?;
        let cid: String = row.get(1)?;
        let mid: String = row.get(2)?;
        let matches = match query.keyword.as_deref().filter(|s| !s.is_empty()) {
            None => true,
            Some(k) => masked_body(&mut body,&cid,&mid)?.is_some_and(|s| s.contains(k)),
        };
        if matches { items.push(json!({"sourceRef":archive_ref(profile,&cid,&mid),"occurredAt":t,"conversation":cid})); }
        last = Some((t,cid,mid));
    }
    let next = next.map(|(t,c,m)| token("page1-", &(profile,&query.keyword,&query.conversation,query.start_ms,query.end_ms,t,c,m)));
    Ok(json!({"items":items,"nextCursor":next,"archiveWatermarkMs":watermark}))
}

pub fn source_get(work: &Connection, root: &Path, profile: &str, reference: &str, now_ms: i64) -> Result<Value, ArchiveError> {
    source_with(work,root,profile,reference,now_ms,snapshot)
}
pub fn desktop_source(work: &Connection, root: &Path, profile: &str, reference: &str, now_ms: i64) -> Result<Value, ArchiveError> {
    source_with(work,root,profile,reference,now_ms,desktop_snapshot)
}
fn source_with(work: &Connection, root: &Path, profile: &str, reference: &str, now_ms: i64, load: Snapshot) -> Result<Value, ArchiveError> {
    let (owner,cid,mid): (String,String,String) = unpack("archive1-", reference)?;
    if owner != profile { return Err(ArchiveError::Denied); }
    let (archive, policy, watermark) = load(work, root, profile, now_ms)?;
    let row: Option<(i64, Option<String>)> = archive.query_row("SELECT occurred_at,sender_id FROM messages WHERE cid=?1 AND mid=?2 AND typeof(occurred_at)='integer' AND occurred_at BETWEEN ?3 AND ?4", params![cid,mid,FIRST_VALID_MS,now_ms.min(LAST_VALID_MS)], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
    let unavailable = |status| json!({"status":status,"archiveWatermarkMs":watermark});
    let Some((time,sender)) = row else { return Ok(unavailable("missing")); };
    if !policy.covers(&cid,sender.as_deref(),time) { return Ok(unavailable("missing")); }
    if time <= policy.cutoff { return Ok(unavailable("expired")); }
    let text = masked_body(&mut archive.prepare(READ_BODY)?, &cid,&mid)?;
    let mut stmt = archive.prepare("SELECT a.status,a.extraction_status FROM attachment_occurrences o
        LEFT JOIN attachments a ON a.id=o.attachment_id AND a.cid=o.cid AND a.mid=o.mid
          AND (a.resource_type='fileId' OR (a.download_cid=o.cid AND a.download_mid=o.mid))
        WHERE o.cid=?1 AND o.mid=?2 AND o.source_cid=o.cid AND o.source_mid=o.mid
        ORDER BY o.path,a.id LIMIT 101")?;
    let rows = stmt.query_map([&cid,&mid], |r| {
        let status: Option<String> = r.get(0)?;
        let extraction: Option<String> = r.get(1)?;
        Ok(json!({"download":status.as_deref().unwrap_or("missing"),"extraction":extraction.as_deref().unwrap_or("unavailable")}))
    })?;
    let mut attachments = rows.collect::<Result<Vec<_>,_>>()?;
    let more = attachments.len() > 100;
    attachments.truncate(100);
    Ok(json!({"sourceRef":reference,"status":if text.is_some(){"redacted"}else{"body_unavailable"},"body":text,"occurredAt":time,"attachments":attachments,"moreAttachments":more,"archiveWatermarkMs":watermark}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_archive_search_and_source_recheck() {
        use std::{fs, time::{SystemTime, UNIX_EPOCH}};
        let root = std::env::temp_dir().join(format!("wc-archive-{}-{}",std::process::id(),SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir_all(root.join("archive")).unwrap();
        let profile = "corp:user";
        let now: i64 = 1_750_000_000_000;
        let mut config = json!({"schema":1,"workspaceAccount":{"profile":profile,"sourceId":"im"},
            "externalAccess":{"profile":profile,"workspace":root.canonicalize().unwrap(),"readArchive":true},
            "retention":{"rawBodyDays":1}, "sources":{"im":{"authorized":true,"profile":profile,
            "scope":{"conversations":["chat","denied"]},"windowStartMs":0,"windowEndMs":null}}});
        let save = |cfg:&Value| fs::write(root.join("config.json"),cfg.to_string()).unwrap();
        save(&config);
        let work = Connection::open_in_memory().unwrap();
        work.execute_batch("CREATE TABLE source(id,kind,scope_json,enabled,authorized_at,revoked_at);
            INSERT INTO source VALUES('im','dws_im','{\"conversations\":[\"chat\"]}',1,1,NULL)").unwrap();
        let db = Connection::open(root.join("archive/archive.sqlite3")).unwrap();
        db.execute_batch("CREATE TABLE archive_state(key TEXT PRIMARY KEY,value TEXT);
            INSERT INTO archive_state VALUES('profile','corp:user'),('messages_ready_ms','1750000000000');
            CREATE TABLE messages(cid TEXT,mid TEXT,occurred_at INTEGER,sender_id TEXT,sender_name TEXT,body TEXT,PRIMARY KEY(cid,mid));
            INSERT INTO messages VALUES('chat','a',1749999999999,'u','Sender','repair 13800138000'),
                ('chat','b',1749999999999,'u','Sender','repair'),('denied','z',1749999999999,'u','Sender','repair'),
                ('chat','old',1749900000000,'u','Sender','repair'),('chat','future',1750000000001,'u','Sender','repair'),
                ('chat','epoch',0,'u','Sender','repair'),('chat','malformed','bad','u','Sender','repair'),
                ('chat','fraction',1749999999999.5,'u','Sender','repair');
            CREATE TABLE message_capture(cid,mid); INSERT INTO message_capture SELECT cid,mid FROM messages;
            CREATE TABLE message_relations(cid,mid,kind);
            CREATE TABLE attachments(id,cid,mid,resource_type,download_cid,download_mid,status,extraction_status);
            INSERT INTO attachments VALUES(1,'chat','a','fileId','','','downloaded','failed');
            CREATE TABLE attachment_occurrences(cid,mid,path,source_cid,source_mid,attachment_id);
            INSERT INTO attachment_occurrences VALUES('chat','a','/1','chat','a',1),('chat','a','/2','chat','a',NULL);").unwrap();
        let mut q = Search {keyword:Some("repair".into()), conversation:Some("chat".into()), start_ms:Some(2),end_ms:Some(now),limit:1,..Search::default()};
        let page = search(&work,&root,profile,&q,now).unwrap();
        assert_eq!(page["archiveWatermarkMs"],now);
        assert_eq!(page["items"][0]["sourceRef"],archive_ref(profile,"chat","b"));
        assert!(page["items"][0].get("body").is_none());
        q.cursor = Some(page["nextCursor"].as_str().unwrap().into());
        let last = search(&work,&root,profile,&q,now).unwrap();
        assert!(last["nextCursor"].is_null());
        let reference = last["items"][0]["sourceRef"].as_str().unwrap();
        let get = |r:&str| source_get(&work,&root,profile,r,now);
        let evidence = get(reference).unwrap();
        assert_eq!(evidence["status"],"redacted");
        assert!(!evidence["body"].as_str().unwrap().contains("13800138000"));
        assert_eq!(evidence["attachments"][0],json!({"download":"downloaded","extraction":"failed"}));
        assert_eq!(evidence["attachments"][1]["download"],"missing");
        for (cid,mid,status) in [("chat","old","expired"),("chat","absent","missing"),("denied","z","missing"),("chat","future","missing"),("chat","epoch","missing"),("chat","malformed","missing"),("chat","fraction","missing")] {
            assert_eq!(get(&archive_ref(profile,cid,mid)).unwrap()["status"],status);
        }
        for invalid in ["0", "86400000", "1750000000001", "bad", "1749999999999.5"] {
            db.execute("UPDATE archive_state SET value=?1 WHERE key='messages_ready_ms'",[invalid]).unwrap();
            assert!(get(reference).unwrap()["archiveWatermarkMs"].is_null());
        }
        let all = search(&work,&root,profile,&Search { limit:100,..Search::default() },now).unwrap();
        assert_eq!(all["items"].as_array().unwrap().len(),2);
        assert!(get(&archive_ref("other:user","chat","a")).is_err());
        q.cursor=None; q.keyword=Some("13800138000".into());
        assert_eq!(search(&work,&root,profile,&q,now).unwrap()["items"],json!([]));
        db.execute("INSERT INTO message_relations VALUES('chat','a','forward')",[]).unwrap();
        assert_eq!(get(reference).unwrap()["status"],"body_unavailable");
        work.execute("UPDATE source SET revoked_at=1",[]).unwrap();
        assert!(get(reference).is_err());
        work.execute("UPDATE source SET revoked_at=NULL",[]).unwrap();
        config["externalAccess"]["readArchive"]=json!(false); save(&config);
        assert!(get(reference).is_err());
        assert_eq!(desktop_source(&work,&root,profile,reference,now).unwrap()["status"],"body_unavailable");
        work.execute("UPDATE source SET revoked_at=1",[]).unwrap();
        assert!(desktop_source(&work,&root,profile,reference,now).is_err());
        work.execute("UPDATE source SET revoked_at=NULL",[]).unwrap();
        use crate::organizer_archive::{authorized_inputs, changes, latest};
        let page = |after, limit| changes(&work,&root,profile,after,limit,now);
        assert!(matches!(page(0,1), Err(ArchiveError::Denied)));
        config["contextOrganizer"] = json!({"profile":profile,"enabled":true,"workspace":root.canonicalize().unwrap(),
            "model":"synthetic","epoch":"synthetic-grant"});
        save(&config);
        db.execute_batch("ALTER TABLE attachments ADD COLUMN text_content TEXT;
            ALTER TABLE messages ADD COLUMN reply_to TEXT;
            ALTER TABLE message_relations ADD COLUMN source_cid TEXT;
            ALTER TABLE message_relations ADD COLUMN source_mid TEXT;
            ALTER TABLE message_relations ADD COLUMN path TEXT;
            UPDATE messages SET reply_to='b' WHERE mid='a';
            UPDATE messages SET occurred_at=1750000000000-86400000+5 WHERE mid='b';
            INSERT INTO message_relations VALUES('chat','a','quote','chat','b','quotedMessage');
            CREATE TABLE attachment_profile(id,selector); INSERT INTO attachment_profile VALUES(1,'corp:user');
            UPDATE attachments SET extraction_status='extracted',text_content='team 13800138000';
            UPDATE attachment_occurrences SET path='/resources/0' WHERE attachment_id=1;
            INSERT INTO attachments VALUES(2,'chat','a','fileId','','','downloaded','extracted','nested secret');
            INSERT INTO attachment_occurrences VALUES('chat','a','/forwarded/resources/0','other','b',2);").unwrap();
        work.execute_batch("CREATE TABLE context_input(id TEXT, origin_ref TEXT, source_version INTEGER,profile TEXT,source_occurred_at INTEGER);
            CREATE TABLE organizer_facet(profile,input_id,reply_ref,reply_version)").unwrap();
        let first = page(0,1).unwrap();
        assert_eq!(first.changes.len(),1);
        let evidence = latest(&work,&root,profile,reference,now).unwrap().unwrap();
        assert_eq!(evidence.seq,first.checkpoint);
        assert!(evidence.body.is_none());
        assert_eq!(evidence.attachments.len(),1);
        assert!(!evidence.attachments[0].contains("13800138000"));
        work.execute("INSERT INTO context_input VALUES('x',?1,?2,?3,?4)",params![reference,evidence.seq,profile,evidence.occurred_at]).unwrap();
        let parent = evidence.reply_parent.unwrap();
        assert_eq!(parent.source_ref,archive_ref(profile,"chat","b"));
        assert_eq!(parent.excerpt,"repair");
        work.execute("INSERT INTO organizer_facet VALUES(?1,'x',?2,?3)",params![profile,parent.source_ref,parent.seq]).unwrap();
        assert_eq!(authorized_inputs(&work,&root,profile,now).unwrap(),vec!["x".to_owned()]);
        assert_eq!(parent.expires_at,now+5);
        assert!(authorized_inputs(&work,&root,profile,parent.expires_at).unwrap().is_empty());
        assert!(latest(&work,&root,profile,reference,parent.expires_at).unwrap().unwrap().reply_parent.is_none());
        db.execute("UPDATE messages SET body='new parent evidence' WHERE mid='b'",[]).unwrap();
        assert!(authorized_inputs(&work,&root,profile,now).unwrap().is_empty());
        db.execute("UPDATE messages SET body='repair' WHERE mid='b'",[]).unwrap();
        let before = crate::organizer_archive::grant_key(&work,&root,profile,now).unwrap();
        assert_eq!(before,crate::organizer_archive::grant_key(&work,&root,profile,now+1).unwrap());
        let tail = page(first.checkpoint,100).unwrap();
        assert_eq!(tail.changes.len(),1);
        assert!(page(tail.checkpoint,100).unwrap().changes.is_empty());
        assert!(latest(&work,&root,profile,&archive_ref(profile,"chat","old"),now).unwrap().is_none());
        assert!(latest(&work,&root,profile,&archive_ref(profile,"denied","z"),now).unwrap().is_none());
        work.execute("UPDATE source SET scope_json='{\"conversations\":[\"chat\",\"denied\"]}'",[]).unwrap();
        assert_ne!(before,crate::organizer_archive::grant_key(&work,&root,profile,now).unwrap());
        assert!(matches!(crate::organizer_archive::changes_for_grant(&work,&root,profile,0,100,now,&before),Err(ArchiveError::Denied)));
        assert!(page(0,100).unwrap().changes.iter().any(|c| c.source_ref==archive_ref(profile,"denied","z")));
        // Exercise the migrated work DB with a real archive page. Simulate an interrupted
        // write after its first input, then replay the exact page and receipt.
        let mut durable = workcontext_store::open(&root.join("work.sqlite3")).unwrap();
        durable.execute("INSERT INTO source(id,kind,display_name,scope_json,authorized_at,created_at)
            VALUES('im','dws_im','synthetic source','{\"conversations\":[\"chat\"]}',1,1)",[]).unwrap();
        let replay = changes(&durable,&root,profile,0,100,now).unwrap();
        assert_eq!(replay.changes.len(),2);
        durable.execute_batch("CREATE TEMP TRIGGER interrupt_input BEFORE INSERT ON context_input
            WHEN EXISTS (SELECT 1 FROM context_input) BEGIN SELECT RAISE(ABORT,'synthetic interruption'); END;").unwrap();
        assert!(crate::organizer::ingest(&mut durable,profile,0,&replay).is_err());
        assert_eq!(crate::organizer::checkpoint(&durable,profile).unwrap(),0);
        assert_eq!(durable.query_row("SELECT count(*) FROM context_input",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        durable.execute_batch("DROP TRIGGER interrupt_input").unwrap();
        assert!(crate::organizer::ingest(&mut durable,profile,0,&replay).unwrap());
        assert!(!crate::organizer::ingest(&mut durable,profile,0,&replay).unwrap());
        assert_eq!(crate::organizer::checkpoint(&durable,profile).unwrap(),replay.checkpoint);
        assert_eq!(authorized_inputs(&durable,&root,profile,now).unwrap().len(),2);
        drop(durable);
        db.execute("UPDATE attachment_profile SET selector='other:user'",[]).unwrap();
        assert!(authorized_inputs(&work,&root,profile,now).unwrap().is_empty());
        work.execute("UPDATE source SET revoked_at=1",[]).unwrap();
        assert!(matches!(latest(&work,&root,profile,reference,now),Err(ArchiveError::Denied)));
        drop(db);
        fs::remove_dir_all(root).unwrap();
    }
}
