import { useEffect, useState } from "react";
import { Link, Navigate, useLocation, useNavigate } from "react-router-dom";
import { EgressPanel } from "../components/EgressPanel";
import {
  IpcErrorNotice,
  LoadingNotice,
  NotConnectedNotice,
} from "../components/IpcNotice";
import { RuntimePanel } from "../components/RuntimePanel";
import { SourcePanel } from "../components/SourcePanel";
import { ArchivePanel } from "../components/ArchivePanel";
import { LocalProgress } from "../components/LocalProgress";
import { OwnershipSettings } from "../components/OwnershipSettings";
import { OrganizerSettings } from "../components/OrganizerSettings";
import ExternalAccess from "../components/ExternalAccess";
import { ExternalSuggestions } from "../components/ExternalSuggestions";
import { hasDesktopBridge, toIpcError, type Diagnostics } from "../lib/ipc";
import { formatDateTime, formatSize, SYNC_STATE_LABEL } from "../lib/format";
import {
  useBackupNow,
  useBackups,
  useDbInfo,
  useDiagnostics,
  useExportDiagnostics,
  useRestore,
} from "../lib/queries";

type Feedback =
  | { tone: "ok"; text: string }
  | { tone: "error"; text: string; error: unknown }
  | null;

const SETTINGS_GROUPS = [
  {
    label: "个人工作台",
    sections: [["ownership-settings", "会话整理"], ["organizer-heading", "工作上下文"], ["settings-runtime", "身份与模型"]],
  },
  {
    label: "数据与权限",
    sections: [["settings-egress", "出域策略"], ["external-access-title", "本地外部接口"], ["external-suggestions", "项目与领域建议"]],
  },
  {
    label: "本机维护",
    sections: [["settings-backup", "备份恢复"], ["settings-diagnostics", "诊断记录"], ["settings-about", "版本信息"]],
  },
] as const;

function focusSection(id: string) {
  const heading = document.getElementById(id);
  if (!heading?.matches(".settings-section > h2")) return false;
  heading.tabIndex = -1;
  heading.focus({ preventScroll: true });
  heading.scrollIntoView?.({ block: "start" });
  return true;
}

function SettingsIndex({ bridge, sources = false }: { bridge: boolean; sources?: boolean }) {
  const { hash } = useLocation();
  const navigate = useNavigate();
  const [current, setCurrent] = useState<string>(sources ? "settings-sources" : bridge ? "ownership-settings" : "settings-runtime");

  useEffect(() => {
    const id = hash.slice(1);
    if (id && focusSection(id)) setCurrent(id);
  }, [hash]);

  useEffect(() => {
    if (typeof IntersectionObserver === "undefined") return;
    const panels = Array.from(document.querySelectorAll(".settings-body > section"));
    const visible = new Set<Element>();
    const observer = new IntersectionObserver((entries) => {
      for (const entry of entries) {
        if (entry.isIntersecting) visible.add(entry.target);
        else visible.delete(entry.target);
      }
      const first = panels.find((panel) => visible.has(panel));
      const id = first?.getAttribute("aria-labelledby");
      if (id) setCurrent(id);
    }, { rootMargin: "-70px 0px 0px 0px" });
    for (const panel of panels) observer.observe(panel);
    return () => observer.disconnect();
  }, []);

  const sections = (sources ? [{label: "数据来源", sections: [["settings-sources", "来源与同步"], ["settings-archive", "会话归档"], ["source-progress", "整理与诊断"]]}] : SETTINGS_GROUPS).map((group) => ({
    ...group,
    sections: group.sections.filter(([id]) => bridge || !["ownership-settings", "organizer-heading", "external-access-title", "external-suggestions", "source-progress"].includes(id)),
  }));

  const choose = (id: string) => {
    setCurrent(id);
    if (hash === `#${id}`) focusSection(id);
    else navigate({ hash: `#${id}` });
  };

  return (
    <nav className="settings-index" aria-label="设置分区">
      <select className="settings-jump" aria-label="跳转到设置分区" value={current} onChange={(event) => choose(event.target.value)}>
        {sections.map((group) => (
          <optgroup key={group.label} label={group.label}>
            {group.sections.map(([id, label]) => <option key={id} value={id}>{label}</option>)}
          </optgroup>
        ))}
      </select>
      <div className="settings-index-groups">
        {sections.map((group) => (
          <div key={group.label} className="settings-index-group">
            <p>{group.label}</p>
            {group.sections.map(([id, label]) => (
              <Link
                key={id}
                to={{ hash: `#${id}` }}
                aria-current={current === id ? "location" : undefined}
                onClick={() => { if (hash === `#${id}`) focusSection(id); }}
              >
                {label}
              </Link>
            ))}
          </div>
        ))}
      </div>
    </nav>
  );
}

