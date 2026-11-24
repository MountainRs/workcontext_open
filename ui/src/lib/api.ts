/** Tauri command 的类型化封装：命令名与参数是唯一事实来源，见 `ui/IPC.md`。 */

import {
  invoke,
  requireShape,
  type ArchiveStatus,
  type Backup,
  type BackupResult,
  type CloseArgs,
  type CorrectionArgs,
  type DbInfo,
  type Delivery,
  type Diagnostics,
  type EgressPolicy,
  type EgressView,
  type ExportResult,
  type Freshness,
  type ItemDetail,
  type ItemPage,
  type ItemSummary,
  type ItemsQueryArgs,
  type JudgeJobStarted,
  type JudgeRetryStarted,
  type RestoreResult,
  type RuntimeConfigView,
  type RuntimeConfigureArgs,
  type SetFollowupArgs,
  type Snapshot,
  type SnapshotCreateArgs,
  type SnapshotCreateResult,
  type SnapshotExportArgs,
  type SnapshotStatus,
  type SourceRegisterArgs,
  type SourceView,
  type SyncJobStarted,
  type SyncStatus,
  type TodayItem,
  type TodayView,
} from "./ipc";

function isDbInfo(value: unknown): value is DbInfo {
  if (value === null || typeof value !== "object") return false;
  const v = value as DbInfo;
  return typeof v.dbPath === "string" && typeof v.schemaVersion === "number";
}

function isSyncStatus(value: unknown): value is SyncStatus {
  if (value === null || typeof value !== "object") return false;
  const v = value as SyncStatus;
  return typeof v.sourceId === "string" && typeof v.displayName === "string";
}

function isSourceView(value: unknown): value is SourceView {
  if (value === null || typeof value !== "object") return false;
  const v = value as SourceView;
  return typeof v.id === "string" && typeof v.displayName === "string";
}

function isFreshness(value: unknown): value is Freshness {
  if (value === null || typeof value !== "object") return false;
  const v = value as Freshness;
  return (
    typeof v.state === "string" &&
    (v.lastSuccessAt === null || typeof v.lastSuccessAt === "number")
  );
}

function isTodayItem(value: unknown): value is TodayItem {
  if (value === null || typeof value !== "object") return false;
  const v = value as TodayItem;
  return (
    typeof v.id === "string" &&
    typeof v.title === "string" &&
    typeof v.attention === "string" &&
    typeof v.nextAction === "string" &&
    typeof v.whyHere === "string" &&
    v.sourceRef !== null &&
    typeof v.sourceRef === "object" &&
    typeof v.sourceRef.label === "string"
  );
}

function isTodayView(value: unknown): value is TodayView {
  if (value === null || typeof value !== "object") return false;
  const v = value as TodayView;
  if (v.groups === null || typeof v.groups !== "object") return false;
  if (!isFreshness(v.freshness)) return false;
  const g = v.groups as TodayView["groups"];
  return (
    Array.isArray(g.mine) &&
    g.mine.every(isTodayItem) &&
    Array.isArray(g.waiting) &&
    g.waiting.every(isTodayItem) &&
    Array.isArray(g.confirm) &&
    g.confirm.every(isTodayItem)
  );
}

function isConfirmation(value: unknown): value is Delivery["confirmation"] {
  return value === "confirmed" || value === "suggested";
}

function isCatalogMetadata(value: { confirmation?: unknown; project?: unknown }): value is Pick<ItemSummary, "confirmation" | "project"> {
  return (value.confirmation === undefined || isConfirmation(value.confirmation)) &&
    (value.project === undefined || value.project === null || typeof value.project === "string");
}

function isOptionalTimestamp(value: unknown): value is number | null {
  return value === null || (typeof value === "number" && Number.isFinite(value));
}

function isDelivery(value: unknown): value is Delivery {
  if (value === null || typeof value !== "object") return false;
  const v = value as Delivery;
  return typeof v.version === "string" &&
    ["mine", "waiting", "triage", "done", "ignored"].includes(v.state) &&
    typeof v.present === "boolean" && typeof v.formal === "boolean" &&
    isConfirmation(v.confirmation) && typeof v.work === "string" &&
    (v.project === null || typeof v.project === "string") &&
    (v.conclusion === null || typeof v.conclusion === "string") &&
    Array.isArray(v.related) && v.related.every(other =>
      other && typeof other.id === "string" && typeof other.title === "string") &&
    Array.isArray(v.evidenceDetails) && v.evidenceDetails.every(line =>
      line && typeof line.time === "string" && typeof line.conversation === "string" &&
      typeof line.kind === "string" && typeof line.excerpt === "string");
}

