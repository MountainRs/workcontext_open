import { useState, type FormEvent } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useLocation, useNavigate, useParams } from "react-router-dom";
import { IpcErrorNotice, LoadingNotice, NotConnectedNotice } from "../components/IpcNotice";
import { ContextWorkspaceSelect } from "../components/ContextWorkspaceSelect";
import { attributions, origins, recordKinds, scopeKind, scopeLabel, useListFilters, useContextScopes, useContextWorkspace, type RecordDetail, type RecordsResult, type Scope } from "../lib/context";
import { formatDateTime } from "../lib/format";
import { hasDesktopBridge, invoke } from "../lib/ipc";

export function RecordForm({ scopes, scopeId, onClose }: { scopes: Scope[]; scopeId?: string; onClose: () => void }) {
  const client = useQueryClient();
  const navigate = useNavigate();
  const workspace = useContextWorkspace();
  const save = useMutation({
    networkMode: "always",
    mutationFn: (args: Record<string, unknown>) => invoke<RecordDetail>("context_record_add", { args, ...workspace.args }),
    onSuccess: detail => {
      void client.invalidateQueries({ queryKey: ["context_records"] });
      client.setQueryData(["context_record_get", detail.record.id, workspace.localOnly], detail);
      navigate(`/records/${encodeURIComponent(detail.record.id)}${workspace.suffix}`);
    },
  });
  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const data = new FormData(event.currentTarget);
    const time = String(data.get("occurredAt") || "");
    save.mutate({ title: data.get("title"), content: data.get("content"), kind: data.get("kind"), scopeIds: data.getAll("scopes"),
      materials: String(data.get("materials")).split(/\r?\n/).map(line => line.trim()).filter(Boolean),
      ...(time && { occurredAt: new Date(time).getTime() }) });
  }
  return <form onSubmit={submit} className="ctx-form">
    <fieldset disabled={save.isPending}>
      <legend>新增工作记录</legend>
      <label>记录原文<textarea autoFocus name="content" required rows={5} /></label>
      <details><summary>标题、类型与关联资料（可选）</summary>
      <label>标题<input name="title" /></label>
      <label>记录类型<select name="kind">{Object.entries(recordKinds).map(([key, label]) => <option key={key} value={key}>{label}</option>)}</select></label>
      <label>发生时间（可选）<input type="datetime-local" name="occurredAt" /></label>
      {scopes.length > 0 && <fieldset className="ctx-checks"><legend>关联项目或领域（可选）</legend>{scopes.map(scope => <label key={scope.id}><input type="checkbox" name="scopes" value={scope.id} defaultChecked={scope.id === scopeId} />{scopeLabel(scope, scopes)}</label>)}</fieldset>}
      <label>资料引用（可选，每行一条）<textarea name="materials" rows={2} /></label>
      </details>
      {save.isError && <IpcErrorNotice title="保存工作记录失败" error={save.error} />}
      <div className="ctx-actions"><button className="btn btn-primary" type="submit">保存记录</button><button className="btn btn-secondary" type="button" onClick={onClose}>取消</button></div>
    </fieldset>
  </form>;
}

type RecordFilters = { origin?: string; fromMs?: number; untilMs?: number; unclassified?: boolean; section?: string };

