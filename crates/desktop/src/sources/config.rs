//! `config.json`：非敏感持久配置（详细设计 §13.1）。
//!
//! - 白名单本体在 SQLite `source.scope_json`（权威存储，随来源注册写入）；
//!   本文件只放 dws profile（账号标识，非凭据）、本人身份与出域/判读参数。
//!   **敏感凭据永不落此文件**（钉钉凭据在系统密钥库，由 `dws` 自管）。
//! - **不保存任何可执行文件路径**：CLI 只按固定 basename（`dws`/`omp`）在
//!   受信位置解析（见 `sources::trusted_exe`），IPC 与配置都不能指定别的
//!   程序，调用方也无法传入任意参数。
//! - 读取用 serde 默认值补全（旧配置/部分字段缺失不炸）；写回整体重写，
//!   临时文件 + rename 原子替换（§13.1「原子：写临时文件 + rename」）。
//! - `extra` 收集未知顶层键并在重写时原样保留：本模块不拥有整份文件的全部
//!   章节（retention/ui 等属其它模块），不得悄悄丢弃别人的配置。

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use workcontext_infra::sync::Scope;

/// 当前配置结构版本。
pub const CONFIG_SCHEMA: u32 = 1;

/// 配置文件读写失败。
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("配置文件读取失败 {path}：{source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("配置文件不是合法 JSON {path}：{source}")]
    Parse {
        path: String,
        source: serde_json::Error,
    },
    #[error("配置文件版本过新（schema {0}），请升级应用后再试")]
    FutureSchema(u32),
}

/// 整份 `config.json`。字段名与 §13.1 示例一致（camelCase）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SettingsFile {
    pub schema: u32,
    /// 本人身份（判读 `MeContext` 与 dws `me_user_id` 的来源）。
    pub me: MeSettings,
    /// 判读 runtime 选择与参数。
    pub judge: JudgeSettings,
    /// 出域策略：默认 `none`——未经批准，正文一律不出本机。
    pub egress: EgressSettings,
    /// 同步引擎参数（喂给 `SyncConfig`）。
    pub sync: SyncSettings,
    /// 正文保留期等（只解析 `rawBodyDays`，段内其余键原样保留）。
    pub retention: RetentionSettings,
    /// 每来源 dws 账号 profile（键 = `source.id`）。
    pub sources: BTreeMap<String, SourceCliSettings>,
    /// 未识别的顶层键：读入后原样保留，重写时不丢失。
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for SettingsFile {
    fn default() -> Self {
        Self {
            schema: CONFIG_SCHEMA,
            me: MeSettings::default(),
            judge: JudgeSettings::default(),
            retention: RetentionSettings::default(),
            egress: EgressSettings::default(),
            sync: SyncSettings::default(),
            sources: BTreeMap::new(),
            extra: Map::new(),
        }
    }
}

/// 本人身份：判读归属锚点（V03）与 @我 预筛都依赖它。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MeSettings {
    /// 钉钉稳定 user id；为空 = 未配置，判读触发将被拒绝（不臆断归属）。
    pub user_id: String,
    pub display_name: Option<String>,
}

/// 判读 runtime：`mock`（离线夹具，默认）或 `cli`（真实 OMP CLI）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JudgeSettings {
    pub mode: JudgeMode,
    /// `cli` 模式必填：精确 provider/model id（写入 `agent_run.model_id`）。
    pub model_id: Option<String>,
    /// 单次判读超时（秒）；内层 `--max-time` 取 timeout-5，外层 kill 兜底
    /// （OMP 调研 §7.4：实测 TTFT 可达 14–19s，默认 60s）。
    pub timeout_seconds: u64,
    /// `mock` 模式的夹具根目录（`fixtures/` 布局）。
    pub fixtures_dir: Option<String>,
    pub batch_size: u8,
    pub min_confidence_auto_active: u8,
}

