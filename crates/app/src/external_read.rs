//! Account-scoped local read snapshots used by the external adapter.
use crate::items::{ItemsError, CATALOG};
use serde::Deserialize;
use serde_json::{json, Value};
use workcontext_store::rusqlite::{self, params, Connection, OptionalExtension};

type Result<T, E = ItemsError> = std::result::Result<T, E>;

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadArgs {
    pub source: Option<String>,
    pub scope_id: Option<String>,
    pub cursor: Option<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub limit: Option<i64>,
}

fn invalid(message: &str) -> ItemsError { ItemsError::InvalidArgument { message: message.into() } }

// Each projection supplies the same ordering tuple, origin, and a bounded summary.
// The archive uses epoch-era placeholders for unknown times. SQLite also accepts text
// in INTEGER columns. Normalize before ordering, filtering, or computing freshness.
const RECORD_TIMES: &str = "records AS (SELECT r.*,
    CASE WHEN typeof(occurred_at)='integer' AND occurred_at BETWEEN 31536000000 AND 253402300799999 THEN occurred_at END AS occurred,
    CASE WHEN typeof(created_at)='integer' AND created_at BETWEEN 31536000000 AND 253402300799999 THEN created_at END AS recorded
    FROM projected_record r WHERE r.profile=?1)";
const INPUT_TIMES: &str = "inputs AS (SELECT i.*,
    CASE WHEN typeof(created_at)='integer' AND created_at BETWEEN 31536000000 AND 253402300799999 THEN created_at END AS recorded
    FROM context_input i WHERE i.profile=?1)";
const RECORDS: &str = "SELECT 'record' AS object,r.id,coalesce(r.occurred,r.recorded) AS at,
    r.origin,
    json_object('title',r.title,'kind',r.kind,'summary',substr(coalesce(r.corrected_content,r.reported_result,r.content),1,320),
      'factStatus',r.attribution,'producer',r.producer,'occurredAt',r.occurred,'recordedAt',r.recorded,
      'derived',json(r.derived_json),'version',r.version,
      'scopeBasis',r.scope_basis,'scopeIds',json((SELECT json_group_array(s.scope_id ORDER BY s.scope_id) FROM projected_scope s WHERE s.record_id=r.id)),
      'inference',substr(r.inference,1,320),'firstOpenQuestion',substr(json_extract(r.open_questions,'$[0]'),1,320),
      'timeBasis',CASE WHEN r.occurred IS NOT NULL THEN 'occurred' WHEN r.recorded IS NOT NULL THEN 'recorded' ELSE 'unknown' END,
      'sourceRef',r.source_ref,'inputId',r.input_id,'processingState',r.processing_state) AS payload
    FROM records r WHERE r.profile=?1 AND (instr(lower(r.content),lower(?2)) OR instr(lower(r.title),lower(?2))
      OR EXISTS(SELECT 1 FROM visible_facet f,json_tree(f.facets_json) j WHERE f.record_id=r.id AND j.type='text' AND instr(lower(j.value),lower(?2))))
    AND (?3 IS NULL OR EXISTS(SELECT 1 FROM projected_scope s WHERE (s.profile,s.record_id,s.scope_id)=(?1,r.id,?3)))";
const MATERIALS: &str = "SELECT 'material' AS object,r.id || ':' || m.position AS id,coalesce(r.occurred,r.recorded) AS at,
    r.origin,
    json_object('title',m.material,'summary',substr(m.summary,1,320),'recordId',r.id,'inputId',r.input_id,
      'availability','registered_unchecked','factStatus','reported','occurredAt',r.occurred,'sourceRef',r.source_ref,
      'timeBasis',CASE WHEN r.occurred IS NOT NULL THEN 'occurred' WHEN r.recorded IS NOT NULL THEN 'recorded' ELSE 'unknown' END) AS payload
    FROM record_ref m JOIN records r ON r.id=m.record_id
    WHERE r.profile=?1 AND (instr(lower(m.material),lower(?2)) OR instr(lower(m.summary),lower(?2)))
    AND (?3 IS NULL OR EXISTS(SELECT 1 FROM projected_scope s WHERE (s.profile,s.record_id,s.scope_id)=(?1,r.id,?3)))";
