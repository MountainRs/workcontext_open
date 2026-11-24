/** TanStack Query 绑定：所有服务端数据都经 Tauri IPC，不从本地伪造。 */

import {
  useMutation,
  useQuery,
  useQueryClient,
  type QueryClient,
} from "@tanstack/react-query";
import * as api from "./api";
import { hasDesktopBridge, invoke, type Delivery, type ItemDetail } from "./ipc";
import { useJobStore } from "./jobs";

export const queryKeys = {
  today: ["today"] as const,
  items: (filter: string, query: string) => ["items", filter, query] as const,
  item: (id: string) => ["item", id] as const,
  snapshots: ["snapshots"] as const,
  sources: ["sources"] as const,
  sync: ["sync-status"] as const,
  backups: ["backups"] as const,
  diagnostics: ["diagnostics"] as const,
  db: ["db-info"] as const,
  egress: ["egress"] as const,
  runtime: ["runtime-config"] as const,
  archive: ["archive-status"] as const,
};

function updateItemViews(client: QueryClient, id: string, detail: ItemDetail) {
  // Formal commands may return the core record without the desktop delivery context.
  if (detail.delivery || !client.getQueryData<ItemDetail>(queryKeys.item(id))?.delivery) {
    client.setQueryData(queryKeys.item(id), detail);
  }
  void client.invalidateQueries({ queryKey: ["ownership_view"] });
  void client.invalidateQueries({ queryKey: ["today"] });
  void client.invalidateQueries({ queryKey: ["items"] });
  void client.invalidateQueries({ queryKey: ["item"] });
  void client.invalidateQueries({ queryKey: ["context_scope_items"] });
}

export function useDbInfo() {
  const bridge = hasDesktopBridge();
  return useQuery({
    queryKey: queryKeys.db,
    queryFn: api.dbInfo,
    enabled: bridge,
    retry: false,
    staleTime: 30_000,
  });
}

export function useSyncStatus() {
  const bridge = hasDesktopBridge();
  return useQuery({
    queryKey: queryKeys.sync,
    queryFn: api.syncStatus,
    enabled: bridge,
    retry: false,
    refetchInterval: bridge ? 30_000 : false,
  });
}

export function useSources() {
  const bridge = hasDesktopBridge();
  return useQuery({
    queryKey: queryKeys.sources,
    queryFn: api.sourceList,
    enabled: bridge,
    retry: false,
  });
}

export function useToday() {
  const bridge = hasDesktopBridge();
  return useQuery({
    queryKey: queryKeys.today,
    queryFn: api.todayList,
    enabled: bridge,
    retry: false,
  });
}

export const ITEMS_PAGE_SIZE = 100;

export function useItems(filter: string, query: string, offset = 0) {
  const bridge = hasDesktopBridge();
  return useQuery({
    queryKey: [...queryKeys.items(filter, query), offset],
    queryFn: () =>
      api.itemsQuery({
        filter: filter as "all" | "me" | "waiting" | "confirm" | "closed",
        query: query || undefined,
        limit: ITEMS_PAGE_SIZE,
        offset,
      }),
    enabled: bridge,
    retry: false,
  });
}

type DeliveryChange =
  | { project: string | null }
  | { scopeId: string | null }
  | { note: string }
  | { target: Delivery["state"]; action?: string };

export function useChangeDelivery(id: string) {
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({ version, change }: { version: string; change: DeliveryChange }) => {
      const args = { id, version, ...change };
      if ("project" in change) return invoke<void>("ownership_set_project", args);
      if ("scopeId" in change) return invoke<void>("context_item_scope_set", args);
      if ("note" in change) return invoke<void>("ownership_set_conclusion", { ...args, resolved: false });
      return invoke<unknown>("ownership_move", args);
    },
    onSettled: () => Promise.all(["item", "items", "ownership_view", "today", "context_scope_items"].map(key =>
      client.invalidateQueries({ queryKey: [key] })
    )).then(() => undefined),
  });
}

