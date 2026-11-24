import { useQuery } from "@tanstack/react-query";
import { useSearchParams } from "react-router-dom";
import { hasDesktopBridge, invoke } from "../lib/ipc";
import { IpcErrorNotice } from "./IpcNotice";

export function ContextWorkspaceSelect() {
  const [params, setParams] = useSearchParams();
  const availability = useQuery({ queryKey: ["context_local_available"], queryFn: () => invoke<boolean>("context_local_available"), enabled: hasDesktopBridge(), retry: false });
  if (availability.isError) return <IpcErrorNotice title="读取本机档案状态失败" error={availability.error} onRetry={() => void availability.refetch()} />;
  if (!availability.data && params.get("local") !== "1") return null;
  return <div className="ctx-filters"><label>工作区<select value={params.get("local") === "1" ? "local" : "current"} onChange={event => {
    const next = new URLSearchParams(params);
    if (event.target.value === "local") next.set("local", "1");
    else next.delete("local");
    setParams(next);
  }}><option value="current">当前档案</option><option value="local">绑定前的本机记录</option></select></label></div>;
}
