import { useMutation, useQuery, useQueryClient, type UseQueryResult } from "@tanstack/react-query";
import { useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent } from "react";
import { createPortal } from "react-dom";
import { Link } from "react-router-dom";
import { hasDesktopBridge, invoke } from "../lib/ipc";
import { queryKeys } from "../lib/queries";
import { useOwnershipAction } from "./useOwnershipAction";

const groups = { mine: "我来办", waiting: "等对方", triage: "待我认领", done: "已完成", ignored: "已忽略" } as const;
type State = keyof typeof groups;
const states = ["mine", "waiting", "triage"] as const;
type ActiveState = typeof states[number];
function isActive(state: State): state is ActiveState {
  return state === "mine" || state === "waiting" || state === "triage";
}
const emptyText: Record<ActiveState, string> = {
  mine: "目前没有轮到你处理的事项。",
  waiting: "目前没有在等对方的事项。",
  triage: "目前没有需要你认领的事项。",
};
type Card = {
  id: string;
  version: string;
  state: State;
  needsVerification?: boolean;
  work: string;
  action: string;
  when: string;
  sourceSummary: { time: string; people: string; place: string; event: string };
  evidenceDetails?: { time: string; conversation: string; kind: string; excerpt: string }[];
  conclusion?: string | null;
};
type Report = { cutoff: string; cards: Card[] | null };
type View = {
  report: Report | null;
  running: boolean;
  progress: string | null;
  error: string | null;
  canRun: boolean;
  autoEnabled: boolean;
  autoRetryAt: number | null;
  profile: string | null;
};

type Move = { item: Card; target: State; action?: string };
type Receipt = { id: string; work: string; from: State; target: State; token: string };
type Conclusion = { item: Card; version: string; note: string; resolved: boolean };
type Change = { move: Move } | { undo: Receipt } | { conclusion: Conclusion };
type Editor = { target: "mine" | "waiting" | "conclusion"; draft: string; version: string };
/** 被移走的卡片在原位置离场期间使用的只读快照。 */
type Leaving = { item: Card; target: State; at: number };
type Row = { card: Card } | { gone: Leaving };
type SectionHeading = { heading: "直接处理" | "先核实"; count: number };

const UNDO_SECONDS = 5;

/** 整理固定提交给这个模型（见 `ui/IPC.md` 的 `ownership_refresh`），授权文案如实注明。 */
const MODEL = "workbuddy/deepseek-v4.1-flash";
const CONSENT = `整理时会将当前账号近 30 天处理后的会话发送给 ${MODEL}。开启自动整理后，工作台运行期间会在归档更新时重新整理。`;
/** 账号在设置页的「全可访问会话同步」里绑定；整理开关在设置页第一个面板「会话清单」。 */
const ARCHIVE_LINK = "/settings#settings-archive";
const SETTINGS_LINK = "/settings#ownership-settings";
/** 离场动画的兜底时长：系统不派发 animationend 时也把快照清掉。 */
const LEAVE_MS = 800;

/** `cutoff` 取前 16 位；带 `T` 的 ISO 写法换成空格，是今天时只显示时刻。 */
function cutoffLabel(cutoff: string): string {
  const [date = "", time = ""] = cutoff.replace("T", " ").slice(0, 16).split(" ");
  const now = new Date();
  const today = [now.getFullYear(), now.getMonth() + 1, now.getDate()]
    .map(part => String(part).padStart(2, "0")).join("-");
  return date === today ? time : `${date} ${time}`;
}

function evidenceTime(value: string) {
  const parts = /^(\d{4})-(\d{2})-(\d{2})(?:[T ](\d{2}:\d{2})(?::\d{2})?)?$/.exec(value);
  if (parts) return `${parts[1]}年${Number(parts[2])}月${Number(parts[3])}日${parts[4] ? ` ${parts[4]}` : ""}`;
  if (/^\d{4}-\d\d-\d\dT/.test(value)) {
    const date = new Date(value);
    if (!Number.isNaN(date.getTime())) return date.toLocaleString("zh-CN", { dateStyle: "long", timeStyle: "short" });
  }
  return value;
}

