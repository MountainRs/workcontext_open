import { useEffect, useState } from "react";
import { Link, useParams } from "react-router-dom";
import {
  IpcErrorNotice,
  LoadingNotice,
  NotConnectedNotice,
} from "../components/IpcNotice";
import { RecordIssueDialog } from "../components/RecordIssueDialog";
import {
  hasDesktopBridge,
  toIpcError,
  type CloseEvidenceType,
  type CorrectionArgs,
  type Delivery,
  type FollowupType,
  type ItemDetail,
} from "../lib/ipc";
import { useContextScopes } from "../lib/context";
import {
  FOLLOWUP_LABEL,
  formatDateTime,
  followupText,
  itemStatusColor,
  itemStatusLabel,
  localInputToMs,
  msToLocalInput,
} from "../lib/format";
import { useChangeDelivery, useCloseItem, useCorrectItem, useItem, useSetFollowup } from "../lib/queries";

const EVENT_LABEL: Record<string, string> = {
  created: "创建",
  updated: "更新",
  closed: "收尾",
  ignored: "忽略",
  corrected: "人工纠错",
  relinked: "重新关联",
  rejected: "判读被拒",
  synced: "同步",
  restored: "恢复",
};

const ACTOR_LABEL: Record<string, string> = {
  system: "系统",
  agent: "判读",
  human: "你",
};

const CLOSE_OPTIONS: { value: CloseEvidenceType; label: string; hint: string }[] = [
  {
    value: "explicit_complete",
    label: "对方明确表示已完成",
    hint: "轻事项仍按规则校验；长期事项可能转人工确认。",
  },
  {
    value: "artifact_accepted",
    label: "交付物被接收方确认",
    hint: "只有文件出现不算。",
  },
  {
    value: "human_confirmed",
    label: "我人工确认已完成",
    hint: "写入人工事件，可从时间线追溯。",
  },
];

type Feedback =
  | { tone: "ok"; text: string }
  | { tone: "error"; text: string; error: unknown }
  | null;

const STATE_LABEL = { mine: "轮到我", waiting: "等别人", triage: "待认领", done: "已完成", ignored: "已忽略" };
type DeliveryField = "project" | "conclusion" | "mine" | "waiting";

function ProjectPicker({ delivery, id, close }: { delivery: Delivery; id: string; close: () => void }) {
  const scopes = useContextScopes();
  const mutation = useChangeDelivery(id);
  const initialSelection = delivery.scopeId ? `scope:${delivery.scopeId}` : "keep";
  const [selection, setSelection] = useState(initialSelection);
  const [version] = useState(delivery.version);
  const stale = delivery.version !== version;
  const projects = scopes.data?.filter(scope => scope.kind === "project") ?? [];
  return <form className="delivery-controls" onSubmit={event => {
    event.preventDefault();
    if (stale || !scopes.isSuccess || mutation.isPending) return;
    if (selection === initialSelection) return close();
    const scopeId = selection === "unlink" ? null : selection.slice("scope:".length);
    mutation.mutate({ version, change: { scopeId } }, { onSuccess: close });
  }}>
    <label className="field"><span className="field-label">选择已有项目</span>
      <select autoFocus className="field-input" value={selection} disabled={!scopes.isSuccess || mutation.isPending}
        onChange={event => setSelection(event.target.value)}>
        {!delivery.scopeId && <option value="keep">保留当前设置（历史关联或项目建议）</option>}
        <option value="unlink">解除关联，不再跟随项目建议</option>
        {projects.map(project => <option key={project.id} value={`scope:${project.id}`}>
          {projects.some(other => other.id !== project.id && other.title === project.title) ? `${project.title} · ${project.id}` : project.title}
        </option>)}
      </select>
    </label>
    <button className="btn btn-primary btn-sm" disabled={stale || !scopes.isSuccess || mutation.isPending}>保存关联</button>
    <button type="button" className="btn btn-secondary btn-sm" disabled={mutation.isPending} onClick={close}>取消</button>
    {stale && <p role="alert">事项已更新，请取消后重新选择项目。</p>}
    {scopes.isError && <IpcErrorNotice title="项目读取失败" error={scopes.error} />}
    {mutation.isError && <IpcErrorNotice title="关联保存失败" error={mutation.error} />}
  </form>;
}

