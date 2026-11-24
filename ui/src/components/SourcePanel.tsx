/**
 * 设置页「来源控制」：显式注册白名单 → 显式授权 → 手动同步。
 *
 * 纪律：
 * - 注册 ≠ 授权；授权/撤权都需要二次点击确认；
 * - 不自动同步、不自动探测本机账号、不提供任何登录入口；
 * - 来源数据只在用户点击「手动同步」后读取，结果逐条来自后端事件。
 */

import { useMemo, useState } from "react";
import { IpcErrorNotice, LoadingNotice } from "./IpcNotice";
import { JobLog } from "./JobLog";
import {
  formatDateTime,
  SYNC_STATE_LABEL,
} from "../lib/format";
import { hasDesktopBridge } from "../lib/ipc";
import { useJobStore } from "../lib/jobs";
import {
  useRegisterSource,
  useRuntimeConfig,
  useSetSourceEnabled,
  useSources,
  useSyncNow,
  useSyncStatus,
} from "../lib/queries";

function parseIds(text: string): string[] {
  return text
    .split(/[\s,，]+/)
    .map((item) => item.trim())
    .filter((item) => item.length > 0);
}

const PROFILE_PATTERN = /^[^\s:]+:[^\s:]+$/;

type PendingAction = { id: string; enabled: boolean } | null;

