import { useEffect, useRef, useState } from "react";
import { hasDesktopBridge, type Severity, type SnapshotKind } from "../lib/ipc";
import { useCreateSnapshot } from "../lib/queries";
import { SNAPSHOT_KIND_LABEL, SEVERITY_LABEL } from "../lib/format";
import { IpcErrorNotice } from "./IpcNotice";

const KIND_OPTIONS = Object.keys(SNAPSHOT_KIND_LABEL) as SnapshotKind[];
const SEVERITY_OPTIONS: (Severity | "")[] = ["", "low", "medium", "high"];

export type IssueDefaults = {
  route: string;
  targetType?: string;
  targetId?: string;
};

export function RecordIssueDialog({
  open,
  onClose,
  defaults,
}: {
  open: boolean;
  onClose: () => void;
  defaults: IssueDefaults;
}) {
  const bridge = hasDesktopBridge();
  const create = useCreateSnapshot();
  const [description, setDescription] = useState("");
  const [expected, setExpected] = useState("");
  const [kind, setKind] = useState<SnapshotKind>("other");
  const [severity, setSeverity] = useState<Severity | "">("");
  const [createdId, setCreatedId] = useState<string | null>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    if (!open) return;
    setDescription("");
    setExpected("");
    setKind("other");
    setSeverity("");
    setCreatedId(null);
    create.reset();
    const timer = window.setTimeout(() => textareaRef.current?.focus(), 0);
    return () => window.clearTimeout(timer);
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !create.isPending) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose, create.isPending]);

  if (!open) return null;

  const canSubmit =
    bridge && description.trim().length > 0 && !create.isPending;

  const submit = () => {
    if (!canSubmit) return;
    create.mutate(
      {
        route: defaults.route,
        targetType: defaults.targetType,
        targetId: defaults.targetId,
        description: description.trim(),
        expected: expected.trim() || undefined,
        kind,
        severity: severity || undefined,
      },
      {
        onSuccess: (result) => setCreatedId(result.id),
      },
    );
  };

  return (
    <div
      className="modal-backdrop"
      role="presentation"
      onMouseDown={() => {
        if (!create.isPending) onClose();
      }}
    >
      <div
        className="modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby="issue-dialog-title"
        onMouseDown={(event) => event.stopPropagation()}
      >
        <div className="modal-head">
          <h2 id="issue-dialog-title" className="modal-title">
            记录问题
          </h2>
          <button
            type="button"
            className="btn btn-ghost btn-sm"
            onClick={onClose}
            aria-label="关闭"
            disabled={create.isPending}
          >
            关闭
          </button>
        </div>

        <p className="modal-note">
          只记录工作台自身的缺陷，不写任务、不做排期。默认脱敏，不附带整段聊天正文。
          触发页面：<code>{defaults.route}</code>
          {defaults.targetId ? (
            <>
              {" "}· 关联对象：<code>{defaults.targetId}</code>
            </>
          ) : null}
        </p>

        {!bridge ? (
          <p className="form-error">未连接桌面端，当前不能写入快照。</p>
        ) : null}

        {createdId ? (
          <div className="modal-success" role="status" aria-live="polite">
            <p>已记录到迭代池：<code>{createdId}</code></p>
            <button type="button" className="btn btn-secondary btn-sm" onClick={onClose}>
              关闭
            </button>
          </div>
        ) : (
          <>
            <label className="field">
              <span className="field-label">哪里错（必填）</span>
              <textarea
                ref={textareaRef}
                className="field-input"
                rows={3}
                value={description}
                maxLength={2000}
                onChange={(event) => setDescription(event.target.value)}
                placeholder="一句话说明问题，例如：把点名同事的群任务算到了我头上"
              />
            </label>

            <label className="field">
              <span className="field-label">期望的正确结果</span>
              <textarea
                className="field-input"
                rows={2}
                value={expected}
                maxLength={2000}
                onChange={(event) => setExpected(event.target.value)}
                placeholder="例如：不生成我的事项，或落到待确认"
              />
            </label>

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
                  onChange={(event) =>
                    setSeverity(event.target.value as Severity | "")
                  }
                >
                  {SEVERITY_OPTIONS.map((option) => (
                    <option key={option || "unset"} value={option}>
                      {option ? SEVERITY_LABEL[option] : "不填"}
                    </option>
                  ))}
                </select>
              </label>
            </div>

            {create.isError ? (
              <IpcErrorNotice title="记录失败" error={create.error} />
            ) : null}

            <div className="modal-actions">
              <button type="button" className="btn btn-ghost" onClick={onClose}>
                取消
              </button>
              <button
                type="button"
                className="btn btn-primary"
                disabled={!canSubmit}
                onClick={submit}
              >
                {create.isPending ? "正在写入…" : "写入迭代池"}
              </button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}