function ReviewCard({ item, busy, restored, move, conclude, editor, onEditing }: {
  item: Card;
  busy: boolean;
  restored: boolean;
  move: (change: Move) => void;
  conclude: (change: Conclusion) => void;
  editor: Editor | undefined;
  onEditing: (editor: Editor | null) => void;
}) {
  const [expanded, setExpanded] = useState(Boolean(editor));
  const input = useRef<HTMLInputElement>(null);
  const noteInput = useRef<HTMLTextAreaElement>(null);
  const conclusionButton = useRef<HTMLButtonElement>(null);
  const previousTarget = useRef<Editor["target"] | null>(null);
  const openButton = useRef<HTMLButtonElement>(null);
  const focusAfterToggle = useRef(false);
  useLayoutEffect(() => {
    if (focusAfterToggle.current) {
      focusAfterToggle.current = false;
      const note = (editor?.target ?? previousTarget.current) === "conclusion";
      (editor ? (note ? noteInput.current : input.current) : (note ? conclusionButton.current : openButton.current))?.focus();
    }
  }, [editor]);
  const { time, place, people, event } = item.sourceSummary;
  const { state } = item;
  const open = (target: Editor["target"]) => {
    focusAfterToggle.current = true;
    previousTarget.current = target;
    onEditing({ target, draft: target === "conclusion" ? item.conclusion ?? "" : "", version: item.version });
  };
  const cancel = () => {
    focusAfterToggle.current = true;
    previousTarget.current = editor?.target ?? null;
    onEditing(null);
  };
  const detailsId = `ownership-details-${item.id}`;
  const evidence = item.evidenceDetails?.filter(line => line.excerpt.trim()) ?? [];
  return <article className={restored ? "ownership-card is-restored" : "ownership-card"}>
    <div className="ownership-summary">
      <h3><Link id={`ownership-card-${item.id}`} to={`/items/${encodeURIComponent(item.id)}`}>{item.work}</Link></h3>
      {item.action.trim() && item.action.trim() !== item.work.trim() && <p className="ownership-action">{item.action}</p>}
    </div>
    {item.when && <span className="ownership-when">{item.when}</span>}
    {!editor && <div className="ownership-tools">
      {state === "mine" ? <button type="button" disabled={busy} onClick={() => move({ item, target: "done" })}>办完了</button>
        : state === "triage" ? <button type="button" disabled={busy} onClick={() => move({ item, target: "mine" })}>我来办</button>
        : <button type="button" disabled={busy} ref={openButton} onClick={() => open("mine")}>
          轮到我了
        </button>}
      <button type="button" className="ownership-disclosure" aria-label={`更多：${item.work}`} title="事情经过与更多操作"
        aria-controls={detailsId} aria-expanded={expanded} onClick={() => setExpanded(value => !value)} />
    </div>}
    <div className="ownership-story" id={detailsId} hidden={!expanded || Boolean(editor && editor.target !== "conclusion")}>
      {event && event.trim().replace(/。$/, "") !== item.action.trim().replace(/。$/, "") &&
        <><p className="ownership-story-title">会话进展</p><p>{event}</p></>}
      {(time || people || place) && <dl className="ownership-context">
        {time && <div><dt>沟通时间</dt><dd>{time}</dd></div>}
        {people && <div><dt>涉及人员</dt><dd>{people}</dd></div>}
        {place && <div><dt>地点</dt><dd>{place}</dd></div>}
      </dl>}
      <div className="ownership-evidence">
        <p className="ownership-story-title">会话原文</p>
        {evidence.length ? <ul>{evidence.map((line, index) => {
          const conversation = line.conversation.trim();
          const kind = line.kind.trim();
          const location = kind && !conversation.includes(kind) ? [kind, conversation].filter(Boolean).join(" · ") : conversation;
          return <li key={index}>
            <p className="ownership-evidence-source">{[evidenceTime(line.time), location].filter(Boolean).join(" · ")}</p>
            <blockquote>{line.excerpt}</blockquote>
          </li>;
        })}</ul> : <p className="ownership-evidence-source">这份清单没有保留原文摘录。</p>}
      </div>
      {item.conclusion && <div className="ownership-conclusion"><p className="ownership-story-title">我的结论</p><p>{item.conclusion}</p></div>}
      {!editor && <button type="button" ref={conclusionButton} className="btn ownership-note-button" disabled={busy} onClick={() => open("conclusion")}>
        {item.conclusion ? "修改结论" : "补充结论"}
      </button>}
      {!editor && <div className="ownership-actions">
        {state === "waiting"
          ? <button type="button" className="btn" disabled={busy} onClick={() => move({ item, target: "done" })}>事情已结束</button>
          : <button type="button" className="btn" disabled={busy} ref={openButton} onClick={() => open("waiting")}>
            {state === "mine" ? "已交给对方" : "我在等对方"}
          </button>}
        <button type="button" className="btn" disabled={busy} onClick={() => move({ item, target: "ignored" })}>不用我跟</button>
      </div>}
    </div>
    {editor && <form className="ownership-editor" onSubmit={e => {
      e.preventDefault();
      const action = editor.draft.trim();
      if (!action || busy) return;
      if (editor.target === "conclusion") conclude({ item, version: editor.version, note: action, resolved: false });
      else move({ item, target: editor.target, action });
    }}>
      {editor.target === "conclusion" ? <label>我的结论
        <textarea ref={noteInput} rows={3} required value={editor.draft} disabled={busy}
          onChange={e => onEditing({ ...editor, draft: e.target.value })}
          onKeyDown={e => { if (e.key === "Escape" && !e.nativeEvent.isComposing && !busy) { e.preventDefault(); cancel(); } }} />
      </label> : <label>
        {editor.target === "waiting" ? "在等谁做什么？" : "接下来你要做什么？"}
        <input ref={input} required maxLength={180} value={editor.draft} disabled={busy}
          placeholder={editor.target === "waiting" ? "例如：等工艺负责人确认三张样图的工序" : "例如：按评审意见修改第二张样图"}
          onChange={e => onEditing({ ...editor, draft: e.target.value })}
          onKeyDown={e => {
            // 输入法组字中的 Esc 只结束组字，不丢草稿。
            if (e.key === "Escape" && !e.nativeEvent.isComposing && !busy) {
              e.preventDefault();
              cancel();
            }
          }} />
      </label>}
      <div className="ownership-actions">
        <button className="btn btn-primary" disabled={busy || !editor.draft.trim()}>{editor.target === "conclusion" ? "仅保存结论" : "保存"}</button>
        {editor.target === "conclusion" && <button type="button" className="btn" disabled={busy || !editor.draft.trim()}
          onClick={() => conclude({ item, version: editor.version, note: editor.draft.trim(), resolved: true })}>
          {state === "waiting" ? "不再等对方，移至已完成" : "无需再跟进，移至已完成"}
        </button>}
        <button className="btn" type="button" disabled={busy} onClick={cancel}>取消</button>
      </div>
    </form>}
  </article>;
}

