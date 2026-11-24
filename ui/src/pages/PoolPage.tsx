import { useMemo, useState } from "react";
import {
  IpcErrorNotice,
  LoadingNotice,
  NotConnectedNotice,
} from "../components/IpcNotice";
import {
  hasDesktopBridge,
  toIpcError,
  type ExportFormat,
  type Severity,
  type Snapshot,
  type SnapshotKind,
  type SnapshotStatus,
} from "../lib/ipc";
import {
  SNAPSHOT_KIND_LABEL,
  SNAPSHOT_STATUS_LABEL,
  SEVERITY_LABEL,
  formatDateTime,
} from "../lib/format";
import {
  useCreateSnapshot,
  useExportSnapshots,
  useSnapshots,
} from "../lib/queries";

const KIND_OPTIONS = Object.keys(SNAPSHOT_KIND_LABEL) as SnapshotKind[];
const SEVERITY_OPTIONS: (Severity | "")[] = ["", "low", "medium", "high"];

const FLOW = ["发现缺陷", "记录快照", "导出", "OMP 修复", "回归并标版本"];

const SNAPSHOT_FIELDS: [string, string][] = [
  ["page", "触发页面路径"],
  ["item_ref", "关联事项 / 消息引用 ID"],
  ["problem", "哪里错：一句话即可"],
  ["expected", "期望的正确结果"],
  ["app_version", "记录时的软件版本"],
  ["status", "未处理 / 已解决"],
  ["created_at", "记录时间"],
];

const STATUS_ORDER: SnapshotStatus[] = ["open", "resolved", "wontfix"];

type Feedback = { tone: "ok" | "error"; text: string; error?: unknown } | null;

function CreateForm() {
  const bridge = hasDesktopBridge();
  const create = useCreateSnapshot();
  const [description, setDescription] = useState("");
  const [expected, setExpected] = useState("");
  const [kind, setKind] = useState<SnapshotKind>("ui");
  const [severity, setSeverity] = useState<Severity | "">("");
  const [feedback, setFeedback] = useState<Feedback>(null);

  const canSubmit = bridge && description.trim().length > 0 && !create.isPending;

  const submit = () => {
    if (!canSubmit) return;
    setFeedback(null);
    create.mutate(
      {
        route: "/pool",
        description: description.trim(),
        expected: expected.trim() || undefined,
        kind,
        severity: severity || undefined,
      },
      {
        onSuccess: (result) => {
          setDescription("");
          setExpected("");
          setKind("ui");
          setSeverity("");
          setFeedback({ tone: "ok", text: `已记录：${result.title}` });
        },
        onError: (error) =>
          setFeedback({ tone: "error", text: toIpcError(error).message, error }),
      },
    );
  };

  return (
    <section className="pool-create" aria-labelledby="pool-create-title">
      <h2 id="pool-create-title" className="panel-title">
        记录一条问题
      </h2>
      <p className="panel-note">
        只记录工作台自身的缺陷。默认脱敏，不附带整段聊天正文；导出前可再确认。
      </p>
      <div className="field-row">
        <label className="field grow">
          <span className="field-label">哪里错（必填）</span>
          <textarea
            className="field-input"
            rows={2}
            value={description}
            maxLength={2000}
            onChange={(event) => setDescription(event.target.value)}
            placeholder="例如：导出按钮在未选中快照时仍可点击"
          />
        </label>
        <label className="field grow">
          <span className="field-label">期望的正确结果</span>
          <textarea
            className="field-input"
            rows={2}
            value={expected}
            maxLength={2000}
            onChange={(event) => setExpected(event.target.value)}
            placeholder="例如：没有选中时禁用并说明原因"
          />
        </label>
      </div>
      <div className="field-row">
        <label className="field">
          <span className="field-label">问题类型</span>
          <select
            className="field-input"
            value={kind}
            onChange={(event) => setKind(event.target.value as SnapshotKind)}
          >
            {KIND_OPTIONS.map((option) => (
              <option key={option} value={option}>
                {SNAPSHOT_KIND_LABEL[option]}
              </option>
            ))}
          </select>
        </label>
        <label className="field">
          <span className="field-label">严重程度</span>
          <select
            className="field-input"
            value={severity}
            onChange={(event) => setSeverity(event.target.value as Severity | "")}
          >
            {SEVERITY_OPTIONS.map((option) => (
              <option key={option || "unset"} value={option}>
                {option ? SEVERITY_LABEL[option] : "不填"}
              </option>
            ))}
          </select>
        </label>
        <div className="field align-end">
          <button
            type="button"
            className="btn btn-primary"
            disabled={!canSubmit}
            title={bridge ? undefined : "需要桌面端连接"}
            onClick={submit}
          >
            {create.isPending ? "正在写入…" : "写入迭代池"}
          </button>
        </div>
      </div>

      {feedback ? (
        feedback.tone === "ok" ? (
          <div className="inline-feedback ok" role="status" aria-live="polite">
            {feedback.text}
          </div>
        ) : feedback.error ? (
          <IpcErrorNotice title="记录失败" error={feedback.error} />
        ) : (
          <div className="inline-feedback error" role="alert">
            {feedback.text}
          </div>
        )
      ) : null}
    </section>
  );
}