export function SourcePanel() {
  const bridge = hasDesktopBridge();
  const sources = useSources();
  const sync = useSyncStatus();
  const register = useRegisterSource();
  const setEnabled = useSetSourceEnabled();
  const syncNow = useSyncNow();
  const runtime = useRuntimeConfig();
  const runtimeReady = runtime.data !== undefined;
  const lookbackHours = runtime.data?.firstPull.lookbackHours;
  // 以「现在」为授权时刻的窗口预览；只展示，不触发授权。
  const windowStart = useMemo(
    () => (lookbackHours === undefined ? null : Date.now() - lookbackHours * 3_600_000),
    [lookbackHours],
  );

  const [displayName, setDisplayName] = useState("");
  const [profile, setProfile] = useState("");
  const [conversations, setConversations] = useState("");
  const [people, setPeople] = useState("");
  const [formFeedback, setFormFeedback] = useState<string | null>(null);
  const [pending, setPending] = useState<PendingAction>(null);
  const [actionFeedback, setActionFeedback] = useState<string | null>(null);

  const jobs = useJobStore((state) => state.jobs);
  const syncById = useMemo(
    () => new Map((sync.data ?? []).map((status) => [status.sourceId, status])),
    [sync.data],
  );
  const nameById = useMemo(
    () => new Map((sources.data ?? []).map((source) => [source.id, source.displayName])),
    [sources.data],
  );
  const enabledCount = (sources.data ?? []).filter((source) => source.enabled).length;

  const scopeConversations = parseIds(conversations);
  const scopePeople = parseIds(people);
  const canRegister =
    bridge &&
    !register.isPending &&
    displayName.trim().length > 0 &&
    PROFILE_PATTERN.test(profile.trim()) &&
    (scopeConversations.length > 0 || scopePeople.length > 0);

  const runRegister = () => {
    if (!canRegister) return;
    setFormFeedback(null);
    register.mutate(
      {
        displayName: displayName.trim(),
        profile: profile.trim(),
        scope: { conversations: scopeConversations, people: scopePeople },
      },
      {
        onSuccess: (source) => {
          setDisplayName("");
          setProfile("");
          setConversations("");
          setPeople("");
          setFormFeedback(
            source.enabled
              ? `已注册并保持原授权：${source.displayName}（${source.id}）。`
              : `已注册「${source.displayName}」（${source.id}），尚未授权：需在下方点「授权」才会读取。`,
          );
        },
      },
    );
  };

  const runSetEnabled = (id: string, enabled: boolean) => {
    if (enabled && !runtimeReady) return;
    if (pending?.id !== id || pending.enabled !== enabled) {
      setPending({ id, enabled });
      setActionFeedback(null);
      return;
    }
    setPending(null);
    setActionFeedback(null);
    setEnabled.mutate(
      { id, enabled },
      {
        onSuccess: (source) =>
          setActionFeedback(
            enabled
              ? `已授权「${source.displayName}」：白名单内数据可在手动同步时读取。`
              : `已撤权「${source.displayName}」：后续同步不再触达该来源，已落库记录保留。`,
          ),
      },
    );
  };

  const runSync = (sourceId?: string) => {
    syncNow.mutate(sourceId);
  };

  const statusLabel = (sourceId: string) => {
    const status = syncById.get(sourceId);
    return status ? SYNC_STATE_LABEL[status.state] : "无检查点";
  };

  return (
    <section className="settings-section" aria-labelledby="settings-sources">
      <h2 id="settings-sources" className="panel-title">
        数据源与同步
      </h2>
      <p className="panel-note">
        注册只是声明来源与白名单；授权后才会在「手动同步」时读取白名单内的数据。
        应用不会自动同步、不会自行读取企业消息，也不提供企业登录。
      </p>


      <div className="settings-subhead">
        <h3>注册来源（白名单会话 / 账户）</h3>
      </div>
      <div className="field-row">
        <label className="field grow">
          <span className="field-label">显示名称</span>
          <input
            className="field-input"
            value={displayName}
            onChange={(event) => setDisplayName(event.target.value)}
            placeholder="例如 OA 项目群"
            aria-label="来源显示名称"
          />
        </label>
        <label className="field grow">
          <span className="field-label">dws profile（corpId:userId，稳定 ID）</span>
          <input
            className="field-input"
            value={profile}
            onChange={(event) => setProfile(event.target.value)}
            placeholder="corpId:userId"
            aria-label="dws profile"
          />
        </label>
      </div>
      <div className="field-row">
        <label className="field grow">
          <span className="field-label">
            白名单会话 ID（每行一个，不接受群名）
          </span>
          <textarea
            className="field-input"
            rows={3}
            value={conversations}
            onChange={(event) => setConversations(event.target.value)}
            placeholder="openConversationId"
            aria-label="白名单会话 ID"
          />
        </label>
        <label className="field grow">
          <span className="field-label">白名单账户 ID（每行一个）</span>
          <textarea
            className="field-input"
            rows={3}
            value={people}
            onChange={(event) => setPeople(event.target.value)}
            placeholder="staffId"
            aria-label="白名单账户 ID"
          />
        </label>
      </div>
      <div className="settings-actions">
        <button
          type="button"
          className="btn btn-secondary"
          disabled={!canRegister}
          onClick={runRegister}
          title={
            !bridge
              ? "需要桌面端连接"
              : !displayName.trim()
                ? "先填显示名称"
                : !PROFILE_PATTERN.test(profile.trim())
                  ? "profile 须为 corpId:userId"
                  : scopeConversations.length === 0 && scopePeople.length === 0
                    ? "至少填一个白名单会话或账户"
                    : undefined
          }
        >
          {register.isPending ? "正在注册…" : "注册来源"}
        </button>
        <span className="panel-note">
          可执行文件由桌面端按固定程序名解析，界面不能指定其它程序或参数。
        </span>
      </div>
      {formFeedback ? (
        <div className="inline-feedback ok" role="status" aria-live="polite">
          {formFeedback}
        </div>
      ) : null}
      {register.isError ? (
        <IpcErrorNotice title="注册失败" error={register.error} />
      ) : null}

      <div className="settings-subhead">
        <h3>首次拉取窗口（授权时生效）</h3>
      </div>
      {!bridge ? (
        <p className="panel-note">未连接桌面端，无法读取首次拉取回溯。</p>
      ) : runtime.isPending ? (
        <LoadingNotice label="正在读取首次拉取回溯…" />
      ) : runtime.isError ? (
        <IpcErrorNotice
          title="运行时配置读取失败，无法显示真实回溯"
          error={runtime.error}
          onRetry={() => void runtime.refetch()}
        />
      ) : runtime.data ? (
        <>
          <dl className="detail-facts compact">
            <div>
              <dt>回溯小时</dt>
              <dd>
                {runtime.data.firstPull.lookbackHours}（允许{" "}
                {runtime.data.firstPull.minHours}–{runtime.data.firstPull.maxHours}）
              </dd>
            </div>
            <div>
              <dt>窗口起点</dt>
              <dd>
                {formatDateTime(windowStart)}（= 授权时刻 −{" "}
                {runtime.data.firstPull.lookbackHours} 小时）
              </dd>
            </div>
            <div>
              <dt>结束边界</dt>
              <dd>开放（未闭合；每次同步推进到当前时刻）</dd>
            </div>
          </dl>
          <p className="panel-note">
            授权只登记白名单并按上述窗口读取；不探测账号、不读取白名单外的企业数据。
            回溯在运行时配置保存后对下一次授权生效。
          </p>
        </>
      ) : null}

      <div className="settings-subhead">
        <h3>已注册来源（{sources.data?.length ?? 0}）</h3>
      </div>

      {!bridge ? (
        <p className="panel-note">未连接桌面端，无法读取来源表。</p>
      ) : sources.isPending ? (
        <LoadingNotice label="正在读取来源…" />
      ) : sources.isError ? (
        <IpcErrorNotice
          title="来源读取失败"
          error={sources.error}
          onRetry={() => void sources.refetch()}
        />
      ) : sources.data.length === 0 ? (
        <p className="settings-empty">
          尚未配置数据源。没有授权白名单时，不读取任何企业数据。
        </p>
      ) : (
        <div className="settings-table-wrap">
          <table className="settings-table">
            <thead>
              <tr>
                <th scope="col">来源</th>
                <th scope="col">类型</th>
                <th scope="col">白名单范围</th>
                <th scope="col">授权状态</th>
                <th scope="col">授权 / 撤销时间</th>
                <th scope="col">最后成功</th>
                <th scope="col">状态</th>
                <th scope="col">操作</th>
              </tr>
            </thead>
            <tbody>
              {sources.data.map((source) => {
                const status = syncById.get(source.id);
                const confirming =
                  pending?.id === source.id
                    ? pending.enabled
                    : null;
                return (
                  <tr key={source.id}>
                    <td>
                      {source.displayName}
                      <span className="cell-muted"> · {source.id}</span>
                    </td>
                    <td className="cell-muted">{source.kind}</td>
                    <td>{source.scopeSummary || "未记录"}</td>
                    <td>{source.enabled ? "已授权" : "未授权"}</td>
                    <td className="tnum">
                      {formatDateTime(source.authorizedAt)}
                      {source.revokedAt ? ` / ${formatDateTime(source.revokedAt)}` : ""}
                    </td>
                    <td className="tnum">
                      {formatDateTime(status?.lastSuccessAt ?? null)}
                    </td>
                    <td>{statusLabel(source.id)}</td>
                    <td>
                      <div className="row-actions">
                        {source.enabled ? (
                          <button
                            type="button"
                            className="btn btn-danger btn-sm"
                            disabled={setEnabled.isPending}
                            onClick={() => runSetEnabled(source.id, false)}
                          >
                            {confirming === false ? "确认撤权" : "撤权"}
                          </button>
                        ) : (
                          <button
                            type="button"
                            className="btn btn-secondary btn-sm"
                            disabled={setEnabled.isPending || !runtimeReady}
                            title={
                              runtimeReady
                                ? undefined
                                : "先读取到运行时配置（首次拉取回溯）才能授权"
                            }
                            onClick={() => runSetEnabled(source.id, true)}
                          >
                            {confirming === true ? "确认授权" : "授权"}
                          </button>
                        )}
                        <button
                          type="button"
                          className="btn btn-secondary btn-sm"
                          disabled={!source.enabled || syncNow.isPending}
                          title={
                            source.enabled
                              ? undefined
                              : "未授权来源不会被读取"
                          }
                          onClick={() => runSync(source.id)}
                        >
                          同步此来源
                        </button>
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}

      {actionFeedback ? (
        <div className="inline-feedback ok" role="status" aria-live="polite">
          {actionFeedback}
        </div>
      ) : null}
      {setEnabled.isError ? (
        <IpcErrorNotice title="授权变更失败" error={setEnabled.error} />
      ) : null}

      {pending ? (
        <p className="restore-warning">
          {pending.enabled
            ? `再次点击「确认授权」后，该来源白名单内的数据才会在手动同步时被读取；首次拉取窗口为 [${formatDateTime(
                windowStart,
              )}, 开放)，不探测账号。`
            : "再次点击「确认撤权」后立即生效：后续同步拒绝触达该来源；已落库记录与游标保留。"}
        </p>
      ) : null}

      <div className="settings-actions">
        <button
          type="button"
          className="btn btn-primary"
          disabled={!bridge || syncNow.isPending || enabledCount === 0}
          title={
            enabledCount === 0
              ? "没有已授权来源：先注册并授权"
              : undefined
          }
          onClick={() => runSync()}
        >
          {syncNow.isPending ? "正在提交…" : `手动同步全部已授权来源（${enabledCount}）`}
        </button>
        <button
          type="button"
          className="btn btn-secondary"
          disabled={!bridge || sync.isFetching}
          onClick={() => void sync.refetch()}
        >
          {sync.isFetching ? "正在刷新…" : "刷新同步状态"}
        </button>
      </div>
      {syncNow.isError ? (
        <IpcErrorNotice title="同步未能启动" error={syncNow.error} />
      ) : null}

      <div className="settings-subhead">
        <h3>手动 job 结果</h3>
      </div>
      <JobLog jobs={jobs} sourceName={(id) => nameById.get(id) ?? id} />
      <p className="panel-note">
        job 状态只来自命令返回与事件；事件丢失时卡片会停在「运行中」，请以刷新后的同步状态为准。
      </p>
      {syncNow.isSuccess ? (
        <p className="panel-note">
          最近一次提交：job {syncNow.data.jobId}（目标 {syncNow.data.sources.length} 个来源）。
        </p>
      ) : null}
    </section>
  );
}
