import { Fragment, useEffect, useState, type ReactNode } from "react";
import { ItemCard } from "../components/ItemCard";
import { OwnershipReview, useOwnershipView } from "../components/OwnershipReview";
import {
  IpcErrorNotice,
  LoadingNotice,
  NotConnectedNotice,
} from "../components/IpcNotice";
import { hasDesktopBridge, type TodayItem, type TodayView } from "../lib/ipc";
import { useToday } from "../lib/queries";

function useTodayLabel() {
  const [label, setLabel] = useState("");
  useEffect(() => {
    const now = new Date();
    const weekday = ["日", "一", "二", "三", "四", "五", "六"][now.getDay()];
    setLabel(
      `${now.getFullYear()}年${now.getMonth() + 1}月${now.getDate()}日 · 周${weekday}`,
    );
  }, []);
  return label;
}

const lanes = [
  { kind: "mine", title: "轮到我" },
  { kind: "waiting", title: "等别人" },
  { kind: "confirm", title: "待确认" },
] as const;

function Lane({
  id,
  kind,
  title,
  items,
  children,
}: {
  id: string;
  kind: "mine" | "waiting" | "confirm";
  title: string;
  items: TodayItem[];
  children: ReactNode;
}) {
  return (
    <section className="lane" aria-labelledby={id}>
      <header className={`lane-header ${kind}`}>
        <h3 id={id} className={`lane-title ${kind}`}>
          {title}
        </h3>
        <span className="lane-count" aria-label={`本地记录 ${items.length} 条`}>
          {items.length}
        </span>
      </header>
      {children}
    </section>
  );
}

function TodayContent({ view, conversationIds, hasReport }: { view: TodayView; conversationIds: Set<string>; hasReport: boolean }) {
  const visibleLanes = lanes.map(lane => ({
    ...lane,
    items: view.groups[lane.kind].filter(item => !conversationIds.has(item.id)),
  }));
  // 同一事项只展示一次，包括已从今日收起的会话事项。
  if (visibleLanes.every(lane => !lane.items.length)) {
    if (lanes.some(lane => view.groups[lane.kind].length > 0)) return null;
    return !hasReport && view.freshness.state === "unknown" ? <p>本地事项库尚未同步。</p> : null;
  }
  return <section className="today-local" aria-labelledby="today-local-title">
    <h2 id="today-local-title" className="today-section-title">本地事项记录</h2>
    <div className="today-grid">{visibleLanes.map(({ kind, title, items }) => items.length > 0 &&
      <Lane key={kind} id={`lane-${kind}`} kind={kind} title={title} items={items}>
        {kind === "waiting" && view.freshness.lastSuccessAt === null
          ? <div role="alert"><p>无法显示等待项</p><p>请先完成一次同步。</p></div>
          : <div className="card-list">{items.map(item => <ItemCard key={item.id} item={item} />)}</div>}
      </Lane>
    )}</div>
  </section>;
}

export function TodayPage() {
  const today = useTodayLabel();
  const bridge = hasDesktopBridge();
  const query = useToday();
  const ownership = useOwnershipView();
  const cards = ownership.data?.report?.cards ?? [];
  const conversationIds = new Set(cards.map(card => card.id));
  // 会话事项是主区。没有会话清单而本地有可操作记录时，本地记录提前；
  // 等会话状态读完再排版，避免加载过程中两块内容换位。
  const groups = query.data?.groups;
  const localHasItems = !!groups && lanes.some(lane => groups[lane.kind].some(item => !conversationIds.has(item.id)));
  const conversationFirst = ownership.isPending || cards.some(card => card.state !== "done" && card.state !== "ignored") || !localHasItems;
  const local = !bridge ? (
    <NotConnectedNotice />
  ) : query.isPending ? (
    <LoadingNotice label="正在读取本地数据库…" />
  ) : query.isError ? (
    <IpcErrorNotice
      title="今日读取失败"
      error={query.error}
      onRetry={() => void query.refetch()}
    />
  ) : (
    <TodayContent view={query.data} conversationIds={conversationIds} hasReport={Boolean(ownership.data?.report)} />
  );

  const review = bridge && <OwnershipReview key="review" view={ownership} />;
  const records = (!bridge || !ownership.isPending) && <Fragment key="local">{local}</Fragment>;

  return (
    <div className="page today-page">
      <header className="today-dateline">
        <h1 className="page-title">今日</h1>
        <p className="page-subtitle">{today}</p>
      </header>

      {conversationFirst ? [review, records] : [records, review]}
    </div>
  );
}
