import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { formatDateTime } from "../lib/format";
import { hasDesktopBridge, invoke } from "../lib/ipc";

type QueueStatus = {
  pending: number;
  claimed: number;
  retrying: number;
  organized: number;
  noRecord: number;
  checkpoint: number;
  oldestPendingAt: number | null;
  lastOrganizedAt: number | null;
  nextRetryAt: number | null;
  lastError: string | null;
};

type OrganizerStatus = {
  profile: string | null;
  consentProfile: string | null;
  model: string;
  enabled: boolean;
  running: boolean;
  lastCheckedAt: number | null;
  credentialsAvailable: boolean;
  blocked: string | null;
  error: string | null;
  queue: QueueStatus | null;
};

const key = ["organizer-status"] as const;

type Action = "enable" | "disable" | "run" | "retry";
const errorText: Record<string, string> = {
  model_failed: "模型请求失败，请检查网络和模型凭据。",
  model_unavailable: "无法启动模型整理程序，请检查 Python 环境。",
  credentials_missing: "模型凭据尚未配置，输入已保留，等待重试。",
  model_timeout: "模型请求超时。",
  invalid_model_response: "模型返回的结果无法使用。",
  consent_changed: "整理授权已变更。",
  source_changed: "来源已更新，需要重新整理。",
  input_limit: "输入超出模型单次整理上限。",
};

export function OrganizerSettings() {
  const desktop = hasDesktopBridge();
  const client = useQueryClient();
  const query = useQuery({
    queryKey: key,
    queryFn: () => invoke<OrganizerStatus>("organizer_status"),
    enabled: desktop,
    networkMode: "always",
    refetchInterval: 5000,
  });
  const state = query.data;
  const action = useMutation({
    networkMode: "always",
    mutationFn: ({ kind, profile, model }: { kind: Action; profile: string; model: string }) => {
      if (kind === "enable" || kind === "disable") {
        return invoke<OrganizerStatus>("organizer_set_consent", { profile, model, enabled: kind === "enable" });
      }
      return invoke<OrganizerStatus>(kind === "run" ? "organizer_run" : "organizer_retry", { profile, model });
    },
    onSuccess: (data, request) => {
      client.setQueryData(key, data);
      if (request.kind === "disable" || request.kind === "enable") {
        void client.invalidateQueries({ queryKey: ["context_records"] });
        void client.invalidateQueries({ queryKey: ["context_record_get"] });
      }
    },
    onSettled: () => client.invalidateQueries({ queryKey: key }),
  });
  const send = (kind: Action) => {
    const profile = kind === "disable" ? state?.consentProfile : state?.profile;
    if (state && profile) action.mutate({ kind, profile, model: state.model });
  };
  const queue = state?.queue;
  const canRun = desktop && state?.enabled && state.profile && !state.blocked && !state.running && !action.isPending && !query.isError;
  const busy = !desktop || !state || action.isPending || query.isError;
  const error = action.error?.message || query.error?.message || state?.error;
  const waitingMinutes = queue?.oldestPendingAt == null ? null : Math.max(0, Math.floor((Date.now() - queue.oldestPendingAt) / 60000));
  let status = "正在读取整理状态…";
  if (!desktop) status = "请在桌面端查看整理状态。";
  else if (state) status = state.running ? "正在整理" : state.enabled ? "自动整理已开启" : "自动整理已关闭";

  return <section className="settings-section" aria-labelledby="organizer-heading">
    <h2 className="panel-title" id="organizer-heading" tabIndex={-1}>工作上下文自动整理</h2>
    <p className="panel-note" id="organizer-egress">开启后，工作台会将此账号的桌面补录、Agent 写入原文、授权消息与附件摘录发送给所选模型。直接回复可附上已授权的原消息摘录。没有直接回复时，新授权允许附上同一会话、同一话题中严格早于当前消息且相隔不超过十分钟的一条已授权消息。旧授权需重新授权才会启用此项。关联已有记录时，还会发送相关范围名称、标题、工单号和交付内容。</p>
    {state && <p className="panel-note">整理模型：{state.model}（本地网关 127.0.0.1:3425）</p>}
    {state?.profile && <p className="panel-note">当前账号：{state.profile}</p>}
    <div className="settings-actions">
      <button className="btn btn-secondary" type="button" role="switch" aria-label="工作上下文自动整理" aria-checked={!!state?.enabled}
        aria-describedby="organizer-egress" disabled={busy || (!state?.enabled && (!state?.profile || !!state.blocked))}
        onClick={() => send(state?.enabled ? "disable" : "enable")}>{state?.enabled ? "关闭自动整理" : "授权自动整理"}</button>
      <button className="btn btn-secondary" type="button" disabled={!canRun} onClick={() => send("run")}>立即整理</button>
      {!!queue?.retrying && <button className="btn btn-secondary" type="button" disabled={!canRun} onClick={() => send("retry")}>立即重试</button>}
    </div>
    <p className="panel-note" role="status">{action.isPending ? "正在提交…" : status}{state?.lastCheckedAt != null && ` · 最近检查 ${formatDateTime(state.lastCheckedAt)}`}</p>
    {state?.blocked && <p className="panel-note">{state.blocked}</p>}
    {state?.credentialsAvailable === false && <p className="panel-note">尚未配置模型凭据。授权后仍会在本地接收输入。设置 WORKCONTEXT_REVIEW_API_KEY 并重启工作台后，可重试模型整理。</p>}
    {queue && <>
      <p className="panel-note">待整理 {queue.pending} · 已认领 {queue.claimed} · 待重试 {queue.retrying} · 已整理 {queue.organized} · 无需建档 {queue.noRecord}</p>
      <p className="panel-note">已接收归档变更序号 {queue.checkpoint}</p>
      {queue.oldestPendingAt != null && <p className="panel-note">最早待处理输入于 {formatDateTime(queue.oldestPendingAt)} 入队，已等待 {waitingMinutes} 分钟。</p>}
      {queue.lastOrganizedAt != null && <p className="panel-note">最近完成：{formatDateTime(queue.lastOrganizedAt)}</p>}
      {queue.lastError && <p className="panel-note">{errorText[queue.lastError] ?? `整理未完成（${queue.lastError}）。`}
        {state?.enabled && !state.blocked && queue.nextRetryAt != null && ` 最早重试时间：${formatDateTime(queue.nextRetryAt)}`}</p>}
    </>}
    {error && <p className="inline-feedback error" role="alert">{error}</p>}
  </section>;
}
