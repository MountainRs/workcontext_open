import { useSyncExternalStore } from "react";
import { formatDateTime } from "../lib/format";
import { hasDesktopBridge, toIpcError } from "../lib/ipc";
import { latestSyncJob, useJobStore, type SyncJobRecord } from "../lib/jobs";
import { useDbInfo, useSources, useSyncNow, useSyncStatus } from "../lib/queries";

function subscribeToNetwork(onChange: () => void) {
  window.addEventListener("online", onChange);
  window.addEventListener("offline", onChange);
  return () => {
    window.removeEventListener("online", onChange);
    window.removeEventListener("offline", onChange);
  };
}

/**
 * 常驻同步状态：只来自本地数据库（`sync_status`）与真实 `sync://progress` 事件。
 * 浏览器预览显示「未连接桌面端」，绝不显示「离线」或伪造同步时间。
 * 文案同时承担 A13 的诚实口径：没查到不等于对方没回复。
 */
export function StatusBar() {
  const bridge = hasDesktopBridge();
  const online = useSyncExternalStore(subscribeToNetwork, () => navigator.onLine);
  const sync = useSyncStatus();
  const db = useDbInfo();
  const sources = useSources();
  const syncNow = useSyncNow();
  const jobs = useJobStore((state) => state.jobs);

  const enabledCount = (sources.data ?? []).filter((source) => source.enabled).length;
  const activeJob = jobs.find(
    (job): job is SyncJobRecord => job.kind === "sync" && !job.done,
  );
  const syncJob = latestSyncJob(jobs);
  const judgeRunning = jobs.some((job) => job.kind === "judge" && job.done === null);

  let text: string;
  let tone: "idle" | "ok" | "warn" | "error" = "idle";
  let title = "";

  if (!bridge) {
    text = "未连接桌面端 · 本地状态未读取";
    tone = "warn";
    title = "浏览器预览无法读取本地数据库。";
  } else if (sync.isPending) {
    text = "正在读取同步状态…";
  } else if (sync.isError) {
    const error = toIpcError(sync.error);
    text = `同步状态不可读 · ${error.code}`;
    tone = "error";
    title = error.message;
  } else if (db.isError) {
    const error = toIpcError(db.error);
    text = `本地数据库不可用 · ${error.code}`;
    tone = "error";
    title = error.message;
  } else if (sync.data.length === 0) {
    text = "尚未同步 · 未配置数据源";
    tone = "warn";
  } else {
    const lastSuccessAt = sync.data.reduce<number | null>((latest, source) => {
      if (source.lastSuccessAt === null) return latest;
      return latest === null ? source.lastSuccessAt : Math.max(latest, source.lastSuccessAt);
    }, null);
    const needsAuth = sync.data.some((source) => source.state === "auth_required");
    const degraded = sync.data.some((source) => source.state === "degraded");
    if (lastSuccessAt === null) {
      text = "尚未同步 · 无最后成功时间";
      tone = "warn";
    } else {
      text = `最后成功 ${formatDateTime(lastSuccessAt)}`;
      tone = needsAuth || degraded ? "warn" : "ok";
      if (needsAuth) text += " · 需重新授权";
      else if (degraded) text += " · 有来源已降级";
    }
    title = sync.data
      .map(
        (source) =>
          `${source.displayName}：${source.state}${
            source.lastErrorMsg ? `（${source.lastErrorMsg}）` : ""
          }`,
      )
      .join("\n");
  }

  // 真实 job 进度：只统计已收到的 sync://progress 事件，不推算百分比。
  if (bridge && activeJob) {
    const reported = activeJob.events.filter((event) => event.phase === "source").length;
    const total = activeJob.sources?.length;
    text = `同步中 · 已收到 ${reported}${total ? `/${total}` : ""} 个来源结果`;
    tone = "idle";
    title = `job ${activeJob.jobId}（本地提交时间 ${formatDateTime(activeJob.requestedAt)}）`;
  } else if (bridge && !sync.isPending && !sync.isError && syncJob?.done) {
    const reports = syncJob.events.flatMap((event) =>
      event.phase === "source" && event.outcome ? [event.outcome] : [],
    );
    if (reports.length > 0) {
      const okCount = reports.filter((report) => report.status === "ok").length;
      text += ` · 上次手动同步 ${okCount}/${reports.length} 个来源完成`;
      if (syncJob.error) tone = "warn";
    } else if (syncJob.error) {
      text += ` · 上次手动同步失败：${syncJob.error}`;
      tone = "warn";
    } else {
      text += " · 上次手动同步结束，未收到来源结果";
      tone = "warn";
    }
  }

  if (bridge && judgeRunning) {
    text += " · 判读中";
  }

  const syncError = syncNow.isError ? toIpcError(syncNow.error) : null;

  return (
    <div className="sidebar-sync" title={title}>
      <span className="sync-label">
        <span className={`sync-dot ${tone}${activeJob && bridge ? " syncing" : ""}`} aria-hidden="true" />
        同步
      </span>
      <div role="status" aria-live="polite">
        {bridge && !online && <span className="sync-text sync-honesty">网络离线 · 仍可查看本地记录</span>}
        <span className="sync-text">{text}</span>
        {syncError ? (
          <span className="sync-text sync-honesty">未启动：{syncError.message}</span>
        ) : null}
      </div>
      <button
        type="button"
        className="sync-button"
        disabled={!bridge || enabledCount === 0 || syncNow.isPending}
        title={
          !bridge
            ? "需要桌面端连接"
            : enabledCount === 0
              ? "没有已授权来源：请在数据来源中注册并授权"
              : "手动同步全部已授权来源（只读白名单内数据）"
        }
        onClick={() => syncNow.mutate(undefined)}
      >
        {syncNow.isPending ? "提交中…" : "立即同步"}
      </button>
    </div>
  );
}