function isItemSummary(value: unknown): value is ItemSummary {
  if (value === null || typeof value !== "object") return false;
  const v = value as ItemSummary;
  return (
    typeof v.id === "string" &&
    typeof v.title === "string" &&
    typeof v.lifecycle === "string" &&
    typeof v.attention === "string" &&
    isOptionalTimestamp(v.updatedAt) && isCatalogMetadata(v) &&
    (v.historical === undefined || typeof v.historical === "boolean")
  );
}

function isItemPage(value: unknown): value is ItemPage {
  if (value === null || typeof value !== "object") return false;
  const v = value as ItemPage;
  return (
    Array.isArray(v.items) &&
    v.items.every(isItemSummary) &&
    typeof v.total === "number" &&
    isFreshness(v.freshness)
  );
}

function isItemDetail(value: unknown): value is ItemDetail {
  if (value === null || typeof value !== "object") return false;
  const v = value as ItemDetail;
  if (v.item === null || typeof v.item !== "object") return false;
  return (
    typeof v.item.id === "string" &&
    typeof v.item.title === "string" &&
    typeof v.item.lifecycle === "string" &&
    typeof v.item.attention === "string" &&
    typeof v.item.followupType === "string" &&
    isCatalogMetadata(v.item) &&
    isOptionalTimestamp(v.item.createdAt) && isOptionalTimestamp(v.item.updatedAt) &&
    (v.delivery === undefined || v.delivery === null || isDelivery(v.delivery)) &&
    Array.isArray(v.sourceFreshness) &&
    Array.isArray(v.item.lockedFields) &&
    Array.isArray(v.timeline) &&
    Array.isArray(v.evidence) &&
    Array.isArray(v.corrections)
  );
}

function isSnapshot(value: unknown): value is Snapshot {
  if (value === null || typeof value !== "object") return false;
  const v = value as Snapshot;
  return (
    typeof v.id === "string" &&
    typeof v.title === "string" &&
    typeof v.status === "string" &&
    typeof v.createdAt === "number"
  );
}

function isBackup(value: unknown): value is Backup {
  if (value === null || typeof value !== "object") return false;
  const v = value as Backup;
  return typeof v.path === "string" && typeof v.createdAt === "number";
}

function isDiagnostics(value: unknown): value is Diagnostics {
  if (value === null || typeof value !== "object") return false;
  const v = value as Diagnostics;
  if (v.app === null || typeof v.app !== "object") return false;
  if (v.db === null || typeof v.db !== "object") return false;
  if (v.backups === null || typeof v.backups !== "object") return false;
  if (v.judge === null || typeof v.judge !== "object") return false;
  const counts = v.db.counts;
  if (counts === null || typeof counts !== "object") return false;
  return (
    typeof v.app.version === "string" &&
    typeof v.app.platform === "string" &&
    typeof v.app.schemaVersion === "number" &&
    typeof v.app.supportedSchemaVersion === "number" &&
    typeof v.db.path === "string" &&
    (v.db.integrity === "ok" || v.db.integrity === "failed") &&
    typeof counts.sourceRecord === "number" &&
    typeof counts.workItem === "number" &&
    typeof counts.workEvent === "number" &&
    typeof counts.feedbackSnapshot === "number" &&
    typeof v.backups.count === "number" &&
    Array.isArray(v.sources) &&
    typeof v.judge.windowDays === "number" &&
    Array.isArray(v.judge.rejectionRules)
  );
}

function isExportResult(value: unknown): value is ExportResult {
  if (value === null || typeof value !== "object") return false;
  const v = value as { status?: unknown; path?: unknown };
  if (v.status === "cancelled") return true;
  return v.status === "saved" && typeof v.path === "string";
}

function isRestoreResult(value: unknown): value is RestoreResult {
  if (value === null || typeof value !== "object") return false;
  const v = value as { status?: unknown; restarted?: unknown };
  if (v.status === "cancelled") return true;
  return v.status === "restored" && typeof v.restarted === "boolean";
}

