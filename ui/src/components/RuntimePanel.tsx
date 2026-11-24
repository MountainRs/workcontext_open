/**
 * 设置页「本人身份与判读运行时」：`runtime_config_get` / `runtime_configure`
 * 的唯一界面入口（后端见 `crates/desktop/src/sources/runtime.rs`）。
 *
 * 纪律：
 * - 只提交五个允许键（`meUserId` / `meDisplayName` / `judgeMode` / `modelId` /
 *   `firstPullLookbackHours`），且只发送与当前值不同的键；可执行路径、参数等
 *   一律不构造（后端也会整体拒绝）；
 * - 换模型会关闭出域批准：保存后以返回值提示，须重新逐字确认出域；
 * - 首次拉取回溯为 1–168 小时，在**下一次授权**时生效，不可无限回扫；
 * - 只写本地配置，不登录、不探测账号、不读取任何企业数据。
 */

import { useEffect, useMemo, useState } from "react";
import { IpcErrorNotice, LoadingNotice } from "./IpcNotice";
import { formatDateTime } from "../lib/format";
import {
  hasDesktopBridge,
  type JudgeMode,
  type RuntimeConfigureArgs,
} from "../lib/ipc";
import { useConfigureRuntime, useRuntimeConfig } from "../lib/queries";

/** 稳定 ID：可见 ASCII、无空白、不以 `-` 开头、≤128（与后端 `is_id_token` 同形）。 */
function isIdToken(value: string): boolean {
  return (
    value.length > 0 &&
    value.length <= 128 &&
    !value.startsWith("-") &&
    /^[\x21-\x7e]+$/.test(value)
  );
}

/** 精确 `provider/model` id：可见 ASCII、无空白、以字母数字开头、含 `/`、≤128。 */
function isModelId(value: string): boolean {
  return (
    value.length > 0 &&
    value.length <= 128 &&
    /^[A-Za-z0-9]/.test(value) &&
    value.includes("/") &&
    /^[\x21-\x7e]+$/.test(value)
  );
}

const HOUR_MS = 3_600_000;

