import { useDeferredValue, useState } from "react";
import { Link } from "react-router-dom";
import {
  IpcErrorNotice,
  LoadingNotice,
  NotConnectedNotice,
} from "../components/IpcNotice";
import {
  hasDesktopBridge,
  type ItemFilter,
  type ItemSummary,
} from "../lib/ipc";
import {
  formatDateTime,
  freshnessText,
  itemStatusColor,
  itemStatusLabel,
} from "../lib/format";
import { ITEMS_PAGE_SIZE, useItems } from "../lib/queries";

type UiFilter = "all" | "mine" | "waiting" | "confirm" | "done";

const FILTERS: { key: UiFilter; label: string; api: ItemFilter }[] = [
  { key: "all", label: "全部", api: "all" },
  { key: "mine", label: "轮到我", api: "me" },
  { key: "waiting", label: "等别人", api: "waiting" },
  { key: "confirm", label: "待确认", api: "confirm" },
  { key: "done", label: "已收尾", api: "closed" },
];

const COLUMNS = [
  { key: "title", label: "事项" },
  { key: "status", label: "当前状态" },
  { key: "next", label: "下一步" },
  { key: "source", label: "证据来源" },
  { key: "time", label: "最近记录" },
];

function StatusBadge({ item }: { item: ItemSummary }) {
  const color = itemStatusColor(item.lifecycle, item.attention);
  return (
    <span className="status-badge" style={{ color }}>
      <span aria-hidden="true" className="status-dot" style={{ background: color }} />
      {itemStatusLabel(item.lifecycle, item.attention)}
    </span>
  );
}

export function ItemsPage() {
  const bridge = hasDesktopBridge();
  const [active, setActive] = useState<UiFilter>("all");
  const [query, setQuery] = useState("");
  const deferredQuery = useDeferredValue(query.trim());
  const apiFilter = FILTERS.find((filter) => filter.key === active)?.api ?? "all";
  const [offset, setOffset] = useState(0);
  const items = useItems(apiFilter, deferredQuery, offset);

  return (
    <div className="page items-page">
      <header className="page-header">
        <h1 className="page-title">全部事项</h1>
        <p className="page-subtitle">
          回查所有出现过的事项，包括历史建议和已收尾的交付。
        </p>
      </header>

      <div className="items-tools">
        <div role="search" className="search-row">
          <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" aria-hidden="true">
            <circle cx="10.5" cy="10.5" r="6.5" />
            <path d="m16 16 4.5 4.5" />
          </svg>
          <input
            id="items-search"
            type="search"
            value={query}
            onChange={(event) => { setQuery(event.target.value); setOffset(0); }}
            placeholder="搜索事项标题、下一步或项目"
            aria-label="事项文字查询"
            className="search-input"
          />
        </div>

        <div className="items-filters" role="group" aria-label="按状态筛选">
          {FILTERS.map((filter) => (
            <button
              key={filter.key}
              type="button"
              className="items-filter"
              aria-pressed={active === filter.key}
              onClick={() => { setActive(filter.key); setOffset(0); }}
            >
              {filter.label}
            </button>
          ))}
        </div>
      </div>

      <div className="items-frame">
        <table className="items-table" aria-label="事项列表">
          <thead>
            <tr>
              {COLUMNS.map((column) => (
                <th key={column.key} scope="col">
                  {column.label}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {!bridge ? (
              <tr>
                <td colSpan={COLUMNS.length} className="items-empty-cell">
                  <NotConnectedNotice />
                </td>
              </tr>
            ) : items.isPending ? (
              <tr>
                <td colSpan={COLUMNS.length} className="items-empty-cell">
                  <LoadingNotice label="正在读取本地事项…" />
                </td>
              </tr>
            ) : items.isError ? (
              <tr>
                <td colSpan={COLUMNS.length} className="items-empty-cell">
                  <IpcErrorNotice
                    title="事项读取失败"
                    error={items.error}
                    onRetry={() => void items.refetch()}
                  />
                </td>
              </tr>
            ) : items.data.items.length > 0 ? (
              items.data.items.map((item) => (
                <tr key={item.id} className="items-record">
                  <td className="cell-title">
                    <Link to={`/items/${encodeURIComponent(item.id)}`}>
                      {item.title}
                    </Link>
                    <div className="catalog-origin">
                      {item.confirmation && <span>{item.confirmation === "confirmed" ? "我已确认" : "系统建议"}</span>}
                      {item.historical && <span>历史事项</span>}
                      {item.project && <span>{item.project}</span>}
                      {item.awaitingConfirmation && !item.confirmation && <span>待人工确认</span>}
                    </div>
                  </td>
                  <td className="items-status">
                    <StatusBadge item={item} />
                  </td>
                  <td className="items-next" data-label="下一步">{item.nextAction ?? "—"}</td>
                  <td className="cell-muted" data-label="证据来源">{item.sourceLabel ?? "来源未记录"}</td>
                  <td className="tnum cell-muted" data-label="最近记录">
                    {formatDateTime(item.lastEvidenceAt)}
                  </td>
                </tr>
              ))
            ) : (
              <tr>
                <td colSpan={COLUMNS.length} className="items-empty-cell">
                  <EmptyResult
                    total={items.data.total}
                    hasQuery={deferredQuery.length > 0}
                    synced={items.data.freshness.state !== "unknown"}
                  />
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>

      {items.isSuccess && (offset > 0 || items.data.total > ITEMS_PAGE_SIZE) && (
        <nav className="catalog-pagination" aria-label="事项分页">
          <button className="btn btn-secondary btn-sm" disabled={offset === 0} onClick={() => setOffset(Math.max(0, offset - ITEMS_PAGE_SIZE))}>上一页</button>
          <span className="tnum">第 {1 + offset / ITEMS_PAGE_SIZE} 页</span>
          <button className="btn btn-secondary btn-sm" disabled={offset + ITEMS_PAGE_SIZE >= items.data.total} onClick={() => setOffset(offset + ITEMS_PAGE_SIZE)}>下一页</button>
        </nav>
      )}

      {bridge && items.isSuccess ? (
        <p className="table-footnote">
          {items.data.items.length > 0
            ? `显示 ${items.data.items.length} 条，共 ${items.data.total} 条。`
            : ""}
          {freshnessText(items.data.freshness)}。纠错在详情页中操作。
        </p>
      ) : null}
    </div>
  );
}

function EmptyResult({
  total,
  hasQuery,
  synced,
}: {
  total: number;
  hasQuery: boolean;
  synced: boolean;
}) {
  if (!synced) {
    return (
      <div className="empty-sync">
        <p className="empty-title">尚未同步：本地没有任何事项记录</p>
        <p className="empty-detail">
          接入数据源并完成首次同步后，事项将出现在这里。每条记录均来自本地缓存，
          查看不会触碰源系统状态。
        </p>
        <p className="empty-detail subtle">本地共 {total} 条记录。</p>
      </div>
    );
  }

  if (total === 0 && !hasQuery) {
    return (
      <div>
        <p className="empty-title">本地记录为空</p>
        <p className="empty-detail">
          数据源同步已完成，但没有生成任何事项。没有明确行动的内容不会变成待办。
        </p>
      </div>
    );
  }

  return (
    <div>
      <p className="empty-title">没有匹配的事项</p>
      <p className="empty-detail">
        换一个关键词或状态；本地共 {total} 条记录。
      </p>
    </div>
  );
}
