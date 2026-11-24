import { useQuery } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import { formatDateTime } from "../lib/format";
import { invoke } from "../lib/ipc";
import { useArchiveStatus } from "../lib/queries";

type Evidence = { cid: string; mid: string; replyMid: string | null; conversation: string; request: string; requestedAt: number; repliedAt: number | null; reply: string | null; files: number; extracted: number; outgoing: boolean; attachmentExcerpt: string | null };
type Progress = { syncedAt: number | null; evidence: Evidence[]; documentStatus: string | null; documents: {name: string; observation: string; units: number}[] };

function observation(e: Evidence): string {
  if (e.repliedAt === null) return "未找到可关联的直接回复";
  const reply = (e.reply ?? "").trim();
  if (/[？?]/.test(reply)) return "已回应，回复中仍有问句";
  if (/^(已验收通过|验收通过|已确认完成)[。！!\s]*$/.test(reply)) return "回复方明确表示已验收或确认完成";
  if (/^(好[的哒]?|收到|明白|了解|OK)[。！!\s]*$/i.test(reply)) return "已收到，暂无交付证明";
  return e.extracted > 0 ? "已回应，关联附件有可读内容；交付结果待证" : "已回应，交付结果待证";
}

export function LocalProgress() {
  const archive = useArchiveStatus();
  const q = useQuery({
    queryKey: ["local_progress", archive.data?.lastSuccessMs ?? null, archive.data?.running === true],
    queryFn: () => invoke<Progress>("local_progress"),
    refetchInterval: archive.data?.running ? 15_000 : false,
    refetchOnWindowFocus: true,
  });
  return <section aria-label="本地进展核验">
    <h2>近七天 · 本地进展核验</h2>
    {q.isPending ? <p>正在核对归档…</p> : q.isError ? <p role="alert">核验失败：{q.error.message}</p> : q.data.syncedAt === null ?
      <p>还没有成功同步的归档。<Link to="/settings">打开同步设置</Link></p> : <>
        <p>数据截至 {formatDateTime(q.data.syncedAt)}。只匹配私聊或群里 @我的请求用语，按原消息引用核对双方回复。无须模型，不改事项状态。</p>
        <p>{q.data.evidence.length} 条请求线索，{q.data.evidence.filter(e => e.repliedAt !== null).length} 条有直接引用回复。未匹配不代表没有跟进。</p>
        <ul>{q.data.evidence.filter(e => e.repliedAt !== null).slice(0, 5).map(e =>
          <li key={JSON.stringify([e.cid,e.mid])}><strong>{observation(e)}</strong>：{e.request.slice(0, 64)}（{formatDateTime(e.repliedAt)}）</li>
        )}</ul>
        <details><summary>查看请求和依据</summary>
        {q.data.evidence.map(e => <details key={JSON.stringify([e.cid,e.mid])}>
          <summary>{e.outgoing ? "我发出" : "发给我"} · {observation(e)} · {e.conversation} · {e.request.slice(0, 48)}</summary>
          <p>请求（{formatDateTime(e.requestedAt)}）：{e.request}</p>
          {e.repliedAt !== null && <p>{e.outgoing ? "对方" : "本人"}回复（{formatDateTime(e.repliedAt)}）：{e.reply || "非文字回复"}。关联附件 {e.files} 个，已提取 {e.extracted} 个；回复和附件均不等于验收。</p>}
          {e.attachmentExcerpt && <blockquote>关联附件文字摘录：{e.attachmentExcerpt}</blockquote>}
          <small>来源消息：{e.mid}{e.replyMid && ` → ${e.replyMid}`}</small>
        </details>)}
        </details>
        <details><summary>近期编辑文档 · {q.data.documentStatus ?? "尚未核验"}</summary>
          <p>只列七天内有修改时间证据的文件。可读文档不是任务已交付的证明。</p>
          <ul>{q.data.documents.map((d, i) => <li key={i}>{d.name}：{d.observation}{d.units > 0 && ` ${d.units}`}</li>)}</ul>
        </details>
      </>}
  </section>;
}
