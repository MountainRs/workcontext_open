use std::path::Path;
use serde_json::{json, Value};
use workcontext_app::skills::{self, Error};
use workcontext_store::rusqlite::Connection;
use crate::{args::Args, Failure};

pub fn find(conn: &Connection, root: &Path, profile: &str, query: &str, args: &Args) -> Result<Value, Failure> {
    args.allow(&["scope", "limit", "cursor"])?;
    let limit = args.limit()?.unwrap_or(20) as usize;
    let offset: usize = args.options.get("cursor").map(|s| s.parse()).transpose().map_err(|_| Failure::invalid("无效的索引游标"))?.unwrap_or(0);
    let page = skills::find(conn, root, Some(profile), query, args.options.get("scope").map(String::as_str), offset, limit)
        .map_err(|e| match e {
            Error::InvalidIndex(message) => Failure::new("INVALID_SKILL_INDEX", message),
            Error::InvalidEntry(message) => Failure::invalid(message),
            Error::Unavailable(_) => Failure::new("SKILL_INDEX_UNAVAILABLE", "无法读取 skills.json"),
            Error::Database(db) => db.into(),
        })?;
    Ok(json!(page))
}
