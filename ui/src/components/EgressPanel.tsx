import { JobLog } from "./JobLog";
import { useJobStore } from "../lib/jobs";
/**
 * 设置页「判读与出域」：默认 `none`，切换需要逐字确认；模型必须与
 * `judge.modelId` 一致（后端二次校验）。判读只在用户点击后执行。
 */

import { useEffect, useState } from "react";
import { IpcErrorNotice, LoadingNotice } from "./IpcNotice";
import { hasDesktopBridge } from "../lib/ipc";
import type { EgressPolicy } from "../lib/ipc";
import {
  useEgress,
  useJudgeNow,
  useJudgeRetry,
  useSetEgress,
} from "../lib/queries";

const POLICY_LABEL: Record<EgressPolicy, string> = {
  none: "none · 不出本机（默认）",
  redacted: "redacted · 脱敏后出域",
  full: "full · 完整正文出域",
};

const POLICY_RISK: Record<EgressPolicy, string | null> = {
  none: null,
  redacted:
    "脱敏后仍可能含业务信息：正文将离开本机，发送到「已批准模型」。仅在明确批准后使用。",
  full: "完整正文将离开本机，发送到「已批准模型」。这是最高风险级别，仅在明确批准后使用。",
};

/**
 * 判读模型候选：常见 provider/model id 形态的示例，可按需调整。
 * 出现在候选里不代表已批准——后端要求 approvedModel 与 judge.modelId
 * 完全一致，且真实 runtime 会再校验一次；不一致直接拒绝。
 */
const JUDGE_MODEL_CANDIDATES = [
  "workbuddy/deepseek-v4.1-flash",
];

const CUSTOM_MODEL = "__custom__";

