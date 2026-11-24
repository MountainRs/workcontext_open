/**
 * 真实事件 → 失效刷新。
 *
 * 订阅 `sync://progress` 与 `judge://done`：
 * - 事件本身只做两件事：写入 job 记录、让相关查询失效（由 TanStack Query 重读 IPC）；
 * - 浏览器预览没有事件桥，本组件不订阅、不发请求；
 * - 不做轮询兜底：事件丢了就是没收到，界面不会补造进度。
 */

import { useEffect } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { hasDesktopBridge } from "../lib/ipc";
import {
  isJudgeDoneEvent,
  isSyncProgressEvent,
  listenEvent,
} from "../lib/events";
import { useJobStore } from "../lib/jobs";
import { queryKeys } from "../lib/queries";
import type { JudgeDoneEvent, SyncProgressEvent } from "../lib/ipc";

export function RealtimeBridge() {
  const client = useQueryClient();
  const bridge = hasDesktopBridge();

  useEffect(() => {
    if (!bridge) return;
    let disposed = false;
    const cleanups: (() => void)[] = [];

    const onSync = (payload: SyncProgressEvent) => {
      useJobStore.getState().applySyncEvent(payload);
      void client.invalidateQueries({ queryKey: queryKeys.sync });
      void client.invalidateQueries({ queryKey: queryKeys.sources });
      if (payload.phase === "done") {
        void client.invalidateQueries({ queryKey: queryKeys.today });
        void client.invalidateQueries({ queryKey: ["items"] });
        void client.invalidateQueries({ queryKey: ["item"] });
      }
    };

    const onJudge = (payload: JudgeDoneEvent) => {
      useJobStore.getState().applyJudgeEvent(payload);
      void client.invalidateQueries({ queryKey: queryKeys.diagnostics });
      void client.invalidateQueries({ queryKey: queryKeys.today });
      void client.invalidateQueries({ queryKey: ["items"] });
      void client.invalidateQueries({ queryKey: ["item"] });
    };

    void (async () => {
      const offSync = await listenEvent<SyncProgressEvent>("sync://progress", (payload) => {
        if (isSyncProgressEvent(payload)) onSync(payload);
      });
      const offJudge = await listenEvent<JudgeDoneEvent>("judge://done", (payload) => {
        if (isJudgeDoneEvent(payload)) onJudge(payload);
      });
      if (disposed) {
        offSync();
        offJudge();
        return;
      }
      cleanups.push(offSync, offJudge);
    })();

    return () => {
      disposed = true;
      for (const cleanup of cleanups) cleanup();
    };
  }, [bridge, client]);

  return null;
}
