use std::collections::BTreeMap;

use crate::Failure;
use workcontext_app::external_read::ReadArgs;

pub const HELP: &str = "WorkContext 本机上下文 CLI\n\nworkcontext mcp（stdio MCP 服务）\nworkcontext overview | recent\nworkcontext search 关键词\nworkcontext projects list | projects show ID\nworkcontext records show ID\nworkcontext records add --json < record.json\nworkcontext records add --producer 名称 --request-id 固定键 < result.txt\nworkcontext project suggest --json < suggestion.json\nworkcontext messages search [关键词] [--conversation ID] [--since 时间] [--until 时间]\nworkcontext source get 引用\nworkcontext skills find [关键词] [--scope ID]\n\n读取参数按命令支持 --scope ID、--source 来源、--since、--until、--cursor 和 --limit。\n时间可用 UTC 毫秒或带时区的 RFC3339。加 --json 输出版本化 JSON。\n账号和工作库由桌面授权决定。";

pub struct Args {
    pub words: Vec<String>,
    pub options: BTreeMap<String, String>,
    pub json: bool,
}

impl Args {
    pub fn parse(argv: &[String]) -> Result<Self, Failure> {
        let mut parsed = Self { words: Vec::new(), options: BTreeMap::new(), json: false };
        let mut words = argv.iter();
        while let Some(word) = words.next() {
            if word == "--json" {
                parsed.json = true;
            } else if let Some(name) = word.strip_prefix("--") {
                let value = words.next().filter(|v| !v.starts_with("--") && !v.is_empty())
                    .ok_or_else(|| Failure::invalid(format!("参数 {word} 缺少值")))?;
                if parsed.options.insert(name.to_owned(), value.clone()).is_some() {
                    return Err(Failure::invalid(format!("重复参数 {word}")));
                }
            } else {
                parsed.words.push(word.clone());
            }
        }
        Ok(parsed)
    }

    pub fn allow(&self, names: &[&str]) -> Result<(), Failure> {
        if let Some(key) = self.options.keys().find(|k| !names.contains(&k.as_str())) {
            return Err(Failure::invalid(format!("此命令不接受 --{key}")));
        }
        Ok(())
    }

    pub fn option(&self, key: &str) -> Option<String> { self.options.get(key).cloned() }

    pub fn time(&self, key: &str) -> Result<Option<i64>, Failure> {
        self.options.get(key).map(|s| {
            s.parse::<i64>().ok().or_else(|| chrono::DateTime::parse_from_rfc3339(s).ok().map(|t| t.timestamp_millis()))
                .ok_or_else(|| Failure::invalid(format!("--{key} 需要 UTC 毫秒或带时区的 RFC3339 时间")))
        }).transpose()
    }

    pub fn limit(&self) -> Result<Option<i64>, Failure> {
        self.options.get("limit").map(|s| s.parse::<i64>().ok().filter(|v| (1..=100).contains(v))
            .ok_or_else(|| Failure::invalid("--limit 须为 1 至 100"))).transpose()
    }

    pub fn filters(&self) -> Result<ReadArgs, Failure> {
        self.allow(&["since", "until", "scope", "source", "cursor", "limit"])?;
        Ok(ReadArgs { source: self.option("source"), scope_id: self.option("scope"), cursor: self.option("cursor"),
            since: self.time("since")?, until: self.time("until")?, limit: self.limit()? })
    }
}