export function EgressPanel() {
  const bridge = hasDesktopBridge();
  const egress = useEgress();
  const setEgress = useSetEgress();
  const jobs = useJobStore(state => state.jobs);
  const judgeNow = useJudgeNow();
  const judgeRetry = useJudgeRetry();

  const [target, setTarget] = useState<EgressPolicy>("none");
  const [modelChoice, setModelChoice] = useState("");
  const [customModel, setCustomModel] = useState("");
  const [token, setToken] = useState("");
  const [feedback, setFeedback] = useState<string | null>(null);

  const current = egress.data;

  useEffect(() => {
    if (!egress.data) return;
    setTarget(egress.data.policy);
    const approved = egress.data.approvedModel ?? "";
    if (!approved) {
      setModelChoice("");
      setCustomModel("");
    } else if (JUDGE_MODEL_CANDIDATES.includes(approved)) {
      setModelChoice(approved);
      setCustomModel("");
    } else {
      setModelChoice(CUSTOM_MODEL);
      setCustomModel(approved);
    }
    setToken("");
  }, [egress.data]);

  const model = modelChoice === CUSTOM_MODEL ? customModel.trim() : modelChoice;
  const dirty = current ? target !== current.policy : false;
  const canSave =
    bridge &&
    current !== undefined &&
    dirty &&
    token === target &&
    (target === "none" || model.length > 0) &&
    !setEgress.isPending;

  const save = () => {
    if (!canSave) return;
    setFeedback(null);
    setEgress.mutate(
      {
        policy: target,
        confirmToken: token,
        approvedModel: target === "none" ? undefined : model,
      },
      {
        onSuccess: (view) =>
          setFeedback(
            `已写入出域策略：${view.policy}${
              view.approvedModel ? `，已批准模型 ${view.approvedModel}` : ""
            }。变更已写入 work_event（actor=human）。`,
          ),
      },
    );
  };

  const runJudge = () => {
    judgeNow.mutate(undefined);
  };

  const runRetry = () => {
    judgeRetry.mutate(undefined);
  };

  return (
    <section className="settings-section" aria-labelledby="settings-egress">
      <h2 id="settings-egress" className="panel-title">
        判读与出域策略
      </h2>
      <p className="panel-note">
        默认 `none`：任何正文都不离开本机，判读被拦截并记入 `egress_log`。
        切换策略需要逐字输入目标策略名；收紧回 `none` 永远允许。判读只在你点击后执行。
      </p>

      {!bridge ? (
        <p className="panel-note">未连接桌面端，无法读取出域配置。</p>
      ) : egress.isPending ? (
        <LoadingNotice label="正在读取出域配置…" />
      ) : egress.isError ? (
        <IpcErrorNotice
          title="出域配置读取失败"
          error={egress.error}
          onRetry={() => void egress.refetch()}
        />
      ) : current ? (
        <>
          <dl className="detail-facts compact">
            <div>
              <dt>当前策略</dt>
              <dd>{POLICY_LABEL[current.policy]}</dd>
            </div>
            <div>
              <dt>已批准模型</dt>
              <dd>{current.approvedModel ?? "未设置（none 不出域）"}</dd>
            </div>
            <div>
              <dt>判读模式</dt>
              <dd>{current.judgeMode}</dd>
            </div>
          </dl>

          <div className="field-row">
            <label className="field grow">
              <span className="field-label">目标策略</span>
              <select
                className="field-input"
                value={target}
                onChange={(event) => {
                  setTarget(event.target.value as EgressPolicy);
                  setToken("");
                  setFeedback(null);
                }}
                aria-label="目标出域策略"
              >
                <option value="none">{POLICY_LABEL.none}</option>
                <option value="redacted">{POLICY_LABEL.redacted}</option>
                <option value="full">{POLICY_LABEL.full}</option>
              </select>
            </label>
            {target === "none" ? null : (
              <>
                <label className="field grow">
                  <span className="field-label">已批准模型（须与 judge.modelId 一致）</span>
                  <select
                    className="field-input"
                    value={modelChoice}
                    onChange={(event) => {
                      setModelChoice(event.target.value);
                      setFeedback(null);
                    }}
                    aria-label="已批准模型"
                  >
                    <option value="">未选择</option>
                    {current.approvedModel &&
                    !JUDGE_MODEL_CANDIDATES.includes(current.approvedModel) ? (
                      <option value={CUSTOM_MODEL}>
                        {current.approvedModel}（当前已批准）
                      </option>
                    ) : null}
                    {JUDGE_MODEL_CANDIDATES.map((candidate) => (
                      <option key={candidate} value={candidate}>
                        {candidate}
                      </option>
                    ))}
                    <option value={CUSTOM_MODEL}>其他已批准模型（手动输入）</option>
                  </select>
                </label>
                {modelChoice === CUSTOM_MODEL ? (
                  <label className="field grow">
                    <span className="field-label">精确 provider/model id</span>
                    <input
                      className="field-input"
                      value={customModel}
                      onChange={(event) => setCustomModel(event.target.value)}
                      placeholder="provider/model"
                      aria-label="自定义已批准模型"
                    />
                  </label>
                ) : null}
              </>
            )}
          </div>
          {target === "none" ? null : (
            <p className="panel-note">
              候选模型取自项目文档，不代表已批准：选中即由你声明批准，后端还会与
              judge.modelId 逐字核对。
            </p>
          )}

          {dirty ? (
            <div className="restore-box">
              {POLICY_RISK[target] ? (
                <p className="restore-warning">{POLICY_RISK[target]}</p>
              ) : (
                <p className="restore-warning">
                  收紧为 none：之后的判读一律被拦截，正文不出本机。
                </p>
              )}
              <label className="field">
                <span className="field-label">
                  输入「{target}」以确认切换（后端逐字校验）
                </span>
                <input
                  className="field-input"
                  value={token}
                  onChange={(event) => setToken(event.target.value)}
                  placeholder={target}
                  aria-label="出域策略确认词"
                />
              </label>
            </div>
          ) : (
            <p className="panel-note">未选择新策略；不会自动启用任何出域。</p>
          )}

          <div className="settings-actions">
            <button
              type="button"
              className="btn btn-primary"
              disabled={!canSave}
              onClick={save}
            >
              {setEgress.isPending ? "正在写入…" : "保存出域策略"}
            </button>
          </div>
          {feedback ? (
            <div className="inline-feedback ok" role="status" aria-live="polite">
              {feedback}
            </div>
          ) : null}
          {setEgress.isError ? (
            <IpcErrorNotice title="出域策略未变" error={setEgress.error} />
          ) : null}
        </>
      ) : null}

      <div className="settings-subhead">
        <h3>手动判读</h3>
      </div>
      <p className="panel-note">
        判读需要配置本人身份（`me.userId`）与相应 runtime；缺失时后端直接拒绝，不回退到假结果。
        `none` 下判读一律被拦截，可在策略获批后用「补判读」重新入队。
      </p>
      <div className="settings-actions">
        <button
          type="button"
          className="btn btn-secondary"
          disabled={!bridge || judgeNow.isPending}
          onClick={runJudge}
        >
          {judgeNow.isPending ? "正在提交…" : "判读待处理记录"}
        </button>
        <button
          type="button"
          className="btn btn-secondary"
          disabled={!bridge || judgeRetry.isPending}
          onClick={runRetry}
        >
          {judgeRetry.isPending ? "正在提交…" : "补判读被拦截记录"}
        </button>
      </div>
      {jobs.some(job => job.kind === "judge") && <JobLog jobs={jobs.filter(job => job.kind === "judge")} sourceName={id => id} />}
      {judgeNow.isError ? (
        <IpcErrorNotice title="判读未能启动" error={judgeNow.error} />
      ) : null}
      {judgeRetry.isError ? (
        <IpcErrorNotice title="补判读未能启动" error={judgeRetry.error} />
      ) : null}
      {judgeNow.isSuccess ? (
        <p className="panel-note">已提交判读 job {judgeNow.data.jobId}。</p>
      ) : null}
      {judgeRetry.isSuccess ? (
        <p className="panel-note">
          已提交补判读 job {judgeRetry.data.jobId}，重新入队 {judgeRetry.data.requeued} 条。
        </p>
      ) : null}
    </section>
  );
}
