/**
 * 真实 job 结果面板：所有数字都来自 `sync_now` 返回与后端事件。
 * 缺失就是缺失——不补 0、不推算进度、不显示「预计」。
 */

import { formatDateTime } from "../lib/format";
import type { JobRecord, JudgeJobRecord, SyncJobRecord } from "../lib/jobs";
import type { JudgeDoneEvent, SyncOutcome } from "../lib/ipc";

const SYNC_STATUS_LABEL: Record<SyncOutcome["status"], string> = {
  ok: "完成",
  partial: "部分完成（游标未推进）",
  failed: "失败",
  auth_required: "需重新授权",
  degraded: "已降级",
  disabled: "已停用",
};

type SummaryStats = {
  batches: number;
  ok: number;
  rejected: number;
  skipped: number;
  blocked: number;
  pendingLeft: number | null;
  skippedMeta: number | null;
};

function judgeSummaryStats(summary: unknown): SummaryStats | null {
  if (summary === null || typeof summary !== "object") return null;
  if (!("batches" in summary) || !Array.isArray(summary.batches)) return null;
  const batches = summary.batches;
  const stats: SummaryStats = {
    batches: batches.length,
    ok: 0,
    rejected: 0,
    skipped: 0,
    blocked: 0,
    pendingLeft:
      "pendingLeft" in summary && typeof summary.pendingLeft === "number"
        ? summary.pendingLeft
        : null,
    skippedMeta:
      "skippedMeta" in summary && typeof summary.skippedMeta === "number"
        ? summary.skippedMeta
        : null,
  };
  for (const batch of batches) {
    if (batch === null || typeof batch !== "object" || !("status" in batch)) continue;
    if (batch.status === "ok") stats.ok += 1;
    else if (batch.status === "rejected") stats.rejected += 1;
    else if (batch.status === "skipped") stats.skipped += 1;
    else if (batch.status === "blocked") stats.blocked += 1;
  }
  return stats;
}

function JobStatusChip({ done, failed }: { done: boolean; failed: boolean }) {
  const label = failed ? "失败" : done ? "已结束" : "运行中";
  return (
    <span className={`job-chip ${failed ? "error" : done ? "ok" : "running"}`}>
      {label}
    </span>
  );
}

function SyncOutcomeLine({
  name,
  outcome,
}: {
  name: string;
  outcome: SyncOutcome;
}) {
  return (
    <li>
      <strong>{name}</strong>
      <span className="job-sep">·</span>
      {SYNC_STATUS_LABEL[outcome.status] ?? outcome.status}
      <span className="job-sep">·</span>
      拉取 {outcome.fetched}（白名单外 {outcome.outOfScope}）
      <span className="job-sep">·</span>
      新增 {outcome.inserted} / 更新 {outcome.updated} / 未变 {outcome.unchanged}
      <span className="job-sep">·</span>
      {outcome.pages} 页
      {outcome.cursorAdvanced ? " · 游标已推进" : " · 游标未推进"}
      {outcome.error ? (
        <span className="job-error">
          {" "}
          （{outcome.error.code}：{outcome.error.message}）
        </span>
      ) : null}
    </li>
  );
}