const SCOPES: &str = "SELECT t.kind AS object,t.id,t.updated AS at,'manual' AS origin,
    json_object('title',t.title,'version',t.revision,'factStatus','confirmed','timeBasis',CASE WHEN updated IS NULL THEN 'unknown' ELSE 'updated' END) AS payload
    FROM (SELECT *,CASE WHEN typeof(updated_at)='integer' AND updated_at BETWEEN 31536000000 AND 253402300799999 THEN updated_at END AS updated
        FROM work_thread WHERE profile=?1) t WHERE instr(lower(t.title),lower(?2)) AND (?3 IS NULL OR t.id=?3)";
const ITEMS: &str = "SELECT 'item' AS object,id,coalesce(evidenced,updated) AS at,'ownership' AS origin,
    json_object('title',title,'lifecycle',lifecycle,'attention',attention,'nextAction',next_action,'conclusion',note,
      'factStatus',confirmation,'historical',json(CASE WHEN historical THEN 'true' ELSE 'false' END),
      'timeBasis',CASE WHEN evidenced IS NOT NULL THEN 'evidence' WHEN updated IS NOT NULL THEN 'updated' ELSE 'unknown' END,
      'evidence',json(coalesce(json_extract(candidate_json,'$.evidence'),'[]'))) AS payload
    FROM (SELECT *,CASE WHEN typeof(last_evidence_at)='integer' AND last_evidence_at BETWEEN 31536000000 AND 253402300799999 THEN last_evidence_at END AS evidenced,
        CASE WHEN typeof(updated_at)='integer' AND updated_at BETWEEN 31536000000 AND 253402300799999 THEN updated_at END AS updated
        FROM catalog WHERE revision IS NOT NULL) WHERE (?3 IS NULL OR scope_id=?3)
    AND (instr(lower(title),lower(?2)) OR instr(lower(next_action),lower(?2)) OR instr(lower(note),lower(?2))
        OR (?10 IS NOT NULL AND formal_rowid IN (SELECT rowid FROM work_item_fts WHERE work_item_fts MATCH ?10)))";

fn validate(profile: &str, args: &ReadArgs) -> Result<()> {
    if profile.trim().is_empty() || profile == "@local" { return Err(invalid("需要已绑定的账号。")); }
    if args.since.zip(args.until).is_some_and(|(a,b)| a>b) { return Err(invalid("时间范围无效。")); }
    if !(1..=100).contains(&args.limit.unwrap_or(20)) { return Err(invalid("每页应为 1 至 100 项。")); }
    Ok(())
}

fn parse_json(text: String) -> rusqlite::Result<Value> {
    serde_json::from_str(&text).map_err(|e| rusqlite::Error::FromSqlConversionFailure(0,rusqlite::types::Type::Text,Box::new(e)))
}

