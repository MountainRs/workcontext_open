//! Reconcile account-bound report candidates with human-owned work items.
//! `ownership_card` holds source identity and the last suggestion. Once an item exists,
//! its lifecycle, attention, title and next action always come from `work_item`.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use workcontext_core::{evaluate_transition, Actor, Attention, Lifecycle, ResolutionEvidence,
    ResolutionEvidenceType, TransitionOutcome, TransitionRequest};
use workcontext_store::rusqlite::{self, params, Connection, OptionalExtension, TransactionBehavior};

use crate::items::{load_item, load_locks, ItemRow, ItemsError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State { Mine, Waiting, Triage, Done, Ignored }

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
pub struct SourceKey {
    pub cid: String,
    pub mid: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct SourceSummary {
    pub time: String,
    pub place: String,
    pub people: String,
    pub event: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct EvidenceDetail {
    pub time: String,
    pub conversation: String,
    pub kind: String,
    pub excerpt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub evidence: Vec<SourceKey>,
    #[serde(default)]
    pub anchors: Vec<SourceKey>,
    #[serde(default)]
    pub evidence_details: Vec<EvidenceDetail>,
    pub source_work: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub state: State,
    #[serde(default)]
    pub needs_verification: bool,
    pub work: String,
    pub action: String,
    pub when: String,
    pub source_summary: SourceSummary,
}

impl Candidate {
    // Verification describes the suggested action and can change between reports.
    fn matching_key(&self) -> impl Eq + std::hash::Hash + '_ {
        (&self.anchors, &self.evidence, &self.source_work, self.state, &self.work, &self.action, &self.when, &self.source_summary)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowCard {
    pub id: String,
    pub version: String,
    pub state: State,
    pub needs_verification: bool,
    pub work: String,
    pub action: String,
    pub when: String,
    pub source_summary: SourceSummary,
    #[serde(default)]
    pub evidence_details: Vec<EvidenceDetail>,
    #[serde(default)]
    pub conclusion: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Receipt { pub undo_token: String }

fn invalid(message: &str) -> ItemsError {
    ItemsError::InvalidArgument { message: message.into() }
}

fn stale() -> ItemsError {
    ItemsError::ValidationRejected {
        message: "事项已发生变化，请刷新清单后再操作。".into(), hint: None,
    }
}

fn json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("flow DTOs serialize without fallible map keys")
}

fn decode<T: serde::de::DeserializeOwned>(value: &str) -> rusqlite::Result<T> {
    serde_json::from_str(value).map_err(|e| rusqlite::Error::FromSqlConversionFailure(
        0, rusqlite::types::Type::Text, Box::new(e),
    ))
}

fn prepare(candidate: &Candidate) -> Result<Cow<'_, Candidate>, ItemsError> {
    if candidate.work.trim().is_empty() || !matches!(candidate.state, State::Mine | State::Waiting | State::Triage) {
        return Err(invalid("报告候选事项缺少标题或建议状态无效。"));
    }
    if candidate.evidence.iter().chain(&candidate.anchors).any(|k| k.cid.is_empty() || k.mid.is_empty()) {
        return Err(invalid("报告中的消息来源不完整。"));
    }
    if candidate.evidence.windows(2).all(|w| w[0] < w[1]) && candidate.anchors.windows(2).all(|w| w[0] < w[1]) {
        return Ok(Cow::Borrowed(candidate));
    }
    let mut normalized = candidate.clone();
    normalized.evidence.sort_unstable();
    normalized.evidence.dedup();
    normalized.anchors.sort_unstable();
    normalized.anchors.dedup();
    Ok(Cow::Owned(normalized))
}

struct Stored {
    id: String,
    revision: i64,
    present: bool,
    tracked: bool,
    candidate: Candidate,
    conclusion: Option<String>,
    conclusion_anchors: Vec<SourceKey>,
}

// Reserve exact duplicates one-to-one, then accept only reciprocal, uniquely best matches.
// Never transfer human state to a second item simply because both cite one message.
fn align(old: &[Stored], new: &[Cow<'_, Candidate>]) -> Vec<Option<usize>> {
    let mut exact: HashMap<_, VecDeque<usize>> = HashMap::new();
    if new.is_empty() || old.is_empty() { return vec![None; new.len()]; }
    for (i, row) in old.iter().enumerate() {
        exact.entry(row.candidate.matching_key()).or_default().push_back(i);
    }
    let mut result: Vec<_> = new.iter().map(|c| exact.get_mut(&c.matching_key()).and_then(VecDeque::pop_front)).collect();
    if result.iter().all(Option::is_some) { return result; }
    let mut by_source: HashMap<&SourceKey, Vec<usize>> = HashMap::new();
    let mut by_anchor: HashMap<&[SourceKey], Vec<usize>> = HashMap::new();
    let mut source_sets: HashMap<&[SourceKey], (usize, usize)> = HashMap::new();
    for candidate in new {
        source_sets.entry(&candidate.evidence).or_default().1 += 1;
    }
    for (i, row) in old.iter().enumerate() {
        let anchors = if row.candidate.anchors.is_empty() { &row.conclusion_anchors } else { &row.candidate.anchors };
        if !anchors.is_empty() { by_anchor.entry(anchors).or_default().push(i); }
        source_sets.entry(&row.candidate.evidence).or_default().0 += 1;
        for key in &row.candidate.evidence {
            by_source.entry(key).or_default().push(i);
        }
    }
    let taken: HashSet<usize> = result.iter().flatten().copied().collect();
    let mut old_best = vec![BestMatch::default(); old.len()];
    let mut new_best = vec![BestMatch::default(); new.len()];
    for (i, candidate) in new.iter().enumerate().filter(|(i, _)| result[*i].is_none()) {
        let mut overlap = HashMap::<usize, usize>::new();
        if let Some(indices) = by_anchor.get(candidate.anchors.as_slice()) {
            for &j in indices {
                if !taken.contains(&j) {
                    old_best[j].consider(32, i);
                    new_best[i].consider(32, j);
                }
            }
        }
        for key in &candidate.evidence {
            if let Some(indices) = by_source.get(key) {
                for &index in indices {
                    if !taken.contains(&index) { *overlap.entry(index).or_default() += 1; }
                }
            }
        }
        for (j, shared) in overlap {
            let previous = &old[j].candidate;
            if !candidate.anchors.is_empty() && (!previous.anchors.is_empty() || !old[j].conclusion_anchors.is_empty()) { continue; }
            if old[j].conclusion.is_some() && previous.anchors.is_empty() && old[j].conclusion_anchors.is_empty() {
                // A broad legacy evidence list has no proven origin. Only the exact
                // snapshot match above can safely carry its human conclusion forward.
                continue;
            }
            if candidate.anchors.is_empty() && !previous.anchors.is_empty() && candidate.evidence != previous.anchors {
                // All references in the legacy card must identify the known delivery.
                continue;
            }
            if !candidate.anchors.iter().all(|k| previous.evidence.binary_search(k).is_ok())
                || !previous.anchors.iter().all(|k| candidate.evidence.binary_search(k).is_ok()) { continue; }
            let same_set = previous.evidence == candidate.evidence;
            let same_subject = !candidate.source_work.is_empty() && candidate.source_work == previous.source_work;
            let same_title = candidate.work == previous.work;
            let same_action = !candidate.action.is_empty() && candidate.action == previous.action;
            if !same_set && (shared == 1 || old[j].conclusion.is_some()) { continue; }
            // Two independently cited messages with a unique complete source set can
            // survive a rewrite of both titles. A lone common message cannot establish that.
            let unique_set = same_set && shared >= 2
                && source_sets.get(candidate.evidence.as_slice()) == Some(&(1, 1));
            let corroborated = same_set && (same_action || (!candidate.source_summary.event.is_empty()
                && candidate.source_summary.event == previous.source_summary.event));
            let missing_subject = candidate.source_work.is_empty() || previous.source_work.is_empty();
            if !(unique_set || same_subject || (same_title && (same_set || missing_subject))
                || (missing_subject && corroborated)) {
                continue;
            }
            let score = 1 + 8 * u8::from(same_set) + 4 * u8::from(same_subject)
                + 2 * u8::from(same_title) + u8::from(same_action);
            old_best[j].consider(score, i);
            new_best[i].consider(score, j);
        }
    }
    for (i, choice) in new_best.iter().enumerate() {
        if let Some(j) = choice.index {
            if old_best[j].index == Some(i) { result[i] = Some(j); }
        }
    }
    result
}

#[derive(Default, Clone, Copy)]
struct BestMatch { score: u8, index: Option<usize> }

impl BestMatch {
    fn consider(&mut self, score: u8, index: usize) {
        if score > self.score { self.score = score; self.index = Some(index); }
        else if score == self.score { self.index = None; }
    }
}

fn item_state(item: &ItemRow) -> State {
    match item.lifecycle {
        Lifecycle::Closed => State::Done,
        Lifecycle::Ignored => State::Ignored,
        Lifecycle::Candidate => State::Triage,
        Lifecycle::Active => match item.attention {
            Attention::Me | Attention::ReportBack => State::Mine,
            Attention::WaitingOther | Attention::AwaitingAcceptance => State::Waiting,
            Attention::NoNext => State::Triage,
        },
    }
}

fn card(id: String, revision: i64, candidate: &Candidate, item: Option<ItemRow>, conclusion: Option<String>) -> FlowCard {
    let needs_verification = item.is_none() && candidate.state == State::Mine && candidate.needs_verification;
    let (state, work, action) = match item {
        Some(item) => (item_state(&item), item.title, item.next_action.unwrap_or_default()),
        None => (candidate.state, candidate.work.clone(), candidate.action.clone()),
    };
    let when = if action == candidate.action { candidate.when.clone() } else { String::new() };
    FlowCard { id, version: revision.to_string(), state, needs_verification, work, action, when,
        source_summary: candidate.source_summary.clone(), evidence_details: candidate.evidence_details.clone(), conclusion }
}

/// Return report order followed by tracked items that have fallen outside the latest window.
pub fn view(conn: &mut Connection, profile: &str, candidates: &[Candidate]) -> Result<Vec<FlowCard>, ItemsError> {
    if profile.trim().is_empty() { return Err(invalid("缺少当前账号。")); }
    let new: Vec<_> = candidates.iter().map(prepare).collect::<Result<_, _>>()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let old = {
        let mut stmt = tx.prepare("SELECT f.id, f.revision, f.present, w.id IS NOT NULL, f.candidate_json, c.note, c.anchors_json
            FROM ownership_card f LEFT JOIN work_item w ON w.id=f.id LEFT JOIN ownership_conclusion c ON c.card_id=f.id
            WHERE f.profile=?1 ORDER BY f.present DESC, f.rowid")?;
        let rows = stmt.query_map([profile], |r| Ok(Stored {
            id: r.get(0)?, revision: r.get(1)?, present: r.get(2)?, tracked: r.get(3)?,
            candidate: decode(&r.get::<_, String>(4)?)?, conclusion: r.get(5)?,
            conclusion_anchors: r.get::<_, Option<String>>(6)?.as_deref().map(decode).transpose()?.unwrap_or_default(),
        }))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let matches = align(&old, &new);
    let mut seen = vec![false; old.len()];
    let mut cards = Vec::with_capacity(new.len());
    for (mut candidate, matched) in new.into_iter().zip(matches) {
        if let Some(i) = matched {
            seen[i] = true;
            let row = &old[i];
            let mut revision = row.revision;
            if candidate.anchors.is_empty() && !row.candidate.anchors.is_empty() {
                candidate.to_mut().anchors.clone_from(&row.candidate.anchors);
            }
            if !row.present || row.candidate != *candidate {
                // Report-only changes have no authority over a formal item's action or undo token.
                revision += i64::from(!row.tracked);
                tx.execute("UPDATE ownership_card SET candidate_json=?2, present=1, revision=?3 WHERE id=?1",
                    params![row.id, json(candidate.as_ref()), revision])?;
            }
            let item = if row.tracked { load_item(&tx, &row.id)? } else { None };
            cards.push(card(row.id.clone(), revision, &candidate, item, row.conclusion.clone()));
        } else {
            let id = tx.query_row("INSERT INTO ownership_card(profile, candidate_json) VALUES (?1,?2) RETURNING id",
                params![profile, json(candidate.as_ref())], |r| r.get(0))?;
            cards.push(card(id, 1, &candidate, None, None));
        }
    }
    for (row, seen) in old.into_iter().zip(seen) {
        if seen { continue; }
        if row.present {
            tx.execute("UPDATE ownership_card SET present=0, revision=revision+?2 WHERE id=?1",
                params![row.id, i64::from(!row.tracked)])?;
        }
        if row.tracked {
            let item = load_item(&tx, &row.id)?;
            cards.push(card(row.id, row.revision, &row.candidate, item, row.conclusion));
        }
    }
    tx.commit()?;
    Ok(cards)
}

// A receipt stores only the columns this service writes. All other work_item fields stay
// in the formal record. Revision checks also cover same-millisecond and ABA changes.
#[derive(Serialize, Deserialize)]
struct Fields {
    lifecycle: Lifecycle,
    attention: Attention,
    action: Option<String>,
    awaiting_confirmation: bool,
    closed_at: Option<i64>,
    close_reason: Option<String>,
    resolution: Option<String>,
    last_evidence_at: Option<i64>,
}

impl From<ItemRow> for Fields {
    fn from(row: ItemRow) -> Self {
        Self { lifecycle: row.lifecycle, attention: row.attention, action: row.next_action,
            awaiting_confirmation: row.awaiting_confirmation, closed_at: row.closed_at,
            close_reason: row.close_reason, resolution: row.resolution_evidence,
            last_evidence_at: row.last_evidence_at }
    }
}

fn update_fields(conn: &Connection, id: &str, f: &Fields, now: i64) -> rusqlite::Result<()> {
    conn.execute("UPDATE work_item SET lifecycle=?2, attention=?3, next_action=?4,
        awaiting_confirmation=?5, closed_at=?6, close_reason=?7, resolution_evidence=?8,
        last_evidence_at=?9, updated_at=?10 WHERE id=?1", params![id, f.lifecycle.as_str(),
            f.attention.as_str(), f.action, f.awaiting_confirmation, f.closed_at, f.close_reason,
            f.resolution, f.last_evidence_at, now])?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Override {
    field: String,
    value: Option<String>,
    locked: bool,
    set_at: i64,
    set_by: String,
    note: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Before {
    item: Option<Fields>,
    locks: Vec<Override>,
    reopen_record: Option<String>,
    #[serde(default)]
    retired_resolution: bool,
}

const FLOW_LOCKS: &str = "target_type='work_item' AND target_id=?1 AND field IN ('lifecycle','attention','next_action','belongs_to_me')";

fn old_overrides(conn: &Connection, id: &str) -> rusqlite::Result<Vec<Override>> {
    let mut stmt = conn.prepare(&format!("SELECT field, value_json, locked, set_at, set_by, note
        FROM field_override WHERE {FLOW_LOCKS} ORDER BY field"))?;
    let rows = stmt.query_map([id], |r| Ok(Override { field: r.get(0)?, value: r.get(1)?,
        locked: r.get(2)?, set_at: r.get(3)?, set_by: r.get(4)?, note: r.get(5)? }))?;
    rows.collect()
}

fn lock(conn: &Connection, id: &str, field: &str, value_json: &str, now: i64, note: &str) -> rusqlite::Result<()> {
    conn.execute("INSERT INTO field_override(target_type,target_id,field,value_json,locked,set_at,set_by,note)
        VALUES ('work_item',?1,?2,?3,1,?4,'human',?5)
        ON CONFLICT(target_type,target_id,field) DO UPDATE SET value_json=excluded.value_json,
        locked=1,set_at=excluded.set_at,set_by='human',note=excluded.note",
        params![id,field,value_json,now,note])?;
    Ok(())
}

fn event(conn: &Connection, id: &str, kind: &str, now: i64, note: &str, source: Option<&str>,
    before: Option<&Fields>, after: Option<&Fields>) -> rusqlite::Result<()> {
    conn.execute("INSERT INTO work_event(id,target_type,target_id,event_type,actor,at,note,source_record_id,before_json,after_json)
        VALUES ('flow-event-'||lower(hex(randomblob(16))),'work_item',?1,?2,'human',?3,?4,?5,?6,?7)",
        params![id,kind,now,note,source,before.map(json),after.map(json)])?;
    Ok(())
}

// A human-entered reopen action is an actual new local source record. No sync scope,
// pending judgement, or archive message is manufactured for this operation.
fn reopen_note(conn: &Connection, action: &str, now: i64) -> rusqlite::Result<String> {
    conn.execute("INSERT INTO source(id,kind,display_name,scope_json,enabled,created_at)
        VALUES ('ownership-reopen-notes','manual','人工重开说明','[]',0,?1) ON CONFLICT(id) DO NOTHING", [now])?;
    let id: String = conn.query_row("SELECT 'flow-note-' || lower(hex(randomblob(16)))", [], |r| r.get(0))?;
    let hash = format!("{:x}", Sha256::digest(action.as_bytes()));
    conn.execute("INSERT INTO source_record(id,source_id,external_id,kind,occurred_at,ingested_at,summary,content_hash,judge_state)
        VALUES (?1,'ownership-reopen-notes',?1,'manual_note',?2,?2,?3,?4,'skipped')", params![id,now,action,hash])?;
    Ok(id)
}

/// Apply a human decision atomically. Reopening a closed item requires an explicitly
/// supplied next action, which is retained as the new evidence required by the guard.
pub fn move_card(conn: &mut Connection, profile: &str, id: &str, version: &str, target: State,
    action: Option<&str>, now_ms: i64) -> Result<Receipt, ItemsError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (candidate, item) = checked_card(&tx, profile, id, version)?;
    if target == State::Done && item.as_ref().map(item_state).unwrap_or(candidate.state) == State::Triage {
        return Err(invalid("当前清单状态不支持此操作。"));
    }
    let before = apply_move(&tx, id, &candidate, item, target, action, now_ms)?;
    tx.execute("DELETE FROM ownership_undo WHERE card_id=?1", [id])?;
    let token = tx.query_row("INSERT INTO ownership_undo(card_id,revision,before_json)
        SELECT id,revision,?2 FROM ownership_card WHERE id=?1 RETURNING token",
        params![id,json(&before)], |r| r.get(0))?;
    tx.commit()?;
    Ok(Receipt { undo_token: token })
}

fn checked_card(tx: &Connection, profile: &str, id: &str, version: &str) -> Result<(Candidate, Option<ItemRow>), ItemsError> {
    let (revision, present, candidate): (i64, bool, Candidate) = tx.query_row(
        "SELECT revision,present,candidate_json FROM ownership_card WHERE id=?1 AND profile=?2",
        params![id,profile], |r| Ok((r.get(0)?,r.get(1)?,decode(&r.get::<_, String>(2)?)?)))
        .optional()?.ok_or(ItemsError::NotFound)?;
    if revision.to_string() != version { return Err(stale()); }
    let item = load_item(&tx, id)?;
    if !present && item.is_none() { return Err(ItemsError::NotFound); }
    Ok((candidate, item))
}

/// Project membership is independent of responsibility and is editable on past deliveries.
pub fn set_project(conn: &Connection, profile: &str, id: &str, version: &str, project: Option<&str>) -> Result<(), ItemsError> {
    let project = project.unwrap_or_default().trim();
    if project.chars().count() > 120 || project.chars().any(char::is_control) {
        return Err(invalid("项目名称须为单行，最多 120 个字。"));
    }
    write_project(conn, profile, id, version, project, None)
}

pub fn context_item_scope_set(conn: &Connection, profile: &str, id: &str, version: &str, scope_id: Option<&str>) -> Result<(), ItemsError> {
    write_project(conn, profile, id, version, "", scope_id)
}

fn write_project(conn: &Connection, profile: &str, id: &str, version: &str, label: &str, scope_id: Option<&str>) -> Result<(), ItemsError> {
    let tx = conn.unchecked_transaction()?;
    if let Some(scope_id) = scope_id {
        let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM work_thread WHERE id=?1 AND profile=?2 AND kind='project')",
            params![scope_id,profile], |row| row.get(0))?;
        if !exists { return Err(invalid("所选项目不存在。")); }
    }
    let updated = tx.execute("UPDATE ownership_card SET context_scope_id=?1, project_override=?5, revision=revision+1
        WHERE id=?2 AND profile=?3 AND CAST(revision AS TEXT)=?4", params![scope_id,id,profile,version,label])?;
    if updated == 0 { return Err(stale()); }
    tx.execute("UPDATE work_item SET thread_id=NULL WHERE id=?1 AND thread_id IS NOT NULL", [id])?;
    tx.commit()?;
    Ok(())
}

pub fn set_conclusion(conn: &mut Connection, profile: &str, id: &str, version: &str, note: &str,
    resolved: bool, now_ms: i64) -> Result<(), ItemsError> {
    let note = note.trim();
    if note.is_empty() || note.chars().count() > 1000 { return Err(invalid("结论须为 1 至 1000 个字。")); }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (candidate, mut item) = checked_card(&tx, profile, id, version)?;
    if resolved && item.as_ref().map(item_state) != Some(State::Done) {
        // The formal state machine permits an ignored item to close only after restoration.
        if item.as_ref().map(item_state) == Some(State::Ignored) {
            apply_move(&tx, id, &candidate, item, State::Mine, Some(note), now_ms)?;
            item = load_item(&tx, id)?;
        }
        apply_move(&tx, id, &candidate, item, State::Done, None, now_ms)?;
    }
    let mut anchors = candidate.anchors.as_slice();
    if anchors.is_empty() && candidate.evidence.len() == 1 {
        let key = &candidate.evidence[0];
        let shared: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM ownership_card f,
            json_each(json_array(json_extract(f.candidate_json,'$.evidence'), json_extract(f.candidate_json,'$.anchors'))) lists,
            json_each(lists.value) e WHERE f.profile=?1 AND f.id<>?2
            AND json_extract(e.value,'$.cid')=?3 AND json_extract(e.value,'$.mid')=?4)",
            params![profile,id,key.cid,key.mid], |r| r.get(0))?;
        if !shared { anchors = &candidate.evidence; }
    }
    tx.execute("INSERT INTO ownership_conclusion(card_id,note,resolved,anchors_json) VALUES (?1,?2,?3,?4)
        ON CONFLICT(card_id) DO UPDATE SET note=excluded.note,
        resolved=max(ownership_conclusion.resolved,excluded.resolved),
        anchors_json=CASE WHEN excluded.anchors_json='[]' THEN ownership_conclusion.anchors_json ELSE excluded.anchors_json END",
        params![id,note,resolved,json(&anchors)])?;
    tx.execute("UPDATE ownership_card SET revision=revision+1 WHERE id=?1", [id])?;
    tx.execute("DELETE FROM ownership_undo WHERE card_id=?1", [id])?;
    tx.commit()?;
    Ok(())
}

fn apply_move(tx: &Connection, id: &str, candidate: &Candidate, item: Option<ItemRow>, target: State,
    action: Option<&str>, now_ms: i64) -> Result<Before, ItemsError> {
    let state = item.as_ref().map(item_state).unwrap_or(candidate.state);
    let prior = item.map(Fields::from);
    let (lifecycle, attention, mut kind, note) = match target {
        State::Mine => (Lifecycle::Active, Attention::Me, "updated", "本人处理"),
        State::Waiting => (Lifecycle::Active, Attention::WaitingOther, "updated", "等待他人"),
        State::Done => (Lifecycle::Closed, Attention::NoNext, "closed", "人工确认完成"),
        State::Ignored => (Lifecycle::Ignored, Attention::NoNext, "ignored", "人工忽略"),
        State::Triage => return Err(invalid("请选择本人处理、等待他人、完成或忽略。")),
    };
    let current = prior.as_ref().map(|f| f.lifecycle).unwrap_or(Lifecycle::Candidate);
    let explicit = action.map(str::trim);
    if explicit == Some("") { return Err(invalid("下一步不能为空。")); }
    let existing_action = match &prior { Some(f) => f.action.as_deref(), None => Some(candidate.action.as_str()) };
    let next_action = match (state, target) {
        (State::Mine, State::Mine) | (State::Waiting, State::Waiting) if prior.is_none() => explicit,
        (State::Triage, State::Mine) => explicit.or(existing_action),
        (State::Mine | State::Triage, State::Waiting)
        | (State::Waiting | State::Done | State::Ignored, State::Mine) => {
            Some(explicit.ok_or_else(|| invalid("交接、接回或恢复跟进时，请填写新的下一步。"))?)
        }
        (State::Mine | State::Waiting | State::Triage, State::Done)
        | (State::Mine | State::Waiting | State::Triage, State::Ignored) => existing_action,
        _ => return Err(invalid("当前清单状态不支持此操作。")),
    };
    let next_action = next_action.map(str::trim).filter(|s| !s.is_empty());
    if lifecycle == Lifecycle::Active && next_action.is_none() { return Err(invalid("请填写具体的下一步。")); }
    let reopening = current == Lifecycle::Closed && lifecycle == Lifecycle::Active;
    let record = if reopening {
        Some(reopen_note(&tx, explicit.ok_or_else(|| invalid("重新打开已完成事项时，请填写新的下一步。"))?, now_ms)?)
    } else { None };
    let resolution = (target == State::Done).then_some(ResolutionEvidence {
        evidence_type: ResolutionEvidenceType::HumanConfirmed, evidence_ids: Vec::new(), note: None,
    });
    let locks = load_locks(&tx, id)?;
    let req = TransitionRequest { from: current, to: lifecycle, actor: Actor::Human,
        reason: Some(note), new_source_record_id: record.as_deref(),
        resolution: resolution.as_ref(), anchor_satisfied: true, locked_fields: &locks };
    if let TransitionOutcome::Denied(denial) = evaluate_transition(&req) {
        return Err(ItemsError::ValidationRejected { message: format!("该事项当前无法这样转换（{denial:?}）。"), hint: None });
    }
    let terminal = matches!(target, State::Done | State::Ignored);
    let fields = Fields { lifecycle, attention, action: next_action.map(str::to_owned), awaiting_confirmation: false,
        closed_at: terminal.then_some(now_ms), close_reason: terminal.then(|| note.to_string()),
        resolution: (target == State::Done).then(|| ResolutionEvidenceType::HumanConfirmed.as_str().to_owned()),
        last_evidence_at: if reopening { Some(now_ms) } else { prior.as_ref().and_then(|f| f.last_evidence_at) },
    };
    let overrides = old_overrides(&tx, id)?;
    if prior.is_none() {
        tx.execute("INSERT INTO work_item(id,title,dedupe_key,created_at,updated_at,lifecycle,attention,
            next_action,closed_at,close_reason,resolution_evidence) VALUES (?1,?2,?1,?3,?3,?4,?5,?6,?7,?8,?9)",
            params![id,candidate.work,now_ms,lifecycle.as_str(),attention.as_str(),fields.action,
                fields.closed_at,fields.close_reason,fields.resolution])?;
        if !terminal { kind = "created"; }
    } else {
        update_fields(&tx, id, &fields, now_ms)?;
        if reopening || current == Lifecycle::Ignored { kind = "reopened"; }
    }
    lock(&tx, id, "lifecycle", &json(&lifecycle), now_ms, note)?;
    if !terminal || overrides.iter().any(|o| o.field == "attention" && o.locked) {
        lock(&tx, id, "attention", &json(&attention), now_ms, note)?;
    }
    if !terminal {
        lock(&tx, id, "next_action", &json(&next_action), now_ms, note)?;
        if overrides.iter().any(|o| o.field == "belongs_to_me" && o.locked) {
            lock(&tx, id, "belongs_to_me", "true", now_ms, note)?;
        }
    }
    if let Some(record) = &record {
        tx.execute("INSERT INTO link_evidence(id,source_record_id,target_type,target_id,link_type,state,decided_by,created_at)
            VALUES (?1,?1,'work_item',?2,'reference','confirmed','human',?3)", params![record,id,now_ms])?;
    }
    event(&tx, id, kind, now_ms, note, record.as_deref(), prior.as_ref(), Some(&fields))?;
    let retired_resolution = reopening && tx.execute(
        "UPDATE ownership_conclusion SET resolved=0 WHERE card_id=?1 AND resolved=1", [id])? != 0;
    Ok(Before { item: prior, locks: overrides, reopen_record: record, retired_resolution })
}

pub fn undo(conn: &mut Connection, profile: &str, token: &str, now_ms: i64) -> Result<(), ItemsError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (id, valid, before): (String, bool, Before) = tx.query_row("SELECT f.id,f.revision=u.revision,u.before_json
        FROM ownership_undo u JOIN ownership_card f ON u.card_id=f.id WHERE u.token=?1 AND f.profile=?2",
        params![token,profile], |r| Ok((r.get(0)?,r.get(1)?,decode(&r.get::<_, String>(2)?)?)))
        .optional()?.ok_or_else(stale)?;
    if !valid { return Err(stale()); }
    let current = load_item(&tx, &id)?.ok_or_else(stale)?.into();
    tx.execute(&format!("DELETE FROM field_override WHERE {FLOW_LOCKS}"), [&id])?;
    for old in before.locks {
        tx.execute("INSERT INTO field_override(target_type,target_id,field,value_json,locked,set_at,set_by,note)
            VALUES ('work_item',?1,?2,?3,?4,?5,?6,?7)", params![id,old.field,old.value,old.locked,old.set_at,old.set_by,old.note])?;
    }
    if let Some(record) = &before.reopen_record {
        tx.execute("DELETE FROM link_evidence WHERE id=?1 AND target_type='work_item' AND target_id=?2", params![record,id])?;
    }
    if let Some(prior) = &before.item {
        update_fields(&tx, &id, prior, now_ms)?;
    } else {
        tx.execute("DELETE FROM work_item WHERE id=?1", [&id])?;
    }
    if before.retired_resolution {
        tx.execute("UPDATE ownership_conclusion SET resolved=1 WHERE card_id=?1", [&id])?;
    }
    event(&tx, &id, "undone", now_ms, "撤销清单操作", None, Some(&current), before.item.as_ref())?;
    tx.execute("DELETE FROM ownership_undo WHERE token=?1", [token])?;
    tx.commit()?;
    Ok(())
}
