//! 单一受控入口（设置页「运行时」）：本人身份、判读模式与精确模型、
//! 首次拉取窗口。
//!
//! 这是这些配置**唯一**的公开修改入口（`config.json` 可手改，但越界值会在
//! 使用点被钳制或拒绝）。约束：
//!
//! - **只接受五个字段**（`deny_unknown_fields`）：`meUserId`、`meDisplayName`、
//!   `judgeMode`（`mock`/`cli`）、`modelId`、`firstPullLookbackHours`。
//!   可执行路径、参数、白名单、出域策略等任何别的键一律拒绝——CLI 只按固定
//!   程序名解析（[`super::trusted_exe`]），白名单走 `source_register`，出域
//!   走带确认词的 `egress_set`。
//! - **模型变更即关闭出域批准**：出域批准绑定的是具体模型，换了模型旧批准
//!   不再成立——策略回到 `none`、已批准模型清空，须重新走 `egress_set`。
//! - **首次拉取窗口有界**：回溯小时数必须在获批区间内（默认 24 小时，
//!   1 小时 – 7 天），不可无限回扫。
//! - 每次实际变更写一条 `work_event(actor=human)`；无变更不落盘、不审计。

use serde::{Deserialize, Serialize};

use super::config::{
    self, JudgeMode, SettingsFile, DEFAULT_EGRESS_POLICY, MAX_FIRST_PULL_LOOKBACK_HOURS,
    MIN_FIRST_PULL_LOOKBACK_HOURS,
};
use super::{
    is_id_token, now_ms, random_id, with_conn, AppError, EgressView, SourceState, DWS_EXE,
    OMP_EXE,
};

/// `runtime_configure` 的入参（键为 camelCase；缺省/`null` = 不改）。
///
/// `deny_unknown_fields`：出现任何别的键（如 `cliPath`、`args`、`exe`）整体拒绝。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeConfigureArgs {
    pub me_user_id: Option<String>,
    pub me_display_name: Option<String>,
    pub judge_mode: Option<JudgeMode>,
    /// 精确 `provider/model` id。
    pub model_id: Option<String>,
    pub first_pull_lookback_hours: Option<u32>,
}

/// `runtime_config_get` / `runtime_configure` 的返回视图（只读事实，不含任何路径）。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeConfigView {
    pub me: MeView,
    pub judge: JudgeView,
    pub egress: EgressView,
    pub first_pull: FirstPullView,
    /// 允许执行的 CLI 固定程序名（调用方无法替换）。
    pub trusted_executables: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MeView {
    /// 为空表示尚未配置：判读会被拒绝（归属锚点不臆断）。
    pub user_id: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JudgeView {
    pub mode: String,
    pub model_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FirstPullView {
    pub lookback_hours: u32,
    pub min_hours: u32,
    pub max_hours: u32,
}

fn mode_str(mode: JudgeMode) -> &'static str {
    match mode {
        JudgeMode::Mock => "mock",
        JudgeMode::Cli => "cli",
    }
}

fn runtime_config_view(cfg: &SettingsFile) -> RuntimeConfigView {
    RuntimeConfigView {
        me: MeView {
            user_id: cfg.me.user_id.clone(),
            display_name: cfg.me.display_name.clone(),
        },
        judge: JudgeView {
            mode: mode_str(cfg.judge.mode).to_string(),
            model_id: cfg.judge.model_id.clone(),
        },
        egress: EgressView {
            policy: cfg.egress.policy.clone(),
            approved_model: cfg.egress.approved_model.clone(),
            judge_mode: mode_str(cfg.judge.mode).to_string(),
        },
        first_pull: FirstPullView {
            lookback_hours: cfg.sync.first_pull_lookback_hours_bounded(),
            min_hours: MIN_FIRST_PULL_LOOKBACK_HOURS,
            max_hours: MAX_FIRST_PULL_LOOKBACK_HOURS,
        },
        trusted_executables: vec![DWS_EXE, OMP_EXE],
    }
}

/// 精确模型 id 的形状：可见 ASCII、无空白、以字母数字开头、含 `/`、≤128 字节。
/// 它会作为 `--model` 的参数值下发给 OMP，因此不可以 `-` 开头。
fn is_model_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && s.contains('/')
        && s.bytes().all(|b| (0x21..=0x7E).contains(&b))
}

