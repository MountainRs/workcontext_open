import { useState, type FormEvent } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { IpcErrorNotice, LoadingNotice, NotConnectedNotice } from "../components/IpcNotice";
import { hasDesktopBridge, invoke } from "../lib/ipc";
import "./SkillsPage.css";

type Entry = { name: string; purpose: string; path: string; scopeId: string | null };
type Registry = { profile: string | null; scopes: { id: string; title: string }[]; items: (Entry & { global: boolean; available: boolean })[] };

function Register({ registry, close, saved }: { registry: Registry; close: () => void; saved: () => void }) {
  const [target, setTarget] = useState(registry.profile ? "profile" : "global");
  const mutation = useMutation({
    mutationFn: (entry: Entry) => invoke<void>("skills_register", { profile: target === "global" ? null : registry.profile, entry }),
    onSuccess: saved,
  });
  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const data = new FormData(event.currentTarget);
    mutation.mutate({ name: String(data.get("name")).trim(), purpose: String(data.get("purpose")).trim(), path: String(data.get("path")).trim(), scopeId: target.startsWith("scope:") ? target.slice(6) : null });
  }
  return <form className="ctx-form" onSubmit={submit}><fieldset disabled={mutation.isPending}>
    <legend>登记已有能力</legend>
    <label>名称<input name="name" autoFocus required /></label>
    <label>用途<textarea name="purpose" required rows={2} /></label>
    <label>原始 SKILL.md 绝对路径<input name="path" required spellCheck={false} /></label>
    <label>关联范围<select value={target} onChange={event => setTarget(event.target.value)}>
      <option value="global">全局（所有账号）</option>
      {registry.profile && <option value="profile">当前账号 · {registry.profile}</option>}
      {registry.scopes.map(scope => <option key={scope.id} value={`scope:${scope.id}`}>{scope.title}</option>)}
    </select></label>
    {mutation.isError && <IpcErrorNotice title="未登记能力" error={mutation.error} />}
    <div className="ctx-actions"><button className="btn btn-primary" type="submit">{mutation.isPending ? "正在登记…" : "保存登记"}</button><button className="btn btn-secondary" type="button" onClick={close}>取消</button></div>
  </fieldset></form>;
}

export function SkillsPage() {
  const desktop = hasDesktopBridge();
  const client = useQueryClient();
  const registry = useQuery({ queryKey: ["skills_list"], queryFn: () => invoke<Registry>("skills_list"), enabled: desktop, retry: false });
  const [registering, setRegistering] = useState(false);
  const [search, setSearch] = useState("");
  const [scopeFilter, setScopeFilter] = useState("");
  const [notice, setNotice] = useState("");
  const data = registry.data;
  const query = search.trim().toLowerCase();
  const rows = data?.items.filter(item => (item.name.toLowerCase().includes(query) || item.purpose.toLowerCase().includes(query)) && (!scopeFilter || scopeFilter === (item.global ? "global" : item.scopeId ? `scope:${item.scopeId}` : "profile")));
  const scopes = new Map(data?.scopes.map(scope => [scope.id, scope.title]));
  return <div className="page ctx-page skills-page">
    <header className="page-header ctx-heading"><h1 className="page-title">能力索引</h1><button className="btn btn-primary" disabled={!desktop || !data || registering || registry.isError} onClick={() => { setNotice(""); setRegistering(true); }}>登记能力</button></header>
    {!desktop ? <NotConnectedNotice compact /> : registry.isError ? <IpcErrorNotice title="读取能力索引失败" error={registry.error} onRetry={() => void registry.refetch()} /> : !data ? <LoadingNotice label="正在读取能力索引…" /> : <>
      {registering && <Register key={data.profile} registry={data} close={() => setRegistering(false)} saved={() => { setRegistering(false); setNotice("能力已登记"); setSearch(""); setScopeFilter(""); void client.invalidateQueries({ queryKey: ["skills_list"] }); }} />}
      {notice && <p role="status" className="ctx-muted">{notice}</p>}
      <div className="skills-filters"><label>查找能力<input type="search" value={search} placeholder="搜索名称或用途" onChange={e => setSearch(e.target.value)} /></label>
        <label>筛选范围<select value={scopeFilter} onChange={e => setScopeFilter(e.target.value)}><option value="">全部范围</option><option value="global">全局</option>{data.profile && <option value="profile">当前账号（未关联范围）</option>}{data.scopes.map(scope => <option key={scope.id} value={`scope:${scope.id}`}>{scope.title}</option>)}</select></label>
        <button className="btn btn-secondary" disabled={registry.isFetching} onClick={() => void registry.refetch()}>刷新</button></div>
      {!rows?.length ? <p className="ctx-muted">{data.items.length ? "没有匹配的能力。" : "尚未登记能力。可登记已有 SKILL.md 的名称、用途和原始路径。"}</p> : <ul className="ctx-list skills-list">{rows.map((item, index) => <li key={`${item.path}-${index}`}>
        <div className="skills-row"><h2>{item.name}</h2><span className={item.available ? "skills-available" : "skills-unavailable"}>{item.available ? "文件可用" : "文件缺失或不可读"}</span></div>
        <p>{item.purpose}</p><div className="ctx-meta">关联范围 · {item.global ? "全局" : item.scopeId ? scopes.get(item.scopeId) ?? item.scopeId : data.profile}</div><code className="skills-path">{item.path}</code>
      </li>)}</ul>}
    </>}
  </div>;
}