function DeliveryPanel({ delivery, id, nextAction }: { delivery: Delivery; id: string; nextAction: string | null }) {
  const mutation = useChangeDelivery(id);
  const [editor, setEditor] = useState<{ field: DeliveryField; value: string; version: string; scopeId: Delivery["scopeId"]; project: Delivery["project"] } | null>(null);
  const [choosingProject, setChoosingProject] = useState(false);
  const historical = !delivery.present && !delivery.formal;
  const closed = delivery.state === "done" || delivery.state === "ignored";
  const mineLabel = delivery.state === "done" ? "重新打开" : delivery.state === "ignored" ? "恢复跟进" : "我来处理";
  const open = (field: DeliveryField, initial = "") => {
    mutation.reset();
    setEditor({ field, value: initial, version: delivery.version, scopeId: delivery.scopeId, project: delivery.project });
  };
  const save = () => {
    if (!editor) return;
    const value = editor.value.trim();
    if (editor.field === "project" && value === (editor.project ?? "").trim()) {
      setEditor(null);
      return;
    }
    const change = editor.field === "project" ? { project: value || null }
      : editor.field === "conclusion" ? { note: value } : { target: editor.field, action: value };
    mutation.mutate({ version: mutation.isError ? delivery.version : editor.version, change }, { onSuccess: () => setEditor(null) });
  };
  const projectEditor = editor?.field === "project";
  const projectChanged = editor?.field === "project" && (editor.scopeId !== delivery.scopeId || editor.project !== delivery.project);
  const editorLabel = editor && { project: "项目名称线索（更改名称将解除已有项目关联，清空名称可移除）", conclusion: "我的结论", mine: "接下来你要做什么？", waiting: "在等谁做什么？" }[editor.field];
  const limit = projectEditor ? 120 : editor?.field === "conclusion" ? 1000 : 180;
  const draftLength = Array.from(editor?.value.trim() ?? "").length;
  const canSave = !projectChanged && !mutation.isPending && draftLength <= limit && (projectEditor || (!historical && draftLength > 0));
  const evidence = [...delivery.evidenceDetails].sort((left, right) => {
    if (!left.time || !right.time) return Number(!left.time) - Number(!right.time);
    return left.time.localeCompare(right.time);
  });
  return <>
    <section className="detail-panel delivery-panel" aria-labelledby="delivery-title">
      <h2 className="panel-title" id="delivery-title">当前交付</h2>
      <p>{delivery.work}</p>
      <div className="detail-meta">
        <span>{STATE_LABEL[delivery.state]}</span>
        <span>{delivery.confirmation === "confirmed" ? "我已确认" : "系统建议"}</span>
        {historical && <span>历史事项</span>}
      </div>
      <dl className="detail-facts">
        <div><dt>{delivery.scopeId ? "已关联项目" : "项目名称线索"}</dt><dd>{delivery.project ?? "未归入项目"}</dd></div>
        <div><dt>下一步</dt><dd>{nextAction || "未记录下一步"}</dd></div>
        <div><dt>我最近的结论</dt><dd>{delivery.conclusion || "尚未记录"}</dd></div>
      </dl>
      {choosingProject && <ProjectPicker delivery={delivery} id={id} close={() => setChoosingProject(false)} />}
      {!editor && !choosingProject && <div className="delivery-controls">
        <button className="btn btn-secondary btn-sm" disabled={mutation.isPending} onClick={() => setChoosingProject(true)}>关联已有项目</button>
        <button className="btn btn-secondary btn-sm" disabled={mutation.isPending} onClick={() => open("project", delivery.project ?? "")}>编辑项目</button>
        {!historical && <button className="btn btn-secondary btn-sm" disabled={mutation.isPending} onClick={() => open("conclusion", delivery.conclusion ?? "")}>编辑结论</button>}
        {!historical && <>
          {(!delivery.formal || delivery.state !== "mine") && (
            <button className="btn btn-secondary btn-sm" disabled={mutation.isPending} onClick={() => open("mine", delivery.state === "mine" ? nextAction ?? "" : "")}>{mineLabel}</button>
          )}
          {!closed && (!delivery.formal || delivery.state !== "waiting") && (
            <button className="btn btn-secondary btn-sm" disabled={mutation.isPending} onClick={() => open("waiting", delivery.state === "waiting" ? nextAction ?? "" : "")}>等对方处理</button>
          )}
          {!closed && <button className="btn btn-secondary btn-sm" disabled={mutation.isPending} onClick={() => mutation.mutate({ version: delivery.version, change: { target: "ignored" } })}>不用我跟</button>}
          {(delivery.state === "mine" || delivery.state === "waiting") && (
            <button className="btn btn-secondary btn-sm" disabled={mutation.isPending} onClick={() => mutation.mutate({ version: delivery.version, change: { target: "done" } })}>事情已结束</button>
          )}
        </>}
      </div>}
      {editor && <form className="delivery-controls" onSubmit={e => { e.preventDefault(); if (canSave) save(); }}>
        <label className="field"><span className="field-label">{editorLabel}</span>
          <input autoFocus className="field-input" value={editor.value} disabled={mutation.isPending} onChange={e => setEditor({ ...editor, value: e.target.value })} />
        </label>
        <button className="btn btn-primary btn-sm" disabled={!canSave}>保存</button>
        <button type="button" className="btn btn-secondary btn-sm" disabled={mutation.isPending} onClick={() => { setEditor(null); mutation.reset(); }}>取消</button>
        {draftLength > limit && <span role="alert">最多可填写 {limit} 个字。</span>}
        {projectChanged && <p role="alert">项目归属已更新，请取消后重新编辑。</p>}
      </form>}
      {mutation.isError && <IpcErrorNotice title={editor ? "保存失败，草稿已保留" : "操作失败"} error={mutation.error} />}
      {mutation.isSuccess && <p role="status" className="panel-note">已保存</p>}
    </section>
    <section className="detail-panel delivery-panel" aria-labelledby="source-excerpts">
      <h2 className="panel-title" id="source-excerpts">来源摘录</h2>
      {evidence.length ? <ol className="evidence-list">
        {evidence.map((line, index) => <li key={`${line.time}-${index}`}>
          <div className="evidence-top"><strong>{line.conversation || "会话未记录"}</strong><span className="tnum">{line.time}</span></div>
          {line.kind && <p className="evidence-meta">{line.kind}</p>}
          <p className="evidence-body">{line.excerpt}</p>
        </li>)}
      </ol> : <p className="panel-note">本地暂无来源摘录。</p>}
    </section>
    {delivery.project && <section className="detail-panel delivery-panel" aria-labelledby="project-deliveries">
      <h2 className="panel-title" id="project-deliveries">{delivery.scopeId ? "同项目的其他交付" : "使用相同项目名称的交付"}</h2>
      {delivery.related.length ? <ul>{delivery.related.map(other => <li key={other.id}>
        <Link to={`/items/${encodeURIComponent(other.id)}`}>{other.title}</Link>
      </li>)}</ul> : <p className="panel-note">此项目暂无其他交付。</p>}
    </section>}
  </>;
}