function SnapshotRow({
  snapshot,
  selected,
  onToggle,
}: {
  snapshot: Snapshot;
  selected: boolean;
  onToggle: () => void;
}) {
  return (
    <li className="snapshot-row" data-selected={selected || undefined}>
      <label className="snapshot-check">
        <input
          type="checkbox"
          checked={selected}
          onChange={onToggle}
          aria-label={`选择 ${snapshot.title}`}
        />
      </label>
      <div className="snapshot-main">
        <div className="snapshot-top">
          <strong>{snapshot.title}</strong>
          <span className="snapshot-time tnum">{formatDateTime(snapshot.createdAt)}</span>
        </div>
        <p className="snapshot-desc">{snapshot.description}</p>
        {snapshot.expected ? (
          <p className="snapshot-expected">
            <span>期望</span>
            {snapshot.expected}
          </p>
        ) : null}
        <p className="snapshot-meta tnum">
          <span>版本 {snapshot.appVersion}</span>
          <span>schema {snapshot.schemaVersion}</span>
          {snapshot.kind ? <span>{SNAPSHOT_KIND_LABEL[snapshot.kind as SnapshotKind] ?? snapshot.kind}</span> : null}
          {snapshot.severity ? <span>严重度 {SEVERITY_LABEL[snapshot.severity]}</span> : null}
          <span>{snapshot.redaction === "masked" ? "默认脱敏" : "含未脱敏内容"}</span>
          {snapshot.route ? <span>{snapshot.route}</span> : null}
        </p>
      </div>
    </li>
  );
}

