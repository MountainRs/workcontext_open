import { formatDateTime } from "../lib/format";
import { useOwnershipView } from "./OwnershipReview";
import { useOwnershipAction } from "./useOwnershipAction";

export function OwnershipSettings() {
  const view = useOwnershipView();
  const action = useOwnershipAction(view.refetch);
  const state = view.data;
  const profile = state?.profile;
  const enabled = state?.autoEnabled === true;
  const busy = action.isPending || view.isError;
  const canStart = !!profile && state?.canRun && !state.running && !busy;
  const error = action.error?.message || view.error?.message || state?.error;
  const retryAt = enabled && !state?.running && !view.isError ? state?.autoRetryAt : null;
  const retry = retryAt != null || (state?.canRun && state.error);
  let status = "";
  if (view.isPending) status = "正在读取整理状态…";
  else if (action.isPending) status = "正在提交…";
  else if (state?.running) status = state.progress || "正在整理会话…";
  else if (retryAt != null) {
    status = `上次整理未完成，自动重试时间为 ${formatDateTime(retryAt)}。`;
    if (state?.report?.cutoff) status += "上次成功的清单仍可查看。";
  } else if (state && !view.isError) status = enabled ? "自动整理已开启" : "自动整理已关闭";

  return <section className="settings-section" aria-labelledby="ownership-settings">
    <h2 id="ownership-settings" className="panel-title" tabIndex={-1}>会话清单</h2>
    <p id="ownership-sending" className="panel-note">整理时会将当前账号近 30 天处理后的会话发送给 workbuddy/deepseek-v4.1-flash。开启自动整理后，工作台运行期间会在归档更新时重新整理。</p>
    <div className="settings-actions">
      <button type="button" className="btn btn-secondary" role="switch" aria-checked={enabled}
        aria-label="自动整理" aria-describedby="ownership-sending"
        disabled={!profile || busy || (!enabled && !canStart)}
        onClick={() => profile && action.mutate({ kind: "auto", profile, enabled: !enabled })}>
        {enabled ? "关闭自动整理" : "开启自动整理"}
      </button>
      <button type="button" className="btn btn-secondary" aria-describedby="ownership-sending" disabled={!canStart}
        onClick={() => profile && action.mutate({ kind: "once", profile })}>{retry ? "立即重试" : "仅整理一次"}</button>
    </div>
    <p className="panel-note" role="status">{status}</p>
    {error && <p role="alert" className="inline-feedback error">{error}</p>}
  </section>;
}
