import { useState, type FormEvent } from "react";
import { useQuery } from "@tanstack/react-query";
import { Link, useLocation, useSearchParams } from "react-router-dom";
import { IpcErrorNotice, LoadingNotice, NotConnectedNotice } from "../components/IpcNotice";
import { ContextWorkspaceSelect } from "../components/ContextWorkspaceSelect";
import { origins, recordKinds, useContextWorkspace } from "../lib/context";
import { ATTENTION_LABEL, formatDateTime } from "../lib/format";
import { hasDesktopBridge, invoke } from "../lib/ipc";
import { RecordForm } from "./RecordsPage";

type Hit = { id: string; title: string; objectType: "project" | "domain" | "material" | "item" | "record"; recordId?: string;
  time: number | null; summary?: string; kind?: keyof typeof recordKinds; origin: keyof typeof origins | "ownership";
  attention?: keyof typeof ATTENTION_LABEL; nextAction?: string; firstOpenQuestion?: string; lifecycle?: string };
type Page<T> = {items: T[]; nextCursor: string | null};
type Brief = Hit & {goal: string; activity: Page<Hit>};
type Overview = Page<Hit> & {projects: Page<Brief>; attentionItems: Page<Hit>; watermarks: {pendingInputs: number}};
const typeLabel = {project: "项目", domain: "领域", material: "资料引用", item: "跟进事项", record: "工作记录"};
const ipcOptions = { retry: false as const, networkMode: "always" as const, gcTime: 0, refetchOnMount: "always" as const };

function HitList({items, suffix, empty}: {items: Hit[]; suffix: string; empty: string}) {
  const location = useLocation();
  if (!items.length) return <p className="ctx-muted">{empty}</p>;
  return <ul className="ctx-list">{items.map(hit => {
    const group = hit.objectType === "item" ? "items" : ["project", "domain"].includes(hit.objectType) ? "projects" : "records";
    const label = hit.kind ? recordKinds[hit.kind] : typeLabel[hit.objectType];
    const origin = hit.origin === "ownership" ? "会话事项" : origins[hit.origin];
    const status = hit.lifecycle === "candidate" ? "待确认" : hit.attention && ATTENTION_LABEL[hit.attention];
    const excerpt = hit.nextAction?.trim() || hit.summary?.trim();
    return <li key={`${hit.objectType}:${hit.id}`}><Link state={location.pathname === "/search" ? {from: `${location.pathname}${location.search}`} : undefined} to={`/${group}/${encodeURIComponent(hit.recordId ?? hit.id)}${suffix}`}>{hit.title}</Link>
      <p className="ctx-meta">{[label, status, origin, hit.time ? formatDateTime(hit.time) : "时间未知"].filter(Boolean).join(" · ")}</p>
      {excerpt && excerpt !== hit.title.trim() && <p className="ctx-excerpt">{excerpt}</p>}</li>;
  })}</ul>;
}

export function OverviewPage() {
  const workspace = useContextWorkspace();
  const [adding, setAdding] = useState(false);
  const desktop = hasDesktopBridge();
  const view = useQuery({ ...ipcOptions, enabled: desktop, queryKey: ["workspace_overview", workspace.localOnly],
    queryFn: () => invoke<Overview>("workspace_overview", workspace.args) });
  const data = view.data;
  return <div className="page ctx-page"><header className="page-header"><h1 className="page-title">工作概览</h1></header>
    <ContextWorkspaceSelect />
    {adding ? <RecordForm key={workspace.suffix} scopes={[]} onClose={() => setAdding(false)} /> : <button type="button" className="workspace-quick" disabled={!desktop} onClick={() => setAdding(true)}>＋ 补充一条工作信息…</button>}
    {!desktop ? <NotConnectedNotice /> : view.isError ? <IpcErrorNotice title="工作概览读取失败" error={view.error} onRetry={() => void view.refetch()} /> : !data || view.isFetching ? <LoadingNotice label="正在读取工作概览…" /> : <>
      <section><div className="ctx-heading"><h2>重点项目</h2><Link to={`/projects${workspace.suffix}`}>查看项目</Link></div>
        {data.projects.items.length ? <div className="workspace-projects">{data.projects.items.map(project => {
          const latest = project.activity.items[0];
          return <article key={project.id}><Link to={`/projects/${encodeURIComponent(project.id)}${workspace.suffix}`}>{project.title}</Link>
            <p className="ctx-excerpt">{latest?.summary || project.goal || "尚未填写目标或记录进展。"}</p>
            {latest?.firstOpenQuestion && <p className="ctx-meta">待解问题 · {latest.firstOpenQuestion}</p>}</article>;
        })}</div> : <p className="ctx-muted">当前档案尚未建立重点项目。</p>}</section>
      <section><div className="ctx-heading"><h2>需要关注</h2><Link to="/follow-up">查看跟进事项</Link></div>
        <HitList items={data.attentionItems.items} suffix={workspace.suffix} empty="当前档案没有可显示的待跟进事项。" /></section>
      <section><div className="ctx-heading"><h2>近期工作动态</h2><Link to={`/records${workspace.suffix}`}>查看工作记录</Link></div>
        {data.watermarks.pendingInputs > 0 && <p className="ctx-meta">还有 {data.watermarks.pendingInputs} 条输入等待整理。<Link to="/sources#source-progress">查看整理状态</Link></p>}
        <HitList items={data.items} suffix={workspace.suffix} empty="此档案尚无可显示的工作记录。" /></section>
    </>}
  </div>;
}

