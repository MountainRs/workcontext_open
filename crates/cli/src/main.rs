mod args;
mod skills;

use std::{io::{self, Read}, path::{Path, PathBuf}, process::ExitCode};
use args::Args;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use workcontext_app::{external_access, external_archive as archive, external_read as read, external_suggestions, external_write, items::ItemsError};
use workcontext_store::rusqlite::{self, Connection};

struct Failure { code: &'static str, message: String }
impl Failure {
    fn new(code: &'static str, message: impl Into<String>) -> Self { Self { code, message: message.into() } }
    fn invalid(message: impl Into<String>) -> Self { Self::new("INVALID_ARGUMENT", message) }
}
impl From<ItemsError> for Failure {
    fn from(e: ItemsError) -> Self { Self::new(e.code(), e.to_string()) }
}
impl From<rusqlite::Error> for Failure {
    fn from(e: rusqlite::Error) -> Self { ItemsError::Db(e).into() }
}
impl From<archive::ArchiveError> for Failure {
    fn from(e: archive::ArchiveError) -> Self { Self::new(e.code(), e.to_string()) }
}
impl From<external_write::WriteError> for Failure {
    fn from(e: external_write::WriteError) -> Self {
        use external_write::WriteError::*;
        let code = match e { Invalid(_) => "INVALID_ARGUMENT", Conflict => "IDEMPOTENCY_CONFLICT", UnknownReference => "NOT_FOUND", Database(err) => return err.into() };
        Self::new(code, e.to_string())
    }
}

mod mcp;
fn root() -> Result<PathBuf, Failure> {
    // Distribution and native acceptance binaries are paired with their Tauri identifier at build time.
    let identifier = option_env!("WORKCONTEXT_BUILD_IDENTIFIER").unwrap_or(if cfg!(feature = "dev-workspace") { "com.local.workcontext.dev" } else { "com.local.workcontext" });
    if identifier.is_empty() || !identifier.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b,b'.'|b'-')) || identifier.starts_with('.') {
        return Err(Failure::new("CONFIG_UNAVAILABLE", "构建的工作区标识无效"));
    }
    Ok(dirs::data_dir().ok_or_else(|| Failure::new("CONFIG_UNAVAILABLE", "找不到本机应用数据目录"))?.join(identifier))
}

fn stdin_text() -> Result<String, Failure> {
    const MAX: usize = 1_048_576;
    let mut data = Vec::new();
    io::stdin().lock().take(MAX as u64 + 1).read_to_end(&mut data).map_err(|_| Failure::invalid("无法读取标准输入"))?;
    if data.len() > MAX { return Err(Failure::invalid("标准输入超过 1 MiB")); }
    if data.starts_with(&[0xef, 0xbb, 0xbf]) { data.drain(..3); }
    String::from_utf8(data).map_err(|_| Failure::invalid("标准输入须使用 UTF-8"))
}
fn decode<T: DeserializeOwned>(text: &str) -> Result<T, Failure> {
    serde_json::from_str(text).map_err(|e| Failure::invalid(format!("JSON 输入无效：{e}")))
}

enum Write { Record(external_write::AddRecord), Suggest(external_suggestions::SuggestArgs) }
fn prepare_write(args: &Args, words: &[&str], input: impl FnOnce() -> Result<String, Failure>) -> Result<Option<Write>, Failure> {
    match words {
        ["records", "add"] => {
            args.allow(&["producer", "request-id"])?;
            let record = if args.options.is_empty() {
                decode(&input()?)?
            } else {
                let producer = args.option("producer").ok_or_else(|| Failure::invalid("缺少 --producer"))?;
                let id = args.option("request-id").ok_or_else(|| Failure::invalid("缺少 --request-id"))?;
                external_write::AddRecord::from_text(producer,id,input()?)
            };
            Ok(Some(Write::Record(record)))
        }
        ["project", "suggest"] => { args.allow(&[])?; Ok(Some(Write::Suggest(decode(&input()?)?))) }
        _ => Ok(None),
    }
}