function DetailBody({ detail, id }: { detail: ItemDetail; id: string }) {
  const item = detail.item;
  const correct = useCorrectItem(id);
  const close = useCloseItem(id);
  const followup = useSetFollowup(id);

  const [note, setNote] = useState("");
  const [feedback, setFeedback] = useState<Feedback>(null);
  const [closeOpen, setCloseOpen] = useState(false);
  const [closeEvidence, setCloseEvidence] = useState<CloseEvidenceType | "">("");
  const [closeNote, setCloseNote] = useState("");
  const [issueOpen, setIssueOpen] = useState(false);
  const [followupType, setFollowupType] = useState<FollowupType>(item.followupType);
  const [followupAt, setFollowupAt] = useState(msToLocalInput(item.followupAt));
  const [followupNote, setFollowupNote] = useState(item.followupNote ?? "");

  useEffect(() => {
    setFollowupType(item.followupType);
    setFollowupAt(msToLocalInput(item.followupAt));
    setFollowupNote(item.followupNote ?? "");
    // 只在切换到另一条事项时重置表单，不覆盖用户正在输入的内容。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [item.id]);

  const statusColor = itemStatusColor(item.lifecycle, item.attention);
  const busy = correct.isPending || close.isPending || followup.isPending;

  const runCorrection = (
    patch: CorrectionArgs["patch"],
    reason: CorrectionArgs["reason"],
    label: string,
  ) => {
    setFeedback(null);
    correct.mutate(
      { id, patch, reason, note: note.trim() || undefined },
      {
        onSuccess: () =>
          setFeedback({
            tone: "ok",
            text: `已纠正：${label}。写入人工锁，后续同步不会静默覆盖。`,
          }),
        onError: (error) =>
          setFeedback({ tone: "error", text: toIpcError(error).message, error }),
      },
    );
  };

  const submitClose = () => {
    if (!closeEvidence) return;
    setFeedback(null);
    close.mutate(
      { id, evidenceType: closeEvidence, note: closeNote.trim() || undefined },
      {
        onSuccess: () => {
          setCloseOpen(false);
          setCloseEvidence("");
          setCloseNote("");
          setFeedback({ tone: "ok", text: "已收尾，并写入收尾证据事件。" });
        },
        onError: (error) =>
          setFeedback({ tone: "error", text: toIpcError(error).message, error }),
      },
    );
  };

  const submitFollowup = () => {
    setFeedback(null);
    if (followupType === "manual_time" || followupType === "deadline") {
      const at = localInputToMs(followupAt);
      if (at === null) {
        setFeedback({ tone: "error", text: "请选择人工跟进日期时间。", error: null });
        return;
      }
      followup.mutate(
        { id, type: followupType, at, note: followupNote.trim() || null },
        {
          onSuccess: () =>
            setFeedback({ tone: "ok", text: "已保存跟进时间。" }),
          onError: (error) =>
            setFeedback({ tone: "error", text: toIpcError(error).message, error }),
        },
      );
      return;
    }
    if (followupType === "event" && !followupNote.trim()) {
      setFeedback({ tone: "error", text: "请填写要等待的事件。", error: null });
      return;
    }
    followup.mutate(
      {
        id,
        type: followupType,
        at: null,
        note: followupType === "event" ? followupNote.trim() : null,
      },
      {
        onSuccess: () =>
          setFeedback({
            tone: "ok",
            text:
              followupType === "none"
                ? "已清除跟进条件。"
                : "已保存跟进条件。",
          }),
        onError: (error) =>
          setFeedback({ tone: "error", text: toIpcError(error).message, error }),
      },
    );
  };

  return (
    <>
      <div className="detail-head">
        <div>
          <p className="eyebrow">
            <Link to="/items">← 全部事项</Link> · 事项详情
          </p>
          <h1 className="detail-title">{item.title}</h1>
        </div>
        {detail.delivery?.formal !== false && (
        <div className="detail-actions">
          <button
            type="button"
            className="btn btn-secondary btn-sm"
            onClick={() => setIssueOpen(true)}
          >
            记录问题
          </button>
        </div>
        )}
      </div>

      {detail.delivery && <DeliveryPanel delivery={detail.delivery} id={id} nextAction={item.nextAction} />}
      {detail.delivery?.formal !== false && <>
      <div className="detail-meta">
        <span className="status-badge" style={{ color: statusColor }}>
          <span
            aria-hidden="true"
            className="status-dot"
            style={{ background: statusColor }}
          />
          {itemStatusLabel(item.lifecycle, item.attention)}
        </span>
        {!detail.delivery && item.confirmation && <span>{item.confirmation === "confirmed" ? "我已确认" : "系统建议"}</span>}
        {!detail.delivery && item.project && <span>所属项目：{item.project}</span>}
        <span>生命周期：{item.lifecycle}</span>
        <span>最近证据：{formatDateTime(item.lastEvidenceAt)}</span>
        <span>更新：{formatDateTime(item.updatedAt)}</span>
        {item.confidence !== null ? <span>置信：{item.confidence}</span> : null}
        {item.awaitingConfirmation ? <span className="meta-warn">待人工确认</span> : null}
      </div>

      {feedback ? (
        feedback.tone === "ok" ? (
          <div className="inline-feedback ok" role="status" aria-live="polite">
            {feedback.text}
          </div>
        ) : feedback.error ? (
          <IpcErrorNotice title="操作失败" error={feedback.error} />
        ) : (
          <div className="inline-feedback error" role="alert">
            {feedback.text}
          </div>
        )
      ) : null}

      <div className="detail-grid">
        <section className="detail-panel span-2" aria-labelledby="detail-next">
          <h2 id="detail-next" className="panel-title">
            {detail.delivery ? "跟进与收尾记录" : "当前状态与下一步"}
          </h2>
          <p className="panel-note">标题和下一步可能来自自动判读，不是消息原文。核实进展请看下方证据来源。</p>
          <dl className="detail-facts">
            {!detail.delivery && <div>
              <dt>下一步</dt>
              <dd>{item.nextAction ?? "未记录下一步"}</dd>
            </div>}
            <div>
              <dt>跟进条件</dt>
              <dd>{followupText(item.followupType, item.followupAt, item.followupNote)}</dd>
            </div>
            <div>
              <dt>关联脉络</dt>
              <dd>{item.threadId ? <code>{item.threadId}</code> : "未关联"}</dd>
            </div>
            <div>
              <dt>{item.lifecycle === "closed" ? "收尾证据" : "收尾提议"}</dt>
              <dd>
                {item.resolutionEvidence === "explicit_complete" ? "消息中明确表示已完成"
                  : CLOSE_OPTIONS.find((option) => option.value === item.resolutionEvidence)?.label
                    ?? item.resolutionEvidence ?? "无"}
                {item.resolutionEvidence && item.lifecycle !== "closed" ? "（未确认收尾）" : null}
              </dd>
            </div>
            {item.closedAt ? (
              <div>
                <dt>收尾时间</dt>
                <dd>
                  {formatDateTime(item.closedAt)}
                  {item.closeReason ? ` · ${item.closeReason}` : ""}
                </dd>
              </div>
            ) : null}
            <div>
              <dt>人工锁字段</dt>
              <dd>
                {item.lockedFields.length > 0
                  ? item.lockedFields.join("、")
                  : "无"}
              </dd>
            </div>
          </dl>
        </section>

        <section className="detail-panel" aria-labelledby="detail-correct">
          <h2 id="detail-correct" className="panel-title">
            立即纠错
          </h2>
          <p className="panel-note">
            一次点击即写入；可补充一句话。纠错只锁你实际表态的字段。
          </p>
          <label className="field">
            <span className="field-label">补充一句（可选）</span>
            <input
              className="field-input"
              value={note}
              maxLength={200}
              onChange={(event) => setNote(event.target.value)}
              placeholder="例如：点名的是同事，不是我"
            />
          </label>
          <div className="correction-buttons">
            <button
              type="button"
              className="btn btn-secondary btn-sm"
              disabled={busy}
              onClick={() => runCorrection({ belongsToMe: false }, "not_mine", "不是我的事")}
            >
              不是我的事
            </button>
            <button
              type="button"
              className="btn btn-secondary btn-sm"
              disabled={busy}
              onClick={() =>
                runCorrection({ lifecycle: "ignored" }, "plain_info", "只是信息")
              }
            >
              只是信息
            </button>
            <button
              type="button"
              className="btn btn-secondary btn-sm"
              disabled={busy || !item.threadId}
              title={item.threadId ? "解除与工作脉络的关联" : "当前没有关联的工作脉络"}
              onClick={() => runCorrection({ threadId: null }, "wrong_link", "关联错了")}
            >
              关联错了
            </button>
            <button
              type="button"
              className="btn btn-secondary btn-sm"
              disabled={busy}
              onClick={() => setCloseOpen((open) => !open)}
              aria-expanded={closeOpen}
            >
              {closeOpen ? "收起收尾" : "已结束"}
            </button>
          </div>

          {closeOpen ? (
            <fieldset className="close-fieldset">
              <legend>收尾证据（必须明确）</legend>
              <p className="panel-note">
                仅「好的」「收到」或「文件已生成」不构成收尾证据。
              </p>
              {CLOSE_OPTIONS.map((option) => (
                <label key={option.value} className="radio-row">
                  <input
                    type="radio"
                    name="close-evidence"
                    value={option.value}
                    checked={closeEvidence === option.value}
                    onChange={() => setCloseEvidence(option.value)}
                    aria-label={option.label}
                  />
                  <span>
                    <strong>{option.label}</strong>
                    <span className="radio-hint">{option.hint}</span>
                  </span>
                </label>
              ))}
              <label className="field">
                <span className="field-label">收尾备注（可选）</span>
                <input
                  className="field-input"
                  value={closeNote}
                  maxLength={200}
                  onChange={(event) => setCloseNote(event.target.value)}
                />
              </label>
              <button
                type="button"
                className="btn btn-primary btn-sm"
                disabled={!closeEvidence || close.isPending}
                onClick={submitClose}
              >
                {close.isPending ? "正在写入…" : "确认收尾"}
              </button>
            </fieldset>
          ) : null}
        </section>

        <section className="detail-panel" aria-labelledby="detail-followup">
          <h2 id="detail-followup" className="panel-title">
            设置跟进
          </h2>
          <p className="panel-note">
            人工日期只用于站内提示，不做自动催办；「优先」「尽快」不折算成日期。
          </p>
          <label className="field">
            <span className="field-label">跟进类型</span>
            <select
              className="field-input"
              value={followupType}
              onChange={(event) => setFollowupType(event.target.value as FollowupType)}
            >
              {(Object.keys(FOLLOWUP_LABEL) as FollowupType[]).map((type) => (
                <option key={type} value={type}>
                  {FOLLOWUP_LABEL[type]}
                </option>
              ))}
            </select>
          </label>

          {followupType === "manual_time" || followupType === "deadline" ? (
            <label className="field">
              <span className="field-label">日期时间</span>
              <input
                type="datetime-local"
                className="field-input tnum"
                value={followupAt}
                onChange={(event) => setFollowupAt(event.target.value)}
              />
            </label>
          ) : null}

          {followupType !== "none" ? (
            <label className="field">
              <span className="field-label">
                {followupType === "event" ? "等什么事件（必填）" : "备注（可选）"}
              </span>
              <input
                className="field-input"
                value={followupNote}
                maxLength={200}
                onChange={(event) => setFollowupNote(event.target.value)}
              />
            </label>
          ) : null}

          <button
            type="button"
            className="btn btn-secondary btn-sm"
            disabled={followup.isPending}
            onClick={submitFollowup}
          >
            {followup.isPending ? "正在保存…" : "保存跟进"}
          </button>
        </section>
      </div>

      <section className="detail-panel" aria-labelledby="detail-evidence">
        <h2 id="detail-evidence" className="panel-title">
          证据来源
        </h2>
        {detail.evidence.length === 0 ? (
          <p className="panel-note">本地没有关联证据。</p>
        ) : (
          <ul className="evidence-list">
            {detail.evidence.map((evidence) => (
              <li key={evidence.id}>
                <div className="evidence-top">
                  <strong>{evidence.label}</strong>
                  <span className="tnum">{formatDateTime(evidence.occurredAt)}</span>
                </div>
                <p className="evidence-meta">
                  {evidence.kind}
                  {evidence.senderName ? ` · ${evidence.senderName}` : ""}
                  {` · 关联 ${evidence.linkType}/${evidence.state}`}
                </p>
                {evidence.summary ? (
                  <p className="evidence-body">来源摘录或摘要：{evidence.summary}</p>
                ) : null}
                {evidence.rawState !== "available" ? (
                  <p className="evidence-raw">
                    {evidence.rawState === "redacted"
                      ? "原文已过保留期或按策略清理，不展示原文。"
                      : "原文不可访问（撤回、权限失效或未获取）。"}
                  </p>
                ) : null}
              </li>
            ))}
          </ul>
        )}
        {detail.sourceFreshness.length > 0 ? (
          <p className="panel-note">
            来源最后成功同步：
            {detail.sourceFreshness
              .map(
                (source) =>
                  `${source.sourceId} ${formatDateTime(source.lastSuccessAt)}`,
              )
              .join("；")}
          </p>
        ) : (
          <p className="panel-note">没有来源同步记录。</p>
        )}
      </section>

      <section className="detail-panel" aria-labelledby="detail-timeline">
        <h2 id="detail-timeline" className="panel-title">
          事件时间线
        </h2>
        {detail.timeline.length === 0 ? (
          <p className="panel-note">暂无事件。</p>
        ) : (
          <ol className="timeline">
            {detail.timeline.map((event) => (
              <li key={event.id}>
                <span className="timeline-time tnum">{formatDateTime(event.at)}</span>
                <span className="timeline-event">
                  {EVENT_LABEL[event.eventType] ?? event.eventType}
                  <span className="timeline-actor">
                    {ACTOR_LABEL[event.actor] ?? event.actor}
                  </span>
                </span>
                {event.note ? <span className="timeline-note">{event.note}</span> : null}
              </li>
            ))}
          </ol>
        )}
      </section>

      <section className="detail-panel" aria-labelledby="detail-corrections">
        <h2 id="detail-corrections" className="panel-title">
          纠错历史
        </h2>
        {detail.corrections.length === 0 ? (
          <p className="panel-note">还没有人工纠错。</p>
        ) : (
          <ul className="correction-list">
            {detail.corrections.map((correction, index) => (
              <li key={`${correction.at}-${index}`}>
                <span className="tnum">{formatDateTime(correction.at)}</span>
                <span>{correction.reason}</span>
                <span>{correction.fields.join("、") || "无字段"}</span>
                {correction.note ? <span>{correction.note}</span> : null}
              </li>
            ))}
          </ul>
        )}
      </section>

      <RecordIssueDialog
        open={issueOpen}
        onClose={() => setIssueOpen(false)}
        defaults={{ route: `/items/${id}`, targetType: "work_item", targetId: id }}
      />
      </>}
    </>
  );
}

export function ItemDetailPage() {
  const { id } = useParams<{ id: string }>();
  const bridge = hasDesktopBridge();
  const query = useItem(id);

  return (
    <div className="page">
      {!id ? (
        <p className="empty-detail">缺少事项 ID。</p>
      ) : !bridge ? (
        <NotConnectedNotice />
      ) : query.isPending ? (
        <LoadingNotice label="正在读取事项详情…" />
      ) : query.isError ? (
        <IpcErrorNotice
          title="事项详情读取失败"
          error={query.error}
          onRetry={() => void query.refetch()}
        />
      ) : (
        <DetailBody key={id} detail={query.data} id={id} />
      )}
    </div>
  );
}