export function RuntimePanel() {
  const bridge = hasDesktopBridge();
  const runtime = useRuntimeConfig();
  const configure = useConfigureRuntime();

  const [userId, setUserId] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [mode, setMode] = useState<JudgeMode>("mock");
  const [modelId, setModelId] = useState("");
  const [hours, setHours] = useState("24");
  const [feedback, setFeedback] = useState<string | null>(null);

  const current = runtime.data;

  useEffect(() => {
    if (!current) return;
    setUserId(current.me.userId);
    setDisplayName(current.me.displayName ?? "");
    setMode(current.judge.mode);
    setModelId(current.judge.modelId ?? "");
    setHours(String(current.firstPull.lookbackHours));
  }, [current]);

  // 以「现在」作为授权时刻的窗口预览；仅展示，不触发任何授权。
  const windowStart = useMemo(
    () => (current ? Date.now() - current.firstPull.lookbackHours * HOUR_MS : null),
    [current],
  );

  const nextUserId = userId.trim();
  const nextName = displayName.trim();
  const nextModel = modelId.trim();
  const hoursNumber = Number(hours);

  const idChanged = current ? nextUserId !== current.me.userId : false;
  const nameChanged = current ? nextName !== (current.me.displayName ?? "") : false;
  const modeChanged = current ? mode !== current.judge.mode : false;
  const modelChanged = current ? nextModel !== (current.judge.modelId ?? "") : false;
  const hoursChanged = current ? hoursNumber !== current.firstPull.lookbackHours : false;

  const hoursValid =
    current !== undefined &&
    Number.isInteger(hoursNumber) &&
    hoursNumber >= current.firstPull.minHours &&
    hoursNumber <= current.firstPull.maxHours;

  const idError =
    idChanged && !isIdToken(nextUserId)
      ? "本人 ID 须为稳定 ID：可见 ASCII、无空白、不以 - 开头，不超过 128 字符。"
      : null;
  const modelError =
    modelChanged && nextModel === ""
      ? "已配置的模型不能清空；如需停止 cli 判读请切换为 mock（模型可保留）。"
      : modelChanged && !isModelId(nextModel)
        ? "模型须为精确的 provider/model id：可见 ASCII、无空白、不以 - 开头。"
        : null;
  const hoursError =
    hoursChanged && !hoursValid && current
      ? `首次拉取回溯须为 ${current.firstPull.minHours}–${current.firstPull.maxHours} 的整数小时。`
      : null;
  const modeError =
    mode === "cli" && nextModel === ""
      ? "cli 判读需要先填写精确模型 ID（不会回落到 mock）。"
      : null;

  const patch = useMemo(() => {
    const next: RuntimeConfigureArgs = {};
    if (idChanged) next.meUserId = nextUserId;
    if (nameChanged) next.meDisplayName = nextName;
    if (modeChanged) next.judgeMode = mode;
    if (modelChanged) next.modelId = nextModel;
    if (hoursChanged) next.firstPullLookbackHours = hoursNumber;
    return next;
  }, [
    idChanged,
    nextUserId,
    nameChanged,
    nextName,
    modeChanged,
    mode,
    modelChanged,
    nextModel,
    hoursChanged,
    hoursNumber,
  ]);

  const dirty = Object.keys(patch).length > 0;
  const blocked = Boolean(idError || modelError || hoursError || modeError);
  const canSave = bridge && current !== undefined && dirty && !blocked && !configure.isPending;

  const save = () => {
    if (!canSave || !current) return;
    const before = current.egress;
    setFeedback(null);
    configure.mutate(patch, {
      onSuccess: (view) => {
        const revoked = before.policy !== "none" && view.egress.policy === "none";
        setFeedback(
          `已保存运行时配置：本人 ${view.me.userId}` +
            `${view.me.displayName ? `（${view.me.displayName}）` : ""}；` +
            `判读 ${view.judge.mode}` +
            `${view.judge.modelId ? ` · ${view.judge.modelId}` : "（未配置模型）"}；` +
            `首次拉取回溯 ${view.firstPull.lookbackHours} 小时（下次授权时生效）。` +
            (revoked
              ? " 出域批准已随模型变更关闭（策略回到 none），须重新逐字确认。"
              : ""),
        );
      },
    });
  };

  return (
    <section className="settings-section" aria-labelledby="settings-runtime">
      <h2 id="settings-runtime" className="panel-title">
        本人身份与判读运行时
      </h2>
      <p className="panel-note">
        这是本人 ID、显示名、判读模式与精确模型、首次拉取回溯的唯一入口。
        只写本地配置：不登录企业账号、不探测本机账号、不读取任何企业数据。
        可执行文件由桌面端按固定程序名解析，这里不能也不接受路径或参数。
      </p>

      {!bridge ? (
        <p className="panel-note">未连接桌面端，无法读取运行时配置。</p>
      ) : runtime.isPending ? (
        <LoadingNotice label="正在读取运行时配置…" />
      ) : runtime.isError ? (
        <IpcErrorNotice
          title="运行时配置读取失败"
          error={runtime.error}
          onRetry={() => void runtime.refetch()}
        />
      ) : current ? (
        <>
          <dl className="detail-facts compact">
            <div>
              <dt>本人 ID</dt>
              <dd>{current.me.userId || "未配置（判读会被拒绝）"}</dd>
            </div>
            <div>
              <dt>显示名</dt>
              <dd>{current.me.displayName ?? "未配置"}</dd>
            </div>
            <div>
              <dt>判读模式</dt>
              <dd>{current.judge.mode}</dd>
            </div>
            <div>
              <dt>精确模型</dt>
              <dd>{current.judge.modelId ?? "未配置"}</dd>
            </div>
            <div>
              <dt>出域策略</dt>
              <dd>
                {current.egress.policy}
                {current.egress.approvedModel
                  ? ` · 已批准 ${current.egress.approvedModel}`
                  : " · 未批准模型"}
              </dd>
            </div>
            <div>
              <dt>固定可执行程序</dt>
              <dd>{current.trustedExecutables.join(" / ") || "未提供"}</dd>
            </div>
          </dl>

          <div className="field-row">
            <label className="field grow">
              <span className="field-label">本人 ID（meUserId，稳定 ID）</span>
              <input
                className="field-input"
                value={userId}
                onChange={(event) => {
                  setUserId(event.target.value);
                  setFeedback(null);
                }}
                placeholder="例如 me01"
                aria-label="本人 ID"
              />
            </label>
            <label className="field grow">
              <span className="field-label">显示名（可选，留空即清空）</span>
              <input
                className="field-input"
                value={displayName}
                onChange={(event) => {
                  setDisplayName(event.target.value);
                  setFeedback(null);
                }}
                placeholder="不超过 60 字"
                aria-label="显示名"
              />
            </label>
          </div>

          <div className="field-row">
            <label className="field grow">
              <span className="field-label">判读模式</span>
              <select
                className="field-input"
                value={mode}
                onChange={(event) => {
                  setMode(event.target.value as JudgeMode);
                  setFeedback(null);
                }}
                aria-label="判读模式"
              >
                <option value="mock">mock · 不出域，用固定样本</option>
                <option value="cli">cli · 本机固定程序 + 精确模型</option>
              </select>
            </label>
            <label className="field grow">
              <span className="field-label">
                精确模型 ID（provider/model；cli 必填）
              </span>
              <input
                className="field-input"
                value={modelId}
                onChange={(event) => {
                  setModelId(event.target.value);
                  setFeedback(null);
                }}
                placeholder="provider/model"
                aria-label="精确模型 ID"
              />
            </label>
            <label className="field">
              <span className="field-label">
                首次拉取回溯（小时，{current.firstPull.minHours}–
                {current.firstPull.maxHours}）
              </span>
              <input
                className="field-input tnum"
                type="number"
                inputMode="numeric"
                min={current.firstPull.minHours}
                max={current.firstPull.maxHours}
                step={1}
                value={hours}
                onChange={(event) => {
                  setHours(event.target.value);
                  setFeedback(null);
                }}
                aria-label="首次拉取回溯"
              />
            </label>
          </div>

          <p className="panel-note">
            首次拉取窗口（下次授权时生效）：回溯 {current.firstPull.lookbackHours} 小时，
            若现在授权则起点为 {formatDateTime(windowStart)}（= 授权时刻 − 回溯），
            结束边界开放（未闭合，随每次同步推进到当前时刻），不可无限回扫。
          </p>
          <p className="panel-note">
            换模型会关闭已绑定的出域批准：策略回到 none、已批准模型清空，
            须重新在「判读与出域策略」逐字确认。已落库记录与游标不受影响。
          </p>

          {idError || modelError || hoursError || modeError ? (
            <div className="inline-feedback error" role="status" aria-live="polite">
              {idError ?? modelError ?? hoursError ?? modeError}
            </div>
          ) : null}

          <div className="settings-actions">
            <button
              type="button"
              className="btn btn-primary"
              disabled={!canSave}
              title={
                !bridge
                  ? "需要桌面端连接"
                  : !dirty
                    ? "没有变更"
                    : blocked
                      ? "先修正校验错误"
                      : undefined
              }
              onClick={save}
            >
              {configure.isPending ? "正在保存…" : "保存运行时配置"}
            </button>
          </div>
          {feedback ? (
            <div className="inline-feedback ok" role="status" aria-live="polite">
              {feedback}
            </div>
          ) : null}
          {configure.isError ? (
            <IpcErrorNotice title="运行时配置未保存" error={configure.error} />
          ) : null}
        </>
      ) : null}
    </section>
  );
}