export function RecordList({ scopeId, query = "", filters = {} }: { scopeId?: string; query?: string; filters?: RecordFilters }) {
  const workspace = useContextWorkspace();
  const location = useLocation();
  const url = useListFilters();
  const pageOffset = Number(url.get("offset"));
  const offset = Number.isSafeInteger(pageOffset) && pageOffset > 0 ? pageOffset : 0;
  const args = { scopeId, query: query || undefined, offset, ...filters };
  const result = useQuery({ queryKey: ["context_records", args, workspace.localOnly], networkMode: "always",
    queryFn: () => invoke<RecordsResult>("context_records", { args, ...workspace.args }), retry: false, refetchOnMount: "always" });
  if (result.isError) return <IpcErrorNotice title="读取工作记录失败" error={result.error} onRetry={() => void result.refetch()} />;
  if (!result.data || result.isFetching) return <LoadingNotice label="正在读取工作记录…" />;
  const { items, total } = result.data;
  return <><ul className="ctx-list ctx-timeline">{items.map((row, index) => {
    const day = new Date(row.occurredAt ?? row.createdAt).toLocaleDateString("zh-CN", { year: "numeric", month: "long", day: "numeric" });
    const prior = items[index - 1];
    const startsDay = !prior || new Date(prior.occurredAt ?? prior.createdAt).toDateString() !== new Date(row.occurredAt ?? row.createdAt).toDateString();
    const highlights = [...row.highlights ?? [], ...(filters.section === "questions" ? row.derived?.flatMap(f => f.classification.openQuestions) ?? [] : [])];
    return <li key={row.id}>{startsDay && <h3 className="ctx-day">{day}</h3>}
      <Link to={`/records/${encodeURIComponent(row.id)}${workspace.suffix}`} state={{ from: location.pathname + location.search, parent: location.state }}>{row.title}</Link>
      {row.content && row.title !== row.content && <p className="ctx-excerpt">{row.content.length > 160 ? `${row.content.slice(0, 160)}…` : row.content}</p>}
      {highlights.map((value, i) => <p className="ctx-prose" key={i}>{value}</p>)}
      <div className="ctx-meta">{[recordKinds[row.kind], origins[row.origin], attributions[row.attribution], row.occurredAt != null ? `发生时间 ${formatDateTime(row.occurredAt)}` : `录入时间 ${formatDateTime(row.createdAt)}`, ...(row.scopes.length ? row.scopes.map(s => s.title) : ["未归类"])].filter(Boolean).join(" · ")}</div>
    </li>;
  })}</ul>
    {items.length === 0 && <p className="ctx-muted">{query ? "没有匹配的工作记录。" : "暂无工作记录。"}</p>}
    {(offset > 0 || offset + items.length < total) && <div className="ctx-actions"><button className="btn btn-secondary" disabled={!offset} onClick={() => url.set("offset", String(Math.max(0, offset - 50)))}>上一页</button><span>共 {total} 条</span><button className="btn btn-secondary" disabled={!items.length || offset + items.length >= total} onClick={() => url.set("offset", String(offset + items.length))}>下一页</button></div>}
  </>;
}

export function RecordsPage() {
  const { localOnly } = useContextWorkspace();
  return <RecordsView key={String(localOnly)} localOnly={localOnly} />;
}

function RecordsView({ localOnly }: { localOnly: boolean }) {
  const scopes = useContextScopes(localOnly);
  const url = useListFilters();
  const query = url.get("q"), scopeId = url.get("scope"), date = url.get("date");
  const start = new Date(`${date}T00:00:00`);
  const end = new Date(start);
  end.setDate(end.getDate() + 1);
  const filters = { origin: url.get("origin") || undefined, unclassified: scopeId === "unclassified" || undefined,
    ...(Number.isFinite(start.getTime()) && { fromMs: start.getTime(), untilMs: end.getTime() }) };
  const [adding, setAdding] = useState(false);
  return <div className="page ctx-page"><header className="page-header ctx-heading"><h1 className="page-title">工作记录</h1><button className="btn btn-primary" disabled={!hasDesktopBridge() || !scopes.data || adding} onClick={() => setAdding(true)}>新增记录</button></header>
    {!hasDesktopBridge() ? <NotConnectedNotice compact /> : <>
      <ContextWorkspaceSelect />
      {scopes.isError && <IpcErrorNotice title="读取项目与领域失败" error={scopes.error} onRetry={() => void scopes.refetch()} />}
      {adding && scopes.data && <RecordForm scopes={scopes.data} scopeId={scopeId} onClose={() => setAdding(false)} />}
      <div className="ctx-filters"><label>搜索工作记录<input type="search" value={query} onChange={e => url.set("q", e.target.value)} /></label>
      <label>日期<input type="date" value={date} onChange={e => url.set("date", e.target.value)} /></label>
      <label>来源<select value={url.get("origin")} onChange={e => url.set("origin", e.target.value)}><option value="">全部来源</option>{Object.entries(origins).map(([k,v]) => <option key={k} value={k}>{v}</option>)}</select></label>
      <label>项目或领域<select value={scopeId} onChange={e => url.set("scope", e.target.value)}><option value="">全部</option><option value="unclassified">未归类</option>{scopes.data?.map(s => <option key={s.id} value={s.id}>{scopeLabel(s, scopes.data)}</option>)}</select></label></div>
      <RecordList scopeId={filters.unclassified ? undefined : scopeId || undefined} query={query.trim()} filters={filters} />
    </>}
  </div>;
}