export function useItem(id: string | undefined) {
  const bridge = hasDesktopBridge();
  return useQuery({
    queryKey: queryKeys.item(id ?? ""),
    queryFn: () => api.itemGet(id as string),
    enabled: bridge && Boolean(id),
    retry: false,
  });
}

export function useSnapshots() {
  const bridge = hasDesktopBridge();
  return useQuery({
    queryKey: queryKeys.snapshots,
    queryFn: () => api.snapshotList(),
    enabled: bridge,
    retry: false,
  });
}

export function useBackups() {
  const bridge = hasDesktopBridge();
  return useQuery({
    queryKey: queryKeys.backups,
    queryFn: api.backupList,
    enabled: bridge,
    retry: false,
  });
}

export function useDiagnostics() {
  const bridge = hasDesktopBridge();
  return useQuery({
    queryKey: queryKeys.diagnostics,
    queryFn: api.diagnostics,
    enabled: bridge,
    retry: false,
  });
}

export function useCorrectItem(id: string) {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (args: Parameters<typeof api.itemCorrect>[0]) => api.itemCorrect(args),
    onSuccess: (detail) => {
      updateItemViews(client, id, detail);
    },
  });
}

export function useCloseItem(id: string) {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (args: Parameters<typeof api.itemClose>[0]) => api.itemClose(args),
    onSuccess: (detail) => {
      updateItemViews(client, id, detail);
    },
  });
}

export function useSetFollowup(id: string) {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (args: Parameters<typeof api.itemSetFollowup>[0]) =>
      api.itemSetFollowup(args),
    onSuccess: (detail) => {
      updateItemViews(client, id, detail);
    },
  });
}

export function useCreateSnapshot() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (args: Parameters<typeof api.snapshotCreate>[0]) =>
      api.snapshotCreate(args),
    onSuccess: () => {
      void client.invalidateQueries({ queryKey: queryKeys.snapshots });
      void client.invalidateQueries({ queryKey: queryKeys.diagnostics });
    },
  });
}

export function useExportSnapshots() {
  return useMutation({
    mutationFn: (args: Parameters<typeof api.snapshotExport>[0]) =>
      api.snapshotExport(args),
  });
}

export function useBackupNow() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: api.backupNow,
    onSuccess: () => {
      void client.invalidateQueries({ queryKey: queryKeys.backups });
      void client.invalidateQueries({ queryKey: queryKeys.diagnostics });
    },
  });
}

export function useRestore() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({ path, token }: { path: string; token: string }) =>
      api.restoreFrom(path, token),
    onSuccess: () => {
      void client.invalidateQueries();
    },
  });
}

export function useExportDiagnostics() {
  return useMutation({ mutationFn: api.diagnosticsExport });
}

// ---------------------------------------------------------------------------
// 来源控制 / 手动同步 / 判读 / 出域（IPC.md §5）
// ---------------------------------------------------------------------------

export function useRegisterSource() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (args: Parameters<typeof api.sourceRegister>[0]) =>
      api.sourceRegister(args),
    onSuccess: () => {
      void client.invalidateQueries({ queryKey: queryKeys.sources });
      void client.invalidateQueries({ queryKey: queryKeys.sync });
      void client.invalidateQueries({ queryKey: queryKeys.diagnostics });
    },
  });
}

export function useSetSourceEnabled() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({ id, enabled }: { id: string; enabled: boolean }) =>
      api.sourceSetEnabled(id, enabled),
    onSuccess: () => {
      void client.invalidateQueries({ queryKey: queryKeys.sources });
      void client.invalidateQueries({ queryKey: queryKeys.sync });
      void client.invalidateQueries({ queryKey: queryKeys.diagnostics });
    },
  });
}

export function useSyncNow() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (sourceId?: string) => api.syncNow(sourceId),
    onSuccess: (started) => {
      useJobStore.getState().beginSync(started.jobId, started.sources);
      void client.invalidateQueries({ queryKey: queryKeys.sync });
      void client.invalidateQueries({ queryKey: queryKeys.sources });
    },
  });
}