fn invalid(message: impl Into<String>, detail: impl std::fmt::Display) -> AppError {
    AppError {
        code: "INVALID_ARGUMENT".into(),
        message: message.into(),
        detail: Some(detail.to_string()),
        retryable: false,
        hint: None,
    }
}

/// 读取当前运行时配置（只读，不触库）。
#[tauri::command]
pub fn runtime_config_get(
    state: tauri::State<'_, SourceState>,
) -> Result<RuntimeConfigView, AppError> {
    current_runtime_config(&state)
}

fn current_runtime_config(state: &SourceState) -> Result<RuntimeConfigView, AppError> {
    let cfg = super::load_settings(state)?;
    Ok(runtime_config_view(&cfg))
}

/// 修改运行时配置（唯一受控入口）。
///
/// - 入参是一个对象，只允许 [`RuntimeConfigureArgs`] 的五个键，其余键整体拒绝；
/// - 模型 id 变化时出域批准随即关闭（策略回 `none`、已批准模型清空）并审计；
/// - `judgeMode = "cli"` 要求（本次或此前）已配置 `modelId`；
/// - 实际有变更才写盘并记一条 `work_event(actor=human)`。
#[tauri::command]
pub fn runtime_configure(
    state: tauri::State<'_, SourceState>,
    args: serde_json::Value,
) -> Result<RuntimeConfigView, AppError> {
    configure(&state, args)
}

