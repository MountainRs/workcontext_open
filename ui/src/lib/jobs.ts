/**
 * 真实 job 的本地记录（只由命令返回与后端事件填充）。
 *
 * - 只在用户显式触发 `sync_now` / `judge_now` / `judge_retry` 后创建；
 * - 进度与结果逐条来自 `sync://progress` / `judge://done`，没有轮询、
 *   没有推算的百分比、没有「预计剩余」；
 * - 同一 jobId 的事件与命令返回合并，先到事件也能显示，但绝不补造缺失结果。
 */

import { create } from "zustand";
import type { JudgeDoneEvent, SyncProgressEvent } from "./ipc";

export type SyncJobRecord = {
  kind: "sync";
  jobId: string;
  /** 本机收到命令返回（或首个事件）的时间，不是后端开始时间。 */
  requestedAt: number;
  /** `sync_now` 返回的目标来源；先收到事件时为 null。 */
  sources: string[] | null;
  events: SyncProgressEvent[];
  done: boolean;
  doneAt: number | null;
  /** 整轮失败说明（`phase=done` 的 `error`），来源级失败留在 events 里。 */
  error: string | null;
};

export type JudgeJobRecord = {
  kind: "judge";
  jobId: string;
  requestedAt: number;
  requeued: number | null;
  done: JudgeDoneEvent | null;
  doneAt: number | null;
};

export type JobRecord = SyncJobRecord | JudgeJobRecord;

const MAX_JOBS = 24;

type JobStore = {
  jobs: JobRecord[];
  beginSync: (jobId: string, sources: string[]) => void;
  applySyncEvent: (event: SyncProgressEvent) => void;
  beginJudge: (jobId: string, requeued?: number | null) => void;
  applyJudgeEvent: (event: JudgeDoneEvent) => void;
};

function trim(jobs: JobRecord[]): JobRecord[] {
  return jobs.length > MAX_JOBS ? jobs.slice(0, MAX_JOBS) : jobs;
}

export const useJobStore = create<JobStore>((set) => ({
  jobs: [],

  beginSync: (jobId, sources) =>
    set((state) => {
      const existing = state.jobs.find(
        (job): job is SyncJobRecord => job.kind === "sync" && job.jobId === jobId,
      );
      if (existing) {
        return {
          jobs: state.jobs.map((job) =>
            job.kind === "sync" && job.jobId === jobId ? { ...job, sources } : job,
          ),
        };
      }
      const record: SyncJobRecord = {
        kind: "sync",
        jobId,
        requestedAt: Date.now(),
        sources,
        events: [],
        done: false,
        doneAt: null,
        error: null,
      };
      return { jobs: trim([record, ...state.jobs]) };
    }),

  applySyncEvent: (event) =>
    set((state) => {
      const index = state.jobs.findIndex(
        (job): job is SyncJobRecord => job.kind === "sync" && job.jobId === event.jobId,
      );
      if (index === -1) {
        const record: SyncJobRecord = {
          kind: "sync",
          jobId: event.jobId,
          requestedAt: Date.now(),
          sources: null,
          events: [event],
          done: event.phase === "done",
          doneAt: event.phase === "done" ? Date.now() : null,
          error: event.phase === "done" ? event.error ?? null : null,
        };
        return { jobs: trim([record, ...state.jobs]) };
      }
      const current = state.jobs[index] as SyncJobRecord;
      const updated: SyncJobRecord = {
        ...current,
        events: [...current.events, event],
        done: current.done || event.phase === "done",
        doneAt:
          event.phase === "done" ? Date.now() : current.doneAt,
        error:
          event.phase === "done" ? event.error ?? null : current.error,
      };
      const jobs = [...state.jobs];
      jobs[index] = updated;
      return { jobs };
    }),

  beginJudge: (jobId, requeued = null) =>
    set((state) => {
      const exists = state.jobs.some(
        (job) => job.kind === "judge" && job.jobId === jobId,
      );
      if (exists) return state;
      const record: JudgeJobRecord = {
        kind: "judge",
        jobId,
        requestedAt: Date.now(),
        requeued,
        done: null,
        doneAt: null,
      };
      return { jobs: trim([record, ...state.jobs]) };
    }),

  applyJudgeEvent: (event) =>
    set((state) => {
      const index = state.jobs.findIndex(
        (job): job is JudgeJobRecord => job.kind === "judge" && job.jobId === event.jobId,
      );
      if (index === -1) {
        const record: JudgeJobRecord = {
          kind: "judge",
          jobId: event.jobId,
          requestedAt: Date.now(),
          requeued: event.requeued ?? null,
          done: event,
          doneAt: Date.now(),
        };
        return { jobs: trim([record, ...state.jobs]) };
      }
      const current = state.jobs[index] as JudgeJobRecord;
      const jobs = [...state.jobs];
      jobs[index] = {
        ...current,
        requeued: current.requeued ?? event.requeued ?? null,
        done: event,
        doneAt: Date.now(),
      };
      return { jobs };
    }),
}));

/** 最近一个同步 job（运行中优先，否则最近一条）。 */
export function latestSyncJob(jobs: JobRecord[]): SyncJobRecord | undefined {
  const syncJobs = jobs.filter((job): job is SyncJobRecord => job.kind === "sync");
  return syncJobs.find((job) => !job.done) ?? syncJobs[0];
}
