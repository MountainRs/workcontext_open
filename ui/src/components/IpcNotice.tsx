import type { ReactNode } from "react";
import { useDbInfo } from "../lib/queries";
import { hasDesktopBridge, toIpcError } from "../lib/ipc";

type Tone = "info" | "warn" | "error";

export function Notice({
  tone = "info",
  title,
  children,
  detail,
  hint,
  onRetry,
  retryLabel = "重试",
}: {
  tone?: Tone;
  title: string;
  children?: ReactNode;
  detail?: string;
  hint?: string;
  onRetry?: () => void;
  retryLabel?: string;
}) {
  return (
    <div className={`notice ${tone}`} role={tone === "error" ? "alert" : undefined}>
      <p className="notice-title">{title}</p>
      {children ? <div className="notice-body">{children}</div> : null}
      {hint ? <p className="notice-hint">{hint}</p> : null}
      {onRetry ? (
        <button type="button" className="btn btn-secondary btn-sm" onClick={onRetry}>
          {retryLabel}
        </button>
      ) : null}
      {detail ? (
        <details className="notice-detail">
          <summary>诊断详情</summary>
          <pre>{detail}</pre>
        </details>
      ) : null}
    </div>
  );
}

export function NotConnectedNotice({ compact = false }: { compact?: boolean }) {
  return (
    <Notice
      tone="warn"
      title="未连接 Tauri 桌面端"
      hint="浏览器预览不显示模拟数据，也不假装同步成功。请从 WorkContext 桌面端打开后读取本地数据库。"
    >
      {compact ? null : (
        <p>
          当前页面只能确认界面外观。今日、事项、迭代池、备份和诊断都依赖本地数据库，
          在连接桌面端之前不可读取，写操作全部禁用。
        </p>
      )}
    </Notice>
  );
}

export function IpcErrorNotice({
  title,
  error,
  onRetry,
}: {
  title: string;
  error: unknown;
  onRetry?: () => void;
}) {
  const ipc = toIpcError(error);
  return (
    <Notice
      tone="error"
      title={`${title}（${ipc.code}）`}
      detail={ipc.detail}
      hint={ipc.hint}
      onRetry={onRetry}
    >
      <p>{ipc.message}</p>
      {ipc.code === "IPC_ERROR" ? (
        <p className="notice-subtle">
          这通常表示桌面端尚未提供该命令。命令契约见 <code>ui/IPC.md</code>。
        </p>
      ) : null}
    </Notice>
  );
}

export function LoadingNotice({ label }: { label: string }) {
  return (
    <p className="loading-line" role="status" aria-live="polite">
      {label}
    </p>
  );
}

/**
 * 全局连接横幅：浏览器预览或本地数据库不可用时始终可见；
 * 正常情况下不渲染任何内容，避免把「已连接」当成「已同步」。
 */
export function BridgeBanner() {
  const bridge = hasDesktopBridge();
  const db = useDbInfo();

  if (!bridge) {
    // 每个页面各自展示「未连接」说明，避免重复横幅。
    return null;
  }
  if (db.isError) {
    return (
      <div className="bridge-banner">
        <IpcErrorNotice
          title="本地数据库不可用"
          error={db.error}
          onRetry={() => void db.refetch()}
        />
      </div>
    );
  }
  return null;
}
