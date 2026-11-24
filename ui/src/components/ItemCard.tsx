import { Link } from "react-router-dom";
import { ATTENTION_LABEL, attentionColor } from "../lib/format";
import type { TodayItem } from "../lib/ipc";

/**
 * 今日卡片：`nextAction` 为空时**不渲染**并上报缺陷（详细设计 §11.3）。
 * 宁可不显示，也不显示「继续跟进」这种空话。
 */
export function ItemCard({ item }: { item: TodayItem }) {
  const nextAction = item.nextAction?.trim() ?? "";
  if (!nextAction) {
    console.warn("MISSING_NEXT_ACTION", item.id);
    return null;
  }
  if (!item.title?.trim()) {
    console.warn("MISSING_TITLE", item.id);
    return null;
  }

  const color = attentionColor(item.attention);

  return (
    <article className="item-card">
      <div className="item-card-top">
        <span className="status-badge" style={{ color }}>
          <span
            aria-hidden="true"
            className="status-dot"
            style={{ background: color }}
          />
          {ATTENTION_LABEL[item.attention]}
        </span>
      </div>

      <h3 className="item-card-title">
        <Link to={`/items/${encodeURIComponent(item.id)}`}>{item.title}</Link>
      </h3>

      <dl className="item-card-fields">
        <div>
          <dt>下一步</dt>
          <dd>{nextAction}</dd>
        </div>
      </dl>
    </article>
  );
}