function FeedbackLine({ feedback }: { feedback: Feedback }) {
  if (!feedback) return null;
  if (feedback.tone === "ok") {
    return (
      <div className="inline-feedback ok" role="status" aria-live="polite">
        {feedback.text}
      </div>
    );
  }
  return <IpcErrorNotice title="操作失败" error={feedback.error} />;
}

export function SourcesPage() {
  const bridge = hasDesktopBridge();
  return <div className="page settings-page"><header className="page-header"><h1 className="page-title">数据来源</h1></header>
    {!bridge && <NotConnectedNotice />}
    <div className="settings-layout"><SettingsIndex bridge={bridge} sources /><div className="settings-body">
      <SourcePanel /><ArchivePanel />
      {bridge && <section className="settings-section" aria-labelledby="source-progress"><h2 id="source-progress" className="panel-title">整理与诊断</h2>
        <LocalProgress /><p><Link to="/settings#organizer-heading">自动整理设置</Link> · <Link to="/settings#settings-diagnostics">本机诊断记录</Link></p>
      </section>}
    </div></div></div>;
}

export function SettingsPage() {
  const {hash,search} = useLocation();
  if (["#settings-sources","#settings-archive"].includes(hash)) return <Navigate replace to={{pathname:"/sources",hash,search}} />;
  return <SettingsContent />;
}

