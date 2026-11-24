/**
 * 设置页「全可访问会话同步」：`archive_status` / `archive_sync_now` /
 * `archive_auto_set` / `archive_judge_now` 的唯一界面入口
 * （后端见 `crates/desktop/src/archive.rs`）。
 *
 * 纪律：
 * - 同步命令立即返回，界面只在 `running` 或刚触发后轮询 `archive_status`；
 *   完成前不报告成功，完成后只展示后端计数；
 * - 登记数与附件下载成功数是两件事：`conversations/messages/attachments`
 *   是登记，`downloaded/failed` 是附件落盘结果；
 * - 自动判读开关需要精确 CLI 模型 + 同一模型的出域批准；开关真实值只
 *   从 `archive_status.autoJudge` 读取，不在前端推断；
 * - 浏览器预览无 bridge 时不发任何命令、不显示模拟结果。
 */

import { useEffect, useRef, useState } from "react";
import { IpcErrorNotice, LoadingNotice, Notice } from "./IpcNotice";
import { formatDateTime } from "../lib/format";
import { hasDesktopBridge, type ArchiveStatus } from "../lib/ipc";
import {
  ARCHIVE_POLL_MS,
  useArchiveAutoSet,
  useArchiveCaptureSet,
  useArchiveJudgeNow,
  useArchiveStatus,
  useArchiveSyncNow,
  useEgress,
  useRuntimeConfig,
} from "../lib/queries";

/** 触发同步后最多轮询约 5 分钟，之后交回手动刷新，避免无限请求。 */
const MAX_POLLS = Math.ceil(300_000 / ARCHIVE_POLL_MS);

function summaryText(status: ArchiveStatus): string {
  if (status.running) {
    return `正在同步：每 ${ARCHIVE_POLL_MS / 1_000} 秒读取一次真实状态；完成前不报告成功。`;
  }
  if (status.lastSuccessMs !== null) {
    return `最近一次成功：${formatDateTime(status.lastSuccessMs)}。登记 ${status.conversations} 个会话 / ${status.messages} 条消息 / ${status.attachments} 个附件；其中附件下载成功 ${status.downloaded} 个，失败 ${status.failed} 个。`;
  }
  return "尚未成功同步过；下列计数是本地已登记的真实数据，附件下载数为 0 不代表同步成功。";
}