impl Default for JudgeSettings {
    fn default() -> Self {
        Self {
            mode: JudgeMode::Mock,
            model_id: None,
            timeout_seconds: 60,
            fixtures_dir: None,
            batch_size: 5,
            min_confidence_auto_active: 60,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JudgeMode {
    Mock,
    Cli,
}

/// 出域策略（§7.3）：默认 `none`，一切模型调用被阻断。
///
/// `Default` 手写而非派生：`String` 的派生默认值是空串，会让首次启动的
/// 策略变成「非法取值」而不是 `none`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EgressSettings {
    /// `none` | `redacted` | `full`；以字符串存储，使用点校验取值。
    pub policy: String,
    pub approved_model: Option<String>,
}

impl Default for EgressSettings {
    fn default() -> Self {
        Self {
            policy: DEFAULT_EGRESS_POLICY.into(),
            approved_model: None,
        }
    }
}

/// 默认出域策略（§13.1：未经批准，正文不出本机）。
pub const DEFAULT_EGRESS_POLICY: &str = "none";

impl EgressSettings {
    /// 策略是否合法取值（不合法按配置损坏处理，拒绝判读而不是猜）。
    pub fn policy_is_valid(&self) -> bool {
        matches!(self.policy.as_str(), "none" | "redacted" | "full")
    }
}

/// 首次拉取回溯的获批区间（小时）：**1 小时 – 7 天**。无游标时的窗口长度
/// 只能落在此区间内——首次拉取不可无限回扫；该上限同时约束补拉（见
/// `sources::sync`）。改动上限属于授权决策，须随获批范围一起调整。
pub const MIN_FIRST_PULL_LOOKBACK_HOURS: u32 = 1;
pub const MAX_FIRST_PULL_LOOKBACK_HOURS: u32 = 7 * 24;
/// 默认回溯 24 小时。
pub const DEFAULT_FIRST_PULL_LOOKBACK_HOURS: u32 = 24;

/// 同步引擎参数（对应 `infra::sync::SyncConfig`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncSettings {
    /// 自动同步间隔（分钟）；0 = 只手动触发（V0.1 默认，无后台调度）。
    pub interval_minutes: u32,
    pub page_size: u16,
    pub overlap_hours: i64,
    pub max_run_minutes: u64,
    /// 首次拉取（无游标）回溯的小时数。只能经 `runtime_configure` 修改；
    /// 手改配置越界时在使用点钳制回获批区间，不报错也不放宽。
    pub first_pull_lookback_hours: u32,
}

impl SyncSettings {
    /// 钳制到获批区间的回溯小时数（0 视为默认）。
    pub fn first_pull_lookback_hours_bounded(&self) -> u32 {
        if self.first_pull_lookback_hours == 0 {
            DEFAULT_FIRST_PULL_LOOKBACK_HOURS
        } else {
            self.first_pull_lookback_hours
                .clamp(MIN_FIRST_PULL_LOOKBACK_HOURS, MAX_FIRST_PULL_LOOKBACK_HOURS)
        }
    }
}

impl Default for SyncSettings {
    fn default() -> Self {
        Self {
            interval_minutes: 0,
            page_size: 100,
            overlap_hours: 6,
            max_run_minutes: 10,
            first_pull_lookback_hours: DEFAULT_FIRST_PULL_LOOKBACK_HOURS,
        }
    }
}

/// 正文保留期的代码级区间（天）：**1 – 90**。缺省与越界值都落回默认 30
/// （§13.1）——正文不会被无限期保留，也不会因配置为 0 而永不清理。
pub const MIN_RAW_BODY_DAYS: u32 = 1;
pub const MAX_RAW_BODY_DAYS: u32 = 90;
pub const DEFAULT_RAW_BODY_DAYS: u32 = 30;

/// `retention` 段。本模块只消费 `rawBodyDays`（喂给 `SyncConfig::raw_body_days`：
/// 入库时写 `retention_at = 首次入库时点 + rawBodyDays`）；`snapshotDays`、
/// `backupKeep` 等属于别的模块，经 `extra` 原样保留。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RetentionSettings {
    pub raw_body_days: u32,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for RetentionSettings {
    fn default() -> Self {
        Self {
            raw_body_days: DEFAULT_RAW_BODY_DAYS,
            extra: Map::new(),
        }
    }
}

impl RetentionSettings {
    /// 钳制到区间内的正文保留天数（0 视为默认）。
    pub fn raw_body_days_bounded(&self) -> u32 {
        if self.raw_body_days == 0 {
            DEFAULT_RAW_BODY_DAYS
        } else {
            self.raw_body_days.clamp(MIN_RAW_BODY_DAYS, MAX_RAW_BODY_DAYS)
        }
    }
}

/// 单来源的本机授权账本（`source.id` 为键）。
///
/// **config.json 是授权上限**：restore 只替换 SQLite、换不回这份账本。有效授权
/// = 库内 `enabled` ∧ `authorized`；有效白名单 = `scope`；有效读取时间窗 =
/// `[window_start_ms, window_end_ms)`。恢复出的旧库（`enabled=1`、更宽的白名单、
/// 更早的游标）无法放宽本机已做的撤权/收窄决定。
///
/// `profile`（`corpId:userId`，DWS 调研 §3.1）只是账号标识：凭据本体在系统
/// 密钥库，不进本文件与数据库。profile 变更即视为换了账号，来源会被撤权。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SourceCliSettings {
    pub profile: String,
    /// 本机是否授权；缺省 `false`——旧配置/陌生来源一律视为未授权。
    pub authorized: bool,
    /// 本机认可的白名单上限。
    pub scope: Scope,
    /// 获批读取时间窗的起点（UTC ms）：授权时刻 − 当时的首次拉取回溯。
    /// 适配器对每个请求窗口与本窗取交集，因此首次拉取与补拉都不会早于它。
    pub window_start_ms: Option<i64>,
    /// 获批读取时间窗的终点（UTC ms，开区间）：撤权时写入；`None` = 开放上界。
    pub window_end_ms: Option<i64>,
}

impl SourceCliSettings {
    /// 读取时间窗是否完整可用：授权必须带有起点，且终点晚于起点。
    pub fn approved_window(&self) -> Option<(i64, Option<i64>)> {
        let start_ms = self.window_start_ms?;
        if self.window_end_ms.is_some_and(|end_ms| end_ms <= start_ms) {
            return None;
        }
        Some((start_ms, self.window_end_ms))
    }
}

