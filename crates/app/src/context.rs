//! Manually recorded work and confirmed project/domain scopes in the local work database.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use workcontext_store::{now_ms, rusqlite::{self, params, Connection, OptionalExtension, Row, TransactionBehavior}};

use crate::items::ItemsError;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SaveScopeArgs {
    pub id: Option<String>,
    pub version: Option<i64>,
    pub kind: String,
    pub title: String,
    pub goal: String,
    pub background: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecordsArgs {
    pub query: Option<String>,
    pub scope_id: Option<String>,
    pub offset: Option<i64>,
    pub origin: Option<String>,
    pub from_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub unclassified: Option<bool>,
    pub section: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AddRecordArgs {
    #[serde(default)]
    pub title: String,
    pub content: String,
    #[serde(default)]
    pub scope_ids: Vec<String>,
    #[serde(default)]
    pub materials: Vec<String>,
    #[serde(default = "default_kind")]
    pub kind: String,
    pub occurred_at: Option<i64>,
}

fn default_kind() -> String { "event".into() }

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CorrectRecordArgs {
    pub id: String,
    pub version: i64,
    pub content: Option<String>,
    pub attribution: Option<String>,
    pub scope_ids: Option<Vec<String>>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Scope {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub goal: String,
    pub background: String,
    pub version: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordSummary {
    pub id: String,
    pub title: String,
    pub content: String,
    pub kind: String,
    pub occurred_at: Option<i64>,
    pub created_at: i64,
    pub scopes: Vec<Scope>,
    pub source_ref: String,
    pub scope_basis: String,
    pub origin: String,
    pub producer: Option<String>,
    pub attribution: String,
    pub version: i64,
    pub derived: Value,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub highlights: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordDetail {
    pub record: RecordSummary,
    pub original: String,
    pub materials: Vec<String>,
    pub correction: Value,
    pub reported_result: Option<String>,
    pub inference: Option<String>,
    pub open_questions: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_source: Option<Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordPage {
    pub items: Vec<RecordSummary>,
    pub total: i64,
}

fn invalid(message: &str) -> ItemsError {
    ItemsError::InvalidArgument { message: message.into() }
}

pub(crate) fn validate_text(value: &str, limit: usize, required: bool) -> Result<(), ItemsError> {
    if (required && value.trim().is_empty()) || value.contains('\0') || value.chars().count() > limit {
        return Err(invalid("文本为空、过长或含有无效字符。"));
    }
    Ok(())
}

fn scope_row(row: &Row<'_>) -> rusqlite::Result<Scope> {
    Ok(Scope { id: row.get(0)?, kind: row.get(1)?, title: row.get(2)?, goal: row.get(3)?,
        background: row.get(4)?, version: row.get(5)? })
}

pub fn require_scope(conn: &Connection, profile: &str, id: &str) -> Result<Scope, ItemsError> {
    validate_text(id, 256, true)?;
    conn.query_row("SELECT id,kind,title,goal,background,revision FROM work_thread WHERE profile=?1 AND id=?2",
        params![profile, id], scope_row).optional()?.ok_or(ItemsError::NotFound)
}

pub fn scopes(conn: &Connection, profile: &str) -> Result<Vec<Scope>, ItemsError> {
    validate_text(profile, 1024, true)?;
    let mut statement = conn.prepare("SELECT id,kind,title,goal,background,revision FROM work_thread
        WHERE profile=?1 ORDER BY title,id")?;
    let rows = statement.query_map([profile], scope_row)?.collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

pub fn save_scope(conn: &mut Connection, profile: &str, args: &SaveScopeArgs) -> Result<Scope, ItemsError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let scope = save_scope_in(&tx, profile, args)?;
    tx.commit()?;
    Ok(scope)
}

pub(crate) fn save_scope_in(tx: &rusqlite::Transaction<'_>, profile: &str, args: &SaveScopeArgs) -> Result<Scope, ItemsError> {
    validate_text(profile, 1024, true)?;
    validate_text(&args.title, 512, true)?;
    validate_text(&args.goal, 65536, false)?;
    validate_text(&args.background, 65536, false)?;
    if !matches!(args.kind.as_str(), "project" | "domain") {
        return Err(invalid("范围类型无效。"));
    }
    let now = now_ms();
    let id = match (&args.id, args.version) {
        (Some(id), Some(version)) if version > 0 => {
            let current = require_scope(tx, profile, id)?;
            if current.version != version {
                return Err(ItemsError::ValidationRejected { message: "范围已被修改，请刷新后重试。".into(), hint: None });
            }
            if current.kind != args.kind {
                return Err(invalid("建档后不能更改范围类型。"));
            }
            tx.execute("UPDATE work_thread SET title=?3,goal=?4,background=?5,updated_at=?6,
                revision=revision+1 WHERE profile=?1 AND id=?2",
                params![profile, id, args.title.trim(), args.goal, args.background, now])?;
            id.clone()
        }
        (None, None) => tx.query_row("INSERT INTO work_thread(id,profile,title,kind,goal,background,created_at,updated_at)
            VALUES ('scope-' || lower(hex(randomblob(16))),?1,?2,?3,?4,?5,?6,?6) RETURNING id",
            params![profile, args.title.trim(), args.kind, args.goal, args.background, now], |row| row.get(0))?,
        _ => return Err(invalid("修改范围时需要有效的范围 ID 和版本。")),
    };
    require_scope(tx, profile, &id)
}

pub(crate) const PROJECTION: &str = include_str!("record_projection.sql");

pub(crate) fn allowed_inputs(conn: &Connection, profile: &str) -> String {
    let allowed = conn.path().and_then(|p| Path::new(p).parent())
        .and_then(|root| crate::organizer_archive::authorized_inputs(conn,root,profile,now_ms()).ok())
        .unwrap_or_default();
    serde_json::json!(allowed).to_string()
}

fn json_col(row: &Row<'_>, col: usize) -> rusqlite::Result<Value> {
    let text: String = row.get(col)?;
    serde_json::from_str(&text).map_err(|e| rusqlite::Error::FromSqlConversionFailure(col,rusqlite::types::Type::Text,Box::new(e)))
}

const SUMMARY: &str = "r.id,r.title,r.content,r.kind,r.occurred_at,r.created_at,r.source_ref,r.origin,r.attribution,r.version,r.derived_json,
    (SELECT json_group_array(json_object('id',t.id,'kind',t.kind,'title',t.title,'goal',t.goal,'background',t.background,'version',t.revision) ORDER BY t.title,t.id)
     FROM projected_scope s JOIN work_thread t ON t.profile=s.profile AND t.id=s.scope_id WHERE s.record_id=r.id),r.scope_basis,r.producer";

fn summary_row(row: &Row<'_>) -> rusqlite::Result<RecordSummary> {
    let scopes = serde_json::from_str(&row.get::<_,String>(11)?).map_err(|e| rusqlite::Error::FromSqlConversionFailure(11,rusqlite::types::Type::Text,Box::new(e)))?;
    Ok(RecordSummary { id: row.get(0)?, title: row.get(1)?, content: row.get(2)?, kind: row.get(3)?,
        occurred_at: row.get(4)?, created_at: row.get(5)?, source_ref: row.get(6)?, origin: row.get(7)?,
        attribution:row.get(8)?,version:row.get(9)?,derived:json_col(row,10)?,scopes,scope_basis:row.get(12)?,producer:row.get(13)?, highlights:Vec::new() })
}

pub fn records(conn: &Connection, profile: &str, args: &RecordsArgs) -> Result<RecordPage, ItemsError> {
    validate_text(profile, 1024, true)?;
    let query = args.query.as_deref().unwrap_or("").trim();
    validate_text(query, 1024, false)?;
    let offset = args.offset.unwrap_or(0);
    if offset < 0 { return Err(invalid("分页偏移不能为负数。")); }
    let tx = conn.unchecked_transaction()?;
    if let Some(id) = &args.scope_id { require_scope(&tx, profile, id)?; }
    let projection = PROJECTION.replace("$allowed", "?4");
    let allowed = allowed_inputs(&tx,profile);
    const FILTER: &str = " FROM projected_record r WHERE r.profile=?1 AND
        (?2 IS NULL OR EXISTS (SELECT 1 FROM projected_scope s WHERE s.record_id=r.id AND s.scope_id=?2))
        AND (?5 IS NULL OR r.origin=?5) AND (?6 IS NULL OR coalesce(r.occurred_at,r.created_at)>=?6)
        AND (?7 IS NULL OR coalesce(r.occurred_at,r.created_at)<?7)
        AND (NOT ?8 OR NOT EXISTS (SELECT 1 FROM projected_scope s WHERE s.record_id=r.id))
        AND (?9 IS NULL OR r.kind=?9 OR (?9='questions' AND (json_array_length(r.open_questions)>0 OR EXISTS
            (SELECT 1 FROM visible_facet f WHERE f.record_id=r.id AND json_array_length(f.facets_json,'$.openQuestions')>0)))
            OR (?9='materials' AND EXISTS (SELECT 1 FROM record_ref m WHERE m.record_id=r.id)))
        AND (?3='' OR instr(lower(r.title),lower(?3)) OR instr(lower(r.content),lower(?3))
            OR EXISTS (SELECT 1 FROM visible_facet f,json_tree(f.facets_json) j WHERE f.record_id=r.id AND j.type='text' AND instr(lower(j.value),lower(?3)))
            OR EXISTS (SELECT 1 FROM record_ref m WHERE m.record_id=r.id AND
                (instr(lower(m.material),lower(?3))>0 OR instr(lower(m.summary),lower(?3))>0)))";
    let unclassified = args.unclassified.unwrap_or(false);
    let filters = params![profile,args.scope_id,query,allowed,args.origin,args.from_ms,args.until_ms,unclassified,args.section];
    let total = tx.query_row(&format!("WITH {projection} SELECT count(*){FILTER}"), filters, |row| row.get(0))?;
    let items = tx.prepare_cached(&format!("WITH {projection} SELECT {SUMMARY},CASE ?9
        WHEN 'questions' THEN r.open_questions WHEN 'materials' THEN (SELECT json_group_array(material ORDER BY position) FROM record_ref WHERE record_id=r.id) ELSE '[]' END{FILTER}
        ORDER BY COALESCE(r.occurred_at,r.created_at) DESC,r.id LIMIT 50 OFFSET ?10"))?
        .query_map(params![profile,args.scope_id,query,allowed,args.origin,args.from_ms,args.until_ms,unclassified,args.section,offset], |row| {
            let mut summary = summary_row(row)?;
            summary.highlights = serde_json::from_value(json_col(row,14)?).map_err(|e| rusqlite::Error::FromSqlConversionFailure(14,rusqlite::types::Type::Text,Box::new(e)))?;
            Ok(summary)
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
    tx.commit()?;
    Ok(RecordPage { items, total })
}

fn detail(conn: &Connection, profile: &str, id: &str, allowed: &str) -> Result<RecordDetail, ItemsError> {
    let projection = PROJECTION.replace("$allowed", "?3");
    let (record,original,correction,reported_result,inference,open_questions) = conn.query_row(&format!("WITH {projection} SELECT {SUMMARY},
        CASE WHEN i.origin_ref IS NULL THEN i.original ELSE '' END,r.corrected_content,r.corrected_attribution,r.corrected_scopes,
        r.reported_result,r.inference,r.open_questions FROM projected_record r JOIN visible_input i ON i.id=r.input_id
        WHERE r.profile=?1 AND r.id=?2"), params![profile,id,allowed], |row| {
            let scope_ids = row.get::<_,Option<String>>(17)?.map(|s| serde_json::from_str::<Value>(&s))
                .transpose().map_err(|e| rusqlite::Error::FromSqlConversionFailure(17,rusqlite::types::Type::Text,Box::new(e)))?;
            let correction = serde_json::json!({"content":row.get::<_,Option<String>>(15)?,"attribution":row.get::<_,Option<String>>(16)?,"scopeIds":scope_ids});
            Ok((summary_row(row)?,row.get(14)?,correction,row.get(18)?,row.get(19)?,json_col(row,20)?))
        }).optional()?.ok_or(ItemsError::NotFound)?;
    let materials = conn.prepare_cached("SELECT material FROM record_ref WHERE record_id=?1 ORDER BY position")?
        .query_map([id], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(RecordDetail { record, original, materials, correction, reported_result, inference, open_questions, archive_source: None })
}

pub fn get_record(conn: &Connection, profile: &str, id: &str) -> Result<RecordDetail, ItemsError> {
    validate_text(profile, 1024, true)?;
    validate_text(id, 256, true)?;
    let tx = conn.unchecked_transaction()?;
    let result = detail(&tx, profile, id, &allowed_inputs(&tx,profile))?;
    tx.commit()?;
    Ok(result)
}

pub fn get_record_with_source(conn: &Connection, profile: &str, id: &str) -> Result<RecordDetail, ItemsError> {
    let tx = conn.unchecked_transaction()?;
    let mut result = detail(&tx, profile, id, &allowed_inputs(&tx, profile))?;
    if result.record.origin == "archive" {
        let root = conn.path().and_then(|p| Path::new(p).parent()).ok_or(ItemsError::NotFound)?;
        let source = crate::organizer_archive::latest(&tx, root, profile, &result.record.source_ref, now_ms())
            .map_err(|_| ItemsError::NotFound)?.ok_or(ItemsError::NotFound)?;
        let version: i64 = tx.query_row("SELECT source_version FROM context_input WHERE profile=?1 AND origin_ref=?2", params![profile,result.record.source_ref], |r| r.get(0))?;
        if version != source.seq { return Err(ItemsError::NotFound); }
        result.archive_source = Some(serde_json::json!({"occurredAt":source.occurred_at,"body":source.body,"attachments":source.attachments}));
    }
    tx.commit()?;
    Ok(result)
}

pub fn add_record(conn: &mut Connection, profile: &str, args: &AddRecordArgs) -> Result<RecordDetail, ItemsError> {
    validate_text(profile, 1024, true)?;
    validate_text(&args.title, 512, false)?;
    validate_text(&args.content, 262144, true)?;
    if !matches!(args.kind.as_str(), "event" | "progress" | "decision" | "experience") {
        return Err(invalid("记录类型无效。"));
    }
    if args.scope_ids.len() > 128 || args.materials.len() > 128 { return Err(invalid("单条记录关联的范围或资料过多。")); }
    for material in &args.materials { validate_text(material, 8192, true)?; }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now = now_ms();
    let input: String = tx.query_row("INSERT INTO context_input(profile,original,created_at,processing_state) VALUES (?1,?2,?3,'pending') RETURNING id",
        params![profile,args.content,now], |row| row.get(0))?;
    let title = if args.title.trim().is_empty() { args.content.trim().lines().next().unwrap_or_default().chars().take(80).collect() } else { args.title.trim().to_owned() };
    let id: String = tx.query_row("INSERT INTO context_record(profile,input_id,title,content,kind,occurred_at,created_at)
        VALUES (?1,?2,?3,?4,?5,?6,?7) RETURNING id", params![profile,input,title,args.content,args.kind,args.occurred_at,now], |row| row.get(0))?;
    for scope in &args.scope_ids {
        require_scope(&tx, profile, scope)?;
        tx.execute("INSERT INTO record_scope(profile,record_id,scope_id) VALUES (?1,?2,?3)
            ON CONFLICT(profile,record_id,scope_id) DO NOTHING", params![profile,id,scope])?;
    }
    for (position, material) in args.materials.iter().enumerate() {
        tx.execute("INSERT INTO record_ref(record_id,position,material) VALUES (?1,?2,?3)
            ON CONFLICT(record_id,material) DO NOTHING", params![id,position as i64,material])?;
    }
    let result = detail(&tx, profile, &id, "[]")?;
    tx.commit()?;
    Ok(result)
}

pub fn correct_record(conn: &mut Connection, profile: &str, args: &CorrectRecordArgs) -> Result<RecordDetail, ItemsError> {
    validate_text(profile,1024,true)?;
    validate_text(&args.id,256,true)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let allowed = allowed_inputs(&tx,profile);
    let projection = PROJECTION.replace("$allowed", "?3");
    let version = tx.query_row(&format!("WITH {projection} SELECT version FROM projected_record WHERE id=?2"),
        params![profile,args.id,allowed],|r|r.get::<_,i64>(0)).optional()?.ok_or(ItemsError::NotFound)?;
    if version != args.version {
        return Err(ItemsError::ValidationRejected { message: "记录已被修改，请刷新后重试。".into(), hint: None });
    }
    if let Some(content) = &args.content { validate_text(content,262144,true)?; }
    if let Some(attr) = &args.attribution {
        if !matches!(attr.as_str(),"reported"|"inference"|"confirmed") { return Err(invalid("归因类型无效。")); }
        if args.content.is_none() { return Err(invalid("请同时保存归因对应的正文。")); }
    }
    let mut ids = args.scope_ids.clone();
    if let Some(ids) = &mut ids {
        if ids.len()>128 { return Err(invalid("单条记录关联的范围过多。")); }
        ids.sort(); ids.dedup();
        for scope in ids { require_scope(&tx,profile,scope)?; }
    }
    let projection = PROJECTION.replace("$allowed", "?8");
    tx.execute(&format!("WITH {projection} INSERT INTO record_correction(profile,record_id,version,content,attribution,scope_ids,corrected_at,evidence_versions)
        SELECT ?1,?2,?3,?4,?5,?6,?7,(SELECT json_group_object(i.id,json(coalesce((
            SELECT json_object('version',i.source_version,'ref',f.reply_ref,'proof',f.reply_version)
            FROM visible_facet f WHERE f.input_id=i.id AND f.record_id=r.id AND f.reply_ref IS NOT NULL LIMIT 1
        ),i.source_version))) FROM visible_input i
            WHERE ?4 IS NOT NULL AND i.origin_ref IS NOT NULL AND (i.id=r.input_id OR EXISTS (
                SELECT 1 FROM visible_facet f WHERE f.record_id=r.id AND f.input_id=i.id)))
        FROM projected_record r WHERE r.id=?2"),params![profile,args.id,args.version+1,args.content,args.attribution,
            ids.map(|v|serde_json::json!(v).to_string()),now_ms(),allowed])?;
    let result = detail(&tx,profile,&args.id,&allowed)?;
    tx.commit()?;
    Ok(result)
}
