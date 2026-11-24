/**
 * 前端 ↔ Tauri 桌面端的唯一入口（wire DTO 见 `ui/IPC.md`）。
 *
 * 浏览器预览没有 `__TAURI_INTERNALS__`：这里显式报 `UI_NO_BRIDGE`，
 * 查询不发、写操作禁用。**任何情况下都不返回样例数据或假成功。**
 */

export type Attention =
  | "me"
  | "waiting_other"
  | "report_back"
  | "awaiting_acceptance"
  | "no_next";

export type Lifecycle = "candidate" | "active" | "closed" | "ignored";
export type FollowupType = "none" | "event" | "deadline" | "manual_time";
export type FreshnessState = "fresh" | "stale" | "expired" | "unknown";
export type SnapshotStatus = "open" | "resolved" | "wontfix";
export type SnapshotKind =
  | "wrong_owner"
  | "wrong_state"
  | "missing_item"
  | "spurious_item"
  | "wrong_link"
  | "ui"
  | "other";
export type Severity = "low" | "medium" | "high";

export type AppError = {
  code: string;
  message: string;
  detail?: string;
  retryable: boolean;
  hint?: string;
};

export type Freshness = {
  state: FreshnessState;
  lastSuccessAt: number | null;
  windowStart: number | null;
  windowEnd: number | null;
};

export type DbInfo = { dbPath: string; schemaVersion: number };

export type SyncStatus = {
  sourceId: string;
  displayName: string;
  state: "idle" | "syncing" | "degraded" | "auth_required" | "disabled";
  lastAttemptAt: number | null;
  lastSuccessAt: number | null;
  consecutiveFailures: number;
  lastErrorCode: string | null;
  lastErrorMsg: string | null;
  windowStart: number | null;
  windowEnd: number | null;
  cursorAt: number | null;
};

export type SourceView = {
  id: string;
  kind: "dws_im" | "dws_report" | "manual" | "import";
  displayName: string;
  scopeSummary: string;
  enabled: boolean;
  authorizedAt: number | null;
  revokedAt: number | null;
  createdAt: number;
};

export type TodayItem = {
  id: string;
  title: string;
  attention: Attention;
  nextAction: string;
  whyHere: string;
  lastEvidenceAt: number | null;
  sourceRef: {
    id: string;
    kind: "im_message" | "weekly_report" | "file" | "manual_note";
    label: string;
  };
};

export type TodayView = {
  groups: { mine: TodayItem[]; waiting: TodayItem[]; confirm: TodayItem[] };
  freshness: Freshness;
};

export type ItemFilter = "all" | "me" | "waiting" | "confirm" | "closed";

export type ItemsQueryArgs = {
  filter: ItemFilter;
  query?: string;
  limit?: number;
  offset?: number;
};

export type Delivery = {
  version: string;
  state: "mine" | "waiting" | "triage" | "done" | "ignored";
  present: boolean;
  formal: boolean;
  confirmation: "suggested" | "confirmed";
  project: string | null;
  scopeId?: string | null;
  work: string;
  evidenceDetails: { time: string; conversation: string; kind: string; excerpt: string }[];
  conclusion: string | null;
  related: { id: string; title: string }[];
};

export type ItemSummary = {
  confirmation?: Delivery["confirmation"];
  historical?: boolean;
  project?: string | null;
  id: string;
  title: string;
  lifecycle: Lifecycle;
  attention: Attention;
  nextAction: string | null;
  sourceLabel: string | null;
  lastEvidenceAt: number | null;
  updatedAt: number | null;
  awaitingConfirmation: boolean;
};

export type ItemPage = {
  items: ItemSummary[];
  total: number;
  freshness: Freshness;
};

export type ItemDetail = {
  delivery?: Delivery | null;
  item: {
    confirmation?: Delivery["confirmation"];
    project?: string | null;
    id: string;
    title: string;
    lifecycle: Lifecycle;
    attention: Attention;
    nextAction: string | null;
    followupType: FollowupType;
    followupAt: number | null;
    followupNote: string | null;
    threadId: string | null;
    confidence: number | null;
    awaitingConfirmation: boolean;
    closedAt: number | null;
    closeReason: string | null;
    resolutionEvidence: string | null;
    lastEvidenceAt: number | null;
    createdAt: number | null;
    updatedAt: number | null;
    lockedFields: string[];
  };
  timeline: {
    id: string;
    eventType: string;
    actor: "system" | "agent" | "human";
    at: number;
    note: string | null;
  }[];
  evidence: {
    id: string;
    kind: string;
    label: string;
    occurredAt: number;
    senderName: string | null;
    summary: string | null;
    rawState: "available" | "redacted" | "unavailable";
    linkType: string;
    state: "candidate" | "confirmed" | "rejected";
  }[];
  corrections: {
    at: number;
    actor: string;
    reason: string;
    note: string | null;
    fields: string[];
  }[];
  sourceFreshness: { sourceId: string; lastSuccessAt: number | null }[];
};