/// 读取配置；文件不存在返回默认值（首次启动）。
pub fn load(path: &Path) -> Result<SettingsFile, ConfigError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(SettingsFile::default()),
        Err(e) => {
            return Err(ConfigError::Io {
                path: path.display().to_string(),
                source: e,
            })
        }
    };
    let cfg: SettingsFile = serde_json::from_str(&raw).map_err(|e| ConfigError::Parse {
        path: path.display().to_string(),
        source: e,
    })?;
    if cfg.schema > CONFIG_SCHEMA {
        return Err(ConfigError::FutureSchema(cfg.schema));
    }
    Ok(cfg)
}

/// 原子写回：同目录临时文件 + rename。
pub fn save(path: &Path, cfg: &SettingsFile) -> Result<(), ConfigError> {
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_string_pretty(cfg).map_err(|e| ConfigError::Parse {
        path: path.display().to_string(),
        source: e,
    })?;
    std::fs::write(&tmp, body).map_err(|e| ConfigError::Io {
        path: tmp.display().to_string(),
        source: e,
    })?;
    std::fs::rename(&tmp, path).map_err(|e| ConfigError::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_design() {
        let cfg = SettingsFile::default();
        assert_eq!(cfg.schema, 1);
        assert_eq!(cfg.egress.policy, "none");
        assert_eq!(cfg.judge.mode, JudgeMode::Mock);
        assert_eq!(cfg.judge.timeout_seconds, 60);
        assert_eq!(cfg.sync.overlap_hours, 6);
        assert_eq!(
            cfg.sync.first_pull_lookback_hours,
            DEFAULT_FIRST_PULL_LOOKBACK_HOURS
        );
        assert_eq!(cfg.retention.raw_body_days, DEFAULT_RAW_BODY_DAYS);
        assert!(cfg.sources.is_empty());
    }

    #[test]
    fn retention_and_lookback_are_bounded_and_keep_foreign_keys() {
        let mut cfg: SettingsFile = serde_json::from_str(
            r#"{"schema":1,"retention":{"rawBodyDays":400,"snapshotDays":365,"backupKeep":7},
                "sync":{"firstPullLookbackHours":0}}"#,
        )
        .unwrap();
        assert_eq!(cfg.retention.raw_body_days_bounded(), MAX_RAW_BODY_DAYS);
        assert_eq!(
            cfg.sync.first_pull_lookback_hours_bounded(),
            DEFAULT_FIRST_PULL_LOOKBACK_HOURS
        );
        cfg.retention.raw_body_days = 7;
        assert_eq!(cfg.retention.raw_body_days_bounded(), 7);
        let text = serde_json::to_string(&cfg).unwrap();
        assert!(text.contains("\"snapshotDays\":365"), "{text}");
        assert!(text.contains("\"backupKeep\":7"), "{text}");
        // 正文保留 0 天视为默认，而不是「永不保留/永不清理」之外的任意值。
        let cfg: SettingsFile = serde_json::from_str(r#"{"schema":1}"#).unwrap();
        assert_eq!(cfg.retention.raw_body_days_bounded(), DEFAULT_RAW_BODY_DAYS);
    }

    #[test]
    fn egress_policy_is_none_whenever_it_is_not_explicitly_set() {
        // 缺字段、空对象、整段缺失：策略都必须回落到 none，而不是空串/非法值。
        for text in [
            r#"{"schema":1}"#,
            r#"{"schema":1,"egress":{}}"#,
            r#"{"schema":1,"egress":{"approvedModel":"vendor/m"}}"#,
        ] {
            let cfg: SettingsFile = serde_json::from_str(text).unwrap();
            assert_eq!(cfg.egress.policy, "none", "{text}");
            assert!(cfg.egress.policy_is_valid(), "{text}");
        }
        // 旧版来源条目（没有 authorized/scope 字段）一律视为未授权。
        let cfg: SettingsFile = serde_json::from_str(
            r#"{"schema":1,"sources":{"s1":{"profile":"corp1:user1"}}}"#,
        )
        .unwrap();
        assert!(!cfg.sources["s1"].authorized);
        assert!(cfg.sources["s1"].scope.conversations.is_empty());
    }

    #[test]
    fn unknown_keys_survive_roundtrip() {
        let dir = std::env::temp_dir().join(format!("wc-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"schema":1,"retention":{"rawBodyDays":30},"egress":{"policy":"none"}}"#,
        )
        .unwrap();
        let mut cfg = load(&path).unwrap();
        assert_eq!(cfg.egress.policy, "none");
        cfg.me.user_id = "u1".into();
        save(&path, &cfg).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"rawBodyDays\": 30"), "未知键被丢弃");
        assert!(text.contains("\"userId\": \"u1\""));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_is_default() {
        let cfg = load(Path::new("/definitely/not/here/config.json")).unwrap();
        assert_eq!(cfg.egress.policy, "none");
    }
}