export function PoolPage() {
  const bridge = hasDesktopBridge();
  const snapshots = useSnapshots();
  const exporter = useExportSnapshots();
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [includeBody, setIncludeBody] = useState(false);
  const [bodyPhrase, setBodyPhrase] = useState("");
  const [feedback, setFeedback] = useState<Feedback>(null);

  const all = snapshots.data ?? [];
  const grouped = useMemo(() => {
    const map = new Map<SnapshotStatus, Snapshot[]>();
    for (const status of STATUS_ORDER) map.set(status, []);
    for (const snapshot of all) {
      const bucket = map.get(snapshot.status);
      if (bucket) bucket.push(snapshot);
      else map.get("open")?.push(snapshot);
    }
    return map;
  }, [all]);

  const toggle = (id: string) => {
    setSelected((current) => {
      const next = new Set(current);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const selectAllOpen = () => {
    const openIds = grouped.get("open")?.map((snapshot) => snapshot.id) ?? [];
    setSelected(new Set(openIds));
  };

  const bodyConfirmed = !includeBody || bodyPhrase.trim() === "正文";
  const canExport =
    bridge &&
    selected.size > 0 &&
    bodyConfirmed &&
    !exporter.isPending;
  const exportHint = !bridge ? "需要桌面端连接"
    : selected.size === 0 ? "先选择至少一条快照"
    : !bodyConfirmed ? "输入「正文」确认后才能导出含正文内容"
    : undefined;

  const runExport = (format: ExportFormat) => {
    if (!canExport) return;
    setFeedback(null);
    exporter.mutate(
      { ids: [...selected], format, includeBody },
      {
        onSuccess: (result) => {
          if (result.status === "cancelled") {
            setFeedback({ tone: "ok", text: "已取消，未写入文件。" });
            return;
          }
          setFeedback({ tone: "ok", text: `已导出：${result.path}` });
        },
        onError: (error) =>
          setFeedback({ tone: "error", text: toIpcError(error).message, error }),
      },
    );
  };

  return (
    <div className="page pool-page">
      <header className="page-header">
        <h1 className="page-title">迭代反馈</h1>
        <p className="page-subtitle">记录工作台的缺陷快照，集中回看、导出和修复。</p>
      </header>

      <div className="pool-layout">
        <CreateForm />
        <div className="pool-records">
          <h2 className="pool-list-title">纠错快照</h2>
          {all.length > 0 && (
            <div className="selection-tools">
              <button type="button" className="btn btn-ghost btn-sm" disabled={!grouped.get("open")?.length} onClick={selectAllOpen}>
                全选未处理
              </button>
              <button
                type="button"
                className="btn btn-ghost btn-sm"
                disabled={selected.size === 0}
                onClick={() => setSelected(new Set())}
              >
                清空选择
              </button>
              <span className="tnum selection-count">已选 {selected.size} 条</span>
            </div>
          )}
          {!bridge ? <NotConnectedNotice /> : snapshots.isPending ? (
            <LoadingNotice label="正在读取迭代池…" />
          ) : snapshots.isError ? (
            <IpcErrorNotice
              title="迭代池读取失败"
              error={snapshots.error}
              onRetry={() => void snapshots.refetch()}
            />
          ) : all.length === 0 ? (
            <div className="pool-empty-area">
              <p className="empty-title">
                迭代池是空的
              </p>
              <p className="empty-detail">
                还没有纠错快照。记录一条问题后，这里会显示问题描述、记录时间与当时的软件版本；
                导出前可先预览字段，默认不带聊天正文。
              </p>
            </div>
          ) : (
            STATUS_ORDER.map((status) => {
              const rows = grouped.get(status) ?? [];
              if (rows.length === 0) return null;
              return (
                <section key={status} className="snapshot-group" aria-labelledby={`snap-${status}`}>
                  <div className="snapshot-group-head">
                    <h3 id={`snap-${status}`}>{SNAPSHOT_STATUS_LABEL[status]}</h3>
                    <span className="lane-count">{rows.length}</span>
                  </div>
                  <ul className="snapshot-list">
                    {rows.map((snapshot) => (
                      <SnapshotRow
                        key={snapshot.id}
                        snapshot={snapshot}
                        selected={selected.has(snapshot.id)}
                        onToggle={() => toggle(snapshot.id)}
                      />
                    ))}
                  </ul>
                </section>
              );
            })
          )}
          <section className="export-controls" aria-labelledby="pool-export-title">
            <h2 id="pool-export-title" className="panel-title">
              导出选项
            </h2>
            <p className="panel-note">
              文件位置由桌面端原生保存对话框选择；取消不会写入文件。默认只导出问题描述、引用与期望。
            </p>
            <label className="checkbox-row">
              <input
                type="checkbox"
                checked={includeBody}
                onChange={(event) => {
                  setIncludeBody(event.target.checked);
                  if (!event.target.checked) setBodyPhrase("");
                }}
              />
              <span>包含原始正文（敏感，默认关闭）</span>
            </label>
            {includeBody ? (
              <div className="body-confirm">
                <p className="body-warning">
                  导出含正文时可能带出企业聊天内容。请输入「正文」确认；后端仍会按脱敏策略处理。
                </p>
                <input
                  className="field-input"
                  value={bodyPhrase}
                  onChange={(event) => setBodyPhrase(event.target.value)}
                  placeholder="输入：正文"
                  aria-label="输入正文以确认导出含正文"
                />
              </div>
            ) : null}
            <div className="export-group" role="group" aria-label="导出快照">
              <button type="button" className="btn btn-secondary" disabled={!canExport} title={exportHint} onClick={() => runExport("md")}>
                导出 Markdown
              </button>
              <button type="button" className="btn btn-secondary" disabled={!canExport} title={exportHint} onClick={() => runExport("json")}>
                导出 JSON
              </button>
            </div>
            {feedback ? (
              feedback.tone === "ok" ? (
                <div className="inline-feedback ok" role="status" aria-live="polite">
                  {feedback.text}
                </div>
              ) : feedback.error ? (
                <IpcErrorNotice title="导出失败" error={feedback.error} />
              ) : (
                <div className="inline-feedback error" role="alert">
                  {feedback.text}
                </div>
              )
            ) : null}
          </section>
        </div>
      </div>

      <details className="pool-about">
        <summary>迭代池怎么用</summary>
        <p className="pool-about-body">
          判断错了就当场纠正；把那一刻的输入、判断和期望留下，等手头空了再一起交给 OMP 改代码。
          日常不必切进开发状态，也不必为每条纠错都建工单。
        </p>
        <div className="pool-flow">
          {FLOW.map((step, index) => (
            <span key={step} className="pool-flow-step">
              {index > 0 ? (
                <span className="pool-sep" aria-hidden="true">
                  →
                </span>
              ) : null}
              <span className="pool-step">{step}</span>
            </span>
          ))}
        </div>
      </details>
      <details className="pool-fields">
        <summary>快照字段（导出的最小集合）</summary>
        <dl>
          {SNAPSHOT_FIELDS.map(([field, meaning]) => (
            <div key={field}>
              <dt>{field}</dt>
              <dd>{meaning}</dd>
            </div>
          ))}
        </dl>
        <p className="panel-note">
          导出为 Markdown（人读）与 JSON（可复放）两种表示，均附软件版本；原文片段需你确认后才附上。
        </p>
      </details>
    </div>
  );
}