export function useJudgeNow() {
  return useMutation({
    mutationFn: (limit?: number) => api.judgeNow(limit),
    onSuccess: (started) => {
      useJobStore.getState().beginJudge(started.jobId);
    },
  });
}

export function useJudgeRetry() {
  return useMutation({
    mutationFn: (recordIds?: string[]) => api.judgeRetry(recordIds),
    onSuccess: (started) => {
      useJobStore.getState().beginJudge(started.jobId, started.requeued);
    },
  });
}

export function useEgress() {
  const bridge = hasDesktopBridge();
  return useQuery({
    queryKey: queryKeys.egress,
    queryFn: api.egressGet,
    enabled: bridge,
    retry: false,
  });
}

export function useSetEgress() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({
      policy,
      confirmToken,
      approvedModel,
    }: {
      policy: Parameters<typeof api.egressSet>[0];
      confirmToken: string;
      approvedModel?: string;
    }) => api.egressSet(policy, confirmToken, approvedModel),
    onSuccess: (view) => {
      client.setQueryData(queryKeys.egress, view);
      void client.invalidateQueries({ queryKey: queryKeys.runtime });
      void client.invalidateQueries({ queryKey: queryKeys.diagnostics });
    },
  });
}

/** 运行时配置：本人身份、判读模式与精确模型、首次拉取回溯（IPC.md §5）。 */
export function useRuntimeConfig() {
  const bridge = hasDesktopBridge();
  return useQuery({
    queryKey: queryKeys.runtime,
    queryFn: api.runtimeConfigGet,
    enabled: bridge,
    retry: false,
  });
}

export function useConfigureRuntime() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (args: Parameters<typeof api.runtimeConfigure>[0]) =>
      api.runtimeConfigure(args),
    onSuccess: (view) => {
      // 保存后刷新：返回值即后端事实；其余视图按依赖关系失效重取。
      client.setQueryData(queryKeys.runtime, view);
      client.setQueryData(queryKeys.egress, view.egress);
      void client.invalidateQueries({ queryKey: queryKeys.runtime });
      void client.invalidateQueries({ queryKey: queryKeys.egress });
      void client.invalidateQueries({ queryKey: queryKeys.sources });
      void client.invalidateQueries({ queryKey: queryKeys.sync });
      void client.invalidateQueries({ queryKey: queryKeys.diagnostics });
      void client.invalidateQueries({ queryKey: queryKeys.today });
      void client.invalidateQueries({ queryKey: ["items"] });
    },
  });
}

// ---------------------------------------------------------------------------
// 全可访问会话归档（IPC.md §5.1）
// ---------------------------------------------------------------------------

/** 同步运行中的状态轮询间隔；只在 `running` 或刚触发同步时启用。 */
export const ARCHIVE_POLL_MS = 1_500;

export function useArchiveStatus(forcePoll = false) {
  const bridge = hasDesktopBridge();
  return useQuery({
    queryKey: queryKeys.archive,
    queryFn: api.archiveStatus,
    enabled: bridge,
    retry: false,
    refetchInterval: (query) =>
      bridge && (forcePoll || query.state.data?.running === true)
        ? ARCHIVE_POLL_MS
        : query.state.data?.periodicCapture ? 60_000 : false,
  });
}

export function useArchiveSyncNow() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: api.archiveSyncNow,
    onSuccess: () => {
      void client.invalidateQueries({ queryKey: queryKeys.archive });
    },
  });
}

export function useArchiveCaptureSet() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: ({profile, enabled}: {profile: string; enabled: boolean}) => api.archiveCaptureSet(profile, enabled),
    onSuccess: () => client.invalidateQueries({ queryKey: queryKeys.archive }),
  });
}

export function useArchiveAutoSet() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (enabled: boolean) => api.archiveAutoSet(enabled),
    onSuccess: () => {
      // 命令返回 void；开关真实值只从 archive_status 读取。
      void client.invalidateQueries({ queryKey: queryKeys.archive });
    },
  });
}

export function useArchiveJudgeNow() {
  return useMutation({ mutationFn: api.archiveJudgeNow });
}
