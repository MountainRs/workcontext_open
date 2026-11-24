//! Version-one skills.json registry shared by desktop and offline queries.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, io::{self, Write}, path::{Path, PathBuf}};
use workcontext_store::rusqlite::{self, Connection};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")] InvalidIndex(&'static str),
    #[error("{0}")] InvalidEntry(&'static str),
    #[error("无法读写能力索引：{0}")] Unavailable(#[from] io::Error),
    #[error(transparent)] Database(#[from] rusqlite::Error),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    version: u32,
    #[serde(default)] global: Vec<Entry>,
    #[serde(default)] profiles: BTreeMap<String, Vec<Entry>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Entry {
    pub name: String,
    pub purpose: String,
    pub path: PathBuf,
    pub scope_id: Option<String>,
}

impl Entry {
    fn valid_path(&self) -> bool {
        self.path.is_absolute() && self.path.file_name().is_some_and(|name| name == "SKILL.md")
    }
    fn available(&self) -> bool {
        self.path.is_file() && fs::File::open(&self.path).is_ok()
    }
}

fn load(root: &Path) -> Result<Index, Error> {
    let bytes = match fs::read(root.join("skills.json")) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Index { version: 1, global: vec![], profiles: BTreeMap::new() }),
        Err(e) => return Err(e.into()),
    };
    let index: Index = serde_json::from_slice(&bytes).map_err(|_| Error::InvalidIndex("skills.json 格式错误"))?;
    if index.version != 1 { return Err(Error::InvalidIndex("不支持该索引版本")); }
    Ok(index)
}

#[derive(Serialize)]
pub struct Hit {
    #[serde(flatten)] pub entry: Entry,
    pub available: bool,
    #[serde(skip)] pub global: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page { pub items: Vec<Hit>, pub next_cursor: Option<String> }

const SCOPE_EXISTS: &str = "SELECT EXISTS(SELECT 1 FROM work_thread WHERE profile=?1 AND id=?2)";

pub fn find(db: &Connection, root: &Path, profile: Option<&str>, query: &str, scope: Option<&str>, offset: usize, limit: usize) -> Result<Page, Error> {
    let mut index = load(root)?;
    let local = profile.and_then(|id| index.profiles.remove(id)).unwrap_or_default();
    let global = index.global.into_iter().filter(|e| e.scope_id.is_none()).map(|e| (true, e));
    let query = query.to_lowercase();
    let mut visible = db.prepare(SCOPE_EXISTS)?;
    let mut items = Vec::new();
    let mut skipped = 0;
    for (global, entry) in global.chain(local.into_iter().map(|e| (false, e))) {
        if scope.is_some() && scope != entry.scope_id.as_deref() { continue; }
        if !query.is_empty() && !entry.name.to_lowercase().contains(&query) && !entry.purpose.to_lowercase().contains(&query) { continue; }
        if let Some(id) = &entry.scope_id {
            if !visible.query_row((profile, id), |r| r.get::<_, bool>(0))? { continue; }
        }
        if skipped < offset { skipped += 1; continue; }
        if items.len() == limit { return Ok(Page { items, next_cursor: Some((offset + limit).to_string()) }); }
        if !entry.valid_path() { return Err(Error::InvalidIndex("索引须登记原始 SKILL.md 的绝对路径")); }
        items.push(Hit { available: entry.available(), global, entry });
    }
    Ok(Page { items, next_cursor: None })
}

pub fn register(db: &Connection, root: &Path, profile: Option<&str>, mut entry: Entry) -> Result<(), Error> {
    entry.name = entry.name.trim().to_owned();
    entry.purpose = entry.purpose.trim().to_owned();
    if entry.name.is_empty() || entry.purpose.is_empty() { return Err(Error::InvalidEntry("请填写名称和用途")); }
    if !entry.valid_path() || !entry.available() { return Err(Error::InvalidEntry("请填写现有、可读的原始 SKILL.md 绝对路径")); }
    if let Some(id) = &entry.scope_id {
        if profile.is_none() || !db.query_row(SCOPE_EXISTS, (profile, id), |r| r.get::<_, bool>(0))? {
            return Err(Error::InvalidEntry("所选范围在当前账号中不存在"));
        }
    }
    // Keep the lock inode stable across replacements and release on process exit.
    let lock = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(root.join("skills.lock"))?;
    lock.lock()?;
    let mut index = load(root)?;
    let group = match profile { Some(p) => index.profiles.entry(p.to_owned()).or_default(), None => &mut index.global };
    if group.iter().any(|e| e.path == entry.path && e.scope_id == entry.scope_id) { return Err(Error::InvalidEntry("该路径已登记在此范围")); }
    group.push(entry);
    let mut replacement = tempfile::NamedTempFile::new_in(root)?;
    serde_json::to_writer_pretty(&mut replacement, &index).map_err(io::Error::other)?;
    replacement.write_all(b"\n")?;
    replacement.as_file().sync_all()?;
    replacement.persist(root.join("skills.json")).map_err(|e| e.error)?;
    Ok(())
}
