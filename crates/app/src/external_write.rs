//! Append-only agent reports. The caller supplies the locally authorized profile.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use workcontext_store::{now_ms, rusqlite::{self, params, Connection, OptionalExtension, TransactionBehavior}};

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Material {
    pub path: String,
    pub summary: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AddRecord {
    pub producer: String,
    pub request_id: String,
    pub title: String,
    pub result: String,
    pub inference: Option<String>,
    #[serde(default)] pub open_questions: Vec<String>,
    #[serde(default)] pub materials: Vec<Material>,
    #[serde(default)] pub scope_ids: Vec<String>,
    #[serde(default)] pub scope_hints: Vec<String>,
    #[serde(default)] pub related_record_ids: Vec<String>,
    #[serde(default = "default_kind")] pub kind: String,
    pub occurred_at: Option<i64>,
}

fn default_kind() -> String { "progress".into() }

impl AddRecord {
    pub fn from_text(producer: String, request_id: String, text: String) -> Self {
        let title = text.trim().lines().next().unwrap_or_default().chars().take(160).collect();
        Self { producer, request_id, title, result: text, inference: None, open_questions: vec![],
            materials: vec![], scope_ids: vec![], scope_hints: vec![], related_record_ids: vec![],
            kind: default_kind(), occurred_at: None }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    pub input_id: String,
    pub record_id: String,
    pub processing_state: String,
    pub saved: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("invalid record field: {0}")] Invalid(&'static str),
    #[error("request ID already used for a different payload")] Conflict,
    #[error("referenced scope or record is unavailable")] UnknownReference,
    #[error(transparent)] Database(#[from] rusqlite::Error),
}

fn text(name: &'static str, value: &str, limit: usize) -> Result<(), WriteError> {
    if value.trim().is_empty() || value.contains('\0') || value.chars().count() > limit {
        return Err(WriteError::Invalid(name));
    }
    Ok(())
}

fn list(name: &'static str, values: &[String], limit: usize) -> Result<(), WriteError> {
    if values.len() > 64 { return Err(WriteError::Invalid(name)); }
    let mut seen = HashSet::new();
    for value in values {
        text(name, value, limit)?;
        if !seen.insert(value) { return Err(WriteError::Invalid(name)); }
    }
    Ok(())
}

fn validate(profile: &str, args: &AddRecord) -> Result<(), WriteError> {
    text("profile", profile, 1024)?;
    text("producer", &args.producer, 256)?;
    text("requestId", &args.request_id, 256)?;
    text("title", &args.title, 512)?;
    text("result", &args.result, 65536)?;
    if let Some(value) = &args.inference { text("inference", value, 65536)?; }
    list("openQuestions", &args.open_questions, 4096)?;
    list("scopeHints", &args.scope_hints, 512)?;
    list("scopeIds", &args.scope_ids, 256)?;
    list("relatedRecordIds", &args.related_record_ids, 256)?;
    if !matches!(args.kind.as_str(), "event" | "progress" | "decision" | "experience") {
        return Err(WriteError::Invalid("kind"));
    }
    if args.materials.len() > 64 { return Err(WriteError::Invalid("materials")); }
    let mut paths = HashSet::new();
    for material in &args.materials {
        text("path", &material.path, 4096)?;
        if !paths.insert(&material.path) { return Err(WriteError::Invalid("materials")); }
        if let Some(value) = &material.summary { text("summary", value, 4096)?; }
    }
    Ok(())
}

pub fn add_record(conn: &mut Connection, profile: &str, args: &AddRecord) -> Result<Receipt, WriteError> {
    validate(profile, args)?;
    // Typed serialization fixes field order and resolves omitted default values before hashing.
    let original = serde_json::to_string(args).map_err(|_| WriteError::Invalid("payload"))?;
    let hash = format!("{:x}", Sha256::digest(original.as_bytes()));
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let previous = tx.query_row("SELECT i.payload_hash,i.id,r.id,i.processing_state FROM context_input i
        JOIN context_record r ON r.input_id=i.id AND r.profile=i.profile AND r.organizer_created=0
        WHERE i.profile=?1 AND i.producer=?2 AND i.request_id=?3", params![profile,args.producer,args.request_id],
        |row| Ok((row.get::<_,String>(0)?, Receipt { input_id: row.get(1)?, record_id: row.get(2)?,
            processing_state: row.get(3)?, saved: true }))).optional()?;
    if let Some((stored, receipt)) = previous {
        if stored != hash { return Err(WriteError::Conflict); }
        tx.commit()?;
        return Ok(receipt);
    }
    let now = now_ms();
    let input_id: String = tx.query_row("INSERT INTO context_input(profile,original,created_at,producer,request_id,payload_hash,processing_state)
        VALUES (?1,?2,?3,?4,?5,?6,'pending') RETURNING id", params![profile,original,now,args.producer,args.request_id,hash], |r| r.get(0))?;
    let mut content = args.result.clone();
    if let Some(value) = &args.inference { content.push_str("\n推断："); content.push_str(value); }
    for value in &args.open_questions { content.push_str("\n未决问题："); content.push_str(value); }
    let questions = serde_json::to_string(&args.open_questions).map_err(|_| WriteError::Invalid("openQuestions"))?;
    let hints = serde_json::to_string(&args.scope_hints).map_err(|_| WriteError::Invalid("scopeHints"))?;
    let record_id: String = tx.query_row("INSERT INTO context_record(profile,input_id,title,content,kind,occurred_at,created_at,
        reported_result,inference,open_questions,scope_hints) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11) RETURNING id",
        params![profile,input_id,args.title,content,args.kind,args.occurred_at,now,args.result,args.inference,questions,hints], |r| r.get(0))?;
    for id in &args.scope_ids {
        if tx.execute("INSERT INTO record_scope(profile,record_id,scope_id) SELECT profile,?2,id FROM work_thread WHERE profile=?1 AND id=?3",
            params![profile,record_id,id])? != 1 { return Err(WriteError::UnknownReference); }
    }
    for id in &args.related_record_ids {
        if tx.execute("INSERT INTO record_related(profile,record_id,related_record_id) SELECT profile,?2,id FROM context_record WHERE profile=?1 AND id=?3",
            params![profile,record_id,id])? != 1 { return Err(WriteError::UnknownReference); }
    }
    for (position, material) in args.materials.iter().enumerate() {
        tx.execute("INSERT INTO record_ref(record_id,position,material,summary) VALUES (?1,?2,?3,?4)",
            params![record_id,position as i64,material.path,material.summary])?;
    }
    tx.commit()?;
    Ok(Receipt { input_id, record_id, processing_state: "pending".into(), saved: true })
}
