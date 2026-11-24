import { useState, type FormEvent } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useLocation, useNavigate, useParams } from "react-router-dom";
import { IpcErrorNotice, LoadingNotice, NotConnectedNotice } from "../components/IpcNotice";
import { ContextWorkspaceSelect } from "../components/ContextWorkspaceSelect";
import { scopeKind, scopeLabel, scopeQueryKey, useListFilters, useContextScopes, useContextWorkspace, type Scope } from "../lib/context";
import { hasDesktopBridge, invoke, type ItemSummary } from "../lib/ipc";
import { ATTENTION_LABEL, LIFECYCLE_LABEL } from "../lib/format";
import { RecordForm, RecordList } from "./RecordsPage";

function ScopeForm({ scope, onClose }: { scope?: Scope; onClose: () => void }) {
  const navigate = useNavigate();
  const workspace = useContextWorkspace();
  const client = useQueryClient();
  // Keep the edit revision paired with the text initially shown in this form.
  const [initial] = useState(scope);
  const save = useMutation({
    mutationFn: (args: Record<string, unknown>) => invoke<Scope>("context_scope_save", { args, ...workspace.args }),
    onSuccess: saved => {
      client.setQueryData<Scope[]>(scopeQueryKey(workspace.localOnly), old => [...(old ?? []).filter(s => s.id !== saved.id), saved]);
      for (const key of ["context_scopes", "context_records", "context_record_get", "item", "items", "ownership_view"]) {
        void client.invalidateQueries({ queryKey: [key] });
      }
      onClose();
      navigate(`/projects/${encodeURIComponent(saved.id)}${workspace.suffix}`);
    },
  });
  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const data = new FormData(event.currentTarget);
    save.mutate({ id: initial?.id, version: initial?.version, kind: initial?.kind ?? data.get("kind"), title: data.get("title"), goal: data.get("goal"), background: data.get("background") });
  }
  return <form className="ctx-form" onSubmit={submit}><fieldset disabled={save.isPending}>
    <legend>{initial ? `编辑${scopeKind(initial)}` : "新建项目或领域"}</legend>
    {!initial && <label>类型<select name="kind"><option value="project">项目</option><option value="domain">领域</option></select></label>}
    <label>名称<input autoFocus required name="title" defaultValue={initial?.title} /></label>
    <label>目标<textarea name="goal" rows={2} defaultValue={initial?.goal} /></label>
    <label>背景<textarea name="background" rows={4} defaultValue={initial?.background} /></label>
    {save.isError && <IpcErrorNotice title="未保存更改" error={save.error} />}
    <div className="ctx-actions"><button className="btn btn-primary" type="submit">保存</button><button className="btn btn-secondary" type="button" onClick={onClose}>取消</button></div>
  </fieldset></form>;
}

type LinkedItem = Pick<ItemSummary, "id" | "title" | "lifecycle" | "attention"> & { nextAction?: string | null; conclusion?: string | null };

function ScopeItems({ id }: { id: string }) {
  const workspace = useContextWorkspace();
  const items = useQuery({ queryKey: ["context_scope_items", id, workspace.localOnly], queryFn: () => invoke<LinkedItem[]>("context_scope_items", { id, ...workspace.args }), retry: false });
  if (items.isError) return <IpcErrorNotice title="读取关联事项失败" error={items.error} onRetry={() => void items.refetch()} />;
  if (!items.data) return <LoadingNotice label="正在读取关联事项…" />;
  if (!items.data.length) return <p className="ctx-muted">暂无关联事项。</p>;
  return <ul className="ctx-list">{items.data.map(item => <li key={item.id}>
    <Link to={`/items/${encodeURIComponent(item.id)}`}>{item.title}</Link>
    <div className="ctx-meta">{LIFECYCLE_LABEL[item.lifecycle]} · {ATTENTION_LABEL[item.attention]}</div>
    {item.nextAction && <p>下一步　{item.nextAction}</p>}
    {item.conclusion && <p>结论　{item.conclusion}</p>}
  </li>)}</ul>;
}

export function ProjectsPage() {
  const { id } = useParams();
  const workspace = useContextWorkspace();
  return <ProjectView key={JSON.stringify([id, workspace.localOnly])} id={id} />;
}