function SettingsContent() {
  const bridge = hasDesktopBridge();
  const dbInfo = useDbInfo();
  const backups = useBackups();
  const diagnostics = useDiagnostics();
  const backupNow = useBackupNow();
  const restore = useRestore();
  const exportDiagnostics = useExportDiagnostics();

  const [selectedBackup, setSelectedBackup] = useState("");
  const [restoreToken, setRestoreToken] = useState("");
  const [backupFeedback, setBackupFeedback] = useState<Feedback>(null);
  const [restoreFeedback, setRestoreFeedback] = useState<Feedback>(null);
  const [diagFeedback, setDiagFeedback] = useState<Feedback>(null);
  const [inspectProgress, setInspectProgress] = useState(false);

  const canRestore =
    bridge && selectedBackup.length > 0 && restoreToken === "恢复" && !restore.isPending;

  const runBackup = () => {
    setBackupFeedback(null);
    backupNow.mutate(undefined, {
      onSuccess: (result) =>
        setBackupFeedback({ tone: "ok", text: `已创建备份：${result.path}` }),
      onError: (error) =>
        setBackupFeedback({ tone: "error", text: toIpcError(error).message, error }),
    });
  };

  const runRestore = () => {
    if (!canRestore) return;
    setRestoreFeedback(null);
    restore.mutate(
      { path: selectedBackup, token: restoreToken },
      {
        onSuccess: (result) => {
          if (result.status === "cancelled") {
            setRestoreFeedback({
              tone: "ok",
              text: "已取消（原生确认框未确认），当前数据库未改动。",
            });
            return;
          }
          setRestoreFeedback({
            tone: "ok",
            text: result.restarted
              ? `已恢复并重启。${result.preRestoreBackupPath ? `恢复前备份：${result.preRestoreBackupPath}` : ""}`
              : `已恢复；未自动重启，请重启应用后核对数据。${result.preRestoreBackupPath ? `恢复前备份：${result.preRestoreBackupPath}` : ""}`,
          });
        },
        onError: (error) =>
          setRestoreFeedback({ tone: "error", text: toIpcError(error).message, error }),
      },
    );
  };

  const runDiagnosticsExport = () => {
    setDiagFeedback(null);
    exportDiagnostics.mutate(undefined, {
      onSuccess: (result) =>
        setDiagFeedback(
          result.status === "saved"
            ? { tone: "ok", text: `诊断包已导出：${result.path}` }
            : { tone: "ok", text: "已取消，未写入文件。" },
        ),
      onError: (error) =>
        setDiagFeedback({ tone: "error", text: toIpcError(error).message, error }),
    });
  };

  return (
    <div className="page settings-page">
      <header className="page-header">
        <h1 className="page-title">设置</h1>
        <p className="page-subtitle">
          管理本人身份、模型授权与本机维护。
        </p>
      </header>

      {!bridge ? <NotConnectedNotice /> : null}

      <div className="settings-layout">
        <SettingsIndex bridge={bridge} />
        <div className="settings-body">
          {bridge && <OwnershipSettings />}
          {bridge && <OrganizerSettings />}
          {bridge && <ExternalAccess />}
          {bridge && <ExternalSuggestions />}
          <RuntimePanel />
          <Link to="/sources">管理数据来源与同步</Link>
          <EgressPanel />

          <section className="settings-section" aria-labelledby="settings-backup">
            <h2 id="settings-backup" className="panel-title">
              备份与恢复
            </h2>
            <p className="panel-note">
              备份只包含本地数据库，不含附件。恢复前会校验 sha256 与完整性，并先备份当前库。
            </p>

            <div className="settings-actions">
              <button
                type="button"
                className="btn btn-secondary"
                disabled={!bridge || backupNow.isPending}
                title={bridge ? undefined : "需要桌面端连接"}
                onClick={runBackup}
              >
                {backupNow.isPending ? "正在备份…" : "立即备份"}
              </button>
              <button
                type="button"
                className="btn btn-secondary"
                disabled={!bridge || backups.isFetching}
                onClick={() => void backups.refetch()}
              >
                {backups.isFetching ? "正在刷新…" : "刷新备份列表"}
              </button>
            </div>
            <FeedbackLine feedback={backupFeedback} />

            {!bridge ? null : backups.isPending ? (
              <LoadingNotice label="正在读取备份列表…" />
            ) : backups.isError ? (
              <IpcErrorNotice
                title="备份列表读取失败"
                error={backups.error}
                onRetry={() => void backups.refetch()}
              />
            ) : backups.data.length === 0 ? (
              <p className="settings-empty">本地还没有备份。备份不会自动创建，除非手动或迁移前触发。</p>
            ) : (
              <>
                <div className="settings-table-wrap">
                  <table className="settings-table">
                    <thead>
                      <tr>
                        <th scope="col">选择</th>
                        <th scope="col">时间</th>
                        <th scope="col">大小</th>
                        <th scope="col">schema</th>
                        <th scope="col">应用版本</th>
                        <th scope="col">校验</th>
                        <th scope="col">路径</th>
                      </tr>
                    </thead>
                    <tbody>
                      {backups.data.map((backup) => (
                        <tr key={backup.path}>
                          <td>
                            <input
                              type="radio"
                              name="backup"
                              checked={selectedBackup === backup.path}
                              onChange={() => setSelectedBackup(backup.path)}
                              aria-label={`选择备份 ${formatDateTime(backup.createdAt)}`}
                            />
                          </td>
                          <td className="tnum">{formatDateTime(backup.createdAt)}</td>
                          <td className="tnum">{formatSize(backup.sizeBytes)}</td>
                          <td className="tnum">{backup.schemaVersion}</td>
                          <td>{backup.appVersion}</td>
                          <td>
                            {backup.sha256Verified === true
                              ? "已通过"
                              : backup.sha256Verified === false
                                ? "未通过"
                                : "未校验"}
                          </td>
                          <td className="cell-path">{backup.path}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>

                <div className="restore-box">
                  <p className="restore-warning">
                    恢复是破坏性操作：会以所选备份覆盖当前库。点击后桌面端还会弹出系统原生确认框；
                    取消则不改动任何数据。
                  </p>
                  <label className="field">
                    <span className="field-label">
                      输入「恢复」以启用按钮（第一道防误触）
                    </span>
                    <input
                      className="field-input"
                      value={restoreToken}
                      onChange={(event) => setRestoreToken(event.target.value)}
                      placeholder="恢复"
                      aria-label="输入恢复以确认"
                    />
                  </label>
                  <button
                    type="button"
                    className="btn btn-danger"
                    disabled={!canRestore}
                    title={
                      !selectedBackup
                        ? "先选择一个备份"
                        : restoreToken !== "恢复"
                          ? "输入「恢复」后才能启用"
                          : undefined
                    }
                    onClick={runRestore}
                  >
                    {restore.isPending ? "正在恢复…" : "从所选备份恢复"}
                  </button>
                  <FeedbackLine feedback={restoreFeedback} />
                </div>
              </>
            )}
          </section>

          <section className="settings-section" aria-labelledby="settings-diagnostics">
            <h2 id="settings-diagnostics" className="panel-title">
              诊断
            </h2>
            <p className="panel-note">
              只读本地库与配置摘要；诊断包不含聊天正文。不会上传到任何云端。
            </p>

            <div className="settings-actions">
              <button
                type="button"
                className="btn btn-secondary"
                disabled={!bridge || exportDiagnostics.isPending}
                onClick={runDiagnosticsExport}
              >
                {exportDiagnostics.isPending ? "正在导出…" : "导出诊断包"}
              </button>
              <button
                type="button"
                className="btn btn-secondary"
                disabled={!bridge || diagnostics.isFetching}
                onClick={() => void diagnostics.refetch()}
              >
                {diagnostics.isFetching ? "正在刷新…" : "刷新诊断"}
              </button>
            </div>
            <FeedbackLine feedback={diagFeedback} />

            {!bridge ? null : diagnostics.isPending ? (
              <LoadingNotice label="正在执行本地诊断…" />
            ) : diagnostics.isError ? (
              <IpcErrorNotice
                title="诊断读取失败"
                error={diagnostics.error}
                onRetry={() => void diagnostics.refetch()}
              />
            ) : (
              <DiagnosticsPanel diagnostics={diagnostics.data} />
            )}
            {bridge && <details onToggle={e => setInspectProgress(e.currentTarget.open)}>
              <summary>查看归档核验记录</summary>
              {inspectProgress && <LocalProgress />}
            </details>}
          </section>

          <section className="settings-section" aria-labelledby="settings-about">
            <h2 id="settings-about" className="panel-title">
              关于
            </h2>
            <dl className="detail-facts compact">
              <div>
                <dt>数据库</dt>
                <dd className="cell-path">{dbInfo.data?.dbPath ?? "不可用"}</dd>
              </div>
              <div>
                <dt>schema 版本</dt>
                <dd className="tnum">
                  {dbInfo.data?.schemaVersion ?? diagnostics.data?.app.schemaVersion ?? "不可用"}
                </dd>
              </div>
              <div>
                <dt>应用版本</dt>
                <dd>{diagnostics.data?.app.version ?? "不可用"}</dd>
              </div>
              <div>
                <dt>平台</dt>
                <dd>{diagnostics.data?.app.platform ?? "不可用"}</dd>
              </div>
            </dl>
            <p className="panel-note">
              V0.1 仅个人受控试运行。不提供企业登录、自动发消息、自动审批或自动发布。
            </p>
          </section>
        </div>
      </div>
    </div>
  );
}

function DiagnosticsPanel({ diagnostics }: { diagnostics: Diagnostics | undefined }) {
  if (!diagnostics) return null;
  const counts = diagnostics.db.counts;
  const judge = diagnostics.judge;
  return (
    <>
      <dl className="fact-grid">
        <div className="fact-cell">
          <dt>数据库完整性</dt>
          <dd className={diagnostics.db.integrity === "ok" ? "ok-text" : "error-text"}>
            {diagnostics.db.integrity === "ok" ? "ok" : "失败"}
          </dd>
        </div>
        <div className="fact-cell">
          <dt>数据库大小</dt>
          <dd className="tnum">{formatSize(diagnostics.db.sizeBytes)}</dd>
        </div>
        <div className="fact-cell">
          <dt>schema</dt>
          <dd className="tnum">
            {diagnostics.app.schemaVersion} / 支持 {diagnostics.app.supportedSchemaVersion}
          </dd>
        </div>
        <div className="fact-cell">
          <dt>备份</dt>
          <dd className="tnum">
            {diagnostics.backups.count} 个
            {diagnostics.backups.last
              ? ` · 最近 ${formatDateTime(diagnostics.backups.last.createdAt)}`
              : ""}
          </dd>
        </div>
        <div className="fact-cell">
          <dt>记录数</dt>
          <dd className="tnum">
            来源 {counts.sourceRecord} / 事项 {counts.workItem} / 事件 {counts.workEvent} /
            快照 {counts.feedbackSnapshot}
          </dd>
        </div>
        <div className="fact-cell">
          <dt>判读（{judge.windowDays} 天）</dt>
          <dd className="tnum">
            ok {judge.ok} / 拒绝 {judge.rejected} / 失败 {judge.failed} / 超时 {judge.timeout}
          </dd>
        </div>
      </dl>

      {diagnostics.db.integrity === "failed" ? (
        <div className="inline-feedback error" role="alert">
          数据库完整性校验未通过：
          {diagnostics.db.integrityDetail ?? "未提供详情"}。请先备份并从可用备份恢复。
        </div>
      ) : null}

      <div className="settings-subhead">
        <h3>拒绝规则分布</h3>
      </div>
      {judge.rejectionRules.length === 0 ? (
        <p className="panel-note">近 {judge.windowDays} 天没有拒绝记录。</p>
      ) : (
        <ul className="rule-list">
          {judge.rejectionRules.map((rule) => (
            <li key={rule.rule}>
              <code>{rule.rule}</code>
              <span className="tnum">{rule.count}</span>
            </li>
          ))}
        </ul>
      )}

      {diagnostics.sources.length > 0 ? (
        <p className="panel-note">
          来源状态：
          {diagnostics.sources
            .map(
              (source) =>
                `${source.displayName}（${SYNC_STATE_LABEL[source.state]}，最后成功 ${formatDateTime(source.lastSuccessAt)}）`,
            )
            .join("；")}
        </p>
      ) : (
        <p className="panel-note">没有来源检查点。</p>
      )}
    </>
  );
}