function ReportItems({ cards }: { cards: Card[] }) {
  const client = useQueryClient();
  const [group, select] = useState<ActiveState>(() => states.find(s => cards.some(c => c.state === s)) ?? "mine");
  /** 编辑中的卡片留在打开它的分组：后台刷新改了归属也不打断输入，关掉编辑后才移走。 */
  const [editing, setEditing] = useState<Record<string, Editor & { group: ActiveState }>>({});
  const [leaving, setLeaving] = useState<Record<string, Leaving>>({});
  const [restored, setRestored] = useState<string | null>(null);
  const [receipt, setReceipt] = useState<Receipt | null>(null);
  const toastReceipt = receipt && !isActive(receipt.target) ? receipt : null;
  const [closedId, setClosedId] = useState<string | null>(null);
  const [notice, setNotice] = useState("");
  const [focusId, setFocusId] = useState<string | null>(null);
  const [indicator, setIndicator] = useState<{ transform: string; width: number; height: number } | null>(null);
  const feedback = useRef<HTMLDivElement>(null);
  const undoButton = useRef<HTMLButtonElement>(null);
  const list = useRef<HTMLDivElement>(null);
  const filters = useRef<HTMLDivElement>(null);
  const lastFocused = useRef<EventTarget | null>(null);
  const writing = useRef(false);
  useEffect(() => {
    if (!toastReceipt) return;
    const timer = window.setTimeout(() => {
      if (document.activeElement === undoButton.current) {
        filters.current?.querySelector<HTMLButtonElement>('[aria-checked="true"]')?.focus();
      }
      setReceipt(current => current === toastReceipt ? null : current);
    }, UNDO_SECONDS * 1000);
    return () => window.clearTimeout(timer);
  }, [toastReceipt]);
  const items = cards.filter(card => isActive(card.state) && (editing[card.id]?.group ?? card.state) === group);
  const editCard = (id: string, editor: Editor | null) => setEditing(prev => {
    if (editor) return { ...prev, [id]: { ...editor, group } };
    if (!(id in prev)) return prev;
    const next = { ...prev };
    delete next[id];
    return next;
  });
  const dismiss = (id: string) => setLeaving(prev => {
    if (!(id in prev)) return prev;
    const next = { ...prev };
    delete next[id];
    return next;
  });
  /** 写入成功后让卡片在原位置收起离场；不拖延保存，动画只跟随结果。 */
  const leave = (item: Card, target: State) => {
    setLeaving(prev => ({ ...prev, [item.id]: { item, target, at: Math.max(0, items.findIndex(card => card.id === item.id)) } }));
    window.setTimeout(dismiss, LEAVE_MS, item.id);
  };
  const choose = (state: ActiveState) => {
    select(state);
    setFocusId(null);
    setLeaving({});
  };
  const navigate = (event: KeyboardEvent<HTMLButtonElement>, index: number) => {
    if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
    let next: number;
    switch (event.key) {
      case "ArrowRight": case "ArrowDown": next = (index + 1) % states.length; break;
      case "ArrowLeft": case "ArrowUp": next = (index + states.length - 1) % states.length; break;
      case "Home": next = 0; break;
      case "End": next = states.length - 1; break;
      default: return;
    }
    event.preventDefault();
    filters.current?.querySelectorAll<HTMLButtonElement>('[role="radio"]')[next]?.focus();
    choose(states[next]);
  };
  const showCard = (state: State, id: string) => {
    if (isActive(state)) choose(state);
    setRestored(id);
    setFocusId(id);
  };
  useLayoutEffect(() => {
    if (!focusId) return;
    const title = document.getElementById(`ownership-card-${focusId}`);
    if (!title) return;
    title.focus();
    setFocusId(null);
  }, [focusId, cards]);
  // 选中底色从上一个分组滑到当前分组；窗口尺寸变化时重新对齐。
  useLayoutEffect(() => {
    const measure = () => {
      const chip = filters.current?.querySelector<HTMLElement>('[aria-checked="true"]');
      if (chip) setIndicator({ transform: `translate(${chip.offsetLeft}px, ${chip.offsetTop}px)`, width: chip.offsetWidth, height: chip.offsetHeight });
    };
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
    if (filters.current) observer.observe(filters.current);
    return () => observer.disconnect();
  }, [group]);
  const change = useMutation({
    mutationFn: async (request: Change): Promise<Receipt | null> => {
      if ("undo" in request) {
        await invoke<void>("ownership_undo", { token: request.undo.token });
        return null;
      }
      if ("conclusion" in request) {
        const { item, version, note, resolved } = request.conclusion;
        await invoke<void>("ownership_set_conclusion", { id: item.id, version, note, resolved });
        return null;
      }
      const { item, target, action } = request.move;
      const { undoToken } = await invoke<{ undoToken: string }>("ownership_move", { id: item.id, version: item.version, target, action });
      return { id: item.id, work: item.work, from: item.state, target, token: undoToken };
    },
    onSettled: async (result, error, request) => {
      const move = "move" in request ? request.move : null;
      const id = "undo" in request ? request.undo.id : "move" in request ? request.move.item.id : request.conclusion.item.id;
      await Promise.all([
        client.invalidateQueries({ queryKey: ["ownership_view"] }),
        client.invalidateQueries({ queryKey: queryKeys.today }),
        client.invalidateQueries({ queryKey: ["items"] }),
        client.invalidateQueries({ queryKey: queryKeys.item(id) }),
      ]);
      if (error) {
        if ("conclusion" in request) {
          const current = client.getQueryData<View>(["ownership_view"])?.report?.cards?.find(card => card.id === id);
          if (current) setEditing(prev => prev[id] ? { ...prev, [id]: { ...prev[id], version: current.version } } : prev);
        }
        feedback.current?.focus();
      } else if (result && move) {
        editCard(id, null);
        if (isActive(result.target)) leave(move.item, result.target);
        setClosedId(null);
        setReceipt(result);
        if (isActive(result.target)) {
          setNotice(`已将「${result.work}」移至${groups[result.target]}。`);
          feedback.current?.focus();
        } else {
          setNotice("");
        }
      } else if ("conclusion" in request) {
        const { item, resolved } = request.conclusion;
        editCard(id, null);
        setReceipt(null);
        setClosedId(resolved ? id : null);
        setNotice(resolved ? `已保存结论，「${item.work}」移至已完成。` : `已保存「${item.work}」的结论。`);
        feedback.current?.focus();
      } else if ("undo" in request) {
        setReceipt(null);
        setClosedId(null);
        setNotice(isActive(request.undo.target) ? `已撤销，「${request.undo.work}」回到${groups[request.undo.from]}。` : "");
        showCard(request.undo.from, id);
      }
    },
  });
  useLayoutEffect(() => {
    if (toastReceipt && !change.isPending && document.activeElement === document.body) {
      undoButton.current?.focus({ preventScroll: true });
    }
  }, [toastReceipt, change.isPending]);
  const submit = (request: Change) => {
    if (writing.current) return;
    writing.current = true;
    change.mutate(request, { onSettled: () => { writing.current = false; } });
  };
  useLayoutEffect(() => {
    const focused = lastFocused.current;
    if (!(focused instanceof HTMLElement) || focused.isConnected) return;
    if (list.current?.contains(document.activeElement)) {
      lastFocused.current = document.activeElement;
      return;
    }
    lastFocused.current = null;
    if (!writing.current && !change.isPending && document.activeElement === document.body) {
      filters.current?.querySelector<HTMLButtonElement>('[aria-checked="true"]')?.focus();
    }
  });
  const counts = Object.fromEntries(states.map(s => [s, cards.filter(card => card.state === s).length])) as Record<State, number>;
  const rows: Row[] = items.map(card => ({ card }));
  Object.values(leaving).forEach(gone => {
    if (!items.some(card => card.id === gone.item.id)) rows.splice(Math.min(gone.at, rows.length), 0, { gone });
  });
  let displayRows: (Row | SectionHeading)[] = rows;
  if (group === "mine") {
    const direct: Row[] = [];
    const verify: Row[] = [];
    for (const row of rows) {
      const card = "card" in row ? row.card : row.gone.item;
      (card.needsVerification ? verify : direct).push(row);
    }
    if (verify.length) {
      displayRows = [];
      const pendingCount = verify.filter(row => "card" in row).length;
      if (direct.length) displayRows.push({ heading: "直接处理", count: items.length - pendingCount }, ...direct);
      displayRows.push({ heading: "先核实", count: pendingCount }, ...verify);
    }
  }
  return <>
    <div className="ownership-filters" role="radiogroup" aria-label="按事项归属浏览" ref={filters}>
      {indicator && <span className="ownership-filter-indicator" style={indicator} aria-hidden="true" />}
      {states.map((s, i) => <button key={s} type="button" role="radio" className="filter-chip" aria-checked={s === group} tabIndex={s === group ? 0 : -1}
          onKeyDown={event => navigate(event, i)} onClick={event => { event.currentTarget.focus(); choose(s); }}>
          {groups[s]} <span key={counts[s]} className="ownership-count">{counts[s]}</span>
        </button>)}
    </div>
    <div className="ownership-feedback" ref={feedback} tabIndex={-1}>
      <span role="status">{change.isPending ? (change.variables && "undo" in change.variables ? "正在撤销…" : "正在保存…") : notice}</span>
      {change.isError && <p role="alert">{change.variables && "undo" in change.variables ? "撤销失败" : "保存失败"}：{change.error.message}</p>}
      {((receipt && !toastReceipt) || closedId) && !change.isPending && <div className="ownership-actions">
        {closedId && <Link className="btn" to={`/items/${encodeURIComponent(closedId)}`}>查看事项详情</Link>}
        {receipt && isActive(receipt.target) && <button type="button" className="btn" onClick={() => showCard(receipt.target, receipt.id)}>查看{groups[receipt.target]}</button>}
        {receipt && <button type="button" className="btn" onClick={() => submit({ undo: receipt })}>撤销</button>}
      </div>}
    </div>
    {toastReceipt && createPortal(
      <div className="ownership-toast" style={{ visibility: change.isPending ? "hidden" : "visible" }}>
        <svg key={toastReceipt.token} width="18" height="18" viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="2"
          role="img" aria-label="五秒内可撤销" className="ownership-toast-ring">
          <circle cx="10" cy="10" r="8" opacity=".3" />
          <circle className="ownership-toast-arc" cx="10" cy="10" r="8" pathLength="100" strokeDasharray="100 100"
            strokeLinecap="round" transform="rotate(-90 10 10)" style={{ animationDuration: `${UNDO_SECONDS}s` }} />
        </svg>
        <span role="status">{groups[toastReceipt.target]}</span>
        <button ref={undoButton} type="button" className="ownership-toast-undo" onClick={() => submit({ undo: toastReceipt })}>撤销</button>
      </div>, document.body)}
    {rows.length ? <div className="ownership-cards" data-state={group} ref={list}
      onFocus={e => { lastFocused.current = e.target; }}
      onBlur={e => { if (e.relatedTarget) lastFocused.current = null; }}>
      {displayRows.map(row => "heading" in row
        ? <h3 className="ownership-section-heading" key={row.heading}>{row.heading}<span>{row.count}</span></h3>
        : "card" in row
        ? <ReviewCard key={row.card.id} item={row.card} busy={change.isPending} restored={row.card.id === restored}
          conclude={conclusion => submit({ conclusion })}
          editor={editing[row.card.id]} move={move => submit({ move })} onEditing={editor => editCard(row.card.id, editor)} />
        : <div key={`gone-${row.gone.item.id}`} className="ownership-leave" aria-hidden="true"
          onAnimationEnd={e => { if (e.target === e.currentTarget) dismiss(row.gone.item.id); }}>
          <article className="ownership-card">
            <div className="ownership-summary">
              <h3>{row.gone.item.work}</h3>
              <p className="ownership-action">{row.gone.item.action}</p>
            </div>
            <p className="ownership-moved">已移到{groups[row.gone.target]}</p>
          </article>
        </div>)}
    </div> : cards.some(card => isActive(card.state)) ? <p className="ownership-empty">{emptyText[group]}</p>
      : <p className="ownership-empty">当前清单没有需要跟进的会话事项。 <Link to="/items">查看全部事项</Link></p>}
  </>;
}

