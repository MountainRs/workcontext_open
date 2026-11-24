import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { hasDesktopBridge, invoke } from "../lib/ipc";

type Pending = { id: string; kind: "project" | "domain"; title: string };
type Receipt = { status: "accepted" | "declined" | "stale" };
const outcomes = { accepted: "已确认并保存变更。", declined: "已拒绝建议。", stale: "范围已变更，建议已过期。请按当前版本重新提交。" };

export function ExternalSuggestions() {
  const cache = useQueryClient();
  const desktop = hasDesktopBridge();
  const list = useQuery({ queryKey: ["external_suggestions_list"],
    queryFn: () => invoke<Pending[]>("external_suggestions_list"), enabled: desktop, retry: false });
  const review = useMutation({
    mutationFn: (id: string) => invoke<Receipt>("external_suggestion_review", { id }),
    onSettled: async () => {
      await Promise.all([list.refetch(), cache.invalidateQueries({ queryKey: ["context_scopes"] })]);
    },
  });
  return <section className="settings-section" aria-labelledby="external-suggestions">
    <h2 id="external-suggestions" className="panel-title">项目与领域建议</h2>
    <button type="button" className="btn btn-secondary" disabled={!desktop || list.isFetching || review.isPending}
      onClick={() => { review.reset(); void list.refetch(); }}>刷新建议</button>
    {!desktop ? <p className="panel-note">请在桌面端查看建议。</p>
      : list.isPending ? <p role="status">正在读取建议…</p>
      : list.error ? <p role="alert" className="inline-feedback error">{list.error.message}</p>
      : !list.data?.length ? <p className="panel-note">当前账号没有待确认建议。</p>
      : <ul>{list.data.map(row => <li key={row.id}>
        {row.kind === "project" ? "项目" : "领域"} · {row.title}{" "}
        <button type="button" className="btn btn-secondary" disabled={review.isPending}
          aria-label={`核对${row.title}的变更`} onClick={() => review.mutate(row.id)}>核对变更</button>
      </li>)}</ul>}
    {review.data && <p role="status">{outcomes[review.data.status]}</p>}
    {review.error && <p role="alert" className="inline-feedback error">{review.error.message}</p>}
  </section>;
}