fn configure(state: &SourceState, args: serde_json::Value) -> Result<RuntimeConfigView, AppError> {
    let args: RuntimeConfigureArgs = serde_json::from_value(args).map_err(|err| {
        invalid(
            "runtime_configure 只接受 meUserId / meDisplayName / judgeMode / modelId / firstPullLookbackHours，不接受可执行路径或参数。",
            err,
        )
    })?;

    let _guard = state.config_lock.lock().unwrap_or_else(|p| p.into_inner());
    let mut cfg = super::load_settings(state)?;
    let mut notes: Vec<String> = Vec::new();
    let mut model_changed = false;
    let previous_mode = cfg.judge.mode;

    if let Some(user_id) = args.me_user_id {
        let user_id = user_id.trim().to_string();
        if !is_id_token(&user_id) {
            return Err(invalid(
                "本人 ID 必须是稳定 ID：可见 ASCII、无空白、不以 - 开头，且不超过 128 字符。",
                user_id,
            ));
        }
        if cfg.me.user_id != user_id {
            cfg.me.user_id = user_id;
            notes.push("本人 ID 已更新".to_string());
        }
    }
    if let Some(display_name) = args.me_display_name {
        let display_name = display_name.trim();
        if display_name.chars().count() > 60 {
            return Err(invalid("显示名不超过 60 字。", display_name.chars().count()));
        }
        let next = (!display_name.is_empty()).then(|| display_name.to_string());
        if cfg.me.display_name != next {
            cfg.me.display_name = next;
            notes.push("显示名已更新".to_string());
        }
    }
    if let Some(mode) = args.judge_mode {
        if cfg.judge.mode != mode {
            notes.push(format!("判读模式 {} → {}", mode_str(cfg.judge.mode), mode_str(mode)));
            cfg.judge.mode = mode;
        }
    }
    if let Some(model_id) = args.model_id {
        let model_id = model_id.trim().to_string();
        if !is_model_id(&model_id) {
            return Err(invalid(
                "模型必须是精确的 provider/model id：可见 ASCII、无空白、不以 - 开头。",
                model_id,
            ));
        }
        if cfg.judge.model_id.as_deref() != Some(model_id.as_str()) {
            notes.push(format!(
                "判读模型 {} → {model_id}",
                cfg.judge.model_id.as_deref().unwrap_or("（未配置）")
            ));
            cfg.judge.model_id = Some(model_id);
            model_changed = true;
        }
    }
    // cli 模式没有精确模型就无法构造 runtime：拒绝，而不是回落到别的模型。
    if matches!(cfg.judge.mode, JudgeMode::Cli) && cfg.judge.model_id.is_none() {
        return Err(invalid(
            "cli 判读需要先配置精确模型（modelId）。",
            "judgeMode=cli 且 modelId 为空",
        ));
    }
    if let Some(hours) = args.first_pull_lookback_hours {
        if !(MIN_FIRST_PULL_LOOKBACK_HOURS..=MAX_FIRST_PULL_LOOKBACK_HOURS).contains(&hours) {
            return Err(invalid(
                format!(
                    "首次拉取回溯须在 {MIN_FIRST_PULL_LOOKBACK_HOURS}–{MAX_FIRST_PULL_LOOKBACK_HOURS} 小时内，不可无限回扫。"
                ),
                hours,
            ));
        }
        if cfg.sync.first_pull_lookback_hours != hours {
            notes.push(format!("首次拉取回溯 → {hours} 小时"));
            cfg.sync.first_pull_lookback_hours = hours;
        }
    }
    if model_changed
        && (cfg.egress.policy != DEFAULT_EGRESS_POLICY || cfg.egress.approved_model.is_some())
    {
        // 出域批准绑定具体模型：换了模型，旧批准不再成立。
        cfg.egress.policy = DEFAULT_EGRESS_POLICY.into();
        cfg.egress.approved_model = None;
        notes.push("出域批准已随模型变更关闭（策略回到 none），须重新确认".to_string());
    }

    if notes.is_empty() {
        return Ok(runtime_config_view(&cfg));
    }
    config::save(&state.config_path, &cfg)?;
    if model_changed || previous_mode != cfg.judge.mode { state.invalidate_organizer(); }

    // 审计：全局运行时决策，actor=human 必须留痕。
    with_conn(state, |conn| {
        let now = now_ms();
        let event_id = random_id(conn)?;
        conn.execute(
            "INSERT INTO work_event(id, target_type, target_id, event_type, actor, at, note)
             VALUES(?1, 'source', 'runtime', 'updated', 'human', ?2, ?3)",
            workcontext_store::rusqlite::params![event_id, now, notes.join("；")],
        )
        .map_err(AppError::from_db)?;
        Ok(runtime_config_view(&cfg))
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::config::{DEFAULT_FIRST_PULL_LOOKBACK_HOURS, DEFAULT_RAW_BODY_DAYS};
    use super::super::tests::Fixture;
    use super::*;

    /// `runtime` 目标上的 `work_event(actor=human)` 条数。
    fn runtime_events(fx: &Fixture) -> i64 {
        fx.with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM work_event
                 WHERE target_type = 'source' AND target_id = 'runtime' AND actor = 'human'",
                [],
                |row| row.get(0),
            )
            .expect("计数")
        })
    }

    fn latest_note(fx: &Fixture) -> String {
        fx.with_conn(|conn| {
            conn.query_row(
                "SELECT note FROM work_event WHERE target_id = 'runtime'
                 ORDER BY at DESC, rowid DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("审计事件")
        })
    }

    #[test]
    fn defaults_are_safe() {
        let fx = Fixture::new();
        let view = current_runtime_config(&fx.state).expect("读取");
        assert_eq!(view.me.user_id, "");
        assert_eq!(view.me.display_name, None);
        assert_eq!(view.judge.mode, "mock");
        assert_eq!(view.judge.model_id, None);
        assert_eq!(view.egress.policy, "none");
        assert_eq!(view.egress.approved_model, None);
        assert_eq!(
            view.first_pull,
            FirstPullView {
                lookback_hours: DEFAULT_FIRST_PULL_LOOKBACK_HOURS,
                min_hours: MIN_FIRST_PULL_LOOKBACK_HOURS,
                max_hours: MAX_FIRST_PULL_LOOKBACK_HOURS,
            }
        );
        assert_eq!(view.trusted_executables, vec![DWS_EXE, OMP_EXE]);
    }

    #[test]
    fn configures_identity_model_mode_and_window() {
        let fx = Fixture::new();
        let view = configure(
            &fx.state,
            json!({
                "meUserId": "me01",
                "meDisplayName": "  我  ",
                "judgeMode": "cli",
                "modelId": "vendor/model-x",
                "firstPullLookbackHours": 48,
            }),
        )
        .expect("配置");
        assert_eq!(view.me.user_id, "me01");
        assert_eq!(view.me.display_name.as_deref(), Some("我"));
        assert_eq!(view.judge.mode, "cli");
        assert_eq!(view.judge.model_id.as_deref(), Some("vendor/model-x"));
        assert_eq!(view.first_pull.lookback_hours, 48);
        assert_eq!(runtime_events(&fx), 1);
        // 判读的前置条件随之满足：有本人身份，cli 模式有精确模型。
        assert!(super::super::judge::validate_preconditions(&fx.config()).is_ok());
        // 空串显示名 = 清空。
        let view = configure(&fx.state, json!({"meDisplayName": ""})).expect("清空显示名");
        assert_eq!(view.me.display_name, None);
    }

    #[test]
    fn a_no_op_configure_writes_nothing() {
        let fx = Fixture::new();
        let view = configure(&fx.state, json!({"firstPullLookbackHours": 24})).expect("无变更");
        assert_eq!(view.first_pull.lookback_hours, 24);
        assert!(!fx.state.config_path.exists(), "无变更不落盘");
        assert_eq!(runtime_events(&fx), 0, "无变更不审计");
    }

    #[test]
    fn changing_the_model_closes_egress_approval_and_is_audited() {
        let fx = Fixture::new();
        configure(
            &fx.state,
            json!({"judgeMode": "cli", "modelId": "vendor/model-a"}),
        )
        .expect("配置模型");
        // 用户另行批准了出域（带确认词）。
        super::super::set_egress(
            &fx.state,
            "full".into(),
            Some("vendor/model-a".into()),
            "full".into(),
        )
        .expect("批准出域");
        assert_eq!(fx.config().egress.policy, "full");
        let before = fx.state.organizer_generation.load(std::sync::atomic::Ordering::Acquire);

        // 只改显示名：与模型无关，出域批准保留。
        configure(&fx.state, json!({"meDisplayName": "我"})).expect("改显示名");
        assert_eq!(fx.config().egress.policy, "full");
        assert_eq!(fx.state.organizer_generation.load(std::sync::atomic::Ordering::Acquire),before);

        // 换模型：旧批准绑定的是旧模型，随即关闭。
        let view = configure(&fx.state, json!({"modelId": "vendor/model-b"})).expect("换模型");
        assert_ne!(fx.state.organizer_generation.load(std::sync::atomic::Ordering::Acquire),before);
        assert_eq!(view.judge.model_id.as_deref(), Some("vendor/model-b"));
        assert_eq!(view.egress.policy, "none");
        assert_eq!(view.egress.approved_model, None);
        let note = latest_note(&fx);
        assert!(note.contains("vendor/model-a → vendor/model-b"), "{note}");
        assert!(note.contains("出域批准已随模型变更关闭"), "{note}");

        // 重复提交同一模型不算变更。
        let events = runtime_events(&fx);
        configure(&fx.state, json!({"modelId": "vendor/model-b"})).expect("重复提交");
        assert_eq!(runtime_events(&fx), events);
    }

    #[test]
    fn rejects_every_key_outside_the_five_allowed_ones() {
        let fx = Fixture::new();
        for bad in [
            json!({"cliPath": "/tmp/not-omp"}),
            json!({"args": ["--anything"]}),
            json!({"exe": "sh"}),
            json!({"ompPath": "~/.local/bin/omp"}),
            json!({"policy": "full"}),
            json!({"scope": {"conversations": ["cidX"]}}),
            json!({"authorized": true}),
        ] {
            let err = configure(&fx.state, bad.clone())
                .err()
                .unwrap_or_else(|| panic!("应拒绝：{bad}"));
            assert_eq!(err.code, "INVALID_ARGUMENT", "{bad}");
            assert!(err.detail.is_some(), "诊断信息应保留被拒绝的原因");
        }
        assert!(fx.config().me.user_id.is_empty());
        assert_eq!(fx.config().egress.policy, "none");
        assert_eq!(runtime_events(&fx), 0);
    }

    #[test]
    fn validates_each_allowed_value() {
        let fx = Fixture::new();
        for bad in [
            json!({"meUserId": ""}),
            json!({"meUserId": "-x"}),
            json!({"meUserId": "has space"}),
            json!({"meUserId": "张三"}),
        ] {
            assert_eq!(
                configure(&fx.state, bad.clone()).expect_err("本人 ID").code,
                "INVALID_ARGUMENT",
                "{bad}"
            );
        }
        for bad in [
            json!({"modelId": ""}),
            json!({"modelId": "no-provider-slash"}),
            json!({"modelId": "-rm/rfcp"}),
            json!({"modelId": "vendor/model x"}),
            json!({"judgeMode": "http"}),
        ] {
            assert_eq!(
                configure(&fx.state, bad.clone()).expect_err("模型/模式").code,
                "INVALID_ARGUMENT",
                "{bad}"
            );
        }
        // cli 模式必须有精确模型；同一次调用里给出即可。
        assert_eq!(
            configure(&fx.state, json!({"judgeMode": "cli"}))
                .expect_err("cli 缺模型")
                .code,
            "INVALID_ARGUMENT"
        );
        assert!(configure(
            &fx.state,
            json!({"judgeMode": "cli", "modelId": "vendor/model-x"})
        )
        .is_ok());
        assert_eq!(fx.config().judge.model_id.as_deref(), Some("vendor/model-x"));
    }

    #[test]
    fn first_pull_lookback_is_confined_to_the_approved_range() {
        let fx = Fixture::new();
        for hours in [0, MAX_FIRST_PULL_LOOKBACK_HOURS + 1, 100_000] {
            assert_eq!(
                configure(&fx.state, json!({"firstPullLookbackHours": hours}))
                    .expect_err("回溯越界")
                    .code,
                "INVALID_ARGUMENT"
            );
        }
        assert_eq!(
            configure(&fx.state, json!({"firstPullLookbackHours": MIN_FIRST_PULL_LOOKBACK_HOURS}))
                .expect("下界")
                .first_pull
                .lookback_hours,
            MIN_FIRST_PULL_LOOKBACK_HOURS
        );
        assert_eq!(
            configure(&fx.state, json!({"firstPullLookbackHours": MAX_FIRST_PULL_LOOKBACK_HOURS}))
                .expect("上界")
                .first_pull
                .lookback_hours,
            MAX_FIRST_PULL_LOOKBACK_HOURS
        );
    }

    #[test]
    fn hand_edited_out_of_range_values_are_clamped_on_read() {
        let fx = Fixture::new();
        std::fs::write(
            &fx.state.config_path,
            r#"{"schema":1,
                "sync":{"firstPullLookbackHours":100000},
                "retention":{"rawBodyDays":0,"snapshotDays":365}}"#,
        )
        .expect("写配置");
        let view = current_runtime_config(&fx.state).expect("读取");
        assert_eq!(view.first_pull.lookback_hours, MAX_FIRST_PULL_LOOKBACK_HOURS);
        let cfg = fx.config();
        assert_eq!(cfg.sync.first_pull_lookback_hours_bounded(), MAX_FIRST_PULL_LOOKBACK_HOURS);
        assert_eq!(cfg.retention.raw_body_days_bounded(), DEFAULT_RAW_BODY_DAYS);
        // retention 段里属于别的模块的键不被丢弃。
        assert_eq!(cfg.retention.extra.get("snapshotDays"), Some(&json!(365)));
    }
}