export type CorrectionReason =
  | "not_mine"
  | "closed"
  | "plain_info"
  | "wrong_link"
  | "other";

export type CorrectionArgs = {
  id: string;
  patch: {
    belongsToMe?: boolean;
    lifecycle?: Lifecycle;
    attention?: Attention;
    nextAction?: string | null;
    threadId?: string | null;
  };
  reason: CorrectionReason;
  note?: string;
};

export type CloseEvidenceType =
  | "explicit_complete"
  | "artifact_accepted"
  | "human_confirmed"
  | "ack_only"
  | "artifact_created";

export type CloseArgs = {
  id: string;
  evidenceType: CloseEvidenceType;
  note?: string;
};

export type SetFollowupArgs = {
  id: string;
  type: FollowupType;
  at?: number | null;
  note?: string | null;
};

export type Snapshot = {
  id: string;
  route: string | null;
  targetType: string | null;
  targetId: string | null;
  title: string;
  description: string;
  expected: string | null;
  kind: string | null;
  severity: Severity | null;
  redaction: "masked" | "none";
  appVersion: string;
  schemaVersion: number;
  status: SnapshotStatus;
  fixedInRelease: string | null;
  createdAt: number;
};

export type SnapshotCreateArgs = {
  route: string;
  targetType?: string;
  targetId?: string;
  description: string;
  expected?: string;
  kind?: SnapshotKind;
  severity?: Severity;
};

export type SnapshotCreateResult = {
  id: string;
  title: string;
  createdAt: number;
};

export type ExportFormat = "md" | "json";
export type ExportResult =
  | { status: "saved"; path: string }
  | { status: "cancelled" };

export type SnapshotExportArgs = {
  ids: string[];
  format: ExportFormat;
  includeBody: boolean;
};

export type BackupResult = {
  path: string;
  createdAt: number;
  sizeBytes: number;
  schemaVersion: number;
  appVersion: string;
};

export type Backup = BackupResult & { sha256Verified: boolean | null };

export type RestoreResult =
  | {
      status: "restored";
      restarted: boolean;
      preRestoreBackupPath: string | null;
    }
  | { status: "cancelled" };

export type Diagnostics = {
  app: {
    version: string;
    platform: string;
    schemaVersion: number;
    supportedSchemaVersion: number;
  };
  db: {
    path: string;
    integrity: "ok" | "failed";
    integrityDetail: string | null;
    sizeBytes: number | null;
    counts: {
      sourceRecord: number;
      workItem: number;
      workEvent: number;
      feedbackSnapshot: number;
    };
  };
  backups: {
    count: number;
    last: { path: string; createdAt: number; sizeBytes: number } | null;
  };
  sources: SyncStatus[];
  judge: {
    windowDays: number;
    ok: number;
    rejected: number;
    failed: number;
    timeout: number;
    rejectionRules: { rule: string; count: number }[];
  };
};

/** `source_register` 入参白名单：稳定 ID，不接受群名。 */
export type SourceScope = { conversations: string[]; people: string[] };

/** `source_register` 入参。注册 ≠ 授权；授权另走 `source_set_enabled`。 */
export type SourceRegisterArgs = {
  id?: string;
  displayName: string;
  scope: SourceScope;
  profile: string;
};

export type SyncJobStarted = { jobId: string; sources: string[] };

export type SyncOutcome = {
  sourceId: string;
  runId: string | null;
  status: "ok" | "partial" | "failed" | "auth_required" | "degraded" | "disabled";
  pages: number;
  fetched: number;
  outOfScope: number;
  inserted: number;
  unchanged: number;
  updated: number;
  cursorAdvanced: boolean;
  error: { code: string; message: string } | null;
};

/** `sync://progress` 事件载荷（`sources/sync.rs`）。 */
export type SyncProgressEvent = {
  jobId: string;
  phase: "source" | "done";
  sourceId?: string | null;
  outcome?: SyncOutcome | null;
  error?: string | null;
};

export type JudgeJobStarted = { jobId: string };
export type JudgeRetryStarted = { jobId: string; requeued: number };

/** `judge://done` 事件载荷；`summary` 是后端 `JudgeRunSummary` 原样 JSON。 */
export type JudgeDoneEvent = {
  jobId: string;
  requeued?: number | null;
  summary: unknown;
  egress: { blockedBatches: number; allowedBatches: number };
  error?: AppError | null;
};

export type EgressPolicy = "none" | "redacted" | "full";

/** `egress_get` / `egress_set` 返回（`sources.rs` `EgressView`）。 */
export type EgressView = {
  policy: EgressPolicy;
  approvedModel: string | null;
  judgeMode: "mock" | "cli";
};

/**
 * `archive_status` 返回（`crates/desktop/src/archive.rs` `ArchiveStatus`）。
 * `conversations` / `messages` / `attachments` 是本地**登记**数；
 * `downloaded` / `failed` 是附件**落盘成功/失败的文件**数，两者不可相加当总数。
 */
