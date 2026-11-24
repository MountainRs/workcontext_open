use super::{formal_detail, freshness_of, unix_now, ItemDetail, ItemDetailCore, ItemPage, ItemSummary, ItemsError, ItemsQueryArgs};
use crate::ownership_flow::{Candidate, EvidenceDetail, State};
use serde::Serialize;
use workcontext_store::rusqlite::{self, params, Connection, OptionalExtension, Row};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryDetail {
    pub version: String,
    pub state: State,
    pub present: bool,
    pub formal: bool,
    pub confirmation: String,
    pub project: Option<String>,
    pub scope_id: Option<String>,
    pub work: String,
    pub evidence_details: Vec<EvidenceDetail>,
    pub conclusion: Option<String>,
    pub related: Vec<RelatedDelivery>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelatedDelivery {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopeItem {
    pub id: String,
    pub title: String,
    pub lifecycle: String,
    pub attention: String,
    pub next_action: Option<String>,
    pub conclusion: Option<String>,
}

pub fn context_scope_items(conn: &Connection, profile: &str, id: &str) -> Result<Vec<ScopeItem>, ItemsError> {
    let mut stmt = conn.prepare(&format!("{CATALOG} SELECT id,title,lifecycle,attention,next_action,note FROM catalog
        WHERE scope_id=?2 ORDER BY title,id"))?;
    let rows = stmt.query_map(params![profile,id], |row| Ok(ScopeItem {
        id: row.get(0)?, title: row.get(1)?, lifecycle: row.get(2)?, attention: row.get(3)?,
        next_action: row.get(4)?, conclusion: row.get(5)?,
    }))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

// Unreserved work items predate account binding and retain their existing local visibility.
// Once reserved, the identity belongs to its profile even when undo removes the formal row.
pub(crate) const CATALOG: &str = "WITH visible(id) AS (
    SELECT id FROM ownership_card WHERE profile=?1
    UNION ALL
    SELECT w.id FROM work_item w WHERE NOT EXISTS (SELECT 1 FROM ownership_card f WHERE f.id=w.id)
), evidence AS NOT MATERIALIZED (
    SELECT f.id, e.key AS position, nullif(trim(e.value->>'$.conversation'),'') AS conversation,
        CASE WHEN datetime(e.value->>'$.time','+0 seconds')=e.value->>'$.time'
             THEN unixepoch((e.value->>'$.time') || '+08:00') * 1000 END AS at
    FROM ownership_card f, json_each(f.candidate_json,'$.evidenceDetails') e WHERE f.profile=?1
), catalog AS (
    SELECT v.id, coalesce(w.title,json_extract(f.candidate_json,'$.work')) AS title,
        coalesce(w.lifecycle, CASE json_extract(f.candidate_json,'$.state')
            WHEN 'triage' THEN 'candidate' WHEN 'done' THEN 'closed' WHEN 'ignored' THEN 'ignored' ELSE 'active' END) AS lifecycle,
        coalesce(w.attention, CASE json_extract(f.candidate_json,'$.state')
            WHEN 'mine' THEN 'me' WHEN 'waiting' THEN 'waiting_other' ELSE 'no_next' END) AS attention,
        CASE WHEN w.lifecycle IN ('closed','ignored') THEN NULL
            WHEN w.id IS NULL AND json_extract(f.candidate_json,'$.state') IN ('done','ignored') THEN NULL
            WHEN w.id IS NULL THEN nullif(json_extract(f.candidate_json,'$.action'),'') ELSE w.next_action END AS next_action,
        CASE WHEN s.id IS NOT NULL THEN s.display_name || CASE WHEN trim(coalesce(sr.sender_name,''))='' THEN '' ELSE ' · ' || sr.sender_name END
            WHEN f.id IS NOT NULL THEN coalesce(nullif(trim(json_extract(f.candidate_json,'$.sourceSummary.place')),''),
                (SELECT p.conversation FROM evidence p WHERE p.id=f.id AND p.conversation IS NOT NULL
                 ORDER BY p.at DESC,p.position DESC LIMIT 1)) ELSE '本地记录' END AS source_label,
        coalesce(w.last_evidence_at,(SELECT max(p.at) FROM evidence p WHERE p.id=f.id)) AS last_evidence_at, w.updated_at,
        coalesce(w.awaiting_confirmation, json_extract(f.candidate_json,'$.state')='triage') AS awaiting_confirmation,
        CASE WHEN c.note IS NOT NULL OR (w.id IS NOT NULL AND (
            w.resolution_evidence='human_confirmed' OR EXISTS (SELECT 1 FROM field_override o WHERE o.target_type='work_item'
                AND o.target_id=w.id AND o.locked=1 AND o.set_by='human' AND o.field IN ('lifecycle','attention','belongs_to_me','next_action'))
            OR (w.lifecycle IN ('closed','ignored') AND EXISTS (SELECT 1 FROM work_event e WHERE e.target_type='work_item'
                AND e.target_id=w.id AND e.event_type=w.lifecycle AND e.actor='human' AND e.at=w.closed_at))))
            THEN 'confirmed' ELSE 'suggested' END AS confirmation,
        f.present IS 0 AND w.id IS NULL AS historical,
        nullif(trim(CASE WHEN f.id IS NULL THEN t.title ELSE coalesce(p.title,f.project_override,json_extract(f.candidate_json,'$.project')) END),'') AS project,
        p.id AS scope_id,
        f.revision, f.present, f.candidate_json, w.id IS NOT NULL AS formal, c.note, w.rowid AS formal_rowid
    FROM visible v LEFT JOIN ownership_card f ON f.id=v.id LEFT JOIN work_item w ON w.id=v.id
    LEFT JOIN ownership_conclusion c ON c.card_id=f.id LEFT JOIN source_record sr ON sr.id=w.origin_source_record_id
    LEFT JOIN source s ON s.id=sr.source_id
    LEFT JOIN work_thread t ON t.id=w.thread_id AND t.profile IS NULL
    LEFT JOIN work_thread p ON p.id=f.context_scope_id AND p.profile=f.profile AND p.kind='project'
)";

const SUMMARY: &str = "id,title,lifecycle,attention,next_action,source_label,last_evidence_at,updated_at,awaiting_confirmation,confirmation,historical,project";

fn summary(row: &Row<'_>) -> rusqlite::Result<ItemSummary> {
    Ok(ItemSummary {
        id: row.get(0)?, title: row.get(1)?, lifecycle: row.get(2)?, attention: row.get(3)?,
        next_action: row.get(4)?, source_label: row.get(5)?, last_evidence_at: row.get(6)?,
        updated_at: row.get(7)?, awaiting_confirmation: row.get(8)?, confirmation: row.get(9)?,
        historical: row.get(10)?, project: row.get(11)?,
    })
}

pub(super) fn query(conn: &Connection, args: &ItemsQueryArgs, profile: Option<&str>) -> Result<ItemPage, ItemsError> {
    let filter = match args.filter.as_deref().unwrap_or("all") {
        "all" => "1",
        "me" => "lifecycle='active' AND attention IN ('me','report_back')",
        "waiting" => "lifecycle='active' AND (attention IN ('waiting_other','awaiting_acceptance') OR awaiting_confirmation=1)",
        "confirm" => "lifecycle='candidate' AND awaiting_confirmation=1",
        "closed" => "lifecycle IN ('closed','ignored')",
        other => return Err(ItemsError::InvalidArgument { message: format!("未知的列表筛选 “{other}”，未执行查询。") }),
    };
    let text = args.query.as_deref().map(str::trim).filter(|s| !s.is_empty());
    // Only the bound, quoted literal reaches FTS. Short strings and suggestions use
    // substring matching because the trigram index has no entries for them.
    let phrase = text.filter(|q| !q.contains('\0') && q.chars().nth(2).is_some())
        .map(|q| format!("\"{}\"", q.replace('"', "\"\"")));
    let predicate = format!("FROM catalog WHERE ({filter}) AND (?2 IS NULL OR instr(lower(project),lower(?2))>0
        OR CASE WHEN ?3 IS NOT NULL AND formal THEN formal_rowid IN (
            SELECT rowid FROM work_item_fts WHERE work_item_fts MATCH ?3)
        ELSE instr(lower(title),lower(?2))>0 OR instr(lower(next_action),lower(?2))>0 END)");
    let tx = conn.unchecked_transaction()?;
    let total = tx.query_row(&format!("{CATALOG} SELECT count(*) {predicate}"), params![profile,text,phrase], |row| row.get(0))?;
    let items = {
        let mut stmt = tx.prepare(&format!("{CATALOG} SELECT {SUMMARY} {predicate}
            ORDER BY last_evidence_at DESC, updated_at DESC, historical, id LIMIT ?4 OFFSET ?5"))?;
        let rows = stmt.query_map(params![profile,text,phrase,args.limit.unwrap_or(100).clamp(1,500),args.offset.unwrap_or(0).max(0)], summary)?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let freshness = freshness_of(&tx, unix_now(&tx)?)?;
    tx.commit()?;
    Ok(ItemPage { items, total, freshness })
}

pub fn item_get_for_profile(conn: &Connection, id: &str, profile: Option<&str>) -> Result<ItemDetail, ItemsError> {
    let tx = conn.unchecked_transaction()?;
    let detail = get(&tx, id, profile)?;
    tx.commit()?;
    Ok(detail)
}

pub(super) fn get(conn: &Connection, id: &str, profile: Option<&str>) -> Result<ItemDetail, ItemsError> {
    let (row, version, present, formal, snapshot, conclusion, scope_id) = conn.query_row(
        &format!("{CATALOG} SELECT {SUMMARY},revision,present,formal,candidate_json,note,scope_id FROM catalog WHERE id=?2"),
        params![profile,id], |r| Ok((summary(r)?, r.get::<_,Option<i64>>(12)?, r.get::<_,Option<bool>>(13)?,
            r.get::<_,bool>(14)?, r.get::<_,Option<String>>(15)?, r.get::<_,Option<String>>(16)?, r.get::<_,Option<String>>(17)?)),
    ).optional()?.ok_or(ItemsError::NotFound)?;
    let delivery = if let (Some(version), Some(present), Some(snapshot)) = (version, present, snapshot) {
        let candidate: Candidate = serde_json::from_str(&snapshot).map_err(|e| rusqlite::Error::FromSqlConversionFailure(
            15, rusqlite::types::Type::Text, Box::new(e)))?;
        let related = if let Some(project) = &row.project {
            let mut stmt = conn.prepare(&format!("{CATALOG} SELECT id,title FROM catalog WHERE id<>?3
                AND (scope_id=?4 OR (?4 IS NULL AND scope_id IS NULL AND project=?2)) ORDER BY title,id"))?;
            let rows = stmt.query_map(params![profile,project,id,scope_id], |r| Ok(RelatedDelivery { id: r.get(0)?, title: r.get(1)? }))?;
            rows.collect::<rusqlite::Result<_>>()?
        } else { Vec::new() };
        let state = if formal {
            match (row.lifecycle.as_str(),row.attention.as_str()) {
                ("closed",_) => State::Done, ("ignored",_) => State::Ignored,
                ("active","me" | "report_back") => State::Mine,
                ("active","waiting_other" | "awaiting_acceptance") => State::Waiting,
                _ => State::Triage,
            }
        } else { candidate.state };
        Some(DeliveryDetail { version: version.to_string(), state, present, formal,
            confirmation: row.confirmation.clone(), project: row.project.clone(), scope_id, work: row.title.clone(),
            evidence_details: candidate.evidence_details, conclusion, related })
    } else { None };
    let mut detail = if formal { formal_detail(conn, id, row.confirmation, row.project)? } else {
        ItemDetail {
            item: ItemDetailCore {
                id: row.id, title: row.title, lifecycle: row.lifecycle, attention: row.attention,
                confirmation: row.confirmation, project: row.project,
                next_action: row.next_action, followup_type: "none".into(), followup_at: None, followup_note: None,
                thread_id: None, confidence: None, awaiting_confirmation: row.awaiting_confirmation,
                closed_at: None, close_reason: None, resolution_evidence: None, last_evidence_at: None,
                created_at: None, updated_at: None, locked_fields: Vec::new(),
            },
            delivery: None, timeline: Vec::new(), evidence: Vec::new(), corrections: Vec::new(), source_freshness: Vec::new(),
        }
    };
    detail.item.last_evidence_at = row.last_evidence_at;
    if matches!(detail.item.lifecycle.as_str(), "closed" | "ignored") { detail.item.next_action = None; }
    if formal {
        if let Some(scope_id) = delivery.as_ref().and_then(|card| card.scope_id.as_ref()) {
            detail.item.thread_id = Some(scope_id.clone());
        }
    }
    detail.delivery = delivery;
    Ok(detail)
}