export function ArchivePanel() {
  const bridge = hasDesktopBridge();
  const runtime = useRuntimeConfig();
  const egress = useEgress();
  const syncNow = useArchiveSyncNow();
  const autoSet = useArchiveAutoSet();
  const captureSet = useArchiveCaptureSet();
  const judgeNow = useArchiveJudgeNow();

  const [awaiting, setAwaiting] = useState(false);
  const [pollStopped, setPollStopped] = useState<string | null>(null);
  const status = useArchiveStatus(awaiting);
  const baselineRef = useRef<ArchiveStatus | null>(null);
  const seenRunningRef = useRef(false);
  const pollsRef = useRef(0);

  const current = status.data;
  const running = current?.running === true;
  const auto = current?.autoJudge === true;

  useEffect(() => {
    if (awaiting && status.isError) setAwaiting(false);
  }, [awaiting, status.isError]);

  useEffect(() => {
    if (!awaiting) return;
    if (!current) return;
    pollsRef.current += 1;
    if (current.running) {
      seenRunningRef.current = true;
      return;
    }
    const base = baselineRef.current;
    const changed =
      base === null ||
      current.lastSuccessMs !== base.lastSuccessMs ||
      current.conversations !== base.conversations ||
      current.messages !== base.messages ||
      current.attachments !== base.attachments ||
      current.downloaded !== base.downloaded ||
      current.failed !== base.failed ||
      current.error !== base.error;
    if (seenRunningRef.current || changed) {
      setAwaiting(false);
      return;
    }
    if (pollsRef.current >= MAX_POLLS) {
      setAwaiting(false);
      setPollStopped(
        "后台同步仍未返回结果，已停止自动刷新；请稍后点「刷新同步状态」查看真实状态。",
      );
    }
  }, [awaiting, current]);

  const runSync = () => {
    if (!bridge) return;
    setPollStopped(null);
    baselineRef.current = status.data ?? null;
    seenRunningRef.current = false;
    pollsRef.current = 0;
    setAwaiting(true);
    syncNow.mutate(undefined, {
      onError: () => setAwaiting(false),
    });
  };

  const judge = runtime.data?.judge;
  const egressView = egress.data;
  const reasons: string[] = [];
  if (runtime.isPending || egress.isPending) {
    reasons.push("正在读取判读与出域配置…");
  }
  if (runtime.isError) reasons.push("运行时配置读取失败，无法确认精确模型");
  if (egress.isError) reasons.push("出域配置读取失败，无法确认授权");
  if (judge && judge.mode !== "cli") {
    reasons.push("判读模式为 mock：自动判读需要 cli 模式");
  }
  if (judge && !judge.modelId) {
    reasons.push("未配置精确 provider/model id");
  }
  if (egressView && egressView.policy === "none") {
    reasons.push("出域策略为 none：判读内容不得离开本机");
  } else if (egressView && egressView.policy === "redacted") {
    reasons.push("出域策略为 redacted：自动判读需要对该模型的 full 出域批准");
  }
  if (egressView && judge?.modelId && egressView.approvedModel !== judge.modelId) {
    reasons.push("已批准模型与 judge.modelId 不一致");
  }
  const prerequisitesReady =
    reasons.length === 0 && runtime.isSuccess && egress.isSuccess;

  return (
    <section className="settings-section" aria-labelledby="settings-archive">
      <h2 id="settings-archive" tabIndex={-1} className="panel-title">
        全可访问会话同步
      </h2>
      <p className="panel-note">
        只读当前账号近七天内有消息的会话；按原消息登记附件。下载、文字提取和任务交付分别核验。
        消息就绪后，「今日」会自动核对请求和引用回复，不需要开启下方的模型判读。
      </p>

      {!bridge ? (
        <p className="panel-note">
          未连接桌面端，无法读取归档状态；这里不显示模拟结果，也不假装同步成功。
        </p>
      ) : status.isPending ? (
        <LoadingNotice label="正在读取归档状态…" />
      ) : status.isError ? (
        <IpcErrorNotice
          title="归档状态读取失败"
          error={status.error}
          onRetry={() => void status.refetch()}
        />
      ) : current ? (
        <>
          <dl className="fact-grid">
            <div className="fact-cell">
              <dt>同步状态</dt>
              <dd>{running ? "同步中" : "空闲"}</dd>
            </div>
            <div className="fact-cell">
              <dt>最近成功</dt>
              <dd className="tnum">{formatDateTime(current.lastSuccessMs)}</dd>
            </div>
            <div className="fact-cell">
              <dt>登记会话</dt>
              <dd className="tnum">{current.conversations}</dd>
            </div>
            <div className="fact-cell">
              <dt>登记消息</dt>
              <dd className="tnum">{current.messages}</dd>
            </div>
            <div className="fact-cell">
              <dt>登记附件</dt>
              <dd className="tnum">{current.attachments}</dd>
            </div>
            <div className="fact-cell">
              <dt>下载成功</dt>
              <dd className="tnum">{current.downloaded}</dd>
            </div>
            <div className="fact-cell">
              <dt>下载失败</dt>
              <dd className={current.failed > 0 ? "tnum error-text" : "tnum"}>
                {current.failed}
              </dd>
            </div>
            <div className="fact-cell">
              <dt>自动判读</dt>
              <dd>{current.autoJudge ? "已开启" : "关闭"}</dd>
            </div>
          </dl>

          <p className="panel-note">范围：{current.scope || "后端未提供范围说明"}</p>
          <p className="panel-note" role="status" aria-live="polite">
            {summaryText(current)}
          </p>

          {current.error ? (
            <Notice tone="error" title="最近一次归档出错">
              <p>{current.error}</p>
            </Notice>
          ) : null}
          {current.failed > 0 ? (
            <Notice tone="warn" title={`${current.failed} 个附件下载失败`}>
              <p>
                失败附件没有落盘，不能作为判读证据；失败数不计入下载成功。
                再次同步会重新尝试，仍失败的条目会继续计入失败数。
              </p>
            </Notice>
          ) : null}
        </>
      ) : null}

      <div className="settings-actions">
        <button
          type="button"
          className="btn btn-secondary"
          aria-label="立即同步全可访问会话"
          disabled={!bridge || syncNow.isPending || running}
          title={
            !bridge
              ? "需要桌面端连接"
              : running
                ? "归档同步正在运行"
                : undefined
          }
          onClick={runSync}
        >
          {running
            ? "正在同步…"
            : syncNow.isPending
              ? "正在提交…"
              : "立即同步全可访问会话"}
        </button>
        <button
          type="button"
          className="btn btn-secondary"
          aria-label="刷新同步状态"
          disabled={!bridge || status.isFetching}
          onClick={() => void status.refetch()}
        >
          {status.isFetching ? "正在刷新…" : "刷新同步状态"}
        </button>
      </div>
      {bridge && current?.profile && <div>
        <label>
          <input type="checkbox" checked={current.periodicCapture} disabled={captureSet.isPending}
            onChange={event => captureSet.mutate({ profile: current.profile!, enabled: event.target.checked })} />
          每 15 分钟在本机同步此账号的全可访问会话和附件（{current.profile}）
        </label>
        <p className="panel-note">关闭窗口后从系统托盘重新打开。选择「退出 WorkContext」会停止采集，下次启动后按归档水位续传。</p>
        {captureSet.isError && <IpcErrorNotice title="定时同步设置未保存" error={captureSet.error} />}
      </div>}
      {current?.periodicCapture && !current.running && (!current.lastSuccessMs || Date.now() - current.lastSuccessMs > 30 * 60_000) &&
        <Notice tone="warn" title="归档同步滞后"><p>最近半小时缺少成功同步水位，工作记录可能尚未包含最新沟通。</p></Notice>}
      {pollStopped ? <p className="panel-note">{pollStopped}</p> : null}
      {syncNow.isError ? (
        <IpcErrorNotice title="同步未能启动" error={syncNow.error} />
      ) : null}

      <div className="settings-subhead">
        <h3>自动判读</h3>
      </div>
      <p className="panel-note">
        判读只选取必要的上下文：与该事项相关的会话片段、发件人与时间，以及判断归属所需
        的消息；必要时附带该条消息的附件抽取内容。不扫描未同步内容，出域策略照常生效。
      </p>

      <label className="checkbox-row">
        <input
          type="checkbox"
          checked={auto}
          disabled={!bridge || autoSet.isPending || (!auto && !prerequisitesReady)}
          onChange={(event) => {
            if (!bridge) return;
            autoSet.mutate(event.target.checked);
          }}
        />
        <span>允许自动判读新同步内容</span>
      </label>

      {reasons.length > 0 ? (
        <ul className="rule-list">
          {reasons.map((reason) => (
            <li key={reason}>
              <span>{reason}</span>
            </li>
          ))}
        </ul>
      ) : (
        <p className="panel-note">
          已具备精确 CLI 模型与同一模型的出域授权；开关状态以 archive_status 返回为准。
        </p>
      )}

      <div className="settings-actions">
        <button
          type="button"
          className="btn btn-secondary"
          aria-label="立即判读已同步内容"
          disabled={!bridge || !prerequisitesReady || judgeNow.isPending}
          title={
            !prerequisitesReady
              ? "需要精确 CLI 模型与已批准同一模型的出域策略"
              : undefined
          }
          onClick={() => judgeNow.mutate()}
        >
          {judgeNow.isPending ? "正在判读本轮数据…" : "立即判读已同步内容"}
        </button>
      </div>
      {judgeNow.isError ? (
        <IpcErrorNotice title="判读未取得有效回执" error={judgeNow.error} />
      ) : null}
      {judgeNow.isSuccess ? (
        <p className="panel-note">
          {judgeNow.data.status === "stopped" ? "本轮提前停止" : "本轮判读已返回"}：扫描 {judgeNow.data.scanned} 条，调用 {judgeNow.data.calls.length} 次。
          {judgeNow.data.calls.map((c, i) => <span key={i}> {c.status}{c.errorCode && `（${c.errorCode}）`}</span>)}
          {judgeNow.data.more && " 仍有未扫描记录。"}
        </p>
      ) : null}
    </section>
  );
}