fn query(conn: &Connection, root: &Path, profile: &str, words: &[&str], args: &Args) -> Result<Value, Failure> {
    let mut result = match words {
        ["overview"] => read::overview(conn, profile, &args.filters()?)?,
        ["recent"] => read::recent(conn, profile, &args.filters()?)?,
        ["search", phrase] => read::search(conn, profile, phrase, &args.filters()?)?,
        ["projects", "list"] => read::projects(conn, profile, &args.filters()?)?,
        ["projects", "show", id] => read::project(conn, profile, id, &args.filters()?)?,
        ["records", "show", id] => { args.allow(&[])?; read::record(conn,profile,id,&Default::default())? }
        ["source", "get", id] => {
            args.allow(&[])?;
            if id.starts_with("input-") { read::input(conn,profile,id,&Default::default())? }
            else { return Ok(archive::source_get(conn, root, profile, id, workcontext_store::now_ms())?); }
        }
        ["messages", "search", ..] if words.len() <= 3 => {
            args.allow(&["conversation", "since", "until", "cursor", "limit"])?;
            let options = archive::Search { keyword: words.get(2).map(|s| s.to_string()), conversation: args.option("conversation"),
                start_ms: args.time("since")?, end_ms: args.time("until")?, cursor: args.option("cursor"), limit: args.limit()?.unwrap_or(20) as usize };
            return Ok(archive::search(conn,root,profile,&options,workcontext_store::now_ms())?);
        }
        ["skills", "find", ..] if words.len() <= 3 => return skills::find(conn,root,profile,words.get(2).copied().unwrap_or(""),args),
        _ => return Err(Failure::invalid("未知命令或参数，使用 workcontext --help 查看用法")),
    };
    if let Some(marks) = result.get_mut("watermarks") { marks["archiveAt"] = json!(archive::watermark(root, profile)); }
    Ok(result)
}

fn run(args: &Args, input: impl FnOnce() -> Result<String, Failure>) -> Result<Value, Failure> {
    let root = root()?;
    let authorize = || external_access::authorize(&root).map_err(|e| Failure::new(e.code(), e.to_string()));
    let profile = authorize()?;
    let words: Vec<&str> = args.words.iter().map(String::as_str).collect();
    // Never hold the maintenance lock while waiting for a pipe, or use a grant changed during input.
    let write = prepare_write(args, &words, input)?;
    let mut conn = workcontext_store::open_external(&root.join(workcontext_core::DB_FILENAME), write.is_some()).map_err(|e| Failure::new(e.code(), e.to_string()))?;
    if authorize()? != profile { return Err(Failure::new("BINDING_MISMATCH", "工作区绑定已变更")); }
    match write {
        Some(Write::Record(record)) => Ok(json!(external_write::add_record(&mut conn, &profile, &record)?)),
        Some(Write::Suggest(suggestion)) => Ok(json!(external_suggestions::suggest(&mut conn, &profile, &suggestion)?)),
        None => query(&conn, &root, &profile, &words, args),
    }
}

fn text(value: &Value, indent: usize) {
    let pad = "  ".repeat(indent);
    match value {
        Value::Object(map) => for (key, value) in map {
            if value.is_object() || value.is_array() { println!("{pad}{key}"); text(value, indent + 1); }
            else { print!("{pad}{key}  "); text(value, 0); }
        },
        Value::Array(rows) if rows.is_empty() => println!("{pad}（无）"),
        Value::Array(rows) => for (i,row) in rows.iter().enumerate() { if i != 0 && row.is_object() { println!(); } text(row,indent); },
        Value::String(s) => println!("{pad}{s}"),
        Value::Null => println!("{pad}—"),
        other => println!("{pad}{other}"),
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv == ["--help"] || argv == ["-h"] { println!("{}", args::HELP); return ExitCode::SUCCESS; }
    if argv == ["mcp"] { return mcp::serve(); }
    let json_output = argv.iter().any(|word| word == "--json");
    match Args::parse(&argv).and_then(|args| run(&args, stdin_text)) {
        Ok(data) => { if json_output { println!("{}", json!({"version":1,"ok":true,"data":data})); } else { text(&data, 0); } ExitCode::SUCCESS }
        Err(e) => { if json_output { println!("{}", json!({"version":1,"ok":false,"error":{"code":e.code,"message":e.message}})); } else { eprintln!("{}  {}", e.code,e.message); } ExitCode::FAILURE }
    }
}
