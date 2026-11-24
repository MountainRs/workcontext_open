import { useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { invoke } from "../lib/ipc";

interface AccessView {
  profiles: string[];
  profile: string | null;
  enabled: boolean;
  readArchive: boolean;
  archiveProfile: string | null;
}

export default function ExternalAccess() {
  const [choice, setChoice] = useState("");
  const status = useQuery({ queryKey: ["external-access"], queryFn: () => invoke<AccessView>("external_access_status") });
  const view = status.data;
  const profile = view?.profile ?? (view?.profiles.includes(choice) ? choice : view?.profiles[0]) ?? "";
  const change = useMutation({
    mutationFn: (args: { enabled: boolean; readArchive: boolean }) => invoke<void>("external_access_set", { ...args, profile }),
    onSettled: async () => { await status.refetch(); },
  });
  const busy = status.isError || !view || change.isPending;
  return <section className="settings-section" aria-labelledby="external-access-title">
    <h2 id="external-access-title" className="panel-title">本地外部接口</h2>
    <p className="panel-note">允许本机 Agent 查看此账号的工作记录、追加成果和提交建议。模型授权单独管理。</p>
    {view && view.profiles.length > 0 && <label>绑定账号{" "}
      <select value={profile} disabled={busy || view.enabled} onChange={event => setChoice(event.target.value)}>
        {view.profiles.map(p => <option key={p}>{p}</option>)}
      </select>
    </label>}
    <div className="settings-actions">
      <button type="button" className="btn btn-secondary" role="switch" aria-checked={!!view?.enabled} aria-label="本地外部接口"
        disabled={busy || !profile} onClick={() => change.mutate({ enabled: !view?.enabled, readArchive: false })}>
        {view?.enabled ? "关闭本地外部接口" : "开启本地外部接口"}
      </button>
    </div>
    <label><input type="checkbox" checked={!!view?.readArchive}
      disabled={busy || !view?.enabled || profile !== view.archiveProfile}
      onChange={event => change.mutate({ enabled: true, readArchive: event.target.checked })} />
      允许外部 Agent 搜索和读取本机已脱敏的钉钉归档依据
    </label>
    <p className="panel-note" role="status">{change.error?.message || status.error?.message ||
      (change.isPending ? "正在保存…" : !view ? "正在读取授权…" : !view.profiles.length ? "请先注册账号来源或完成首次归档同步。" :
       view.enabled ? "本地外部接口已开启" : "本地外部接口已关闭")}</p>
  </section>;
}