function CorrectionForm({ detail, onClose }: { detail: RecordDetail; onClose: () => void }) {
  const { record, correction } = detail;
  const workspace = useContextWorkspace();
  const scopes = useContextScopes(workspace.localOnly);
  const client = useQueryClient();
  const [lockText, setLockText] = useState(correction.content !== null);
  const [lockScopes, setLockScopes] = useState(correction.scopeIds !== null);
  const save = useMutation({ mutationFn: (args: Record<string, unknown>) => invoke<RecordDetail>("context_record_correct", { args, ...workspace.args }),
    networkMode: "always",
    onSuccess: updated => {
      client.setQueryData(["context_record_get", record.id, workspace.localOnly], updated);
      void client.invalidateQueries({ queryKey: ["context_records"] });
      onClose();
    },
  });
  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const form = new FormData(event.currentTarget);
    save.mutate({ id: record.id, version: record.version, content: lockText ? form.get("content") : null,
      attribution: lockText ? form.get("attribution") : null, scopeIds: lockScopes ? form.getAll("scopeIds") : null });
  }
  return <form className="ctx-form" onSubmit={submit}><fieldset disabled={save.isPending}><legend>纠正记录</legend>
    <label><input type="checkbox" checked={lockText} onChange={e => setLockText(e.target.checked)} />固定正文与归因</label>
    {lockText && <><label>修订正文<textarea name="content" required defaultValue={correction.content ?? record.content} rows={4} /></label>
      <label>正文归因<select name="attribution" defaultValue={correction.attribution ?? "reported"}>
        <option value="reported">来源陈述</option><option value="inference">推断</option><option value="confirmed">本人确认</option></select></label></>}
    <label><input type="checkbox" checked={lockScopes} onChange={e => setLockScopes(e.target.checked)} />固定范围（可全部取消以解除关联）</label>
    {lockScopes && (scopes.isError ? <IpcErrorNotice title="读取项目与领域失败" error={scopes.error} onRetry={() => void scopes.refetch()} /> : scopes.data ?
      <fieldset className="ctx-checks"><legend>项目或领域</legend>{scopes.data.map(s => <label key={s.id}><input type="checkbox" name="scopeIds" value={s.id} defaultChecked={record.scopes.some(v => v.id === s.id)} />{scopeLabel(s, scopes.data)}</label>)}</fieldset> : <LoadingNotice label="正在读取范围…" />)}
    <p className="ctx-muted">取消固定并保存，可恢复来源或自动整理的结果。</p>
    {save.isError && <IpcErrorNotice title="保存修订失败" error={save.error} />}
    <div className="ctx-actions"><button className="btn btn-primary" disabled={lockScopes && !scopes.data}>保存修订</button><button type="button" className="btn btn-secondary" onClick={onClose}>取消</button></div>
  </fieldset></form>;
}

function Statements({ title, values }: { title: string; values?: string[] }) {
  if (!values?.length) return null;
  return <section><h3>{title}</h3><ul>{values.map((text, i) => <li key={i} className="ctx-prose">{text}</li>)}</ul></section>;
}