export function dbInfo(): Promise<DbInfo> {
  return invoke<DbInfo>("db_info").then((value) =>
    requireShape<DbInfo>(value, isDbInfo, "db_info"),
  );
}

export function syncStatus(): Promise<SyncStatus[]> {
  return invoke<SyncStatus[]>("sync_status").then((value) =>
    requireShape<SyncStatus[]>(
      value,
      (v) => Array.isArray(v) && v.every(isSyncStatus),
      "sync_status",
    ),
  );
}

export function sourceList(): Promise<SourceView[]> {
  return invoke<SourceView[]>("source_list").then((value) =>
    requireShape<SourceView[]>(
      value,
      (v) => Array.isArray(v) && v.every(isSourceView),
      "source_list",
    ),
  );
}

export function todayList(): Promise<TodayView> {
  return invoke<TodayView>("today_list").then((value) =>
    requireShape<TodayView>(value, isTodayView, "today_list"),
  );
}

export function itemsQuery(args: ItemsQueryArgs): Promise<ItemPage> {
  // 后端命令签名是 `items_query(state, args: ItemsQueryArgs)`，因此 DTO 放在
  // 顶层 `args` 键下；`item_correct` / `item_close` / `item_set_followup` 同理。
  return invoke<ItemPage>("items_query", { args }).then((value) =>
    requireShape<ItemPage>(value, isItemPage, "items_query"),
  );
}

export function itemGet(id: string): Promise<ItemDetail> {
  return invoke<ItemDetail>("item_get", { id }).then((value) =>
    requireShape<ItemDetail>(value, isItemDetail, "item_get"),
  );
}

export function itemCorrect(args: CorrectionArgs): Promise<ItemDetail> {
  return invoke<ItemDetail>("item_correct", { args });
}

export function itemClose(args: CloseArgs): Promise<ItemDetail> {
  return invoke<ItemDetail>("item_close", { args });
}

export function itemSetFollowup(args: SetFollowupArgs): Promise<ItemDetail> {
  return invoke<ItemDetail>("item_set_followup", { args });
}

export function snapshotCreate(
  args: SnapshotCreateArgs,
): Promise<SnapshotCreateResult> {
  return invoke<SnapshotCreateResult>("snapshot_create", { ...args }).then((value) =>
    requireShape<SnapshotCreateResult>(
      value,
      (v) => (v !== null && typeof v === "object") && typeof (v as SnapshotCreateResult).id === "string",
      "snapshot_create",
    ),
  );
}

export function snapshotList(status?: SnapshotStatus): Promise<Snapshot[]> {
  const args = status ? { status } : {};
  return invoke<Snapshot[]>("snapshot_list", args).then((value) =>
    requireShape<Snapshot[]>(
      value,
      (v) => Array.isArray(v) && v.every(isSnapshot),
      "snapshot_list",
    ),
  );
}

export function snapshotExport(args: SnapshotExportArgs): Promise<ExportResult> {
  return invoke<ExportResult>("snapshot_export", { ...args }).then((value) =>
    requireShape<ExportResult>(value, isExportResult, "snapshot_export"),
  );
}

export function backupNow(): Promise<BackupResult> {
  return invoke<BackupResult>("backup_now").then((value) =>
    requireShape<BackupResult>(
      value,
      (v) =>
        (v !== null && typeof v === "object") &&
        typeof (v as BackupResult).path === "string" &&
        typeof (v as BackupResult).createdAt === "number",
      "backup_now",
    ),
  );
}

export function backupList(): Promise<Backup[]> {
  return invoke<Backup[]>("backup_list").then((value) =>
    requireShape<Backup[]>(
      value,
      (v) => Array.isArray(v) && v.every(isBackup),
      "backup_list",
    ),
  );
}

export function restoreFrom(path: string, confirmToken: string): Promise<RestoreResult> {
  return invoke<RestoreResult>("restore_from", { path, confirmToken }).then((value) =>
    requireShape<RestoreResult>(value, isRestoreResult, "restore_from"),
  );
}

export function diagnostics(): Promise<Diagnostics> {
  return invoke<Diagnostics>("diagnostics").then((value) =>
    requireShape<Diagnostics>(value, isDiagnostics, "diagnostics"),
  );
}

