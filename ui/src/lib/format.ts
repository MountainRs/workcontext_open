import type {
  Attention,
  Freshness,
  FollowupType,
  Lifecycle,
  Severity,
  SnapshotKind,
  SnapshotStatus,
  SyncStatus,
} from "./ipc";

const dateTimeFormatter = new Intl.DateTimeFormat("zh-CN", {
  year: "numeric",
  month: "2-digit",
  day: "2-digit",
  hour: "2-digit",
  minute: "2-digit",
  hour12: false,
});

export function formatDateTime(ms: number | null | undefined): string {
  if (ms === null || ms === undefined) return "无记录";
  const date = new Date(ms);
  if (Number.isNaN(date.getTime())) return "时间无效";
  return dateTimeFormatter.format(date);
}

export function formatDate(ms: number | null | undefined): string {
  if (ms === null || ms === undefined) return "无记录";
  const date = new Date(ms);
  if (Number.isNaN(date.getTime())) return "时间无效";
  return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, "0")}-${String(
    date.getDate(),
  ).padStart(2, "0")}`;
}

export function formatSize(bytes: number | null | undefined): string {
  if (bytes === null || bytes === undefined || !Number.isFinite(bytes)) return "未知";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

export function freshnessText(freshness: Freshness): string {
  switch (freshness.state) {
    case "unknown":
      return "尚未同步 · 无最后成功时间";
    case "fresh":
      return `数据截至 ${formatDateTime(freshness.lastSuccessAt)}`;
    case "stale":
      return `数据截至 ${formatDateTime(freshness.lastSuccessAt)}（可能已陈旧）`;
    case "expired":
      return `同步已停：最后成功 ${formatDateTime(freshness.lastSuccessAt)}，以下内容可能不完整`;
    default:
      return "同步状态未知";
  }
}

export const ATTENTION_LABEL: Record<Attention, string> = {
  me: "轮到我",
  waiting_other: "等别人",
  report_back: "待回报",
  awaiting_acceptance: "待验收",
  no_next: "暂无下一步",
};

export const LIFECYCLE_LABEL: Record<Lifecycle, string> = {
  candidate: "待确认",
  active: "活跃",
  closed: "已收尾",
  ignored: "已忽略",
};

export function attentionColor(attention: Attention): string {
  if (attention === "me" || attention === "report_back") return "var(--c-mine)";
  if (attention === "waiting_other" || attention === "awaiting_acceptance")
    return "var(--c-waiting)";
  return "var(--n-500)";
}

export function itemStatusLabel(lifecycle: Lifecycle, attention: Attention): string {
  if (lifecycle === "closed") return LIFECYCLE_LABEL.closed;
  if (lifecycle === "ignored") return LIFECYCLE_LABEL.ignored;
  if (lifecycle === "candidate") return LIFECYCLE_LABEL.candidate;
  return ATTENTION_LABEL[attention];
}

export function itemStatusColor(lifecycle: Lifecycle, attention: Attention): string {
  if (lifecycle === "closed") return "var(--c-done)";
  if (lifecycle === "ignored") return "var(--n-400)";
  if (lifecycle === "candidate") return "var(--c-confirm)";
  return attentionColor(attention);
}

export const FOLLOWUP_LABEL: Record<FollowupType, string> = {
  none: "无",
  event: "等事件",
  deadline: "截止时间",
  manual_time: "人工时间",
};

export const SNAPSHOT_KIND_LABEL: Record<SnapshotKind, string> = {
  wrong_owner: "归属错误",
  wrong_state: "状态错误",
  missing_item: "漏建事项",
  spurious_item: "误建事项",
  wrong_link: "关联错误",
  ui: "界面问题",
  other: "其他",
};

export const SEVERITY_LABEL: Record<Severity, string> = {
  low: "低",
  medium: "中",
  high: "高",
};

export const SNAPSHOT_STATUS_LABEL: Record<SnapshotStatus, string> = {
  open: "未处理",
  resolved: "已解决",
  wontfix: "不再修复",
};

export const SYNC_STATE_LABEL: Record<SyncStatus["state"], string> = {
  idle: "空闲",
  syncing: "同步中",
  degraded: "已降级",
  auth_required: "需重新授权",
  disabled: "已停用",
};

export function followupText(
  type: FollowupType,
  at: number | null,
  note: string | null,
): string {
  if (type === "none") return "未设置";
  if (type === "event") return `等事件${note ? `：${note}` : ""}`;
  return `${FOLLOWUP_LABEL[type]} · ${formatDateTime(at)}${note ? ` · ${note}` : ""}`;
}

/** `<input type="datetime-local">` 的本地时间 → UTC 毫秒；无效返回 null。 */
export function localInputToMs(value: string): number | null {
  if (!value) return null;
  const ms = new Date(value).getTime();
  return Number.isFinite(ms) ? ms : null;
}

/** UTC 毫秒 → `<input type="datetime-local">` 的本地表示。 */
export function msToLocalInput(ms: number | null): string {
  if (ms === null) return "";
  const date = new Date(ms);
  if (Number.isNaN(date.getTime())) return "";
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(
    date.getHours(),
  )}:${pad(date.getMinutes())}`;
}