export function RecordDetailPage() {
  const { id } = useParams();
  const workspace = useContextWorkspace();
  const location = useLocation();
  const from = location.state?.from ?? `/records${workspace.suffix}`;
  const [sourceOpen, setSourceOpen] = useState(false);
  const [editing, setEditing] = useState(false);
  const key = ["context_record_get", id, workspace.localOnly];
  const detail = useQuery({ queryKey: sourceOpen ? [...key, "source"] : key, queryFn: () => invoke<RecordDetail>("context_record_get", { id, ...workspace.args, ...(sourceOpen && { includeSource: true }) }), enabled: hasDesktopBridge(), retry: false, refetchOnMount: "always", networkMode: "always", gcTime: sourceOpen ? 0 : undefined });
  const data = detail.isError || detail.isFetching ? undefined : detail.data;
  return <div className="page ctx-page"><Link to={from} state={location.state?.parent}>{from.startsWith("/search") ? "返回全局检索" : from.startsWith("/projects/") ? "返回项目详情" : "返回工作记录"}</Link><header className="page-header"><h1 className="page-title">{data?.record.title ?? "工作记录详情"}</h1></header>
    {!hasDesktopBridge() ? <NotConnectedNotice compact /> : detail.isError ? <IpcErrorNotice title="读取记录失败" error={detail.error} onRetry={() => void detail.refetch()} /> : !data ? <LoadingNotice label="正在读取工作记录…" /> : <>
      <p className="ctx-meta">{[recordKinds[data.record.kind], origins[data.record.origin], data.record.producer, attributions[data.record.attribution], `录入时间 ${formatDateTime(data.record.createdAt)}`].filter(Boolean).join(" · ")}</p>
      {data.record.occurredAt != null && <p>发生时间　{formatDateTime(data.record.occurredAt)}</p>}
      {data.record.scopes.length > 0 && <div className="ctx-actions"><span>{data.record.scopeBasis === "organizer" ? "建议范围" : data.record.scopeBasis === "correction" ? "本人指定范围" : "来源关联范围"}</span>{data.record.scopes.map(s => <Link key={s.id} to={`/projects/${encodeURIComponent(s.id)}${workspace.suffix}`}>{scopeKind(s)} · {s.title}</Link>)}</div>}
      {data.record.content !== data.original && <section><h2>当前正文</h2><p className="ctx-prose">{data.record.content}</p></section>}
      {data.original && <section><h2>记录原文</h2><p className="ctx-prose">{data.original}</p></section>}
      {data.record.sourceRef && <div className="ctx-source"><button className="btn btn-secondary" aria-expanded={sourceOpen} onClick={() => setSourceOpen(!sourceOpen)}>{sourceOpen ? "收起来源详情" : "查看来源详情"}</button>
        {sourceOpen && <div><p className="ctx-meta">来源引用　<code>{data.record.sourceRef}</code></p>{data.archiveSource && <><p className="ctx-meta">归档消息时间　{formatDateTime(data.archiveSource.occurredAt)}</p><p className="ctx-prose">{data.archiveSource.body || "消息无可展示正文。"}</p><Statements title="归档附件摘录" values={data.archiveSource.attachments} /></>}</div>}</div>}
      {data.reportedResult && <section><h2>报告结果</h2><p>{data.reportedResult}</p></section>}
      {data.inference && <section><h2>报告者推断</h2><p>{data.inference}</p></section>}
      <Statements title="报告者的未决问题" values={data.openQuestions} />
      {data.record.derived?.map((facet, i) => <section key={`${facet.sourceRef}-${i}`}><h2>自动整理 · {facet.title}</h2>
        <Statements title="来源摘录" values={facet.classification.facts} /><Statements title="AI 推断" values={facet.classification.inferences} />
        <Statements title="未决问题" values={facet.classification.openQuestions} />
        {sourceOpen && <p className="ctx-meta">依据　<code>{facet.sourceRef}</code> · 版本 {facet.version}</p>}</section>)}
      {data.correction && (editing ? <CorrectionForm key={data.record.id} detail={data} onClose={() => { setEditing(false); setSourceOpen(false); }} /> :
        <button className="btn btn-secondary" onClick={() => setEditing(true)}>纠正记录</button>)}
      <section><h2>资料引用</h2>{data.materials.length ? <ul className="ctx-materials">{data.materials.map((m, i) => <li key={i}>{m}</li>)}</ul> : <p className="ctx-muted">未附资料。</p>}</section>
    </>}
  </div>;
}