export function diagnosticsExport(): Promise<ExportResult> {
  return invoke<ExportResult>("diagnostics_export").then((value) =>
    requireShape<ExportResult>(value, isExportResult, "diagnostics_export"),
  );
}

// ---------------------------------------------------------------------------
// 来源注册 / 授权 / 手动同步 / 判读 / 出域（wire 见 IPC.md §5）
// ---------------------------------------------------------------------------

function isSyncJobStarted(value: unknown): value is SyncJobStarted {
  if (value === null || typeof value !== "object") return false;
  const v = value as SyncJobStarted;
  return typeof v.jobId === "string" && Array.isArray(v.sources);
}

function isJudgeRetryStarted(value: unknown): value is JudgeRetryStarted {
  if (value === null || typeof value !== "object") return false;
  const v = value as JudgeRetryStarted;
  return typeof v.jobId === "string" && typeof v.requeued === "number";
}

function isEgressView(value: unknown): value is EgressView {
  if (value === null || typeof value !== "object") return false;
  const v = value as EgressView;
  return (
    (v.policy === "none" || v.policy === "redacted" || v.policy === "full") &&
    (v.approvedModel === null || typeof v.approvedModel === "string") &&
    (v.judgeMode === "mock" || v.judgeMode === "cli")
  );
}

function isArchiveStatus(value: unknown): value is ArchiveStatus {
  if (value === null || typeof value !== "object") return false;
  const v = value as ArchiveStatus;
  return (
    typeof v.running === "boolean" &&
    typeof v.scope === "string" &&
    (v.lastSuccessMs === null || typeof v.lastSuccessMs === "number") &&
    typeof v.conversations === "number" &&
    typeof v.messages === "number" &&
    typeof v.attachments === "number" &&
    typeof v.downloaded === "number" &&
    typeof v.failed === "number" &&
    (v.error === null || typeof v.error === "string") &&
    typeof v.autoJudge === "boolean" &&
    (v.profile === null || typeof v.profile === "string") &&
    typeof v.periodicCapture === "boolean"
  );
}

/**
 * 注册（或更新）受控来源。注册 ≠ 授权：后端写入 `enabled=0`，
 * 需显式 `sourceSetEnabled(id, true)` 才可同步。
 */
export function sourceRegister(args: SourceRegisterArgs): Promise<SourceView> {
  return invoke<SourceView>("source_register", {
    id: args.id?.trim() || undefined,
    displayName: args.displayName,
    scope: args.scope,
    profile: args.profile,
  }).then((value) => requireShape<SourceView>(value, isSourceView, "source_register"));
}

export function sourceSetEnabled(id: string, enabled: boolean): Promise<SourceView> {
  return invoke<SourceView>("source_set_enabled", { id, enabled }).then((value) =>
    requireShape<SourceView>(value, isSourceView, "source_set_enabled"),
  );
}

/** 手动同步；省略 `sourceId` 时同步全部已授权来源。立即返回 jobId。 */
export function syncNow(sourceId?: string): Promise<SyncJobStarted> {
  return invoke<SyncJobStarted>("sync_now", sourceId ? { sourceId } : {}).then((value) =>
    requireShape<SyncJobStarted>(value, isSyncJobStarted, "sync_now"),
  );
}

/** `limit` 省略时用后端默认批次上限。 */
export function judgeNow(limit?: number): Promise<JudgeJobStarted> {
  return invoke<JudgeJobStarted>("judge_now", limit ? { limit } : {}).then((value) =>
    requireShape<JudgeJobStarted>(
      value,
      (v) =>
        v !== null && typeof v === "object" && typeof (v as JudgeJobStarted).jobId === "string",
      "judge_now",
    ),
  );
}

/** 重新入队 `blocked`/`rejected` 记录并触发判读；`recordIds` 省略 = 全部。 */
export function judgeRetry(recordIds?: string[]): Promise<JudgeRetryStarted> {
  return invoke<JudgeRetryStarted>(
    "judge_retry",
    recordIds && recordIds.length > 0 ? { recordIds } : {},
  ).then((value) => requireShape<JudgeRetryStarted>(value, isJudgeRetryStarted, "judge_retry"));
}

export function egressGet(): Promise<EgressView> {
  return invoke<EgressView>("egress_get").then((value) =>
    requireShape<EgressView>(value, isEgressView, "egress_get"),
  );
}