function SyncJobCard({
  job,
  sourceName,
}: {
  job: SyncJobRecord;
  sourceName: (id: string) => string;
}) {
  return (
    <article className="job-card">
      <header className="job-head">
        <span className="job-kind">同步</span>
        <code className="job-id">{job.jobId}</code>
        <JobStatusChip done={job.done} failed={Boolean(job.error)} />
        <span className="job-time tnum">提交于 {formatDateTime(job.requestedAt)}</span>
      </header>
      {job.sources ? (
        <p className="job-meta">
          目标来源 {job.sources.length} 个：
          {job.sources.map((id) => sourceName(id)).join("、")}
        </p>
      ) : (
        <p className="job-meta">未收到命令返回的目标列表（事件先到）。</p>
      )}
      {job.events.length === 0 ? (
        <p className="job-meta">尚未收到 sync://progress 事件。</p>
      ) : (
        <ul className="job-events">
          {job.events.map((event, index) => {
            if (event.phase === "done") {
              return (
                <li key={`${event.jobId}-done-${index}`}>
                  <strong>整轮结束</strong>
                  {event.error ? (
                    <span className="job-error"> · {event.error}</span>
                  ) : (
                    <span className="job-sep"> · 后端未报告整轮错误</span>
                  )}
                </li>
              );
            }
            const id = event.sourceId ?? "未知来源";
            const name = sourceName(id);
            if (event.outcome) {
              return (
                <SyncOutcomeLine
                  key={`${event.jobId}-${id}-${index}`}
                  name={name}
                  outcome={event.outcome}
                />
              );
            }
            return (
              <li key={`${event.jobId}-${id}-${index}`}>
                <strong>{name}</strong>
                {event.error ? (
                  <span className="job-error"> · {event.error}</span>
                ) : (
                  <span className="job-sep"> · 未返回结果</span>
                )}
              </li>
            );
          })}
        </ul>
      )}
    </article>
  );
}

function JudgeDoneDetail({ done }: { done: JudgeDoneEvent }) {
  const stats = judgeSummaryStats(done.summary);
  return (
    <>
      {done.requeued !== null && done.requeued !== undefined ? (
        <p className="job-meta">重新入队 {done.requeued} 条（真实计数）。</p>
      ) : null}
      {done.error ? (
        <p className="job-error">
          判读失败（{done.error.code}）：{done.error.message}
        </p>
      ) : stats ? (
        <p className="job-meta">
          批次 {stats.batches}：通过 {stats.ok} / 拒绝 {stats.rejected} / 跳过{" "}
          {stats.skipped} / 出域拦截 {stats.blocked}
          {stats.pendingLeft !== null ? ` · 仍待判读 ${stats.pendingLeft}` : ""}
          {stats.skippedMeta !== null ? ` · 缺元数据跳过 ${stats.skippedMeta}` : ""}
        </p>
      ) : (
        <p className="job-meta">后端未返回可识别的 summary；原始载荷见下。</p>
      )}
      <p className="job-meta">
        出域审计：拦截批次 {done.egress.blockedBatches} / 放行批次{" "}
        {done.egress.allowedBatches}
        {done.egress.allowedBatches > 0
          ? "（放行表示正文已发往已批准模型）"
          : ""}
      </p>
      <details className="job-raw">
        <summary>原始 summary（后端原样）</summary>
        <pre>{JSON.stringify(done.summary, null, 2)}</pre>
      </details>
    </>
  );
}

function JudgeJobCard({ job }: { job: JudgeJobRecord }) {
  return (
    <article className="job-card">
      <header className="job-head">
        <span className="job-kind">判读</span>
        <code className="job-id">{job.jobId}</code>
        <JobStatusChip done={job.done !== null} failed={Boolean(job.done?.error)} />
        <span className="job-time tnum">提交于 {formatDateTime(job.requestedAt)}</span>
      </header>
      {job.requeued !== null ? (
        <p className="job-meta">judge_retry 重新入队 {job.requeued} 条。</p>
      ) : null}
      {job.done === null ? (
        <p className="job-meta">尚未收到 judge://done 事件。</p>
      ) : (
        <JudgeDoneDetail done={job.done} />
      )}
    </article>
  );
}

export function JobLog({
  jobs,
  sourceName,
}: {
  jobs: JobRecord[];
  sourceName: (id: string) => string;
}) {
  if (jobs.length === 0) {
    return (
      <p className="settings-empty">
        还没有手动触发的 job。同步与判读只在你点击后执行，结果会逐条出现在这里。
      </p>
    );
  }
  return (
    <div className="job-log">
      {jobs.map((job) =>
        job.kind === "sync" ? (
          <SyncJobCard key={`sync-${job.jobId}`} job={job} sourceName={sourceName} />
        ) : (
          <JudgeJobCard key={`judge-${job.jobId}`} job={job} />
        ),
      )}
    </div>
  );
}