function ProjectView({ id }: { id?: string }) {
  const workspace = useContextWorkspace();
  const scopes = useContextScopes(workspace.localOnly);
  const url = useListFilters();
  const location = useLocation();
  const query = url.get("q");
  const section = url.get("section");
  const projectList = new URLSearchParams(location.search);
  projectList.delete("section");
  projectList.delete("offset");
  const [editing, setEditing] = useState(false);
  const [addingRecord, setAddingRecord] = useState(false);
  const selected = scopes.data?.find(s => s.id === id);
  const searchOrigin = location.state?.from?.startsWith("/search") ? location.state.from : undefined;
  return <div className="page ctx-page">{id && <Link to={searchOrigin ?? { pathname: "/projects", search: projectList.toString() }}>{searchOrigin ? "返回全局检索" : "返回重点项目"}</Link>}
    <header className="page-header ctx-heading"><h1 className="page-title">{selected?.title ?? "重点项目"}</h1>
      {(!id || selected) && <button className="btn btn-primary" disabled={!hasDesktopBridge() || editing || !scopes.data} onClick={() => setEditing(true)}>{selected ? `编辑${scopeKind(selected)}` : "新建项目或领域"}</button>}
    </header>
    {!id && hasDesktopBridge() && <><ContextWorkspaceSelect /><div className="ctx-filters"><label>搜索项目或领域<input type="search" value={query} onChange={e => url.set("q", e.target.value)} /></label></div></>}
    {!hasDesktopBridge() ? <NotConnectedNotice compact /> : scopes.isError ? <IpcErrorNotice title="读取项目与领域失败" error={scopes.error} onRetry={() => void scopes.refetch()} /> : !scopes.data ? <LoadingNotice label="正在读取项目与领域…" /> : <>
      {editing && <ScopeForm scope={selected} onClose={() => setEditing(false)} />}
      {selected ? <>
        <section><h2>{scopeKind(selected)}目标</h2><p className="ctx-prose">{selected.goal || "尚未填写目标。"}</p><h2>背景</h2><p className="ctx-prose">{selected.background || "尚未填写背景。"}</p></section>
        <section><div className="ctx-heading"><h2>关联工作记录</h2><button className="btn btn-secondary" disabled={addingRecord} onClick={() => setAddingRecord(true)}>新增记录</button></div>
          {addingRecord && <RecordForm scopes={scopes.data} scopeId={selected.id} onClose={() => setAddingRecord(false)} />}
          <div className="ctx-sections" role="group" aria-label="项目记录分类">{Object.entries({ "": "近期动态", progress: "当前进展", decision: "结论", questions: "未决问题", materials: "资料" }).map(([value, label]) => <button type="button" key={value} aria-pressed={section === value} onClick={() => url.set("section", value)}>{label}</button>)}</div>
          <RecordList scopeId={selected.id} filters={{ section: section || undefined }} />
        </section>
        <section><h2>关联事项</h2><ScopeItems id={selected.id} /></section>
      </> : id ? <p role="alert">当前档案中找不到这个项目或领域。</p> : (["project", "domain"] as const).map(kind => {
        const keyword = query.trim().toLocaleLowerCase();
        const entries = scopes.data.filter(s => s.kind === kind && [s.title, s.goal, s.background, s.id].some(v => v.toLocaleLowerCase().includes(keyword)));
        const label = kind === "project" ? "项目" : "领域";
        return <section key={kind}><h2>{label}</h2>{entries.length ? <ul className="ctx-list">{entries.map(s => <li key={s.id}><Link to={{ pathname: `/projects/${encodeURIComponent(s.id)}`, search: projectList.toString() }}>{s.title}</Link><p className="ctx-meta">{scopes.data.some(other => other.id !== s.id && other.kind === s.kind && other.title === s.title) ? scopeLabel(s, scopes.data) : s.goal || s.background}</p></li>)}</ul> : <p className="ctx-muted">{keyword ? `没有匹配的${label}。` : `暂无${label}。`}</p>}</section>;
      })}
    </>}
  </div>;
}