type ArchiveHit = {sourceRef: string; conversation: string; occurredAt: number};
type Source = {status: string; body?: string; occurredAt?: number; attachments?: {download: string; extraction: string}[]};
const sourceStatus: Record<string,string> = { redacted: "已脱敏", missing: "来源不存在或已超出授权范围", expired: "原文已超过保留期限", body_unavailable: "当前来源没有可读取的原文" };

export function SearchPage() {
  const [params,setParams] = useSearchParams();
  const query = params.get("q") ?? "";
  const archive = params.get("type") === "archive";
  const reference = params.get("source");
  const cursor = params.get("cursor");
  const workspace = useContextWorkspace();
  const desktop = hasDesktopBridge();
  const enabled = desktop && Boolean(query.trim());
  const localResults = useQuery({...ipcOptions, queryKey:["workspace_search",workspace.localOnly,query,cursor], enabled:enabled && !archive,
    queryFn:() => invoke<Page<Hit>>("workspace_search",{query,cursor,...workspace.args})});
  const archiveResults = useQuery({...ipcOptions, queryKey:["workspace_archive_search",query,cursor], enabled:enabled && archive && !reference && !workspace.localOnly,
    queryFn:() => invoke<Page<ArchiveHit>>("workspace_archive_search",{query,cursor})});
  const source = useQuery({...ipcOptions, queryKey:["workspace_source",reference], enabled:desktop && archive && !!reference && !workspace.localOnly,
    queryFn:() => invoke<Source>("workspace_source",{reference})});
  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const form = new FormData(event.currentTarget);
    const next = new URLSearchParams(workspace.suffix);
    next.set("q",String(form.get("query") ?? "").trim());
    if (form.get("type") === "archive") next.set("type","archive");
    setParams(next);
  }
  function link(key: string,value: string | null) {
    const next = new URLSearchParams(params);
    if (value) next.set(key,value); else next.delete(key);
    return `?${next}`;
  }
  const results = archive ? archiveResults : localResults;
  return <div className="page ctx-page"><header className="page-header"><h1 className="page-title">全局检索</h1></header>
    <ContextWorkspaceSelect /><form className="ctx-filters" key={`${query}:${archive}:${workspace.localOnly}`} onSubmit={submit}>
      <label>关键词<input type="search" name="query" required maxLength={512} defaultValue={query} placeholder="输入项目、经验或来源关键词" /></label>
      <label>检索范围<select name="type" defaultValue={archive && !workspace.localOnly ? "archive" : "work"}><option value="work">工作记录、项目、事项与资料</option>{!workspace.localOnly && <option value="archive">已授权的钉钉来源</option>}</select></label>
      <button className="btn btn-primary" disabled={!desktop}>检索</button></form>
    {!desktop ? <NotConnectedNotice /> : archive && workspace.localOnly ? <p>绑定前的本机档案没有钉钉来源。</p> : archive && reference ? <section><Link to={link("source",null)}>返回检索结果</Link><h2>钉钉来源</h2>
      {source.isError ? <IpcErrorNotice title="来源读取失败" error={source.error} onRetry={() => void source.refetch()} /> : source.isFetching || !source.data ? <LoadingNotice label="正在核对来源授权…" /> : <>
        <p className="ctx-meta">{sourceStatus[source.data.status] ?? source.data.status}{source.data.occurredAt && ` · ${formatDateTime(source.data.occurredAt)}`}</p>
        {source.data.body && <p className="ctx-prose">{source.data.body}</p>}
        {source.data.attachments?.map((attachment,i) => <p key={i}>附件 {i+1} · 下载 {attachment.download} · 提取 {attachment.extraction}</p>)}
      </>}</section> : !query ? <p className="ctx-muted">输入关键词后检索当前工作区。</p> : results.isError ? <IpcErrorNotice title="检索失败" error={results.error} onRetry={() => void results.refetch()} /> : results.isFetching || !results.data ? <LoadingNotice label="正在检索当前工作区…" /> : <section>
      <h2>检索结果</h2>{archive ? <>{archiveResults.data?.items.length ? <ul className="ctx-list">{archiveResults.data.items.map(hit => <li key={hit.sourceRef}><Link to={link("source",hit.sourceRef)}>钉钉消息 · {formatDateTime(hit.occurredAt)}</Link><p className="ctx-meta">会话 {hit.conversation}</p></li>)}</ul> : <p className="ctx-muted">本页没有匹配的已授权来源。</p>}</> : <HitList items={localResults.data?.items ?? []} suffix={workspace.suffix} empty="没有匹配的工作记录、项目、事项或资料。" />}
      <div className="ctx-actions">{cursor && <Link to={link("cursor",null)}>返回首页</Link>}{results.data.nextCursor && <Link to={link("cursor",results.data.nextCursor)}>下一页</Link>}</div>
    </section>}
  </div>;
}