export type ArchiveStatus = {
  running: boolean;
  scope: string;
  lastSuccessMs: number | null;
  conversations: number;
  messages: number;
  attachments: number;
  downloaded: number;
  failed: number;
  error: string | null;
  autoJudge: boolean;
  profile: string | null;
  periodicCapture: boolean;
};

/** 判读模式：`mock` 不出域，`cli` 走本机固定程序 + 精确模型。 */
export type JudgeMode = "mock" | "cli";

/** `runtime_config_get` / `runtime_configure` 返回（`sources/runtime.rs` `RuntimeConfigView`）。 */
export type RuntimeConfigView = {
  /** `userId` 为空表示尚未配置：判读会被拒绝（归属锚点不臆断）。 */
  me: { userId: string; displayName: string | null };
  judge: { mode: JudgeMode; modelId: string | null };
  egress: EgressView;
  firstPull: { lookbackHours: number; minHours: number; maxHours: number };
  /** 允许执行的 CLI 固定程序名（调用方无法替换，UI 只读展示）。 */
  trustedExecutables: string[];
};

/**
 * `runtime_configure` 入参：只允许这五个键（其余键后端整体拒绝）；
 * 省略/`undefined` = 不改。`firstPullLookbackHours` 为 1–168 的整数，
 * 在**下一次授权**时生效；换 `modelId` 会关闭出域批准。
 */
export type RuntimeConfigureArgs = {
  meUserId?: string;
  meDisplayName?: string;
  judgeMode?: JudgeMode;
  modelId?: string;
  firstPullLookbackHours?: number;
};

type TauriInternals = {
  invoke?: (
    command: string,
    args?: Record<string, unknown>,
    options?: unknown,
  ) => Promise<unknown>;
  /** Tauri v2 事件回调注册（与 `@tauri-apps/api` 的 `listen` 同协议）。 */
  transformCallback?: (callback: (message: unknown) => void, once?: boolean) => number;
};

declare global {
  interface Window {
    __TAURI_INTERNALS__?: TauriInternals;
  }
}

export class IpcError extends Error implements AppError {
  code: string;
  detail?: string;
  retryable: boolean;
  hint?: string;

  constructor(error: AppError) {
    super(error.message);
    this.name = "IpcError";
    this.code = error.code;
    this.detail = error.detail;
    this.retryable = error.retryable;
    this.hint = error.hint;
  }
}

function bridge(): TauriInternals | undefined {
  if (typeof window === "undefined") return undefined;
  return window.__TAURI_INTERNALS__;
}

/** 当前是否运行在 Tauri 桌面端（而非浏览器预览）。 */
export function hasDesktopBridge(): boolean {
  return typeof bridge()?.invoke === "function";
}

function normalizeError(raw: unknown): IpcError {
  if (raw instanceof IpcError) return raw;
  if (typeof raw === "string") {
    return new IpcError({
      code: "IPC_ERROR",
      message: raw,
      retryable: false,
      hint: "后端命令可能尚未实现；请确认桌面端版本。",
    });
  }
  if (raw && typeof raw === "object") {
    const value = raw as Record<string, unknown>;
    if (typeof value.code === "string" && typeof value.message === "string") {
      return new IpcError({
        code: value.code,
        message: value.message,
        detail: typeof value.detail === "string" ? value.detail : undefined,
        retryable: value.retryable === true,
        hint: typeof value.hint === "string" ? value.hint : undefined,
      });
    }
  }
  let detail = "";
  try {
    detail = JSON.stringify(raw);
  } catch {
    detail = String(raw);
  }
  return new IpcError({
    code: "IPC_ERROR",
    message: "桌面端返回了无法识别的错误。",
    detail,
    retryable: false,
  });
}

/** 调用一个 Tauri command；无桥时抛 `UI_NO_BRIDGE`，不静默降级。 */
export async function invoke<T>(
  command: string,
  args?: Record<string, unknown>,
): Promise<T> {
  const ipc = bridge();
  if (typeof ipc?.invoke !== "function") {
    throw new IpcError({
      code: "UI_NO_BRIDGE",
      message: "浏览器预览未连接 Tauri 桌面端，无法读取本地数据库。",
      retryable: false,
      hint: "请从桌面端启动 WorkContext；此页面不展示模拟数据。",
    });
  }
  try {
    return (await ipc.invoke(command, args ?? {})) as T;
  } catch (raw) {
    throw normalizeError(raw);
  }
}

/** 粗校验：后端返回结构不符时显式报错，而不是把半截数据当空列表。 */
export function requireShape<T>(
  value: unknown,
  predicate: (v: unknown) => boolean,
  command: string,
): T {
  if (!predicate(value)) {
    throw new IpcError({
      code: "IPC_BAD_SHAPE",
      message: `桌面端返回的 ${command} 结构不符合契约（见 ui/IPC.md）。`,
      detail: "unexpected response shape",
      retryable: true,
      hint: "请检查后端版本与 IPC 契约是否一致。",
    });
  }
  return value as T;
}

export function toIpcError(error: unknown): IpcError {
  return normalizeError(error);
}
