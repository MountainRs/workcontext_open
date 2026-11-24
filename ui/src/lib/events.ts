/**
 * Tauri v2 事件订阅（`sync://progress` / `judge://done`）。
 *
 * 与 `@tauri-apps/api` 的 `listen` 使用同一底层协议：
 * `transformCallback(handler)` 注册回调 → `plugin:event|listen` 订阅 →
 * `plugin:event|unlisten` 取消。项目未依赖额外 npm 包，这里直接走
 * `window.__TAURI_INTERNALS__`；浏览器预览没有该对象，一律不发起请求。
 */

import type { JudgeDoneEvent, SyncProgressEvent } from "./ipc";

type EventBridge = {
  invoke: (command: string, args?: Record<string, unknown>) => Promise<unknown>;
  transformCallback: (callback: (message: unknown) => void, once?: boolean) => number;
};

function bridge(): EventBridge | undefined {
  if (typeof window === "undefined") return undefined;
  const internals = window.__TAURI_INTERNALS__;
  if (
    typeof internals?.invoke !== "function" ||
    typeof internals.transformCallback !== "function"
  ) {
    return undefined;
  }
  return internals as EventBridge;
}

/**
 * 订阅一个 Tauri 事件；返回取消函数。
 *
 * 无事件桥（浏览器预览 / 旧壳）时返回 no-op 取消函数，不抛错、不发请求。
 * 订阅本身失败也只返回 no-op：事件是失效刷新的加速器，缺失不影响真读。
 */
export async function listenEvent<T>(
  event: string,
  handler: (payload: T) => void,
): Promise<() => void> {
  const ipc = bridge();
  if (!ipc) return () => {};

  let callbackId: number | null = null;
  try {
    callbackId = ipc.transformCallback((message) => {
      const payload = (message as { payload?: unknown } | null)?.payload;
      if (payload === undefined) return;
      handler(payload as T);
    });
    const eventId = (await ipc.invoke("plugin:event|listen", {
      event,
      target: { kind: "Any" },
      handler: callbackId,
    })) as string;

    let active = true;
    return () => {
      if (!active) return;
      active = false;
      void ipc
        .invoke("plugin:event|unlisten", {
          event,
          target: { kind: "Any" },
          eventId,
        })
        .catch(() => {});
    };
  } catch {
    return () => {};
  }
}

export function isSyncProgressEvent(value: unknown): value is SyncProgressEvent {
  if (value === null || typeof value !== "object") return false;
  const v = value as SyncProgressEvent;
  return (
    typeof v.jobId === "string" &&
    (v.phase === "source" || v.phase === "done")
  );
}

export function isJudgeDoneEvent(value: unknown): value is JudgeDoneEvent {
  if (value === null || typeof value !== "object") return false;
  const v = value as JudgeDoneEvent;
  return typeof v.jobId === "string" && v.egress !== null && typeof v.egress === "object";
}
