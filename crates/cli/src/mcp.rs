//! Newline-delimited MCP stdio adapter. All workspace operations go through `run`.
use crate::{run, Args, Failure};
use serde_json::{json, Map, Value};
use std::{io::{self, BufRead, Read, Write}, process::ExitCode};

const MAX_FRAME: u64 = 1_048_576;
const VERSION: &str = "2025-06-18";
struct Tool {
    name: &'static str, description: &'static str, command: &'static str,
    positional: Option<&'static str>, options: &'static [&'static str], required: &'static [&'static str], fields: &'static [&'static str],
}
const FILTERS: &[&str] = &["source", "scope", "since", "until", "limit", "cursor"];
const fn read(name: &'static str, description: &'static str, command: &'static str, positional: Option<&'static str>, options: &'static [&'static str], required: &'static [&'static str]) -> Tool {
    Tool { name, description, command, positional, options, required, fields: &[] }
}
const TOOLS: &[Tool] = &[
    read("overview", "Summarize the current workspace", "overview", None, FILTERS, &[]),
    read("recent", "List recent context records", "recent", None, FILTERS, &[]),
    read("search", "Search context by keyword", "search", Some("keyword"), FILTERS, &["keyword"]),
    read("projects_list", "List projects and domains", "projects list", None, FILTERS, &[]),
    read("projects_show", "Read one project or domain", "projects show", Some("id"), FILTERS, &["id"]),
    read("records_show", "Read one context record", "records show", Some("id"), &[], &["id"]),
    read("source_get", "Read a source by its workspace reference", "source get", Some("id"), &[], &["id"]),
    read("messages_search", "Search authorized archived messages", "messages search", Some("keyword"), &["conversation", "since", "until", "cursor", "limit"], &[]),
    read("skills_find", "Find indexed skills", "skills find", Some("keyword"), &["scope", "limit", "cursor"], &[]),
    Tool { name: "records_add", description: "Append an idempotent agent record", command: "records add", positional: None, options: &[], required: &["producer", "requestId", "title", "result"], fields: &["producer", "requestId", "title", "result", "inference", "openQuestions", "materials", "scopeIds", "scopeHints", "relatedRecordIds", "kind", "occurredAt"] },
    Tool { name: "project_suggest", description: "Submit an idempotent project or domain proposal for desktop review", command: "project suggest", positional: None, options: &[], required: &["producer", "requestId", "kind", "title", "goal", "background", "reason", "source"], fields: &["producer", "requestId", "scopeId", "baseVersion", "kind", "title", "goal", "background", "reason", "source"] },
];
impl Tool {
    fn schema(&self) -> Value {
        let mut properties = Map::new();
        for &key in self.positional.iter().chain(self.options).chain(self.fields) {
            let field = match key {
                "since" | "until" => json!({"type":["integer","string"],"description":"UTC milliseconds or RFC3339 timestamp"}),
                "limit" => json!({"type":"integer","minimum":1,"maximum":100}),
                "baseVersion" | "occurredAt" => json!({"type":["integer","null"]}),
                "inference" | "scopeId" => json!({"type":["string","null"]}),
                "materials" => json!({"type":"array","items":{"type":"object","properties":{"path":{"type":"string","description":"Material reference stored with the record"},"summary":{"type":["string","null"]}},"required":["path"],"additionalProperties":false}}),
                "openQuestions" | "scopeIds" | "scopeHints" | "relatedRecordIds" => json!({"type":"array","items":{"type":"string"}}),
                "kind" if self.name == "records_add" => json!({"enum":["event","progress","decision","experience"]}),
                "kind" => json!({"enum":["project","domain"]}),
                _ => json!({"type":"string"}),
            };
            properties.insert(key.into(), field);
        }
        json!({"name":self.name,"description":self.description,"inputSchema":{"type":"object","properties":properties,"required":self.required,"additionalProperties":false}})
    }
    fn call(&self, input: &Map<String, Value>) -> Result<Value, Failure> {
        let mut args = Args { words: self.command.split_whitespace().map(str::to_owned).collect(), options: Default::default(), json: true };
        if self.fields.is_empty() {
            for (key, value) in input {
                let text = match value {
                    Value::String(s) if key != "limit" => s.clone(),
                    Value::Number(n) if n.is_i64() && matches!(key.as_str(), "since" | "until" | "limit") => n.to_string(),
                    _ => return Err(Failure::invalid(format!("Invalid argument {key}"))),
                };
                if self.positional == Some(key.as_str()) { args.words.push(text); }
                else if self.options.contains(&key.as_str()) { args.options.insert(key.clone(), text); }
                else { return Err(Failure::invalid(format!("Unknown argument {key}"))); }
            }
            if self.required.iter().any(|key| !input.contains_key(*key)) { return Err(Failure::invalid("Missing required argument")); }
        }
        run(&args, || serde_json::to_string(input).map_err(|e| Failure::invalid(e.to_string())))
    }
}
fn error(id: &Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
#[derive(PartialEq)]
enum Session { New, Initializing, Ready }
fn dispatch(session: &mut Session, msg: Value) -> Option<Value> {
    let id = &msg["id"];
    let method = msg["method"].as_str();
    let has_id = msg.get("id").is_some();
    if msg["jsonrpc"] != "2.0" || method.is_none() || (has_id && !(id.is_string() || id.is_i64() || id.is_u64())) {
        return Some(error(&Value::Null, -32600, "Invalid request"));
    }
    let method = method?;
    if !has_id {
        if method == "notifications/initialized" && *session == Session::Initializing { *session = Session::Ready; }
        return None;
    }
    let params = &msg["params"];
    let invalid = || Some(error(id, -32602, "Invalid params"));
    if msg.get("params").is_some() && !params.is_object() { return invalid(); }
    let result = match method {
        "initialize" if *session == Session::New => {
            let Some(version) = params["protocolVersion"].as_str() else { return invalid() };
            if !params["capabilities"].is_object() || !params["clientInfo"]["name"].is_string() || !params["clientInfo"]["version"].is_string() { return invalid(); }
            *session = Session::Initializing;
            let version = match version { "2024-11-05" | "2025-06-18" => version, _ => VERSION };
            json!({"protocolVersion":version,"serverInfo":{"name":"workcontext","version":env!("CARGO_PKG_VERSION")},"capabilities":{"tools":{}}})
        }
        "ping" => json!({}),
        _ if *session != Session::Ready => return Some(error(id,-32002,"Server not initialized")),
        "tools/list" => {
            if params.get("cursor").is_some() { return invalid(); }
            json!({"tools":TOOLS.iter().map(Tool::schema).collect::<Vec<_>>()})
        }
        "tools/call" => {
            let Some(tool) = TOOLS.iter().find(|tool| Some(tool.name) == params["name"].as_str()) else { return invalid() };
            let empty = Map::new();
            let args = match params.get("arguments") { Some(Value::Object(args)) => args, None => &empty, _ => return invalid() };
            let (payload, failed) = match tool.call(args) {
                Ok(data) => (json!({"version":1,"ok":true,"data":data}),false),
                Err(e) => (json!({"version":1,"ok":false,"error":{"code":e.code,"message":e.message}}),true),
            };
            json!({"content":[{"type":"text","text":payload.to_string()}],"isError":failed})
        }
        _ => return Some(error(id,-32601,"Method not found")),
    };
    Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
}
fn stream(mut input: impl BufRead, mut out: impl Write) -> io::Result<()> {
    let mut session = Session::New;
    let mut frame = Vec::new();
    loop {
        frame.clear();
        if (&mut input).take(MAX_FRAME + 2).read_until(b'\n', &mut frame)? == 0 { return Ok(()); }
        let terminated = frame.last() == Some(&b'\n');
        if terminated { frame.pop(); }
        if frame.last() == Some(&b'\r') { frame.pop(); }
        let response = if frame.len() as u64 > MAX_FRAME {
            if !terminated { input.skip_until(b'\n')?; }
            Some(error(&Value::Null,-32700,"Frame exceeds 1 MiB"))
        } else {
            match serde_json::from_slice(&frame) { Ok(message) => dispatch(&mut session,message), Err(_) => Some(error(&Value::Null,-32700,"Parse error")) }
        };
        if let Some(response) = response { serde_json::to_writer(&mut out,&response)?; out.write_all(b"\n")?; out.flush()?; }
    }
}
pub(super) fn serve() -> ExitCode {
    match stream(io::stdin().lock(),io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => { eprintln!("MCP stdio: {e}"); ExitCode::FAILURE }
    }
}