fn page(conn: &Connection, profile: &str, allowed: &str, args: &ReadArgs, query: &str, projections: &[&str]) -> Result<Value> {
    let cursor: Option<(i64,String,String)> = args.cursor.as_ref().map(|s| {
        if s.len()>4096 { return Err(invalid("分页游标无效。")); }
        serde_json::from_str(s).map_err(|_| invalid("分页游标无效。"))
    }).transpose()?;
    let (after,object,id) = cursor.as_ref().map(|(t,o,i)|(Some(*t),o.as_str(),i.as_str())).unwrap_or((None,"",""));
    let union = projections.join(" UNION ALL ");
    let prefix = if projections.iter().any(|p| p.starts_with(ITEMS)) { format!("{CATALOG},") } else { "WITH".into() };
    let projection = crate::context::PROJECTION.replace("$allowed", "?12");
    let sql = format!("{prefix} {projection}, {RECORD_TIMES}, hits AS ({union}) SELECT object,id,at,origin,payload FROM hits
        WHERE (?4 IS NULL OR origin=?4) AND (?5 IS NULL OR at>=?5) AND (?6 IS NULL OR at<=?6)
        AND (?7 IS NULL OR coalesce(at,-9223372036854775808)<?7 OR (coalesce(at,-9223372036854775808)=?7 AND (object,id)>(?8,?9)))
        ORDER BY coalesce(at,-9223372036854775808) DESC,object,id LIMIT ?11");
    let phrase = (query.chars().nth(2).is_some()).then(||format!("\"{}\"",query.replace('"',"\"\"")));
    let limit = args.limit.unwrap_or(20);
    let mut stmt = conn.prepare_cached(&sql)?;
    let mut results = stmt.query(params![profile,query,args.scope_id,args.source,args.since,args.until,after,object,id,phrase,limit+1,allowed])?;
    let mut items = Vec::new();
    let mut last = None;
    let mut next = None;
    while let Some(row) = results.next()? {
        if items.len()==limit as usize { next = last; break; }
        let object: String = row.get(0)?;
        let id: String = row.get(1)?;
        let at: Option<i64> = row.get(2)?;
        let mut value = parse_json(row.get(4)?)?;
        if let Some(evidence) = value.as_object_mut().and_then(|v| v.remove("evidence")) {
            let refs: Vec<_> = evidence.as_array().into_iter().flatten().take(128).filter_map(|e| {
                Some(crate::external_archive::archive_ref(profile,e["cid"].as_str()?,e["mid"].as_str()?))
            }).collect();
            value["sourceRefs"] = json!(refs);
        }
        last = Some(json!([at.unwrap_or(i64::MIN),object,id]).to_string());
        value["objectType"] = json!(object);
        value["id"] = json!(id);
        value["time"] = json!(at);
        value["origin"] = json!(row.get::<_,String>(3)?);
        items.push(value);
    }
    Ok(json!({"items":items,"nextCursor":next}))
}

fn snapshot(conn: &Connection, profile: &str, args: &ReadArgs, f: impl FnOnce(&Connection,&str)->Result<Value>) -> Result<Value> {
    validate(profile,args)?;
    read_snapshot(conn,profile,f)
}

fn read_snapshot(conn: &Connection, profile: &str, f: impl FnOnce(&Connection,&str)->Result<Value>) -> Result<Value> {
    let tx = conn.unchecked_transaction()?;
    let allowed = crate::context::allowed_inputs(&tx,profile);
    let mut result = f(&tx,&allowed)?;
    let watermark = tx.query_row(&format!("WITH {INPUT_TIMES} SELECT max(recorded),max(CASE WHEN processing_state IN ('organized','no_record') THEN recorded END),
        count(CASE WHEN processing_state='pending' THEN 1 END) FROM inputs WHERE origin_ref IS NULL OR id IN (SELECT value FROM json_each(?2))"),params![profile,allowed],|r| {
        Ok(json!({"latestInputAt":r.get::<_,Option<i64>>(0)?,"latestOrganizedInputAt":r.get::<_,Option<i64>>(1)?,
            "pendingInputs":r.get::<_,i64>(2)?,"archiveAt":null}))
    })?;
    result["watermarks"] = watermark;
    tx.commit()?;
    Ok(result)
}

pub fn overview(conn: &Connection, profile: &str, args: &ReadArgs) -> Result<Value> {
    validate(profile,args)?;
    overview_snapshot(conn,profile,args,false)
}

pub fn desktop_overview(conn: &Connection, profile: &str) -> Result<Value> {
    overview_snapshot(conn,profile,&ReadArgs {limit:Some(6),..Default::default()},true)
}

fn overview_snapshot(conn: &Connection, profile: &str, args: &ReadArgs, desktop: bool) -> Result<Value> {
    read_snapshot(conn,profile,|db,allowed| {
        let mut recent = page(db,profile,allowed,args,"",&[RECORDS])?;
        let attention = format!("{ITEMS} AND NOT historical AND lifecycle NOT IN ('closed','ignored') AND (attention!='no_next' OR awaiting_confirmation)");
        let projects = if desktop { format!("{SCOPES} AND t.kind='project'") } else { SCOPES.to_owned() };
        recent["projects"] = page(db,profile,allowed,args,"",&[&projects])?;
        if desktop {
            for project in recent["projects"]["items"].as_array_mut().into_iter().flatten() {
                let id = project["id"].as_str().ok_or(ItemsError::NotFound)?;
                let scope = crate::context::require_scope(db,profile,id)?;
                let scoped = ReadArgs {scope_id:Some(id.into()),limit:Some(1),..Default::default()};
                project["goal"] = json!(scope.goal);
                project["activity"] = page(db,profile,allowed,&scoped,"",&[RECORDS])?;
            }
        }
        recent["attentionItems"] = if profile == "@local" { json!({"items":[],"nextCursor":null}) } else { page(db,profile,allowed,args,"",&[&attention])? };
        Ok(recent)
    })
}
pub fn recent(conn: &Connection, profile: &str, args: &ReadArgs) -> Result<Value> {
    snapshot(conn,profile,args,|db,allowed|page(db,profile,allowed,args,"",&[RECORDS]))
}
pub fn projects(conn: &Connection, profile: &str, args: &ReadArgs) -> Result<Value> {
    snapshot(conn,profile,args,|db,allowed|page(db,profile,allowed,args,"",&[SCOPES]))
}
pub fn search(conn: &Connection, profile: &str, query: &str, args: &ReadArgs) -> Result<Value> {
    let query = query.trim();
    if query.is_empty() || query.len()>4096 || query.contains('\0') { return Err(invalid("检索词为空或无效。")); }
    snapshot(conn,profile,args,|db,allowed|page(db,profile,allowed,args,query,&[RECORDS,SCOPES,ITEMS,MATERIALS]))
}
pub fn desktop_search(conn: &Connection, profile: &str, query: &str, cursor: Option<String>) -> Result<Value> {
    let args = ReadArgs {cursor,..Default::default()};
    if profile != "@local" { return search(conn,profile,query,&args); }
    let query = query.trim();
    if query.is_empty() || query.len()>4096 || query.contains('\0') { return Err(invalid("检索词为空或无效。")); }
    read_snapshot(conn,profile,|db,allowed|page(db,profile,allowed,&args,query,&[RECORDS,SCOPES,MATERIALS]))
}

pub fn project(conn: &Connection, profile: &str, id: &str, args: &ReadArgs) -> Result<Value> {
    snapshot(conn,profile,args,|db,allowed| {
        let mut scope = db.query_row("SELECT kind,title,goal,background,revision FROM work_thread WHERE profile=?1 AND id=?2",
            params![profile,id],|r| Ok(json!({"id":id,"kind":r.get::<_,String>(0)?,"title":r.get::<_,String>(1)?,
                "goal":r.get::<_,String>(2)?,"background":r.get::<_,String>(3)?,"version":r.get::<_,i64>(4)?,"factStatus":"confirmed"})))
            .optional()?.ok_or(ItemsError::NotFound)?;
        let scoped = ReadArgs {scope_id:Some(id.into()),..args.clone()};
        scope["activity"] = page(db,profile,allowed,&scoped,"",&[RECORDS,ITEMS,MATERIALS])?;
        Ok(scope)
    })
}
pub fn record(conn: &Connection, profile: &str, id: &str, args: &ReadArgs) -> Result<Value> {
    snapshot(conn,profile,args,|db,allowed| {
        let projection = crate::context::PROJECTION.replace("$allowed", "?3");
        let mut record = db.query_row(&format!("WITH {projection},{RECORD_TIMES} SELECT r.title,r.kind,r.occurred,r.recorded,r.producer,r.input_id,r.processing_state,
            coalesce(r.reported_result,CASE WHEN r.organizer_created=0 THEN base.content END),r.inference,r.open_questions,r.scope_hints,
            r.content,r.attribution,r.version,r.derived_json,r.origin,r.source_ref,r.scope_basis
            FROM records r JOIN context_record base ON base.id=r.id WHERE r.id=?2"),params![profile,id,allowed],|r| {
            let occurred: Option<i64> = r.get(2)?;
            let recorded: Option<i64> = r.get(3)?;
            Ok(json!({"id":id,"objectType":"record","title":r.get::<_,String>(0)?,"kind":r.get::<_,String>(1)?,
                "occurredAt":occurred,"recordedAt":recorded,"timeBasis":if occurred.is_some(){"occurred"}else if recorded.is_some(){"recorded"}else{"unknown"},
                "sourceRef":r.get::<_,String>(16)?,"content":r.get::<_,String>(11)?,"version":r.get::<_,i64>(13)?,"derived":parse_json(r.get(14)?)?,
                "scopeBasis":r.get::<_,String>(17)?,
                "origin":r.get::<_,String>(15)?,"producer":r.get::<_,Option<String>>(4)?,"inputId":r.get::<_,String>(5)?,
                "processingState":r.get::<_,String>(6)?,"factStatus":r.get::<_,String>(12)?,"reportedResult":r.get::<_,Option<String>>(7)?,
                "inference":r.get::<_,Option<String>>(8)?,"openQuestions":parse_json(r.get(9)?)?,"scopeHints":parse_json(r.get(10)?)?}))
        }).optional()?.ok_or(ItemsError::NotFound)?;
        let mut materials = db.prepare_cached("SELECT material,summary,position FROM record_ref WHERE record_id=?1 ORDER BY position LIMIT 128")?;
        record["materials"] = json!(materials.query_map([id],|r|Ok(json!({"path":r.get::<_,String>(0)?,"summary":r.get::<_,Option<String>>(1)?,
            "id":format!("{}:{}",id,r.get::<_,i64>(2)?),"availability":"registered_unchecked"})))?.collect::<rusqlite::Result<Vec<_>>>()?);
        let mut scopes = db.prepare_cached(&format!("WITH {projection} SELECT scope_id FROM projected_scope WHERE profile=?1 AND record_id=?2 ORDER BY scope_id LIMIT 128"))?;
        record["scopeIds"] = json!(scopes.query_map(params![profile,id,allowed],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?);
        let mut related = db.prepare_cached(&format!("WITH {projection} SELECT rel.related_record_id FROM record_related rel JOIN projected_record r ON r.id=rel.related_record_id WHERE rel.profile=?1 AND rel.record_id=?2 ORDER BY rel.related_record_id LIMIT 128"))?;
        record["relatedRecordIds"] = json!(related.query_map(params![profile,id,allowed],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?);
        Ok(record)
    })
}

pub fn input(conn: &Connection, profile: &str, id: &str, args: &ReadArgs) -> Result<Value> {
    snapshot(conn,profile,args,|db,_| {
        db.query_row(&format!("WITH {INPUT_TIMES} SELECT original,producer,recorded,processing_state FROM inputs WHERE id=?2 AND origin_ref IS NULL"),
            params![profile,id],|r| {
                let producer: Option<String> = r.get(1)?;
                Ok(json!({"sourceRef":id,"status":"available","body":r.get::<_,String>(0)?,"origin":if producer.is_some(){"agent"}else{"manual"},
                    "producer":producer,"recordedAt":r.get::<_,Option<i64>>(2)?,"processingState":r.get::<_,String>(3)?,"factStatus":"reported"}))
            }).optional()?.ok_or(ItemsError::NotFound)
    })
}