/**
 * 变更出域策略。`confirmToken` 必须逐字等于目标 `policy`（后端校验）；
 * `approvedModel` 在策略非 `none` 时必填，且须与 `judge.modelId` 一致。
 */
export function egressSet(
  policy: EgressPolicy,
  confirmToken: string,
  approvedModel?: string,
): Promise<EgressView> {
  return invoke<EgressView>("egress_set", {
    policy,
    confirmToken,
    approvedModel: approvedModel?.trim() || undefined,
  }).then((value) => requireShape<EgressView>(value, isEgressView, "egress_set"));
}

// ---------------------------------------------------------------------------
// 运行时配置（本人身份 / 判读模式与模型 / 首次拉取回溯；IPC.md §5）
// ---------------------------------------------------------------------------

function isRuntimeConfigView(value: unknown): value is RuntimeConfigView {
  if (value === null || typeof value !== "object") return false;
  const v = value as RuntimeConfigView;
  if (v.me === null || typeof v.me !== "object") return false;
  if (typeof v.me.userId !== "string") return false;
  if (v.me.displayName !== null && typeof v.me.displayName !== "string") return false;
  if (v.judge === null || typeof v.judge !== "object") return false;
  if (v.judge.mode !== "mock" && v.judge.mode !== "cli") return false;
  if (v.judge.modelId !== null && typeof v.judge.modelId !== "string") return false;
  if (!isEgressView(v.egress)) return false;
  if (v.firstPull === null || typeof v.firstPull !== "object") return false;
  const { lookbackHours, minHours, maxHours } = v.firstPull;
  if (![lookbackHours, minHours, maxHours].every(Number.isInteger)) return false;
  return (
    Array.isArray(v.trustedExecutables) &&
    v.trustedExecutables.every((name) => typeof name === "string")
  );
}

export function runtimeConfigGet(): Promise<RuntimeConfigView> {
  return invoke<RuntimeConfigView>("runtime_config_get").then((value) =>
    requireShape<RuntimeConfigView>(value, isRuntimeConfigView, "runtime_config_get"),
  );
}

/**
 * 修改运行时配置。只发送有变化的五个键；其余键不构造。
 * 换 `modelId` 时后端会关闭出域批准并把策略回到 `none`，返回值即新事实。
 */
export function runtimeConfigure(args: RuntimeConfigureArgs): Promise<RuntimeConfigView> {
  return invoke<RuntimeConfigView>("runtime_configure", { args }).then((value) =>
    requireShape<RuntimeConfigView>(value, isRuntimeConfigView, "runtime_configure"),
  );
}

// ---------------------------------------------------------------------------
// 全可访问会话归档（wire 见 IPC.md §5.1）
// ---------------------------------------------------------------------------

export function archiveStatus(): Promise<ArchiveStatus> {
  return invoke<ArchiveStatus>("archive_status").then((value) =>
    requireShape<ArchiveStatus>(value, isArchiveStatus, "archive_status"),
  );
}

/** 立即返回；同步在桌面端后台线程执行，结果只经 `archive_status` 读取。 */
export function archiveSyncNow(): Promise<void> {
  return invoke<void>("archive_sync_now");
}

export function archiveCaptureSet(profile: string, enabled: boolean): Promise<void> {
  return invoke<void>("archive_capture_set", { profile, enabled });
}

/** 开关自动判读；后端要求精确 CLI 模型 + 同一模型的出域批准，否则拒绝。 */
export function archiveAutoSet(enabled: boolean): Promise<void> {
  return invoke<void>("archive_auto_set", { enabled });
}

type ArchiveReceipt = {
  status: string; scanned: number; more: boolean;
  calls: { status: string; errorCode: string | null }[];
};

export async function archiveJudgeNow(): Promise<ArchiveReceipt> {
  const result = await invoke<ArchiveReceipt>("archive_judge_now");
  return requireShape<ArchiveReceipt>(result, () => result != null && typeof result.status === "string"
    && Number.isInteger(result.scanned) && typeof result.more === "boolean" && Array.isArray(result.calls)
    && result.calls.every(c => c != null && typeof c.status === "string" && (c.errorCode === null || typeof c.errorCode === "string")), "archive_judge_now");
}