/** 判读运行中时的读取间隔。 */
export const OWNERSHIP_RUNNING_POLL_MS = 1_500;
/** 自动整理开启时拾取后台新报告的间隔；调度由桌面端负责，这里只读结果。 */
export const OWNERSHIP_AUTO_POLL_MS = 30_000;

export function useOwnershipView() {
  return useQuery({
    queryKey: ["ownership_view"],
    queryFn: () => invoke<View>("ownership_view"),
    enabled: hasDesktopBridge(),
    retry: false,
    refetchInterval: q => {
      const state = q.state.data;
      return state?.running ? OWNERSHIP_RUNNING_POLL_MS : state?.autoEnabled ? OWNERSHIP_AUTO_POLL_MS : false;
    },
  });
}

export function OwnershipReview({ view }: { view: UseQueryResult<View, Error> }) {
  const action = useOwnershipAction(view.refetch);
  const state = view.data;
  const profile = state?.profile ?? null;
  const auto = state?.autoEnabled === true;
  const running = state?.running === true || action.isPending;
  // `canRun` 在运行中为 false；账号或凭据不可用时也为 false，后两种给指引，不按失败提示。
  const canOrganize = profile !== null && state?.canRun === true;
  const accountMissing = state !== undefined && profile === null;
  const credentialMissing = state !== undefined && profile !== null && !state.canRun && !state.running;
  const runError = state?.error && !accountMissing && !credentialMissing ? state.error : null;
  const autoLabel = auto ? "自动整理已开启" : "自动整理未开启";
  const cutoff = state?.report?.cutoff || "";
  // 只在清单于本页期间被后台更新时高亮「截至」时间；首次读取不算。
  const [updated, setUpdated] = useState(false);
  const previousCutoff = useRef(cutoff);
  useEffect(() => {
    if (previousCutoff.current && cutoff !== previousCutoff.current) setUpdated(true);
    previousCutoff.current = cutoff;
  }, [cutoff]);
  const enable = () => {
    if (profile) action.mutate({ kind: "auto", profile, enabled: true });
  };
  const locked = action.isPending || view.isError;
  return <section className="ownership-review" aria-labelledby="ownership-heading">
    <header className="ownership-heading">
      <h2 id="ownership-heading">会话事项</h2>
      {state && <p className="ownership-status">
        <span role="status">{running ? "正在整理会话…" : <>{autoLabel}{cutoff && <>
          ，<span key={cutoff} className={updated ? "ownership-cutoff is-updated" : "ownership-cutoff"}>清单截至 {cutoffLabel(cutoff)}</span>
        </>}</>}</span>
        {running && <span className="ownership-progress" aria-hidden="true">
          <span className="ownership-live-dot" />
          {state.progress || (action.isPending ? "正在提交…" : "正在启动整理…")}
        </span>}
      </p>}
    </header>
    {view.isError && <p className="ownership-alert" role="alert">读取判读状态失败：{view.error.message} <button type="button" className="btn" onClick={() => void view.refetch()}>重新读取</button></p>}
    {action.error && <p className="ownership-alert" role="alert">未能启动整理：{action.error.message}</p>}
    {runError && <p className="ownership-alert" role="alert">本轮整理失败：{runError}{state?.report ? " 清单仍是上一次整理的结果。" : ""}</p>}
    {accountMissing && <div className="ownership-guidance" role="status">
      <p className="ownership-guidance-title">还没有当前账号的清单</p>
      <p>{state?.error || "尚未绑定归档账号。"}</p>
      <p>请先到「设置 → 全可访问会话同步」完成一次同步，桌面端会记住当前账号；回到这里就能开启自动整理。</p>
      <Link className="btn" to={ARCHIVE_LINK}>去同步</Link>
    </div>}
    {view.isPending && <p className="ownership-empty" role="status">正在读取会话清单…</p>}
    {state?.report ? (state.report.cards
      ? <ReportItems key={profile ?? ""} cards={state.report.cards} />
      : <p className="ownership-empty">上次的报告还没有事件摘要。到「设置 → 会话清单」整理一次后即可查看。</p>)
      : state && !accountMissing && <p className="ownership-empty">还没有当前账号的清单。</p>}
    {canOrganize && !auto && <div className="ownership-organize">
      <p id="ownership-consent">{CONSENT}</p>
      <div className="ownership-actions">
        <button type="button" className="btn btn-primary" aria-describedby="ownership-consent" disabled={locked} onClick={enable}>开启自动整理</button>
      </div>
    </div>}
    {credentialMissing && <p className="ownership-note">
      自动整理暂不可用：{state?.error || "桌面进程读不到模型凭据。"} <Link to={SETTINGS_LINK}>去设置</Link>
    </p>}
  </section>;
}
